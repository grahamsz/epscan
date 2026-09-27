// SPDX-License-Identifier: MIT
use super::{
    CaptureBatch, CaptureJob, CaptureSelection, acquire, prepare_batches, prepare_jobs,
    result_document,
};
use crate::cli::{Action, Capture, Cli};
use clap::Parser;
use epscan::{
    Backend, Capabilities, Device, Gamma, Result, ScanMode, ScanOptions, ScanSettings, Session,
    transport::Transport,
};
use serde_json::Value;
use std::{
    collections::VecDeque,
    fs::{self, File},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

struct SimTransport {
    responses: VecDeque<u8>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Transport for SimTransport {
    fn read(&mut self, size: usize, _: Duration) -> Result<Vec<u8>> {
        let size = size.min(self.responses.len());
        Ok(self.responses.drain(..size).collect())
    }

    fn write(&mut self, bytes: &[u8], _: Duration) -> Result<usize> {
        self.writes.lock().unwrap().push(bytes.to_vec());
        Ok(bytes.len())
    }
}

fn identity() -> (Capabilities, Vec<u8>) {
    let fixture: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/v800-identity.json")).unwrap();
    let hex = fixture["extended_identity_hex"].as_str().unwrap();
    let extended: Vec<_> = (0..hex.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
        .collect();
    let caps = Capabilities::parse(&extended).unwrap();
    let mut responses = vec![2, 0x12, 2, 0, b'B', b'8'];
    responses.extend(extended);
    (caps, responses)
}

fn session(responses: Vec<u8>) -> (Session, Arc<Mutex<Vec<Vec<u8>>>>) {
    let (_, mut identity) = identity();
    identity.extend(responses);
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = Session::with_transport(
        Device {
            location: "synthetic batch scanner".into(),
            name: "simulator".into(),
            vid: 0x04b8,
            pid: 0x0151,
            backend: Backend::Nusb,
        },
        Box::new(SimTransport {
            responses: identity.into(),
            writes: writes.clone(),
        }),
        Duration::from_secs(1),
    )
    .unwrap();
    (session, writes)
}

fn capture(directory: &Path, frames: &str) -> Capture {
    let cli = Cli::try_parse_from(["epscan", "scan", "--holder", "v800-35mm", "--frame", frames])
        .unwrap();
    let Action::Scan(scan) = cli.action else {
        panic!("expected scan arguments")
    };
    let mut capture = scan.capture;
    capture.basename = directory.join("film");
    capture
}

fn rectangle_capture(directory: &Path, rectangles: &str) -> Capture {
    let cli = Cli::try_parse_from([
        "epscan",
        "scan",
        "--source",
        "film-holder",
        "--rect",
        rectangles,
        "--measure-sharpness",
    ])
    .unwrap();
    let Action::Scan(scan) = cli.action else {
        panic!("expected scan arguments")
    };
    let mut capture = scan.capture;
    capture.basename = directory.join("film");
    capture
}

fn settings() -> ScanSettings {
    ScanSettings {
        dpi: 25,
        depth: 8,
        gamma: Gamma::DeviceDefault,
        ..Default::default()
    }
}

fn options() -> ScanOptions {
    ScanOptions {
        settle_time: Duration::ZERO,
        ..Default::default()
    }
}

fn configuration(responses: &mut Vec<u8>, job: &CaptureJob, caps: &Capabilities) {
    assert_eq!(job.plan.passes.len(), 1);
    let pass = &job.plan.passes[0];
    assert_eq!(pass.settings.gamma, Gamma::DeviceDefault);
    // Reset, FS W command/payload, and focus command/payload ACKs for every capture.
    responses.extend([6; 5]);
    responses.extend(
        pass.settings
            .parameters_with_capabilities(caps, false)
            .unwrap(),
    );
    responses.extend([0; 16]);
}

fn complete_pass(responses: &mut Vec<u8>, job: &CaptureJob, caps: &Capabilities, sample: u8) {
    configuration(responses, job, caps);
    let bytes = u32::try_from(job.plan.passes[0].expected_bytes).unwrap();
    responses.extend([2, 0x12]);
    for word in [0, 0, bytes] {
        responses.extend(word.to_le_bytes());
    }
    responses.extend(std::iter::repeat_n(sample, bytes as usize));
    responses.push(0);
}

/// Give each requested frame a distinct constant intensity while the film gaps
/// remain bright. Cropping the wrong rows therefore also changes sharpness.
fn complete_strip(
    responses: &mut Vec<u8>,
    batch: &CaptureBatch,
    jobs: &[CaptureJob],
    caps: &Capabilities,
) {
    complete_pass(responses, &batch.capture, caps, 0xff);
    let source = &batch.capture.plan.passes[0];
    let payload_start = responses.len() - 1 - source.expected_bytes as usize;
    let pixel_bytes = usize::from(source.channels) * usize::from(source.settings.depth / 8);
    let stride = source.pixels[2] as usize * pixel_bytes;
    for index in &batch.targets {
        let target = &jobs[*index].plan.passes[0];
        let x = (target.pixels[0] - source.pixels[0]) as usize;
        let y = (target.pixels[1] - source.pixels[1]) as usize;
        let width = target.pixels[2] as usize * pixel_bytes;
        for row in 0..target.pixels[3] as usize {
            let start = payload_start + (y + row) * stride + x * pixel_bytes;
            responses[start..start + width].fill(0x31 + *index as u8);
        }
    }
}

fn assert_cropped_frame(metadata: &Value, job: &CaptureJob) {
    assert_eq!(metadata["derived_crop"], true);
    assert!(metadata.get("parameters_sent_hex").is_none());
    assert!(metadata.get("parameters_readback_hex").is_none());
    assert!(metadata["source_capture"]["manifest"].is_string());
    assert_eq!(
        metadata["effective_pixels"],
        serde_json::json!(job.plan.passes[0].pixels)
    );
}

fn interrupted_pass(responses: &mut Vec<u8>, job: &CaptureJob, caps: &Capabilities) -> usize {
    configuration(responses, job, caps);
    let pass = &job.plan.passes[0];
    let row_bytes = pass.pixels[2] * u32::from(pass.channels) * u32::from(pass.settings.depth / 8);
    let remaining = u32::try_from(pass.expected_bytes).unwrap() - row_bytes;
    assert!(remaining > 4);
    responses.extend([2, 0x12]);
    for word in [row_bytes, 1, remaining] {
        responses.extend(word.to_le_bytes());
    }
    responses.extend(std::iter::repeat_n(0x42, row_bytes as usize));
    responses.push(0);
    // A second block truncates after one row was already written to disk.
    responses.extend([0x42; 4]);
    row_bytes as usize
}

fn output(directory: &Path, frame: u32, extension: &str) -> PathBuf {
    directory.join(format!("film_frame{frame:02}_1.{extension}"))
}

fn manifest(directory: &Path, frame: u32) -> Value {
    serde_json::from_slice(&fs::read(output(directory, frame, "json")).unwrap()).unwrap()
}

fn assert_rgb_batch_commands(writes: &[Vec<u8>], jobs: &[CaptureJob], caps: &Capabilities) {
    assert!(!jobs.is_empty());
    for command in [b"\x1b@", b"\x1cW", b"\x1bp", b"\x1cS", b"\x1cF", b"\x1cG"] {
        assert_eq!(
            writes
                .iter()
                .filter(|bytes| bytes.as_slice() == command)
                .count(),
            jobs.len(),
            "command {} must run for every capture",
            epscan::protocol::hex(command),
        );
    }
    let parameter_payloads: Vec<_> = writes
        .windows(2)
        .filter(|pair| pair[0] == b"\x1cW")
        .map(|pair| &pair[1])
        .collect();
    for (payload, job) in parameter_payloads.into_iter().zip(jobs) {
        assert_eq!(
            payload.as_slice(),
            job.plan.passes[0]
                .settings
                .parameters_with_capabilities(caps, false)
                .unwrap(),
            "each capture must send its own ROI",
        );
    }
}

fn assert_parameter_readback(metadata: &Value, job: &CaptureJob, caps: &Capabilities) {
    let expected = epscan::protocol::hex(
        &job.plan.passes[0]
            .settings
            .parameters_with_capabilities(caps, false)
            .unwrap(),
    );
    assert_eq!(metadata["parameters_sent_hex"], expected);
    assert_eq!(metadata["parameters_readback_hex"], expected);
}

fn sharpness_capture(directory: &Path, frames: &str, raw_only: bool) -> Capture {
    let mut arguments = vec![
        "epscan",
        "scan",
        "--holder",
        "v800-35mm",
        "--frame",
        frames,
        "--overage",
        "-10",
        "--measure-sharpness",
    ];
    if raw_only {
        arguments.push("--raw-only");
    }
    let Action::Scan(scan) = Cli::try_parse_from(arguments).unwrap().action else {
        panic!("expected scan arguments");
    };
    let mut args = scan.capture;
    args.basename = directory.join("film");
    assert!(args.measure_sharpness);
    args
}

fn assert_constant_sharpness(scores: &Value, width: u32, height: u32) {
    assert_eq!(scores["method_version"], 1);
    assert_eq!(scores["sample_normalization"], "full-scale-0..1");
    assert_eq!(scores["luminance"], "rec709");
    assert_eq!(
        scores["evaluated_pixels"].as_u64(),
        Some(u64::from(width - 2) * u64::from(height - 2)),
    );
    // Constant RGB has zero derivatives. Permit only floating-point roundoff
    // from luminance/kernel evaluation, far below one normalized sample step.
    for metric in ["tenengrad", "variance_of_laplacian"] {
        let score = scores[metric].as_f64().expect("numeric sharpness score");
        assert!(score.abs() < 1e-24, "{metric}: {score}");
    }
}

#[test]
fn successful_batch_writes_distinct_frames_and_cleans_up_only_after_all_succeed() {
    let directory = tempfile::tempdir().unwrap();
    let args = capture(directory.path(), "1,2");
    let (caps, _) = identity();
    let settings = settings();
    let options = options();
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    assert_eq!(jobs.len(), 2);
    let batches = prepare_batches(&args, &jobs, &caps).unwrap();
    assert_eq!(batches.len(), 1);
    let mut responses = Vec::new();
    complete_strip(&mut responses, &batches[0], &jobs, &caps);
    let (scanner, writes) = session(responses);
    acquire(scanner, args, settings, options).unwrap();

    for (index, job) in jobs.iter().enumerate() {
        let frame = index as u32 + 1;
        let sidecar = manifest(directory.path(), frame);
        assert_eq!(sidecar["complete"], true);
        assert_eq!(sidecar["holder_selection"]["frame"], frame);
        assert_eq!(sidecar["holder_selection"]["holder"], "v800-35mm");
        assert_eq!(
            sidecar["passes"][0]["holder_selection"],
            sidecar["holder_selection"]
        );
        assert_eq!(sidecar["cleanup"]["state"], "complete");
        assert_cropped_frame(&sidecar["passes"][0], job);
        assert!(sidecar["passes"][0]["payload_file"].is_null());
        assert_eq!(sidecar["passes"][0]["payload_retained"], false);
        assert!(!output(directory.path(), frame, "bin").exists());
        assert!(!output(directory.path(), frame, "protocol.jsonl").exists());
        assert!(!output(directory.path(), frame, "partial.bin").exists());

        let mut tiff = tiff::decoder::Decoder::new(
            File::open(output(directory.path(), frame, "tiff")).unwrap(),
        )
        .unwrap();
        let pass = &job.plan.passes[0];
        assert_eq!(tiff.dimensions().unwrap(), (pass.pixels[2], pass.pixels[3]));
        let tiff::decoder::DecodingResult::U8(samples) = tiff.read_image().unwrap() else {
            panic!("expected 8-bit RGB TIFF")
        };
        assert_eq!(samples.len() as u64, pass.expected_bytes);
        assert!(samples.iter().all(|sample| *sample == 0x31 + index as u8));
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 5);
    let source: Value =
        serde_json::from_slice(&fs::read(directory.path().join("film_strip01_1.json")).unwrap())
            .unwrap();
    assert_eq!(source["cleanup"]["state"], "complete");
    assert!(source["passes"][0]["payload_file"].is_null());
    assert_parameter_readback(&source["passes"][0], &batches[0].capture, &caps);
    let writes = writes.lock().unwrap();
    assert_eq!(writes.iter().filter(|bytes| **bytes == b"\x1bI").count(), 1);
    assert_rgb_batch_commands(&writes, &[batches[0].capture.clone()], &caps);
}

#[test]
fn grayscale_strip_frames_preserve_samples_scores_and_cleanup_at_both_depths() {
    for depth in [8, 16] {
        for raw_only in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut args = sharpness_capture(directory.path(), "1,3", raw_only);
            args.mode = ScanMode::Gray;
            let settings = ScanSettings {
                mode: args.mode,
                depth,
                ..settings()
            };
            let options = ScanOptions {
                export_tiff: !raw_only,
                measure_sharpness: true,
                ..options()
            };
            let (caps, _) = identity();
            let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
            let batches = prepare_batches(&args, &jobs, &caps).unwrap();
            assert_eq!(batches.len(), 1);
            let mut responses = Vec::new();
            complete_strip(&mut responses, &batches[0], &jobs, &caps);
            let (scanner, writes) = session(responses);
            acquire(scanner, args, settings, options).unwrap();

            for (index, frame) in [1, 3].into_iter().enumerate() {
                let job = &jobs[index];
                let pass = &job.plan.passes[0];
                assert_eq!(pass.channels, 1);
                let sidecar = manifest(directory.path(), frame);
                let metadata = &sidecar["passes"][0];
                assert_eq!(sidecar["cleanup"]["state"], "complete");
                assert_eq!(metadata["pass"], "gray");
                assert_eq!(metadata["channel_layout"], "gray");
                assert_eq!(metadata["sharpness"]["luminance"], "gray");
                for metric in ["tenengrad", "variance_of_laplacian"] {
                    assert!(metadata["sharpness"][metric].as_f64().unwrap().abs() < 1e-24);
                }
                assert_cropped_frame(metadata, job);
                assert_eq!(output(directory.path(), frame, "bin").exists(), raw_only);
                assert_eq!(metadata["payload_retained"], raw_only);
                assert!(!output(directory.path(), frame, "protocol.jsonl").exists());
                let sample = 0x31 + index as u8;
                if raw_only {
                    let bytes = fs::read(output(directory.path(), frame, "bin")).unwrap();
                    assert_eq!(bytes.len() as u64, pass.expected_bytes);
                    assert!(bytes.iter().all(|byte| *byte == sample));
                } else {
                    let mut tiff = tiff::decoder::Decoder::new(
                        File::open(output(directory.path(), frame, "tiff")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(tiff.colortype().unwrap(), tiff::ColorType::Gray(depth));
                    assert_eq!(tiff.dimensions().unwrap(), (pass.pixels[2], pass.pixels[3]));
                    match tiff.read_image().unwrap() {
                        tiff::decoder::DecodingResult::U8(samples) if depth == 8 => {
                            assert_eq!(samples.len() as u64, pass.expected_bytes);
                            assert!(samples.iter().all(|value| *value == sample));
                        }
                        tiff::decoder::DecodingResult::U16(samples) if depth == 16 => {
                            assert_eq!(samples.len() as u64 * 2, pass.expected_bytes);
                            let expected = u16::from_le_bytes([sample; 2]);
                            assert!(samples.iter().all(|value| *value == expected));
                        }
                        _ => panic!("expected {depth}-bit grayscale TIFF"),
                    }
                }
            }
            let source: Value = serde_json::from_slice(
                &fs::read(directory.path().join("film_strip01_1.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(source["cleanup"]["state"], "complete");
            assert!(source["passes"][0]["payload_file"].is_null());
            assert_parameter_readback(&source["passes"][0], &batches[0].capture, &caps);
            assert_rgb_batch_commands(
                &writes.lock().unwrap(),
                &[batches[0].capture.clone()],
                &caps,
            );
            let writes = writes.lock().unwrap();
            let params = &writes.windows(2).find(|pair| pair[0] == b"\x1cW").unwrap()[1];
            assert_eq!(params[24], 0);
            assert_eq!(params[26], 1);
            assert!(!writes.iter().any(|bytes| bytes == b"\x1b#"));
        }
    }
}

#[test]
fn reordered_batch_preserves_frame_numbers_and_roi_order() {
    let directory = tempfile::tempdir().unwrap();
    let args = capture(directory.path(), "8,2");
    let (caps, _) = identity();
    let settings = settings();
    let options = options();
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    assert_eq!(jobs.len(), 2);
    let mut responses = Vec::new();
    for (index, job) in jobs.iter().enumerate() {
        complete_pass(&mut responses, job, &caps, 0x51 + index as u8);
    }
    let (scanner, writes) = session(responses);
    acquire(scanner, args, settings, options).unwrap();

    for (index, frame) in [8, 2].into_iter().enumerate() {
        assert_eq!(jobs[index].options.holder_selection.unwrap().frame, frame);
        let sidecar = manifest(directory.path(), frame);
        assert_eq!(sidecar["complete"], true);
        assert_eq!(sidecar["holder_selection"]["frame"], frame);
        assert_parameter_readback(&sidecar["passes"][0], &jobs[index], &caps);
    }
    assert_rgb_batch_commands(&writes.lock().unwrap(), &jobs, &caps);
}

#[test]
fn sharpness_batch_keeps_scores_in_sidecars_and_tiffs_after_intermediate_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let args = sharpness_capture(directory.path(), "1-2", false);
    let (caps, _) = identity();
    let settings = settings();
    let options = ScanOptions {
        measure_sharpness: true,
        ..options()
    };
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    assert_eq!(jobs.len(), 2);
    let batches = prepare_batches(&args, &jobs, &caps).unwrap();
    assert_eq!(batches.len(), 1);
    let mut responses = Vec::new();
    for job in &jobs {
        assert!(job.options.measure_sharpness);
        assert_eq!(job.options.holder_selection.unwrap().overage_percent, -10.0);
    }
    complete_strip(&mut responses, &batches[0], &jobs, &caps);
    let (scanner, writes) = session(responses);
    acquire(scanner, args, settings, options).unwrap();

    for (index, job) in jobs.iter().enumerate() {
        let frame = job.options.holder_selection.unwrap().frame;
        let sidecar = manifest(directory.path(), frame);
        let pass = &job.plan.passes[0];
        let scores = &sidecar["passes"][0]["sharpness"];
        assert_cropped_frame(&sidecar["passes"][0], job);
        assert_constant_sharpness(scores, pass.pixels[2], pass.pixels[3]);
        assert_eq!(sidecar["complete"], true);
        assert_eq!(sidecar["cleanup"]["state"], "complete");
        assert_eq!(sidecar["holder_selection"]["overage_percent"], -10.0);
        assert!(!output(directory.path(), frame, "bin").exists());
        assert!(!output(directory.path(), frame, "protocol.jsonl").exists());
        assert!(!output(directory.path(), frame, "partial.bin").exists());

        let mut decoder = tiff::decoder::Decoder::new(
            File::open(output(directory.path(), frame, "tiff")).unwrap(),
        )
        .unwrap();
        let description = decoder
            .get_tag_ascii_string(tiff::tags::Tag::ImageDescription)
            .unwrap();
        let metadata: Value = serde_json::from_str(&description).unwrap();
        assert_eq!(&metadata["sharpness"], scores);
        assert_eq!(
            decoder.dimensions().unwrap(),
            (pass.pixels[2], pass.pixels[3])
        );
        let tiff::decoder::DecodingResult::U8(samples) = decoder.read_image().unwrap() else {
            panic!("expected unchanged 8-bit RGB samples");
        };
        assert_eq!(samples.len() as u64, pass.expected_bytes);
        assert!(samples.iter().all(|sample| *sample == 0x31 + index as u8));
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 5);
    assert_rgb_batch_commands(
        &writes.lock().unwrap(),
        &[batches[0].capture.clone()],
        &caps,
    );
}

#[test]
fn raw_only_sharpness_capture_retains_scores_and_unchanged_payload() {
    let directory = tempfile::tempdir().unwrap();
    let args = sharpness_capture(directory.path(), "8", true);
    let (caps, _) = identity();
    let settings = settings();
    let options = ScanOptions {
        measure_sharpness: true,
        export_tiff: false,
        ..options()
    };
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    let mut responses = Vec::new();
    complete_pass(&mut responses, &jobs[0], &caps, 0x52);
    let (scanner, _) = session(responses);
    acquire(scanner, args, settings, options).unwrap();

    let sidecar = manifest(directory.path(), 8);
    let pass = &jobs[0].plan.passes[0];
    assert_parameter_readback(&sidecar["passes"][0], &jobs[0], &caps);
    assert_constant_sharpness(
        &sidecar["passes"][0]["sharpness"],
        pass.pixels[2],
        pass.pixels[3],
    );
    assert_eq!(sidecar["complete"], true);
    assert_eq!(sidecar["passes"][0]["payload_retained"], true);
    assert_eq!(
        fs::read(output(directory.path(), 8, "bin")).unwrap(),
        vec![0x52; pass.expected_bytes as usize]
    );
    assert!(!output(directory.path(), 8, "tiff").exists());
    assert!(!output(directory.path(), 8, "protocol.jsonl").exists());
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
}

#[test]
fn too_short_sharpness_region_fails_before_configuration_or_output() {
    let directory = tempfile::tempdir().unwrap();
    let Action::Scan(scan) = Cli::try_parse_from([
        "epscan",
        "scan",
        "--source",
        "film-holder",
        "--rect",
        "0,0,10,2",
        "--measure-sharpness",
    ])
    .unwrap()
    .action
    else {
        panic!("expected scan arguments");
    };
    let mut args = scan.capture;
    args.basename = directory.path().join("small");
    let settings = settings();
    let (scanner, writes) = session(Vec::new());
    let ordinary = prepare_jobs(&args, &scanner.capabilities, &settings, &options()).unwrap();
    assert_eq!(ordinary[0].plan.passes[0].pixels[3], 2);
    let options = ScanOptions {
        measure_sharpness: true,
        ..options()
    };
    let error = acquire(scanner, args, settings, options).unwrap_err();
    assert!(error.to_string().to_ascii_lowercase().contains("sharpness"));
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
    assert_eq!(
        *writes.lock().unwrap(),
        vec![b"\x1bI".to_vec(), b"\x1cI".to_vec()]
    );
}

#[test]
fn later_frame_failure_keeps_prior_intermediates_and_failed_partial_and_stops_batch() {
    let directory = tempfile::tempdir().unwrap();
    let args = capture(directory.path(), "1,7,13");
    let (caps, _) = identity();
    let settings = settings();
    let options = options();
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    let mut responses = Vec::new();
    complete_pass(&mut responses, &jobs[0], &caps, 0x31);
    let retained_row_bytes = interrupted_pass(&mut responses, &jobs[1], &caps);
    let (scanner, writes) = session(responses);
    assert!(acquire(scanner, args, settings, options).is_err());

    let first = manifest(directory.path(), 1);
    assert_parameter_readback(&first["passes"][0], &jobs[0], &caps);
    assert_eq!(first["complete"], true);
    assert!(first.get("cleanup").is_none());
    assert_eq!(
        fs::read(output(directory.path(), 1, "bin")).unwrap(),
        vec![0x31; jobs[0].plan.passes[0].expected_bytes as usize]
    );
    assert!(output(directory.path(), 1, "tiff").is_file());
    assert!(output(directory.path(), 1, "protocol.jsonl").is_file());
    assert!(!output(directory.path(), 1, "partial.bin").exists());

    let second = manifest(directory.path(), 7);
    assert_parameter_readback(&second["passes"][0], &jobs[1], &caps);
    assert_eq!(second["complete"], false);
    assert_eq!(second["holder_selection"]["frame"], 7);
    assert!(second["error"].is_string());
    assert!(second.get("cleanup").is_none());
    assert_eq!(
        fs::read(output(directory.path(), 7, "partial.bin")).unwrap(),
        vec![0x42; retained_row_bytes]
    );
    assert!(output(directory.path(), 7, "protocol.jsonl").is_file());
    assert!(!output(directory.path(), 7, "bin").exists());
    assert!(!output(directory.path(), 7, "tiff").exists());
    for extension in ["json", "tiff", "bin", "partial.bin", "protocol.jsonl"] {
        assert!(!output(directory.path(), 13, extension).exists());
    }
    // The failed transfer stops the batch without retrying or configuring frame three.
    assert_rgb_batch_commands(&writes.lock().unwrap(), &jobs[..2], &caps);
}

#[test]
fn all_eighteen_frames_use_three_acquisitions_and_preserve_16_bit_samples() {
    let directory = tempfile::tempdir().unwrap();
    let mut args = sharpness_capture(directory.path(), "1-18", true);
    args.keep_intermediates = true;
    let (caps, _) = identity();
    let settings = ScanSettings {
        depth: 16,
        ..settings()
    };
    let options = ScanOptions {
        export_tiff: false,
        measure_sharpness: true,
        ..options()
    };
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    let batches = prepare_batches(&args, &jobs, &caps).unwrap();
    assert_eq!(batches.len(), 3);
    let mut responses = Vec::new();
    for (index, batch) in batches.iter().enumerate() {
        assert_eq!(batch.strip, Some(index as u32 + 1));
        assert_eq!(
            batch.targets,
            (index * 6..index * 6 + 6).collect::<Vec<_>>()
        );
        complete_strip(&mut responses, batch, &jobs, &caps);
    }
    let (scanner, writes) = session(responses);
    acquire(scanner, args, settings, options).unwrap();
    for (index, job) in jobs.iter().enumerate() {
        let frame = index as u32 + 1;
        let sidecar = manifest(directory.path(), frame);
        let pass = &job.plan.passes[0];
        assert_cropped_frame(&sidecar["passes"][0], job);
        assert_constant_sharpness(
            &sidecar["passes"][0]["sharpness"],
            pass.pixels[2],
            pass.pixels[3],
        );
        assert_eq!(
            fs::read(output(directory.path(), frame, "bin")).unwrap(),
            vec![0x31 + index as u8; pass.expected_bytes as usize]
        );
    }
    let captures: Vec<_> = batches.into_iter().map(|batch| batch.capture).collect();
    assert_rgb_batch_commands(&writes.lock().unwrap(), &captures, &caps);
}

#[test]
fn strip_raw_only_retains_frames_and_cleans_shared_source_unless_requested() {
    for keep in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let mut args = sharpness_capture(directory.path(), "2,1", true);
        args.keep_intermediates = keep;
        let (caps, _) = identity();
        let settings = settings();
        let options = ScanOptions {
            export_tiff: false,
            measure_sharpness: true,
            ..options()
        };
        let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
        let batches = prepare_batches(&args, &jobs, &caps).unwrap();
        assert_eq!(batches.len(), 1);
        assert_eq!(batches[0].targets, vec![0, 1]);
        let mut responses = Vec::new();
        complete_strip(&mut responses, &batches[0], &jobs, &caps);
        let (scanner, _) = session(responses);
        acquire(scanner, args, settings, options).unwrap();
        for (index, frame) in [2, 1].into_iter().enumerate() {
            assert_eq!(
                fs::read(output(directory.path(), frame, "bin")).unwrap(),
                vec![0x31 + index as u8; jobs[index].plan.passes[0].expected_bytes as usize]
            );
            assert!(!output(directory.path(), frame, "tiff").exists());
            assert!(!output(directory.path(), frame, "protocol.jsonl").exists());
        }
        for extension in ["bin", "protocol.jsonl"] {
            assert_eq!(
                directory
                    .path()
                    .join(format!("film_strip01_1.{extension}"))
                    .exists(),
                keep
            );
        }
        assert!(directory.path().join("film_strip01_1.json").exists());
    }
}

#[test]
fn later_strip_failure_retains_shared_sources_and_completed_frame_crops() {
    let directory = tempfile::tempdir().unwrap();
    let args = capture(directory.path(), "1,2,7,8,13,14");
    let (caps, _) = identity();
    let settings = settings();
    let options = options();
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    let batches = prepare_batches(&args, &jobs, &caps).unwrap();
    assert_eq!(batches.len(), 3);
    let mut responses = Vec::new();
    complete_strip(&mut responses, &batches[0], &jobs, &caps);
    let retained = interrupted_pass(&mut responses, &batches[1].capture, &caps);
    let (scanner, writes) = session(responses);
    assert!(acquire(scanner, args, settings, options).is_err());
    for frame in [1, 2] {
        let sidecar = manifest(directory.path(), frame);
        assert_eq!(sidecar["complete"], true);
        assert!(sidecar.get("cleanup").is_none());
        assert!(output(directory.path(), frame, "bin").exists());
        assert!(output(directory.path(), frame, "tiff").exists());
    }
    for extension in ["bin", "protocol.jsonl", "json"] {
        assert!(
            directory
                .path()
                .join(format!("film_strip01_1.{extension}"))
                .exists()
        );
    }
    assert_eq!(
        fs::metadata(directory.path().join("film_strip02_1.partial.bin"))
            .unwrap()
            .len(),
        retained as u64
    );
    assert!(
        directory
            .path()
            .join("film_strip02_1.protocol.jsonl")
            .exists()
    );
    assert!(!directory.path().join("film_strip03_1.json").exists());
    for frame in [7, 8, 13, 14] {
        assert!(!output(directory.path(), frame, "json").exists());
    }
    assert_rgb_batch_commands(
        &writes.lock().unwrap(),
        &[batches[0].capture.clone(), batches[1].capture.clone()],
        &caps,
    );
}

#[test]
fn invalid_later_frame_or_source_bounds_rejects_entire_batch_before_configuration() {
    for invalid_frame in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let args = capture(
            directory.path(),
            if invalid_frame { "1,999" } else { "1,2" },
        );
        let (mut scanner, writes) = session(Vec::new());
        if !invalid_frame {
            // Keep frame one within the advertised area while frame two falls
            // outside it, exercising full-batch geometric preflight.
            let model = scanner.capabilities.scanner_model().unwrap();
            let holder = args.holder.unwrap();
            let first = model.holder_frame(holder, 1, 0.0).unwrap();
            let second = model.holder_frame(holder, 2, 0.0).unwrap();
            let boundary_mm = (first[1] + first[3] + second[1] + second[3]) / 2.0;
            scanner.capabilities.transparency_pixels[1] =
                (boundary_mm * f64::from(scanner.capabilities.basic_dpi) / 25.4).floor() as u32;
        }
        assert!(acquire(scanner, args, settings(), options()).is_err());
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"\x1bI".to_vec(), b"\x1cI".to_vec()]
        );
    }
}

#[test]
fn result_document_preserves_single_result_and_multi_frame_order() {
    let first = serde_json::json!({
        "rgb": {"tiff":"film_frame08_1.tiff", "payload":null},
        "manifest":"film_frame08_1.json"
    });
    let second = serde_json::json!({
        "rgb": {"tiff":"film_frame02_1.tiff", "payload":null},
        "manifest":"film_frame02_1.json"
    });
    for selection in [CaptureSelection::Area(1), CaptureSelection::Frame(8)] {
        assert_eq!(result_document(vec![(selection, first.clone())]), first);
    }
    assert_eq!(
        result_document(vec![
            (CaptureSelection::Frame(8), first.clone()),
            (CaptureSelection::Frame(2), second.clone())
        ]),
        serde_json::json!({"frames":[
            {"frame":8, "result":first},
            {"frame":2, "result":second}
        ]})
    );
    let first_area = serde_json::json!({"manifest":"film_area01_1.json"});
    let second_area = serde_json::json!({"manifest":"film_area02_1.json"});
    assert_eq!(
        result_document(vec![
            (CaptureSelection::Area(1), first_area.clone()),
            (CaptureSelection::Area(2), second_area.clone())
        ]),
        serde_json::json!({"areas":[
            {"area":1, "result":first_area},
            {"area":2, "result":second_area}
        ]})
    );
}

#[test]
fn rectangle_batch_preserves_order_samples_and_sharpness_after_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let args = rectangle_capture(directory.path(), "10,20,24,12;60,80,32,18");
    let (caps, _) = identity();
    let settings = settings();
    let options = ScanOptions {
        measure_sharpness: true,
        ..options()
    };
    let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    let rectangles = [[10.0, 20.0, 24.0, 12.0], [60.0, 80.0, 32.0, 18.0]];
    assert_eq!(jobs.len(), 2);
    assert_ne!(
        jobs[0].plan.passes[0].pixels[2..],
        jobs[1].plan.passes[0].pixels[2..]
    );
    let mut responses = Vec::new();
    for (index, job) in jobs.iter().enumerate() {
        assert_eq!(job.settings.rect_mm, rectangles[index]);
        assert!(job.options.holder_selection.is_none());
        assert_eq!(
            job.basename,
            directory.path().join(format!("film_area{:02}", index + 1))
        );
        assert!(matches!(job.selection, CaptureSelection::Area(area) if area == index + 1));
        complete_pass(&mut responses, job, &caps, 0x41 + index as u8);
    }
    let (scanner, writes) = session(responses);
    acquire(scanner, args, settings, options).unwrap();

    for (index, job) in jobs.iter().enumerate() {
        let stem = format!("film_area{:02}_1", index + 1);
        let sidecar: Value = serde_json::from_slice(
            &fs::read(directory.path().join(format!("{stem}.json"))).unwrap(),
        )
        .unwrap();
        assert_eq!(sidecar["complete"], true);
        assert_eq!(sidecar["cleanup"]["state"], "complete");
        assert!(sidecar.get("holder_selection").is_none());
        let pass_metadata = &sidecar["passes"][0];
        assert_parameter_readback(pass_metadata, job, &caps);
        assert_eq!(
            pass_metadata["requested_rect_mm"],
            serde_json::json!(rectangles[index])
        );
        assert!(pass_metadata.get("holder_selection").is_none());
        assert!(pass_metadata["payload_file"].is_null());
        assert_eq!(pass_metadata["payload_retained"], false);
        let pass = &job.plan.passes[0];
        assert_constant_sharpness(&pass_metadata["sharpness"], pass.pixels[2], pass.pixels[3]);
        for extension in ["bin", "partial.bin", "protocol.jsonl"] {
            assert!(
                !directory
                    .path()
                    .join(format!("{stem}.{extension}"))
                    .exists()
            );
        }
        let mut decoder = tiff::decoder::Decoder::new(
            File::open(directory.path().join(format!("{stem}.tiff"))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            decoder.dimensions().unwrap(),
            (pass.pixels[2], pass.pixels[3])
        );
        let metadata: Value = serde_json::from_str(
            &decoder
                .get_tag_ascii_string(tiff::tags::Tag::ImageDescription)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(metadata["sharpness"], pass_metadata["sharpness"]);
        assert_parameter_readback(&metadata, job, &caps);
        assert_eq!(
            metadata["requested_rect_mm"],
            pass_metadata["requested_rect_mm"]
        );
        let tiff::decoder::DecodingResult::U8(samples) = decoder.read_image().unwrap() else {
            panic!("expected unchanged 8-bit RGB samples");
        };
        assert_eq!(samples.len() as u64, pass.expected_bytes);
        assert!(samples.iter().all(|sample| *sample == 0x41 + index as u8));
    }
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 4);
    let writes = writes.lock().unwrap();
    assert_eq!(writes.iter().filter(|bytes| **bytes == b"\x1bI").count(), 1);
    assert_rgb_batch_commands(&writes, &jobs, &caps);
}

#[test]
fn single_rectangle_keeps_original_basename_and_duplicates_keep_distinct_area_numbers() {
    let directory = tempfile::tempdir().unwrap();
    let (caps, _) = identity();
    let settings = settings();
    let options = options();
    let args = rectangle_capture(directory.path(), "10,20,24,12");
    let single = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    assert_eq!(single.len(), 1);
    assert_eq!(single[0].basename, directory.path().join("film"));
    assert!(matches!(single[0].selection, CaptureSelection::Area(1)));
    let args = rectangle_capture(directory.path(), "10,20,24,12;10,20,24,12");
    let repeated = prepare_jobs(&args, &caps, &settings, &options).unwrap();
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0].settings.rect_mm, repeated[1].settings.rect_mm);
    assert_eq!(repeated[0].basename, directory.path().join("film_area01"));
    assert_eq!(repeated[1].basename, directory.path().join("film_area02"));
}

#[test]
fn banding_runs_on_each_output_crop_after_shared_acquisition() {
    for holder in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let mut args = if holder {
            capture(directory.path(), "1,2")
        } else {
            rectangle_capture(directory.path(), "10,20,24,30;10,55,24,30")
        };
        args.reduce_banding = true;
        args.save_raw_tiff = true;
        args.save_band_signal = true;
        let options = ScanOptions {
            banding: args.banding_options().unwrap(),
            ..options()
        };
        let settings = ScanSettings {
            dpi: 100,
            mode: ScanMode::Gray,
            ..settings()
        };
        let (caps, _) = identity();
        let jobs = prepare_jobs(&args, &caps, &settings, &options).unwrap();
        let batches = prepare_batches(&args, &jobs, &caps).unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0].strip.is_some());
        assert!(!batches[0].capture.options.export_tiff);
        assert!(batches[0].capture.options.banding.is_none());
        assert!(jobs.iter().all(|job| job.options.banding.is_some()));
        let mut responses = Vec::new();
        complete_strip(&mut responses, &batches[0], &jobs, &caps);
        let (scanner, _) = session(responses);
        acquire(scanner, args, settings, options).unwrap();

        for (index, job) in jobs.iter().enumerate() {
            let base = format!("{}_1", job.basename.file_name().unwrap().to_str().unwrap());
            let metadata = sidecar_value(&directory.path().join(format!("{base}.json")));
            assert_eq!(metadata["cleanup"]["state"], "complete");
            let pass = &metadata["passes"][0];
            assert!(pass["derived_crop"].as_bool().unwrap());
            for field in ["corrected_tiff_file", "raw_tiff_file", "signal_png_file"] {
                assert!(Path::new(pass["banding"][field].as_str().unwrap()).is_file());
            }
            for ending in [".tiff", "_raw.tiff"] {
                let mut decoder = tiff::decoder::Decoder::new(
                    File::open(directory.path().join(format!("{base}{ending}"))).unwrap(),
                )
                .unwrap();
                assert_eq!(
                    decoder.dimensions().unwrap(),
                    (job.plan.passes[0].pixels[2], job.plan.passes[0].pixels[3])
                );
                let tiff::decoder::DecodingResult::U8(samples) = decoder.read_image().unwrap()
                else {
                    panic!("expected 8-bit gray crop");
                };
                assert!(samples.iter().all(|sample| *sample == 0x31 + index as u8));
            }
            assert!(!directory.path().join(format!("{base}.bin")).exists());
        }
        assert!(!directory.path().join("film_strip01_1.bin").exists());
    }
}

fn sidecar_value(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn invalid_later_rectangle_or_pixel_plan_rejects_all_areas_before_scanning() {
    for (rectangles, physical_area_valid, error_context) in [
        ("0,0,10,10;149,240,10,10", false, "source area"),
        ("0,0,10,10;20,20,1,10", true, "columns"),
        ("0,0,10,10;20,20,10,2", true, "sharpness"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let args = rectangle_capture(directory.path(), rectangles);
        let (scanner, writes) = session(Vec::new());
        assert_eq!(
            args.resolve_areas(&scanner.capabilities).is_ok(),
            physical_area_valid
        );
        let error = acquire(
            scanner,
            args,
            settings(),
            ScanOptions {
                measure_sharpness: true,
                ..options()
            },
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .to_ascii_lowercase()
                .contains(error_context),
            "{error}"
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 0);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![b"\x1bI".to_vec(), b"\x1cI".to_vec()]
        );
    }
}
