// SPDX-License-Identifier: MIT
//! End-to-end streaming tests with deterministic scanner/callback barriers.
use crate::{
    Backend, Capabilities, Device, Error, Result, ScanMode, ScanOptions, ScanResult, ScanSettings,
    Session,
    scan::regions::{RegionScanPlan, plan_region_batches},
    session::image::ImageResult,
    transport::Transport,
};
use sha2::{Digest, Sha256};
use std::{
    fs,
    sync::{Arc, Condvar, Mutex, atomic::AtomicBool},
    thread,
    time::Duration,
};

#[derive(Default)]
struct GateState {
    reached: bool,
    released: bool,
    consumed: bool,
}

#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl Gate {
    fn wait_for(&self, predicate: impl Fn(&GateState) -> bool) -> bool {
        let (state, _) = self
            .changed
            .wait_timeout_while(
                self.state.lock().unwrap(),
                Duration::from_secs(5),
                |state| !predicate(state),
            )
            .unwrap();
        predicate(&state)
    }

    fn release(&self) {
        self.state.lock().unwrap().released = true;
        self.changed.notify_all();
    }
}

struct StreamingTransport {
    bytes: Vec<u8>,
    offset: usize,
    gate_at: Option<usize>,
    gate: Arc<Gate>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    cancel_ack: bool,
}

impl Transport for StreamingTransport {
    fn read(&mut self, size: usize, _: Duration) -> Result<Vec<u8>> {
        if self.cancel_ack {
            self.cancel_ack = false;
            return Ok(vec![6]);
        }
        if self.gate_at == Some(self.offset) {
            self.gate_at = None;
            self.gate.state.lock().unwrap().reached = true;
            self.gate.changed.notify_all();
            if !self.gate.wait_for(|state| state.released) {
                return Err(Error::Timeout(
                    "streaming test received no early frame callback".into(),
                ));
            }
        }
        let end = (self.offset + size).min(self.bytes.len());
        let result = self.bytes[self.offset..end].to_vec();
        self.offset = end;
        if end == self.bytes.len() {
            self.gate.state.lock().unwrap().consumed = true;
            self.gate.changed.notify_all();
        }
        Ok(result)
    }

    fn write(&mut self, bytes: &[u8], _: Duration) -> Result<usize> {
        self.writes.lock().unwrap().push(bytes.to_vec());
        if bytes == [0x18] {
            self.cancel_ack = true;
        }
        Ok(bytes.len())
    }
}

struct Fixture {
    session: Session,
    settings: ScanSettings,
    options: ScanOptions,
    plan: RegionScanPlan,
    gate: Arc<Gate>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
}

fn samples(pixels: [u32; 4], channels: u8, depth: u8) -> Vec<u8> {
    let [left, top, width, height] = pixels;
    let mut bytes = Vec::new();
    for y in top..top + height {
        for x in left..left + width {
            for channel in 0..channels {
                let value = (x * 61 + y * 29 + u32::from(channel) * 113) as u16;
                if depth == 8 {
                    bytes.push(value as u8);
                } else {
                    bytes.extend(value.to_le_bytes());
                }
            }
        }
    }
    bytes
}

fn fixture(
    regions: &[[f64; 4]],
    depth: u8,
    mode: ScanMode,
    gate_after_first: bool,
    fault_status: Option<u8>,
    truncate_second: bool,
) -> Fixture {
    fixture_with_sampling(
        regions,
        depth,
        mode,
        gate_after_first,
        fault_status,
        truncate_second,
        1,
    )
}

fn fixture_with_sampling(
    regions: &[[f64; 4]],
    depth: u8,
    mode: ScanMode,
    gate_after_first: bool,
    fault_status: Option<u8>,
    truncate_second: bool,
    y_oversampling: u32,
) -> Fixture {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../../tests/fixtures/v800-identity.json")).unwrap();
    let hex = fixture["extended_identity_hex"].as_str().unwrap();
    let identity: Vec<_> = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect();
    let caps = Capabilities::parse(&identity).unwrap();
    let settings = ScanSettings {
        dpi: 254,
        depth,
        mode,
        y_oversampling,
        ..Default::default()
    };
    let options = ScanOptions::default();
    let plan = plan_region_batches(regions, &settings, &options, &caps, 10.0).unwrap();
    assert_eq!(plan.batches.len(), 1);
    assert_eq!(plan.batches[0].region_indices.len(), regions.len());
    let pass = &plan.batches[0].plan.passes[0];
    let first = &plan.region_plans[0].passes[0];
    let stride = pass.pixels[2] as usize * usize::from(pass.channels) * usize::from(depth / 8);
    let first_rows = (first.pixels[1] - pass.pixels[1] + first.pixels[3]) as usize;
    // Every full image block cuts through a row (and a 16-bit sample), rather
    // than relying on favorable scanner block boundaries.
    let block_size = first_rows * stride * y_oversampling as usize + 1;
    // Each repeated source row should average back to its original value.
    let square_payload = samples(pass.pixels, pass.channels, depth);
    let payload: Vec<u8> = square_payload
        .chunks_exact(stride)
        .flat_map(|row| (0..y_oversampling).flat_map(move |_| row.iter().copied()))
        .collect();
    assert!(payload.len() > block_size * 2);
    let full_blocks = payload.len() / block_size;
    let tail_size = payload.len() % block_size;
    let mut bytes = vec![2, 0x12, 2, 0, b'B', b'8'];
    bytes.extend(identity);
    bytes.extend([6; 5]);
    bytes.extend(pass.settings.parameters(false).unwrap());
    bytes.extend([0; 16]);
    bytes.extend([2, 0x12]);
    for word in [block_size, full_blocks, tail_size] {
        bytes.extend((word as u32).to_le_bytes());
    }
    let image_start = bytes.len();
    for (index, block) in payload.chunks(block_size).enumerate() {
        bytes.extend(block);
        bytes.push(if index == 1 {
            fault_status.unwrap_or(0)
        } else {
            0
        });
    }
    let second_start = image_start + block_size + 1;
    if truncate_second {
        bytes.truncate(second_start + block_size / 2);
    }
    let gate = Arc::new(Gate::default());
    let writes = Arc::new(Mutex::new(Vec::new()));
    let session = Session::with_transport(
        Device {
            location: "synthetic-streaming".into(),
            name: "streaming simulator".into(),
            vid: 0x04b8,
            pid: 0x0151,
            backend: Backend::Nusb,
        },
        Box::new(StreamingTransport {
            bytes,
            offset: 0,
            gate_at: gate_after_first.then_some(second_start),
            gate: gate.clone(),
            writes: writes.clone(),
            cancel_ack: false,
        }),
        Duration::from_secs(6),
    )
    .unwrap();
    Fixture {
        session,
        settings,
        options,
        plan,
        gate,
        writes,
    }
}

fn image(result: &ScanResult) -> &ImageResult {
    result.rgb.as_ref().or(result.gray.as_ref()).unwrap()
}

fn assert_exact_image(result: &ScanResult, pixels: [u32; 4], channels: u8, depth: u8) {
    let image = image(result);
    let expected = samples(pixels, channels, depth);
    assert_eq!(fs::read(&image.payload).unwrap(), expected);
    assert_eq!((image.width, image.height), (pixels[2], pixels[3]));
    let mut decoder =
        tiff::decoder::Decoder::new(fs::File::open(image.tiff.as_ref().unwrap()).unwrap()).unwrap();
    assert_eq!(decoder.dimensions().unwrap(), (pixels[2], pixels[3]));
    match decoder.read_image().unwrap() {
        tiff::decoder::DecodingResult::U8(actual) => assert_eq!(actual, expected),
        tiff::decoder::DecodingResult::U16(actual) => assert_eq!(
            actual,
            crate::session::image::decode_u16_le(&expected).unwrap()
        ),
        other => panic!("Unexpected streaming TIFF: {other:?}"),
    }
    let manifest: serde_json::Value =
        serde_json::from_slice(&fs::read(&result.manifest).unwrap()).unwrap();
    assert_eq!(manifest["complete"], true);
}

#[test]
fn averaged_rows_deliver_early_frames_without_waiting_for_the_strip() {
    let regions = [[4.0, 8.0, 2.4, 0.6], [4.4, 9.0, 1.6, 0.6]];
    for depth in [8, 16] {
        for mode in [ScanMode::Rgb, ScanMode::Gray] {
            let mut fixture = fixture_with_sampling(&regions, depth, mode, true, None, false, 3);
            let directory = tempfile::tempdir().unwrap();
            let mut delivered = Vec::new();
            let results = fixture
                .session
                .scan_regions(
                    &regions,
                    &fixture.settings,
                    &fixture.options,
                    &directory.path().join("roll"),
                    10.0,
                    &AtomicBool::new(false),
                    &mut |_| true,
                    &mut |index, result| {
                        let pass = &fixture.plan.region_plans[index].passes[0];
                        assert_exact_image(result, pass.pixels, pass.channels, depth);
                        assert_eq!(
                            image(result).metadata["source_capture"]["settings"]["y_oversampling"],
                            3
                        );
                        delivered.push(index);
                        if index == 0 {
                            assert!(fixture.gate.wait_for(|state| state.reached));
                            assert!(!fixture.gate.state.lock().unwrap().consumed);
                            fixture.gate.release();
                            assert!(fixture.gate.wait_for(|state| state.consumed));
                        }
                        Ok(true)
                    },
                )
                .unwrap();
            assert_eq!(delivered, [0, 1]);
            assert_eq!(results.len(), 2);
        }
    }
}

#[test]
fn early_frame_callback_runs_during_ingest_without_blocking_remaining_scanner_reads() {
    // A nonzero left edge, narrower second frame, and shuffled channel/sample
    // patterns catch incorrect byte offsets as well as callback timing.
    let regions = [[4.0, 8.0, 2.4, 0.6], [4.4, 9.0, 1.6, 0.6]];
    for depth in [8, 16] {
        for mode in [ScanMode::Rgb, ScanMode::Gray] {
            let mut fixture = fixture(&regions, depth, mode, true, None, false);
            let directory = tempfile::tempdir().unwrap();
            let caller = thread::current().id();
            let mut delivered = Vec::new();
            let mut progress = Vec::new();
            let results = fixture
                .session
                .scan_regions(
                    &regions,
                    &fixture.settings,
                    &fixture.options,
                    &directory.path().join("roll"),
                    10.0,
                    &AtomicBool::new(false),
                    &mut |update| {
                        assert_eq!(thread::current().id(), caller);
                        progress.push((update.done, update.total));
                        true
                    },
                    &mut |index, result| {
                        assert_eq!(thread::current().id(), caller);
                        let pass = &fixture.plan.region_plans[index].passes[0];
                        assert_exact_image(result, pass.pixels, pass.channels, depth);
                        delivered.push(index);
                        if index == 0 {
                            assert!(fixture.gate.wait_for(|state| state.reached));
                            assert!(!fixture.gate.state.lock().unwrap().consumed);
                            fixture.gate.release();
                            // This callback deliberately waits for scanner
                            // ingestion to finish. It must run independently.
                            assert!(fixture.gate.wait_for(|state| state.consumed));
                        }
                        Ok(true)
                    },
                )
                .unwrap();
            assert_eq!(delivered, [0, 1]);
            assert_eq!(results.len(), 2);
            assert!(fixture.session.is_open());
            assert!(progress.windows(2).all(|pair| pair[0].0 <= pair[1].0));
            let final_progress = progress.last().unwrap();
            assert_eq!(final_progress.0, final_progress.1);
            for (result, plan) in results.iter().zip(&fixture.plan.region_plans) {
                let pass = &plan.passes[0];
                assert_exact_image(result, pass.pixels, pass.channels, depth);
            }
        }
    }
}

#[test]
fn later_fault_cancel_or_partial_block_keeps_only_frames_from_healthy_complete_blocks() {
    // Block two contains the entire second region, so counting its bytes
    // before validating its status would wrongly publish that frame.
    let regions = [
        [4.0, 8.0, 2.4, 0.4],
        [4.0, 8.5, 2.4, 0.3],
        [4.0, 9.0, 2.4, 0.4],
    ];
    for (fault, truncated) in [
        (Some(0x80), false),
        (Some(0x40), false),
        (Some(0x10), false),
        (None, true),
    ] {
        let mut fixture = fixture(&regions, 8, ScanMode::Gray, false, fault, truncated);
        let directory = tempfile::tempdir().unwrap();
        let mut delivered = Vec::new();
        let error = fixture
            .session
            .scan_regions(
                &regions,
                &fixture.settings,
                &fixture.options,
                &directory.path().join("roll"),
                10.0,
                &AtomicBool::new(false),
                &mut |_| true,
                &mut |index, result| {
                    let pass = &fixture.plan.region_plans[index].passes[0];
                    assert_exact_image(result, pass.pixels, pass.channels, 8);
                    delivered.push(index);
                    Ok(true)
                },
            )
            .unwrap_err();
        if fault == Some(0x10) {
            assert!(matches!(error, Error::Cancelled));
        } else {
            assert!(matches!(error, Error::Protocol(_) | Error::Timeout(_)));
        }
        assert_eq!(delivered, [0]);
        assert!(!fixture.session.is_open());
        let writes = fixture.writes.lock().unwrap();
        assert_eq!(writes.iter().filter(|bytes| *bytes == &[6]).count(), 1);
        assert_eq!(
            writes.iter().filter(|bytes| *bytes == &[0x18]).count(),
            usize::from(fault == Some(0x10))
        );
        let filenames: Vec<_> = fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(filenames.iter().any(|name| name.ends_with(".partial.bin")));
        assert!(
            filenames
                .iter()
                .all(|name| !name.contains("_area02") && !name.contains("_area03"))
        );
    }
}

#[test]
fn early_callback_cancellation_joins_ingest_and_keeps_the_completed_frame() {
    let regions = [[4.0, 8.0, 2.4, 0.6], [4.4, 9.0, 1.6, 0.6]];
    let mut fixture = fixture(&regions, 8, ScanMode::Gray, true, None, false);
    let directory = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    let mut delivered = Vec::new();
    let error = fixture
        .session
        .scan_regions(
            &regions,
            &fixture.settings,
            &fixture.options,
            &directory.path().join("roll"),
            10.0,
            &cancel,
            &mut |_| true,
            &mut |index, result| {
                assert_eq!(index, 0);
                assert!(fixture.gate.wait_for(|state| state.reached));
                let pass = &fixture.plan.region_plans[0].passes[0];
                assert_exact_image(result, pass.pixels, pass.channels, 8);
                delivered.push(image(result).payload.clone());
                fixture.gate.release();
                Ok(false)
            },
        )
        .unwrap_err();
    assert!(matches!(error, Error::Cancelled));
    assert_eq!(delivered.len(), 1);
    assert!(!fixture.session.is_open());
    let pass = &fixture.plan.region_plans[0].passes[0];
    assert_eq!(
        fs::read(&delivered[0]).unwrap(),
        samples(pass.pixels, pass.channels, 8)
    );
    assert!(
        fixture
            .writes
            .lock()
            .unwrap()
            .iter()
            .filter(|bytes| *bytes == &[0x18])
            .count()
            <= 1
    );
}

#[test]
fn banding_waits_for_full_strip_and_reuses_its_analysis_for_both_frames() {
    let regions = [[4.0, 8.0, 12.8, 6.4], [4.0, 16.0, 12.8, 6.4]];
    let mut fixture = fixture(&regions, 16, ScanMode::Gray, false, None, false);
    fixture.options.banding = Some(Default::default());
    let strip = &fixture.plan.batches[0].plan.passes[0];
    assert_eq!([strip.pixels[2], strip.pixels[3]], [128, 144]);
    let expected_hash = crate::protocol::hex(&Sha256::digest(samples(
        strip.pixels,
        strip.channels,
        strip.settings.depth,
    )));
    let directory = tempfile::tempdir().unwrap();
    let mut analyses = Vec::new();
    let results = fixture
        .session
        .scan_regions(
            &regions,
            &fixture.settings,
            &fixture.options,
            &directory.path().join("roll"),
            10.0,
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |index, result| {
                assert!(fixture.gate.state.lock().unwrap().consumed);
                let image = image(result);
                let banding = &image.metadata["banding"];
                assert_eq!(banding["analysis_scope"], "shared_capture");
                assert_eq!(banding["analysis_source"]["width"], 128);
                assert_eq!(banding["analysis_source"]["height"], 144);
                assert_eq!(banding["analysis_source"]["sha256"], expected_hash);
                analyses.push(banding["analysis_source"].clone());
                let pass = &fixture.plan.region_plans[index].passes[0];
                assert_eq!(
                    fs::read(&image.payload).unwrap(),
                    samples(pass.pixels, pass.channels, 16)
                );
                let mut decoder = tiff::decoder::Decoder::new(
                    fs::File::open(image.tiff.as_ref().unwrap()).unwrap(),
                )
                .unwrap();
                assert_eq!(decoder.dimensions().unwrap(), (128, 64));
                assert!(matches!(
                    decoder.read_image().unwrap(),
                    tiff::decoder::DecodingResult::U16(_)
                ));
                Ok(true)
            },
        )
        .unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(analyses.len(), 2);
    assert_eq!(analyses[0], analyses[1]);
    assert!(fixture.session.is_open());
}
