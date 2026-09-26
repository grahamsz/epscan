// SPDX-License-Identifier: MIT OR Apache-2.0
//! CLI output retention. Library callers retain their reusable raw payloads.
use epscan::{Result, ScanResult};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

/// Called only after the entire acquisition and all requested exports succeed.
/// Cleanup problems are reported separately from successful image acquisition.
pub fn finish(result: ScanResult, keep_intermediates: bool) -> Result<Value> {
    finish_capture(result, keep_intermediates, false)
}

/// Finish a shared acquisition only after every derived frame is durable and
/// its own cleanup has succeeded. The source manifest remains as provenance;
/// source payloads are intermediates even when no source TIFF was requested.
pub fn finish_source(result: ScanResult, keep_intermediates: bool) -> Result<Value> {
    finish_capture(result, keep_intermediates, true)
}

fn finish_capture(
    result: ScanResult,
    keep_intermediates: bool,
    remove_raw_without_tiff: bool,
) -> Result<Value> {
    let mut output = serde_json::to_value(&result)?;
    if keep_intermediates {
        return Ok(output);
    }
    let trace = result.manifest.with_extension("protocol.jsonl");
    let mut cleanup = json!({
        "state":"pending", "keep_intermediates":false,
        "protocol_trace_file":trace, "errors":[]
    });
    let images: Vec<_> = [
        ("rgb", result.rgb.as_ref()),
        ("gray", result.gray.as_ref()),
        ("ir", result.ir.as_ref()),
        ("thumbnail", result.thumbnail.as_ref()),
    ]
    .into_iter()
    .filter_map(|(name, image)| image.map(|image| (name, image)))
    .collect();

    let prepared: Result<Value> = (|| {
        let mut manifest: Value = serde_json::from_slice(&fs::read(&result.manifest)?)?;
        if manifest["complete"] != true {
            return Err(epscan::Error::Invalid(
                "Capture manifest is not complete".into(),
            ));
        }
        if remove_raw_without_tiff
            && (images.is_empty()
                || manifest["passes"].as_array().map(Vec::len) != Some(images.len()))
        {
            return Err(epscan::Error::Invalid(
                "Source manifest does not match its acquired passes".into(),
            ));
        }
        for (name, image) in &images {
            // Compare in the same representation as the on-disk manifest.
            // JSON's default f64 parser may round a diagnostic by one ULP;
            // comparing it with the original in-memory value can block valid
            // exports. Strings, paths, flags and configuration stay exact.
            let expected_banding: Value =
                serde_json::from_slice(&serde_json::to_vec(&image.metadata["banding"])?)?;
            let pass = manifest["passes"]
                .as_array_mut()
                .and_then(|passes| passes.iter_mut().find(|pass| pass["pass"] == *name))
                .ok_or_else(|| {
                    epscan::Error::Invalid(format!("Missing {name} pass in capture manifest"))
                })?;
            if pass["complete"] != true
                || pass["payload_file"] != serde_json::to_value(&image.payload)?
                || pass["tiff_file"] != serde_json::to_value(&image.tiff)?
                || pass["banding"] != expected_banding
            {
                return Err(epscan::Error::Invalid(format!(
                    "Capture manifest does not match {name} output"
                )));
            }
            pass["payload_retained"] = true.into();
            output[*name]["metadata"]["payload_retained"] = true.into();
            if remove_raw_without_tiff {
                // Preflight every source before removing any payload. A missing
                // or truncated later source must retain all remaining evidence.
                let expected = u64::from(image.width)
                    .checked_mul(u64::from(image.height))
                    .and_then(|bytes| bytes.checked_mul(u64::from(image.channels)))
                    .and_then(|bytes| bytes.checked_mul(u64::from(image.depth / 8)))
                    .filter(|bytes| *bytes > 0 && matches!(image.depth, 8 | 16))
                    .ok_or_else(|| {
                        epscan::Error::Invalid(format!("Invalid {name} source dimensions"))
                    })?;
                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&image.payload)?;
                let info = file.metadata()?;
                if !info.is_file() || info.len() != expected {
                    return Err(epscan::Error::Invalid(format!(
                        "Source payload is not a complete file: {}",
                        image.payload.display()
                    )));
                }
                file.sync_all()?;
            }
            if let Some(tiff) = &image.tiff {
                // Every TIFF must be durable before deleting the first raw
                // payload, including in jobs with a later IR/thumbnail pass.
                sync_artifact(tiff)?;
            }
            if let Some(banding) = image.metadata.get("banding") {
                if !banding.is_object()
                    || image.tiff.is_none()
                    || banding["corrected_tiff_file"] != serde_json::to_value(&image.tiff)?
                {
                    return Err(epscan::Error::Invalid(format!(
                        "Banding metadata does not match {name} corrected TIFF"
                    )));
                }
                for (field, requested) in [
                    ("raw_tiff_file", "save_raw"),
                    ("signal_png_file", "save_signal"),
                ] {
                    if let Some(path) = banding[field].as_str().filter(|path| !path.is_empty()) {
                        sync_artifact(Path::new(path))?;
                    } else if !banding[field].is_null() || banding["config"][requested] == true {
                        return Err(epscan::Error::Invalid(format!(
                            "Missing {field} in {name} banding metadata"
                        )));
                    }
                }
            }
        }
        // Persist an explicit pending state before any deletion. If the
        // process stops mid-cleanup, old paths are not claimed to be current.
        manifest["cleanup"] = cleanup.clone();
        write_manifest(&result.manifest, &manifest)?;
        Ok(manifest)
    })();
    let mut manifest = match prepared {
        Ok(manifest) => manifest,
        Err(error) => {
            cleanup["state"] = "blocked".into();
            record_error(&mut cleanup, format!("No intermediates removed: {error}"));
            output["cleanup"] = cleanup;
            return Ok(output);
        }
    };

    for (name, image) in &images {
        // With --raw-only the payload is the requested output, not temporary.
        if image.tiff.is_none() && !remove_raw_without_tiff {
            continue;
        }
        match remove_if_present(&image.payload) {
            Ok(()) => {
                output[*name]["payload"] = Value::Null;
                output[*name]["metadata"]["payload_file"] = Value::Null;
                output[*name]["metadata"]["payload_retained"] = false.into();
                let pass = manifest["passes"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .find(|pass| pass["pass"] == *name)
                    .unwrap();
                pass["payload_file"] = Value::Null;
                pass["payload_retained"] = false.into();
            }
            Err(error) => record_error(
                &mut cleanup,
                format!("Could not remove {}: {error}", image.payload.display()),
            ),
        }
    }
    // Preserve the trace if raw cleanup fails, along with any remaining raw.
    if cleanup["errors"].as_array().unwrap().is_empty() {
        match remove_if_present(&trace) {
            Ok(()) => cleanup["protocol_trace_file"] = Value::Null,
            Err(error) => record_error(
                &mut cleanup,
                format!("Could not remove {}: {error}", trace.display()),
            ),
        }
    }
    cleanup["state"] = if cleanup["errors"].as_array().unwrap().is_empty() {
        "complete".into()
    } else {
        "partial".into()
    };
    manifest["cleanup"] = cleanup.clone();
    if let Err(error) = write_manifest(&result.manifest, &manifest) {
        cleanup["state"] = "metadata_update_failed".into();
        record_error(
            &mut cleanup,
            format!(
                "Cleanup ran, but final metadata could not be saved; the sidecar remains marked pending: {error}"
            ),
        );
    }
    output["cleanup"] = cleanup;
    Ok(output)
}

fn sync_artifact(path: &Path) -> Result<()> {
    let file = OpenOptions::new().read(true).write(true).open(path)?;
    let info = file.metadata()?;
    if !info.is_file() || info.len() == 0 {
        return Err(epscan::Error::Invalid(format!(
            "Exported artifact is not a complete file: {}",
            path.display()
        )));
    }
    file.sync_all()?;
    Ok(())
}

fn record_error(cleanup: &mut Value, error: String) {
    log::warn!("Capture succeeded; cleanup: {error}");
    cleanup["errors"].as_array_mut().unwrap().push(error.into());
}

fn remove_if_present(path: &Path) -> std::io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Replace the sidecar only after the new contents have been flushed. A failed
/// write leaves the previous valid manifest intact. Never reuse an existing temp.
fn write_manifest(path: &Path, manifest: &Value) -> Result<()> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let mut bytes = serde_json::to_vec_pretty(manifest)?;
    bytes.push(b'\n');
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(format!(
        ".cleanup-{}-{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let temporary = std::path::PathBuf::from(temp_name);
    // If creation fails, the existing file belongs to somebody else: do not
    // remove it in the error cleanup below.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    let saved = (|| {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)
    })();
    if saved.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    Ok(saved?)
}

#[cfg(test)]
#[path = "retention_tests.rs"]
mod tests;
