// SPDX-License-Identifier: MIT
//! Shared acquisitions and exact extraction for explicitly placed crop regions.
use super::{
    Progress, ScanOptions, ScanPlan, ScanResult,
    crop::{FrameExtraction, PreparedFrameBanding, extract_frame_with_banding},
    io::suffix,
    regions::{RegionScanPlan, plan_region_batches},
    stream::{self, StreamBatch},
};
use crate::{Capabilities, Error, Result, ScanSettings, Session};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
};

impl Session {
    /// Scan nearby regions together and export completed frames without resampling.
    ///
    /// Results retain input order. `region_done` receives the original zero-based
    /// index as each output completes; false cancels. Completed files are retained
    /// on failure. Progress follows global acquisition bytes, with `pass`
    /// identifying the acquisition, and reaches 100% after all output callbacks.
    /// Grouped visible scans acquire on a worker while the caller exports ready
    /// frames and runs callbacks. Banding waits for the strip and reuses its fit.
    #[allow(clippy::too_many_arguments)]
    pub fn scan_regions(
        &mut self,
        regions: &[[f64; 4]],
        settings: &ScanSettings,
        options: &ScanOptions,
        basename: &Path,
        max_gap_mm: f64,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
        region_done: &mut dyn FnMut(usize, &ScanResult) -> Result<bool>,
    ) -> Result<Vec<ScanResult>> {
        self.protocol()?;
        let caps = self.capabilities.clone();
        let plan = plan_region_batches(regions, settings, options, &caps, max_gap_mm)?;
        let outcome = execute_with_capture(
            regions,
            settings,
            options,
            basename,
            &caps,
            &plan,
            cancel,
            progress,
            region_done,
            self,
        );
        if outcome.is_err() {
            self.close();
        }
        outcome
    }
}

fn plan_bytes(plan: &ScanPlan) -> Result<u64> {
    plan.passes.iter().try_fold(0u64, |total, pass| {
        total
            .checked_add(pass.expected_bytes)
            .ok_or_else(|| Error::Invalid("Region batch size overflow".into()))
    })
}

struct BatchProgress<'a> {
    callback: &'a mut dyn FnMut(Progress<'_>) -> bool,
    total: u64,
    completed: u64,
    reported: u64,
    batch: usize,
}

impl BatchProgress<'_> {
    fn report(&mut self, update: Progress<'_>, plan: &ScanPlan) -> bool {
        // Setup, saving and other processing phases use sentinel counters,
        // not acquired bytes. Only a pass's transfer phase advances progress.
        let Some(pass) = plan
            .passes
            .get(update.pass)
            .filter(|pass| update.phase == pass.kind.name())
        else {
            return self.hold(update);
        };
        let preceding: u64 = plan
            .passes
            .iter()
            .take(update.pass)
            .map(|pass| pass.expected_bytes)
            .sum();
        let fraction = if update.total > 0 {
            (u128::from(pass.expected_bytes) * u128::from(update.done.min(update.total))
                / u128::from(update.total)) as u64
        } else {
            0
        };
        self.emit(update.phase, self.completed + preceding + fraction)
    }

    fn hold(&mut self, update: Progress<'_>) -> bool {
        self.emit(update.phase, self.completed)
    }

    fn emit(&mut self, phase: &str, acquired: u64) -> bool {
        // Reserve the final byte until extraction and the output callbacks
        // succeed. A failed final save must never appear as a completed job.
        self.reported = self
            .reported
            .max(acquired.min(self.total.saturating_sub(1)));
        (self.callback)(Progress {
            phase,
            pass: self.batch,
            done: self.reported,
            total: self.total,
        })
    }

    fn finish_batch(&mut self) -> Result<()> {
        self.reported = self.completed;
        if !(self.callback)(Progress {
            phase: "regions",
            pass: self.batch,
            done: self.reported,
            total: self.total,
        }) {
            return Err(Error::Cancelled);
        }
        Ok(())
    }
}

#[cfg(test)]
type Capture<'a> = dyn FnMut(
        &ScanSettings,
        &ScanOptions,
        &Path,
        &mut dyn FnMut(Progress<'_>) -> bool,
    ) -> Result<ScanResult>
    + 'a;

trait RegionCapture {
    fn capture(
        &mut self,
        settings: &ScanSettings,
        options: &ScanOptions,
        basename: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
    ) -> Result<ScanResult>;

    fn stream(
        &mut self,
        _batch: &StreamBatch<'_>,
        _cancel: &AtomicBool,
        _progress: &mut dyn FnMut(Progress<'_>) -> bool,
        _region_done: &mut dyn FnMut(usize, &ScanResult) -> Result<bool>,
    ) -> Option<Result<Vec<(usize, ScanResult)>>> {
        None
    }
}

impl RegionCapture for Session {
    fn capture(
        &mut self,
        settings: &ScanSettings,
        options: &ScanOptions,
        basename: &Path,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
    ) -> Result<ScanResult> {
        self.scan(settings, options, basename, cancel, progress)
    }

    fn stream(
        &mut self,
        batch: &StreamBatch<'_>,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
        region_done: &mut dyn FnMut(usize, &ScanResult) -> Result<bool>,
    ) -> Option<Result<Vec<(usize, ScanResult)>>> {
        Some(stream::capture(self, batch, cancel, progress, region_done))
    }
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn execute_regions(
    regions: &[[f64; 4]],
    settings: &ScanSettings,
    options: &ScanOptions,
    basename: &Path,
    caps: &Capabilities,
    plan: &RegionScanPlan,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    region_done: &mut dyn FnMut(usize, &ScanResult) -> Result<bool>,
    capture: &mut Capture<'_>,
) -> Result<Vec<ScanResult>> {
    struct FixtureCapture<'a, 'b>(&'a mut Capture<'b>);
    impl RegionCapture for FixtureCapture<'_, '_> {
        fn capture(
            &mut self,
            settings: &ScanSettings,
            options: &ScanOptions,
            basename: &Path,
            _cancel: &AtomicBool,
            progress: &mut dyn FnMut(Progress<'_>) -> bool,
        ) -> Result<ScanResult> {
            (self.0)(settings, options, basename, progress)
        }
    }
    execute_with_capture(
        regions,
        settings,
        options,
        basename,
        caps,
        plan,
        cancel,
        progress,
        region_done,
        &mut FixtureCapture(capture),
    )
}

#[allow(clippy::too_many_arguments)]
fn execute_with_capture(
    regions: &[[f64; 4]],
    settings: &ScanSettings,
    options: &ScanOptions,
    basename: &Path,
    caps: &Capabilities,
    plan: &RegionScanPlan,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    region_done: &mut dyn FnMut(usize, &ScanResult) -> Result<bool>,
    capture: &mut dyn RegionCapture,
) -> Result<Vec<ScanResult>> {
    let mut total = 0u64;
    for batch in &plan.batches {
        total = total
            .checked_add(plan_bytes(&batch.plan)?)
            .ok_or_else(|| Error::Invalid("Region batch size overflow".into()))?;
    }
    let mut progress = BatchProgress {
        callback: progress,
        total,
        completed: 0,
        reported: 0,
        batch: 0,
    };
    let mut results: Vec<Option<ScanResult>> = (0..regions.len()).map(|_| None).collect();
    let target_name = |index| {
        if regions.len() == 1 {
            basename.to_owned()
        } else {
            suffix(basename, &format!("_area{:02}", index + 1))
        }
    };
    for (batch_index, batch) in plan.batches.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        progress.batch = batch_index;
        let grouped = batch.region_indices.len() > 1;
        let capture_options = if grouped {
            ScanOptions {
                export_tiff: false,
                measure_sharpness: false,
                banding: None,
                holder_selection: None,
                ..options.clone()
            }
        } else {
            options.clone()
        };
        let capture_name = if grouped {
            suffix(basename, &format!("_strip{:02}", batch_index + 1))
        } else {
            target_name(batch.region_indices[0])
        };
        let targets: Vec<_> = batch
            .region_indices
            .iter()
            .map(|&index| {
                (
                    index,
                    FrameExtraction {
                        settings: ScanSettings {
                            rect_mm: regions[index],
                            ..settings.clone()
                        },
                        options: options.clone(),
                        basename: target_name(index),
                    },
                )
            })
            .collect();
        if grouped
            && options.banding.is_none()
            && let Some(streamed) = capture.stream(
                &StreamBatch {
                    settings: &batch.settings,
                    options: &capture_options,
                    basename: &capture_name,
                    plan: &batch.plan,
                    targets: &targets,
                    caps,
                },
                cancel,
                &mut |update| progress.report(update, &batch.plan),
                region_done,
            )
        {
            for (index, result) in streamed? {
                results[index] = Some(result);
            }
            progress.completed += plan_bytes(&batch.plan)?;
            progress.finish_batch()?;
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            continue;
        }
        let source = capture.capture(
            &batch.settings,
            &capture_options,
            &capture_name,
            cancel,
            &mut |update| progress.report(update, &batch.plan),
        )?;
        progress.completed += plan_bytes(&batch.plan)?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
        if grouped {
            if options.banding.is_some()
                && !progress.hold(Progress {
                    phase: "banding",
                    pass: 0,
                    done: 0,
                    total: 0,
                })
            {
                return Err(Error::Cancelled);
            }
            let banding = PreparedFrameBanding::prepare(&source, &batch.plan, options, cancel)?;
            for (index, target) in targets {
                let result = extract_frame_with_banding(
                    &source,
                    &batch.plan,
                    &target,
                    caps,
                    cancel,
                    &mut |update| progress.hold(update),
                    banding.as_ref(),
                )?;
                if !region_done(index, &result)? || cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                results[index] = Some(result);
            }
        } else {
            let index = batch.region_indices[0];
            if !region_done(index, &source)? || cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            results[index] = Some(source);
        }
        progress.finish_batch()?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Error::Cancelled);
        }
    }
    results
        .into_iter()
        .map(|result| result.ok_or_else(|| Error::Invalid("Region batch omitted an output".into())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Holder, PassKind, ScanMode, protocol::hex, session::image::ImageResult};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use std::{cell::Cell, fs};

    fn capabilities() -> Capabilities {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/v800-identity.json")).unwrap();
        let text = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes: Vec<_> = (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect();
        Capabilities::parse(&bytes).unwrap()
    }

    fn regions(caps: &Capabilities) -> Vec<[f64; 4]> {
        caps.scanner_model()
            .unwrap()
            .holder(Holder::V800Film35mm)
            .unwrap()
            .frames_mm
            .iter()
            .map(|[x, y, w, h]| [x + 0.2, y + 0.4, w - 1.0, h - 1.0])
            .collect()
    }

    fn sample_bytes(pixels: [u32; 4], channels: u8, depth: u8) -> Vec<u8> {
        let [left, top, width, height] = pixels;
        let mut bytes = Vec::new();
        for y in top..top + height {
            for x in left..left + width {
                for channel in 0..channels {
                    let value = ((x * 17 + y * 23 + u32::from(channel) * 47) % 256) as u8;
                    if depth == 8 {
                        bytes.push(value);
                    } else {
                        bytes.extend((u16::from(value) * 257).to_le_bytes());
                    }
                }
            }
        }
        bytes
    }

    fn capture_fixture(
        caps: &Capabilities,
        settings: &ScanSettings,
        options: &ScanOptions,
        basename: &Path,
        progress: &mut dyn FnMut(Progress<'_>) -> bool,
    ) -> Result<ScanResult> {
        let plan = options.plan(settings, caps)?;
        let mut result = ScanResult {
            rgb: None,
            gray: None,
            ir: None,
            thumbnail: None,
            manifest: suffix(basename, ".json"),
        };
        for (index, pass) in plan.passes.iter().enumerate() {
            if !progress(Progress {
                phase: "setup",
                pass: index,
                done: 0,
                total: 1,
            }) {
                return Err(Error::Cancelled);
            }
            let bytes = sample_bytes(pass.pixels, pass.channels, pass.settings.depth);
            let payload = suffix(basename, &format!("_{}.bin", pass.kind.name()));
            fs::write(&payload, &bytes)?;
            let image = ImageResult {
                payload,
                tiff: None,
                width: pass.pixels[2],
                height: pass.pixels[3],
                channels: pass.channels,
                depth: pass.settings.depth,
                dpi: pass.settings.dpi,
                metadata: json!({"complete":true,"acquisition_complete":true,"settings":pass.settings,
                    "sha256":hex(&Sha256::digest(&bytes))}),
            };
            match pass.kind {
                PassKind::Rgb => result.rgb = Some(image),
                PassKind::Gray => result.gray = Some(image),
                PassKind::Ir => result.ir = Some(image),
                PassKind::Thumbnail => result.thumbnail = Some(image),
            }
            for quarter in 1..=4 {
                if !progress(Progress {
                    phase: pass.kind.name(),
                    pass: index,
                    done: pass.expected_bytes * quarter / 4,
                    total: pass.expected_bytes,
                }) {
                    return Err(Error::Cancelled);
                }
            }
            if !progress(Progress {
                phase: "saving",
                pass: index,
                done: 1,
                total: 1,
            }) {
                return Err(Error::Cancelled);
            }
        }
        fs::write(&result.manifest, br#"{"complete":true}"#)?;
        Ok(result)
    }

    #[test]
    fn progress_tracks_acquisition_bytes_without_counting_extraction_again() {
        let caps = capabilities();
        let nominal = regions(&caps);
        // Cover both three full strips and unequal acquisitions, including a
        // single frame that does not require extraction from a shared source.
        for indices in [(0..18).collect::<Vec<_>>(), (0..9).chain([12]).collect()] {
            let regions: Vec<_> = indices.iter().map(|&index| nominal[index]).collect();
            let directory = tempfile::tempdir().unwrap();
            let settings = ScanSettings {
                dpi: 100,
                depth: 8,
                mode: ScanMode::Gray,
                ..Default::default()
            };
            let options = ScanOptions {
                export_tiff: false,
                ..Default::default()
            };
            let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
            assert_eq!(plan.batches.len(), 3);
            let bytes: Vec<_> = plan
                .batches
                .iter()
                .map(|batch| plan_bytes(&batch.plan).unwrap())
                .collect();
            let total = bytes.iter().sum::<u64>();
            let completed_outputs = Cell::new(0);
            let mut updates = Vec::new();
            execute_regions(
                &regions,
                &settings,
                &options,
                &directory.path().join("roll"),
                &caps,
                &plan,
                &AtomicBool::new(false),
                &mut |p| {
                    updates.push((
                        p.phase.to_owned(),
                        p.pass,
                        p.done,
                        p.total,
                        completed_outputs.get(),
                    ));
                    true
                },
                &mut |_, _| {
                    completed_outputs.set(completed_outputs.get() + 1);
                    Ok(true)
                },
                &mut |settings, options, basename, progress| {
                    capture_fixture(&caps, settings, options, basename, progress)
                },
            )
            .unwrap();
            assert!(updates.iter().all(|update| update.3 == total));
            assert!(updates.windows(2).all(|pair| pair[0].2 <= pair[1].2));
            assert!(
                updates[..updates.len() - 1]
                    .iter()
                    .all(|update| update.2 < total)
            );
            assert_eq!(
                updates.last().unwrap(),
                &("regions".into(), 2, total, total, regions.len())
            );
            for (batch, &capture_bytes) in bytes.iter().enumerate() {
                let preceding = bytes[..batch].iter().sum::<u64>();
                let transfer: Vec<_> = updates
                    .iter()
                    .filter(|update| update.0 == "gray" && update.1 == batch)
                    .map(|update| update.2)
                    .collect();
                assert_eq!(
                    transfer,
                    (1..=4)
                        .map(|quarter| { (preceding + capture_bytes * quarter / 4).min(total - 1) })
                        .collect::<Vec<_>>()
                );
                let settled = (preceding + capture_bytes).min(total - 1);
                assert!(
                    updates
                        .iter()
                        .filter(|update| {
                            update.1 == batch
                                && matches!(update.0.as_str(), "extracting" | "saving")
                        })
                        .all(|update| update.2 == settled)
                );
            }
        }
    }

    #[test]
    fn only_transfer_phases_advance_multipass_progress() {
        let caps = capabilities();
        let plan = ScanOptions {
            thumbnail: true,
            infrared: true,
            ..Default::default()
        }
        .plan(&ScanSettings::default(), &caps)
        .unwrap();
        let mut updates = Vec::new();
        let mut callback = |p: Progress<'_>| {
            updates.push(p.done);
            true
        };
        let mut progress = BatchProgress {
            callback: &mut callback,
            total: plan_bytes(&plan).unwrap(),
            completed: 0,
            reported: 0,
            batch: 0,
        };
        let mut expected = Vec::new();
        let mut preceding = 0;
        for (index, pass) in plan.passes.iter().enumerate() {
            for phase in ["setup", "settling", "saving", "banding", "sharpness"] {
                assert!(progress.report(
                    Progress {
                        phase,
                        pass: index,
                        done: 1,
                        total: 1
                    },
                    &plan
                ));
                expected.push(preceding);
            }
            assert!(progress.report(
                Progress {
                    phase: pass.kind.name(),
                    pass: index,
                    done: pass.expected_bytes / 2,
                    total: pass.expected_bytes,
                },
                &plan
            ));
            expected.push(preceding + pass.expected_bytes / 2);
            // Indeterminate postprocessing and a stale transfer report cannot
            // move progress forwards or backwards.
            assert!(progress.report(
                Progress {
                    phase: "saving",
                    pass: index,
                    done: 1,
                    total: 1
                },
                &plan
            ));
            expected.push(preceding + pass.expected_bytes / 2);
            assert!(progress.report(
                Progress {
                    phase: pass.kind.name(),
                    pass: index,
                    done: 0,
                    total: pass.expected_bytes
                },
                &plan
            ));
            expected.push(preceding + pass.expected_bytes / 2);
            assert!(progress.report(
                Progress {
                    phase: pass.kind.name(),
                    pass: index,
                    done: pass.expected_bytes,
                    total: pass.expected_bytes,
                },
                &plan
            ));
            preceding += pass.expected_bytes;
            expected.push(preceding.min(progress.total - 1));
        }
        assert_eq!(updates, expected);
    }

    #[test]
    fn eighteen_edited_regions_use_three_acquisitions_with_exact_original_samples() {
        let caps = capabilities();
        let regions = regions(&caps);
        for depth in [8, 16] {
            for mode in [ScanMode::Rgb, ScanMode::Gray] {
                let directory = tempfile::tempdir().unwrap();
                let settings = ScanSettings {
                    dpi: 100,
                    depth,
                    mode,
                    ..Default::default()
                };
                let options = ScanOptions {
                    export_tiff: false,
                    ..Default::default()
                };
                let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
                let mut captures = Vec::new();
                let mut completed = Vec::new();
                let mut updates = Vec::new();
                let results = execute_regions(
                    &regions,
                    &settings,
                    &options,
                    &directory.path().join("roll"),
                    &caps,
                    &plan,
                    &AtomicBool::new(false),
                    &mut |p| {
                        updates.push((p.done, p.total));
                        true
                    },
                    &mut |index, result| {
                        assert!(result.manifest.exists());
                        completed.push(index);
                        Ok(true)
                    },
                    &mut |settings, options, basename, progress| {
                        captures.push(settings.rect_mm);
                        capture_fixture(&caps, settings, options, basename, progress)
                    },
                )
                .unwrap();
                assert_eq!(captures.len(), 3);
                assert_eq!(completed, (0..18).collect::<Vec<_>>());
                assert_eq!(results.len(), 18);
                for (result, expected) in results.iter().zip(&plan.region_plans) {
                    let image = result.rgb.as_ref().or(result.gray.as_ref()).unwrap();
                    let pass = &expected.passes[0];
                    assert_eq!(
                        fs::read(&image.payload).unwrap(),
                        sample_bytes(pass.pixels, pass.channels, depth)
                    );
                    assert_eq!(image.metadata["effective_pixels"], json!(pass.pixels));
                    assert_eq!(
                        image.metadata["sample_transform"],
                        "none; exact packed samples"
                    );
                }
                assert!(updates.windows(2).all(|pair| pair[0].0 <= pair[1].0));
                assert_eq!(updates.last().unwrap().0, updates.last().unwrap().1);
            }
        }
    }

    #[test]
    fn interleaved_requests_return_in_input_order_with_original_callback_indices() {
        let caps = capabilities();
        let nominal = regions(&caps);
        let regions: Vec<_> = [0, 6, 12, 1, 7, 13].map(|index| nominal[index]).into();
        let directory = tempfile::tempdir().unwrap();
        let settings = ScanSettings {
            dpi: 100,
            depth: 8,
            ..Default::default()
        };
        let options = ScanOptions {
            export_tiff: false,
            ..Default::default()
        };
        let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
        let mut completed = Vec::new();
        let results = execute_regions(
            &regions,
            &settings,
            &options,
            &directory.path().join("roll"),
            &caps,
            &plan,
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |index, _| {
                completed.push(index);
                Ok(true)
            },
            &mut |settings, options, basename, progress| {
                capture_fixture(&caps, settings, options, basename, progress)
            },
        )
        .unwrap();
        assert_eq!(completed, [0, 3, 1, 4, 2, 5]);
        for (index, result) in results.iter().enumerate() {
            assert!(
                result
                    .manifest
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("roll_area{:02}_", index + 1))
            );
            assert_eq!(
                result.rgb.as_ref().unwrap().metadata["requested_rect_mm"],
                json!(regions[index])
            );
        }
    }

    #[test]
    fn cancellation_or_output_failure_retains_finished_regions_without_another_scan() {
        let caps = capabilities();
        let regions = regions(&caps);
        let settings = ScanSettings {
            dpi: 100,
            depth: 8,
            ..Default::default()
        };
        let options = ScanOptions {
            export_tiff: false,
            ..Default::default()
        };
        let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
        for (fail, last_output) in [(false, 5), (true, 5), (false, 17), (true, 17)] {
            let directory = tempfile::tempdir().unwrap();
            let mut captures = 0;
            let mut completed = Vec::new();
            let mut updates = Vec::new();
            let outcome = execute_regions(
                &regions,
                &settings,
                &options,
                &directory.path().join("roll"),
                &caps,
                &plan,
                &AtomicBool::new(false),
                &mut |p| {
                    updates.push((p.done, p.total));
                    true
                },
                &mut |index, result| {
                    completed.push(result.manifest.clone());
                    if index == last_output {
                        if fail {
                            return Err(Error::Invalid("Output consumer failed".into()));
                        }
                        return Ok(false);
                    }
                    Ok(true)
                },
                &mut |settings, options, basename, progress| {
                    captures += 1;
                    capture_fixture(&caps, settings, options, basename, progress)
                },
            );
            assert!(outcome.is_err());
            assert_eq!(captures, last_output / 6 + 1);
            assert_eq!(completed.len(), last_output + 1);
            assert!(completed.iter().all(|path| path.exists()));
            assert_eq!(matches!(outcome, Err(Error::Cancelled)), !fail);
            assert!(updates.iter().all(|(done, total)| done < total));
        }
    }

    #[test]
    fn a_later_capture_failure_keeps_prior_strip_outputs() {
        let caps = capabilities();
        let regions = regions(&caps);
        let directory = tempfile::tempdir().unwrap();
        let settings = ScanSettings {
            dpi: 100,
            depth: 8,
            ..Default::default()
        };
        let options = ScanOptions {
            export_tiff: false,
            ..Default::default()
        };
        let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
        let mut captures = 0;
        let mut completed = Vec::new();
        let outcome = execute_regions(
            &regions,
            &settings,
            &options,
            &directory.path().join("roll"),
            &caps,
            &plan,
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |_, result| {
                completed.push(result.manifest.clone());
                Ok(true)
            },
            &mut |settings, options, basename, progress| {
                captures += 1;
                if captures == 2 {
                    return Err(Error::Protocol("Transfer failed".into()));
                }
                capture_fixture(&caps, settings, options, basename, progress)
            },
        );
        assert!(matches!(outcome, Err(Error::Protocol(_))));
        assert_eq!(captures, 2);
        assert_eq!(completed.len(), 6);
        assert!(completed.iter().all(|path| path.exists()));
    }

    #[test]
    fn final_progress_can_cancel_by_flag_and_keep_the_completed_output() {
        let caps = capabilities();
        let regions = vec![regions(&caps)[0]];
        let directory = tempfile::tempdir().unwrap();
        let settings = ScanSettings {
            dpi: 100,
            depth: 8,
            ..Default::default()
        };
        let options = ScanOptions {
            export_tiff: false,
            ..Default::default()
        };
        let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
        let cancel = AtomicBool::new(false);
        let mut completed = Vec::new();
        let outcome = execute_regions(
            &regions,
            &settings,
            &options,
            &directory.path().join("roll"),
            &caps,
            &plan,
            &cancel,
            &mut |p| {
                if p.phase == "regions" {
                    cancel.store(true, Ordering::Relaxed);
                }
                true
            },
            &mut |_, result| {
                completed.push(result.manifest.clone());
                Ok(true)
            },
            &mut |settings, options, basename, progress| {
                capture_fixture(&caps, settings, options, basename, progress)
            },
        );
        assert!(matches!(outcome, Err(Error::Cancelled)));
        assert_eq!(completed.len(), 1);
        assert!(completed[0].exists());
    }

    #[test]
    fn separate_thumbnail_rgb_and_infrared_requests_keep_their_original_settings() {
        let caps = capabilities();
        let regions = regions(&caps)[..2].to_vec();
        let directory = tempfile::tempdir().unwrap();
        let settings = ScanSettings {
            dpi: 150,
            depth: 16,
            ..Default::default()
        };
        let options = ScanOptions {
            export_tiff: false,
            thumbnail: true,
            infrared: true,
            ..Default::default()
        };
        let plan = plan_region_batches(&regions, &settings, &options, &caps, 10.0).unwrap();
        let mut captures = 0;
        let results = execute_regions(
            &regions,
            &settings,
            &options,
            &directory.path().join("roll"),
            &caps,
            &plan,
            &AtomicBool::new(false),
            &mut |_| true,
            &mut |_, _| Ok(true),
            &mut |settings, options, basename, progress| {
                captures += 1;
                assert_eq!(
                    (settings.dpi, settings.depth, settings.preview),
                    (150, 16, false)
                );
                capture_fixture(&caps, settings, options, basename, progress)
            },
        )
        .unwrap();
        assert_eq!(captures, 2);
        for result in results {
            let rgb = result.rgb.unwrap();
            let ir = result.ir.unwrap();
            let thumbnail = result.thumbnail.unwrap();
            assert_eq!((rgb.dpi, rgb.depth), (150, 16));
            assert_eq!((ir.dpi, ir.depth), (150, 8));
            assert_eq!((thumbnail.dpi, thumbnail.depth), (100, 8));
        }
    }
}
