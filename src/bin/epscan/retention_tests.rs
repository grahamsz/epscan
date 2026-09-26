// SPDX-License-Identifier: MIT OR Apache-2.0
use super::{finish, finish_source};
use epscan::{ScanResult, session::image::ImageResult};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
};

const PASSES: [&str; 3] = ["rgb", "ir", "thumbnail"];

struct CaptureFixture {
    directory: tempfile::TempDir,
    result: ScanResult,
    payloads: Vec<PathBuf>,
    tiffs: Vec<PathBuf>,
    manifest: PathBuf,
    trace: PathBuf,
}

fn capture(export_tiff: bool) -> CaptureFixture {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("scan_1.json");
    let trace = directory.path().join("scan_1.protocol.jsonl");
    let mut images = Vec::new();
    let mut payloads = Vec::new();
    let mut tiffs = Vec::new();
    let mut passes = Vec::new();
    for (name, suffix) in [("rgb", ""), ("ir", "_IR"), ("thumbnail", "_thumbnail")] {
        let payload = directory.path().join(format!("scan_1{suffix}.bin"));
        let tiff = export_tiff.then(|| directory.path().join(format!("scan_1{suffix}.tiff")));
        let channels = if name == "ir" { 1 } else { 3 };
        fs::write(&payload, &b"abc"[..usize::from(channels)]).unwrap();
        let metadata = json!({
            "pass":name,"complete":true,"acquisition_complete":true,
            "payload_file":payload,"tiff_file":tiff,"width":1,"height":1
        });
        let image = ImageResult {
            payload: payload.clone(),
            tiff: tiff.clone(),
            width: 1,
            height: 1,
            channels,
            depth: 8,
            dpi: 300,
            metadata: metadata.clone(),
        };
        if let Some(path) = &tiff {
            image.save_tiff(path).unwrap();
            tiffs.push(path.clone());
        }
        payloads.push(payload);
        passes.push(metadata);
        images.push(image);
    }
    fs::write(
        &manifest,
        serde_json::to_vec_pretty(&json!({
            "schema":1,"complete":true,"passes":passes
        }))
        .unwrap(),
    )
    .unwrap();
    fs::write(&trace, b"{\"direction\":\"in\",\"hex\":\"06\"}\n").unwrap();
    let mut images = images.into_iter();
    let result = ScanResult {
        rgb: images.next(),
        gray: None,
        ir: images.next(),
        thumbnail: images.next(),
        manifest: manifest.clone(),
    };
    CaptureFixture {
        directory,
        result,
        payloads,
        tiffs,
        manifest,
        trace,
    }
}

fn sidecar(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn pass<'a>(manifest: &'a Value, name: &str) -> &'a Value {
    manifest["passes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["pass"] == name)
        .unwrap()
}

#[test]
fn successful_job_removes_all_raw_and_trace_but_keeps_tiffs_and_matching_json() {
    let fixture = capture(true);
    let tiff_bytes: Vec<_> = fixture
        .tiffs
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    let output = finish(fixture.result, false).unwrap();
    let manifest = sidecar(&fixture.manifest);
    assert_eq!(output["cleanup"]["state"], "complete");
    assert_eq!(output["cleanup"], manifest["cleanup"]);
    assert!(output["cleanup"]["protocol_trace_file"].is_null());
    assert!(!fixture.trace.exists());
    for (index, name) in PASSES.iter().enumerate() {
        assert!(!fixture.payloads[index].exists());
        assert_eq!(fs::read(&fixture.tiffs[index]).unwrap(), tiff_bytes[index]);
        assert!(output[name]["payload"].is_null());
        assert!(output[name]["metadata"]["payload_file"].is_null());
        assert_eq!(output[name]["metadata"]["payload_retained"], false);
        assert!(pass(&manifest, name)["payload_file"].is_null());
        assert_eq!(pass(&manifest, name)["payload_retained"], false);
        assert_eq!(
            output[name]["tiff"],
            serde_json::to_value(&fixture.tiffs[index]).unwrap()
        );
    }
    assert!(manifest["complete"].as_bool().unwrap());
}

#[test]
fn keep_intermediates_leaves_outputs_and_metadata_unchanged() {
    let fixture = capture(true);
    let expected = serde_json::to_value(&fixture.result).unwrap();
    let before_manifest = fs::read(&fixture.manifest).unwrap();
    let before_trace = fs::read(&fixture.trace).unwrap();
    let output = finish(fixture.result, true).unwrap();
    assert_eq!(output, expected);
    assert_eq!(fs::read(&fixture.manifest).unwrap(), before_manifest);
    assert_eq!(fs::read(&fixture.trace).unwrap(), before_trace);
    assert!(fixture.payloads.iter().all(|path| path.is_file()));
    assert!(fixture.tiffs.iter().all(|path| path.is_file()));
}

#[test]
fn raw_only_retains_requested_payloads_and_removes_only_trace() {
    let fixture = capture(false);
    let before_payloads: Vec<_> = fixture
        .payloads
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    let output = finish(fixture.result, false).unwrap();
    let manifest = sidecar(&fixture.manifest);
    assert_eq!(output["cleanup"]["state"], "complete");
    assert!(!fixture.trace.exists());
    for (index, name) in PASSES.iter().enumerate() {
        assert_eq!(
            fs::read(&fixture.payloads[index]).unwrap(),
            before_payloads[index]
        );
        assert_eq!(
            output[name]["payload"],
            serde_json::to_value(&fixture.payloads[index]).unwrap()
        );
        assert_eq!(output[name]["metadata"]["payload_retained"], true);
        assert_eq!(pass(&manifest, name)["payload_retained"], true);
    }
}

#[test]
fn shared_source_cleanup_removes_raw_without_tiff_and_keeps_accurate_provenance() {
    let fixture = capture(false);
    let mut expected_manifest = sidecar(&fixture.manifest);
    let output = finish_source(fixture.result, false).unwrap();
    let manifest = sidecar(&fixture.manifest);
    assert_eq!(output["cleanup"]["state"], "complete");
    assert!(output["cleanup"]["protocol_trace_file"].is_null());
    assert!(!fixture.trace.exists());
    for (index, name) in PASSES.iter().enumerate() {
        assert!(!fixture.payloads[index].exists());
        assert!(output[name]["payload"].is_null());
        assert!(output[name]["tiff"].is_null());
        assert!(output[name]["metadata"]["payload_file"].is_null());
        assert_eq!(output[name]["metadata"]["payload_retained"], false);
        expected_manifest["passes"][index]["payload_file"] = Value::Null;
        expected_manifest["passes"][index]["payload_retained"] = false.into();
    }
    expected_manifest["cleanup"] = output["cleanup"].clone();
    assert_eq!(manifest, expected_manifest);
    assert_eq!(fs::read_dir(fixture.directory.path()).unwrap().count(), 1);
}

#[test]
fn keep_intermediates_preserves_shared_source_payloads_trace_and_metadata() {
    let fixture = capture(false);
    let expected = serde_json::to_value(&fixture.result).unwrap();
    let before_manifest = fs::read(&fixture.manifest).unwrap();
    let before_trace = fs::read(&fixture.trace).unwrap();
    let before_payloads: Vec<_> = fixture
        .payloads
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    let output = finish_source(fixture.result, true).unwrap();
    assert_eq!(output, expected);
    assert_eq!(fs::read(&fixture.manifest).unwrap(), before_manifest);
    assert_eq!(fs::read(&fixture.trace).unwrap(), before_trace);
    for (path, before) in fixture.payloads.iter().zip(before_payloads) {
        assert_eq!(fs::read(path).unwrap(), before);
    }
}

#[test]
fn invalid_source_blocks_all_deletions_and_preserves_manifest_and_trace() {
    for problem in [
        "incomplete_capture",
        "incomplete_pass",
        "mismatched_path",
        "missing_pass",
        "missing_payload",
        "truncated_payload",
        "invalid_dimensions",
    ] {
        let mut fixture = capture(false);
        let mut manifest = sidecar(&fixture.manifest);
        match problem {
            "incomplete_capture" => manifest["complete"] = false.into(),
            "incomplete_pass" => manifest["passes"][2]["complete"] = false.into(),
            "mismatched_path" => manifest["passes"][2]["payload_file"] = Value::Null,
            "missing_pass" => fixture.result.thumbnail = None,
            "missing_payload" => fs::remove_file(&fixture.payloads[2]).unwrap(),
            "truncated_payload" => fs::write(&fixture.payloads[2], b"a").unwrap(),
            "invalid_dimensions" => fixture.result.thumbnail.as_mut().unwrap().height = 0,
            _ => unreachable!(),
        }
        fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let before_manifest = fs::read(&fixture.manifest).unwrap();
        let before_trace = fs::read(&fixture.trace).unwrap();
        let output = finish_source(fixture.result, false).unwrap();
        assert_eq!(output["cleanup"]["state"], "blocked", "{problem}");
        assert!(!output["cleanup"]["errors"].as_array().unwrap().is_empty());
        assert_eq!(fs::read(&fixture.manifest).unwrap(), before_manifest);
        assert_eq!(fs::read(&fixture.trace).unwrap(), before_trace);
        assert_eq!(fs::read(&fixture.payloads[0]).unwrap(), b"abc");
        assert_eq!(fs::read(&fixture.payloads[1]).unwrap(), b"a");
        if problem != "missing_payload" {
            assert!(fixture.payloads[2].is_file());
        }
    }
}

#[cfg(windows)]
#[test]
fn failed_shared_source_deletion_keeps_trace_and_records_remaining_raw() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = capture(false);
    let locked_payload = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&fixture.payloads[1])
        .unwrap();
    let output = finish_source(fixture.result, false).unwrap();
    let manifest = sidecar(&fixture.manifest);
    assert_eq!(output["cleanup"]["state"], "partial");
    assert_eq!(output["cleanup"], manifest["cleanup"]);
    assert!(fixture.trace.is_file());
    for (index, name) in PASSES.iter().enumerate() {
        let retained = index == 1;
        assert_eq!(fixture.payloads[index].exists(), retained);
        assert_eq!(output[name]["payload"].is_null(), !retained);
        assert_eq!(output[name]["metadata"]["payload_retained"], retained);
        assert_eq!(pass(&manifest, name)["payload_retained"], retained);
        assert_eq!(pass(&manifest, name)["payload_file"].is_null(), !retained);
    }
    drop(locked_payload);
}

#[test]
fn missing_later_tiff_blocks_all_deletions_before_touching_metadata() {
    let fixture = capture(true);
    fs::remove_file(&fixture.tiffs[2]).unwrap();
    let before_manifest = fs::read(&fixture.manifest).unwrap();
    let output = finish(fixture.result, false).unwrap();
    assert_eq!(output["cleanup"]["state"], "blocked");
    assert!(fixture.payloads.iter().all(|path| path.is_file()));
    assert!(fixture.trace.is_file());
    assert_eq!(fs::read(&fixture.manifest).unwrap(), before_manifest);
    assert!(!output["cleanup"]["errors"].as_array().unwrap().is_empty());
}

fn add_banding_artifacts(fixture: &mut CaptureFixture) -> Vec<PathBuf> {
    let image = fixture.result.rgb.as_mut().unwrap();
    let raw = fixture.directory.path().join("scan_1_raw.tiff");
    let signal = fixture.directory.path().join("scan_1_banding.png");
    image.save_tiff(&raw).unwrap();
    fs::write(&signal, b"synthetic PNG export").unwrap();
    image.metadata["banding"] = json!({
        "corrected_tiff_file": image.tiff,
        "raw_tiff_file": raw,
        "signal_png_file": signal,
        "config": {"save_raw":true,"save_signal":true}
    });
    let mut manifest = sidecar(&fixture.manifest);
    manifest["passes"][0]["banding"] = image.metadata["banding"].clone();
    fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
    vec![raw, signal]
}

#[test]
fn successful_banding_exports_survive_raw_payload_cleanup() {
    let mut fixture = capture(true);
    let artifacts = add_banding_artifacts(&mut fixture);
    let bytes: Vec<_> = artifacts
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();
    let output = finish(fixture.result, false).unwrap();
    assert_eq!(output["cleanup"]["state"], "complete");
    assert!(fixture.payloads.iter().all(|path| !path.exists()));
    assert!(!fixture.trace.exists());
    for (path, expected) in artifacts.iter().zip(bytes) {
        assert_eq!(fs::read(path).unwrap(), expected);
    }
    assert_eq!(
        output["rgb"]["metadata"]["banding"],
        pass(&sidecar(&fixture.manifest), "rgb")["banding"]
    );
}

#[test]
fn banding_diagnostic_rounding_in_json_does_not_block_valid_cleanup() {
    let mut fixture = capture(true);
    let artifacts = add_banding_artifacts(&mut fixture);
    // A realistic fitted amplitude that serde_json's default float parser
    // changes by one ULP, alongside other synthetic 13.25-pixel diagnostics.
    let diagnostics = json!({
        "amplitude": 0.044999026256985604,
        "period_pixels": 13.249963986455674,
        "y": 10.636363636363637,
        "residual_along_original": 0.3315898110732042
    });
    let roundtripped: Value =
        serde_json::from_slice(&serde_json::to_vec(&diagnostics).unwrap()).unwrap();
    assert_ne!(
        diagnostics, roundtripped,
        "fixture must exercise float rounding"
    );
    let image = fixture.result.rgb.as_mut().unwrap();
    image.metadata["banding"]["diagnostics"] = diagnostics;
    let mut manifest = sidecar(&fixture.manifest);
    manifest["passes"][0]["banding"] = image.metadata["banding"].clone();
    fs::write(
        &fixture.manifest,
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let output = finish(fixture.result, false).unwrap();
    assert_eq!(output["cleanup"]["state"], "complete");
    assert!(fixture.payloads.iter().all(|path| !path.exists()));
    assert!(artifacts.iter().all(|path| path.is_file()));
    assert!(!fixture.trace.exists());
}

#[test]
fn invalid_banding_artifact_keeps_all_remaining_raw_payloads_and_trace() {
    for problem in [
        "missing_raw",
        "missing_signal",
        "empty_signal",
        "mismatched_metadata",
        "mismatched_config",
        "missing_requested_path",
        "wrong_corrected_tiff",
    ] {
        let mut fixture = capture(true);
        let artifacts = add_banding_artifacts(&mut fixture);
        let mut manifest = sidecar(&fixture.manifest);
        match problem {
            "missing_raw" => fs::remove_file(&artifacts[0]).unwrap(),
            "missing_signal" => fs::remove_file(&artifacts[1]).unwrap(),
            "empty_signal" => fs::write(&artifacts[1], []).unwrap(),
            "mismatched_metadata" => {
                manifest["passes"][0]["banding"]["signal_png_file"] = Value::Null
            }
            "mismatched_config" => {
                manifest["passes"][0]["banding"]["config"]["save_raw"] = false.into()
            }
            "missing_requested_path" | "wrong_corrected_tiff" => {
                let field = if problem == "missing_requested_path" {
                    "signal_png_file"
                } else {
                    "corrected_tiff_file"
                };
                fixture.result.rgb.as_mut().unwrap().metadata["banding"][field] = Value::Null;
                manifest["passes"][0]["banding"][field] = Value::Null;
            }
            _ => unreachable!(),
        }
        fs::write(&fixture.manifest, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let before_manifest = fs::read(&fixture.manifest).unwrap();
        let output = finish(fixture.result, false).unwrap();
        assert_eq!(output["cleanup"]["state"], "blocked", "{problem}");
        assert!(
            fixture.payloads.iter().all(|path| path.is_file()),
            "{problem}"
        );
        assert!(fixture.trace.is_file(), "{problem}");
        assert_eq!(
            fs::read(&fixture.manifest).unwrap(),
            before_manifest,
            "{problem}"
        );
    }
}

#[test]
fn cleanup_does_not_touch_unrelated_or_historical_similarly_named_files() {
    let fixture = capture(true);
    let preserved: Vec<_> = [
        "scan_1.extra.bin",
        "scan_1.partial.bin",
        "scan_1_backup.protocol.jsonl",
        "scan_10.bin",
        "scan_10.protocol.jsonl",
        "scan_2.json",
        "scan_1.json.cleanup-old.tmp",
    ]
    .iter()
    .map(|name| {
        let path = fixture.directory.path().join(name);
        fs::write(&path, name.as_bytes()).unwrap();
        (path, name.as_bytes().to_vec())
    })
    .collect();
    assert_eq!(
        finish(fixture.result, false).unwrap()["cleanup"]["state"],
        "complete"
    );
    for (path, bytes) in preserved {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

#[test]
fn failed_raw_deletion_preserves_trace_and_reports_actual_retention() {
    let fixture = capture(true);
    #[cfg(windows)]
    let locked_payload = {
        use std::os::windows::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&fixture.payloads[1])
            .unwrap()
    };
    #[cfg(not(windows))]
    {
        fs::remove_file(&fixture.payloads[1]).unwrap();
        fs::create_dir(&fixture.payloads[1]).unwrap();
    }
    let output = finish(fixture.result, false).unwrap();
    let manifest = sidecar(&fixture.manifest);
    assert_eq!(output["cleanup"]["state"], "partial");
    assert_eq!(output["cleanup"], manifest["cleanup"]);
    assert!(fixture.trace.is_file());
    assert!(fixture.payloads[1].exists());
    assert!(!fixture.payloads[0].exists());
    assert!(!fixture.payloads[2].exists());
    for (index, name) in PASSES.iter().enumerate() {
        let retained = index == 1;
        assert_eq!(output[name]["payload"].is_null(), !retained);
        assert_eq!(output[name]["metadata"]["payload_retained"], retained);
        assert_eq!(pass(&manifest, name)["payload_retained"], retained);
        assert_eq!(pass(&manifest, name)["payload_file"].is_null(), !retained);
    }
    assert!(fixture.tiffs.iter().all(|path| path.is_file()));
    #[cfg(windows)]
    drop(locked_payload);
}

#[cfg(windows)]
#[test]
fn manifest_replace_failure_removes_nothing_and_preserves_original_json() {
    use std::os::windows::fs::OpenOptionsExt;
    let fixture = capture(true);
    let original = fs::read(&fixture.manifest).unwrap();
    // Permit ordinary reads/writes but deny replacing/deleting this open file.
    let locked_manifest = fs::OpenOptions::new()
        .read(true)
        .share_mode(3)
        .open(&fixture.manifest)
        .unwrap();
    let output = finish(fixture.result, false).unwrap();
    assert_eq!(output["cleanup"]["state"], "blocked");
    assert_eq!(fs::read(&fixture.manifest).unwrap(), original);
    assert!(fixture.payloads.iter().all(|path| path.is_file()));
    assert!(fixture.tiffs.iter().all(|path| path.is_file()));
    assert!(fixture.trace.is_file());
    assert!(
        !fs::read_dir(fixture.directory.path())
            .unwrap()
            .any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains(".cleanup-")
            })
    );
    drop(locked_manifest);
}
