// SPDX-License-Identifier: MIT
use super::{
    cancel,
    cli::{Capture, Preview, Scan},
    progress, retention,
};
use epscan::scan::{
    PassKind, Progress, ScanPlan,
    crop::{FrameExtraction, extract_frame},
    holder::{HolderFramePlan, plan_holder_batches},
    io::suffix,
    regions::plan_region_batches,
};
use epscan::{
    Backend, Capabilities, Error, Gamma, Result, ScanOptions, ScanResult, ScanSettings, Session,
};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};
use std::{borrow::Cow, path::PathBuf, time::Duration};
use tracing::info;

/// How often the spinner moves while that is going on
const SPINNER_TICK: Duration = Duration::from_millis(120);

pub fn scan(args: Scan, backend: Backend, timeout: Duration) -> Result<()> {
    args.validate_options()?;
    let session = Session::connect(args.capture.device.as_deref(), backend, timeout)?;
    let settings = ScanSettings {
        mode: args.capture.mode,
        dpi: args.dpi,
        y_oversampling: args.y_oversampling,
        depth: args.depth,
        gamma: args.gamma,
        preview: false,
        ..ScanSettings::default()
    };
    let options = ScanOptions {
        infrared: args.ir,
        infrared_only: args.ir_only,
        ir_gamma: args.ir_gamma.unwrap_or(Gamma::DeviceDefault),
        thumbnail: args.thumbnail,
        export_tiff: !args.capture.raw_only,
        banding: args.capture.banding_options()?,
        film: args.capture.film_name().into(),
        measure_sharpness: args.capture.measure_sharpness,
        pass_timeout: Duration::from_secs(args.capture.scan_timeout),
        settle_time: Duration::from_secs(args.settle_seconds),
        ..ScanOptions::default()
    };
    acquire(session, args.capture, settings, options)
}

pub fn preview(args: Preview, backend: Backend, timeout: Duration) -> Result<()> {
    args.capture.validate_area()?;
    let session = Session::connect(args.capture.device.as_deref(), backend, timeout)?;
    let model = session.capabilities.scanner_model()?;
    let settings = ScanSettings {
        mode: args.capture.mode,
        dpi: model.preview_dpi,
        depth: model.preview_depth,
        gamma: args.gamma,
        preview: true,
        ..ScanSettings::default()
    };
    let options = ScanOptions {
        export_tiff: !args.capture.raw_only,
        banding: args.capture.banding_options()?,
        film: args.capture.film_name().into(),
        measure_sharpness: args.capture.measure_sharpness,
        pass_timeout: Duration::from_secs(args.capture.scan_timeout),
        ..ScanOptions::default()
    };
    acquire(session, args.capture, settings, options)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CaptureSelection {
    Frame(u32),
    Area(usize),
}

impl CaptureSelection {
    fn label(self) -> String {
        match self {
            Self::Frame(frame) => format!("frame {frame}"),
            Self::Area(area) => format!("area {area}"),
        }
    }
}

#[derive(Clone)]
struct CaptureJob {
    selection: CaptureSelection,
    settings: ScanSettings,
    options: ScanOptions,
    plan: ScanPlan,
    basename: PathBuf,
}

struct CaptureBatch {
    targets: Vec<usize>,
    strip: Option<u32>,
    capture: CaptureJob,
}

/// Resolve and validate every requested area and pass before creating files or
/// sending scan setup commands. A bad later area must not leave a partial batch.
fn prepare_jobs(
    args: &Capture,
    caps: &Capabilities,
    settings: &ScanSettings,
    options: &ScanOptions,
) -> Result<Vec<CaptureJob>> {
    let areas = args.resolve_areas(caps)?;
    let count = areas.len();
    areas
        .into_iter()
        .enumerate()
        .map(|(index, area)| {
            let selection = match area.holder_selection {
                Some(holder) => CaptureSelection::Frame(holder.frame),
                None => CaptureSelection::Area(index + 1),
            };
            let settings = ScanSettings {
                source: area.source,
                rect_mm: area.rect_mm,
                ..settings.clone()
            };
            let options = ScanOptions {
                holder_selection: area.holder_selection,
                ..options.clone()
            };
            let plan = options.plan(&settings, caps)?;
            let basename = match selection {
                CaptureSelection::Frame(frame) => {
                    suffix(&args.basename, &format!("_frame{frame:02}"))
                }
                CaptureSelection::Area(area) if count > 1 => {
                    suffix(&args.basename, &format!("_area{area:02}"))
                }
                CaptureSelection::Area(_) => args.basename.clone(),
            };
            Ok(CaptureJob {
                selection,
                settings,
                options,
                plan,
                basename,
            })
        })
        .collect()
}

/// Combine registered strips or nearby explicit regions into shared acquisitions.
fn prepare_batches(
    args: &Capture,
    jobs: &[CaptureJob],
    caps: &Capabilities,
) -> Result<Vec<CaptureBatch>> {
    if args.holder.is_none() {
        let first = jobs
            .first()
            .ok_or_else(|| Error::Invalid("No scan regions".into()))?;
        let regions: Vec<_> = jobs.iter().map(|job| job.settings.rect_mm).collect();
        return plan_region_batches(&regions, &first.settings, &first.options, caps, 10.0)?
            .batches
            .into_iter()
            .enumerate()
            .map(|(index, batch)| {
                let mut capture = jobs[batch.region_indices[0]].clone();
                let strip = if batch.region_indices.len() > 1 {
                    capture.settings = batch.settings;
                    capture.options.holder_selection = None;
                    capture.options.export_tiff = false;
                    capture.options.banding = None;
                    capture.options.measure_sharpness = false;
                    capture.plan = batch.plan;
                    capture.basename = suffix(&args.basename, &format!("_strip{:02}", index + 1));
                    Some((index + 1) as u32)
                } else {
                    None
                };
                Ok(CaptureBatch {
                    targets: batch.region_indices,
                    strip,
                    capture,
                })
            })
            .collect();
    }
    let frames = jobs
        .iter()
        .map(|job| {
            Ok(HolderFramePlan {
                selection: job.options.holder_selection.ok_or_else(|| {
                    Error::Invalid("Holder frame is missing its selection".into())
                })?,
                plan: job.plan.clone(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    plan_holder_batches(&frames, caps)?
        .into_iter()
        .map(|batch| {
            let mut capture = jobs[batch.frame_indices[0]].clone();
            let strip = if batch.frame_indices.len() > 1 {
                let strip = batch.strip.ok_or_else(|| {
                    Error::Invalid("Grouped holder frames have no strip number".into())
                })?;
                capture.settings = batch.settings;
                capture.options.holder_selection = None;
                capture.options.export_tiff = false;
                capture.options.banding = None;
                capture.options.measure_sharpness = false;
                capture.plan = capture.options.plan(&capture.settings, caps)?;
                if capture.plan.passes.len() != 1
                    || capture.plan.passes[0].pixels != batch.plan.passes[0].pixels
                {
                    return Err(Error::Invalid("Strip acquisition geometry changed".into()));
                }
                capture.basename = suffix(&args.basename, &format!("_strip{strip:02}"));
                Some(strip)
            } else {
                None
            };
            Ok(CaptureBatch {
                targets: batch.frame_indices,
                strip,
                capture,
            })
        })
        .collect()
}

fn acquire(
    mut session: Session,
    args: Capture,
    settings: ScanSettings,
    options: ScanOptions,
) -> Result<()> {
    info!("connected to scanner");
    let jobs = prepare_jobs(&args, &session.capabilities, &settings, &options)?;
    let batches = prepare_batches(&args, &jobs, &session.capabilities)?;
    if cancel::requested() {
        return Err(Error::Cancelled);
    }
    if options.infrared || options.infrared_only {
        log::warn!(
            "IR is experimental: transfer completion does not prove spectral identity or RGB registration"
        );
    }
    let count = jobs.len();
    let mut completed: Vec<(usize, ScanResult)> = Vec::with_capacity(count);
    let mut sources: Vec<ScanResult> = Vec::new();
    for batch in batches {
        let job = &batch.capture;
        if let Some(strip) = batch.strip {
            let regions: Vec<_> = batch
                .targets
                .iter()
                .map(|index| jobs[*index].selection.label())
                .collect();
            info!(strip, ?regions, "scanning strip bounding box");
        } else {
            match job.selection {
                CaptureSelection::Frame(frame) => info!(
                    frame,
                    position = batch.targets[0] + 1,
                    total = count,
                    "scanning holder frame"
                ),
                CaptureSelection::Area(area) => {
                    info!(area, total = count, "scanning rectangle area")
                }
            }
        }
        let mut display = ScanProgress::new(&job.plan, job.options.export_tiff);
        let outcome = session.scan(
            &job.settings,
            &job.options,
            &job.basename,
            cancel::flag(),
            &mut |p| {
                display.report(p);
                true
            },
        );
        drop(display);
        let outcome = outcome.and_then(|result| {
            if batch.strip.is_some() {
                sources.push(result);
                let source = sources.last().expect("strip capture");
                for index in batch.targets {
                    let target = &jobs[index];
                    info!("extracting {}", target.selection.label());
                    let mut display = ScanProgress::new(&target.plan, target.options.export_tiff);
                    let extracted = extract_frame(
                        source,
                        &job.plan,
                        &FrameExtraction {
                            settings: target.settings.clone(),
                            options: target.options.clone(),
                            basename: target.basename.clone(),
                        },
                        &session.capabilities,
                        cancel::flag(),
                        &mut |p| {
                            display.report(p);
                            true
                        },
                    )?;
                    completed.push((index, extracted));
                }
            } else {
                completed.push((batch.targets[0], result));
            }
            Ok(())
        });
        match outcome {
            Ok(()) => (),
            Err(error) => {
                for (_, result) in &completed {
                    info!("completed capture retained: {}", result.manifest.display());
                }
                for result in &sources {
                    info!("strip capture retained: {}", result.manifest.display());
                }
                return Err(error);
            }
        }
    }
    // Retain all diagnostics if a later acquisition fails or is cancelled.
    // Only a completely captured batch is eligible for normal CLI cleanup.
    if cancel::requested() {
        return Err(Error::Cancelled);
    }
    completed.sort_by_key(|(index, _)| *index);
    let mut outputs = Vec::with_capacity(count);
    let mut clean_sources = true;
    for (index, result) in completed {
        let job = &jobs[index];
        let output = retention::finish(result, args.keep_intermediates)?;
        if !args.keep_intermediates && output["cleanup"]["state"] != "complete" {
            clean_sources = false;
        }
        if !args.json {
            report_completion(job.selection, &job.plan, &output);
        }
        outputs.push((job.selection, output));
    }
    // The shared raw is recoverable evidence until every requested frame has
    // been extracted, scored, exported, and its normal retention step succeeded.
    if clean_sources {
        for source in sources {
            retention::finish_source(source, args.keep_intermediates)?;
        }
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&result_document(outputs))?
        );
    }
    Ok(())
}

fn result_document(mut outputs: Vec<(CaptureSelection, serde_json::Value)>) -> serde_json::Value {
    if outputs.len() == 1 {
        return outputs.pop().expect("one capture").1;
    }
    let collection = match outputs.first().map(|(selection, _)| selection) {
        Some(CaptureSelection::Frame(_)) => "frames",
        _ => "areas",
    };
    let records: Vec<_> = outputs
        .into_iter()
        .map(|(selection, result)| match selection {
            CaptureSelection::Frame(frame) => serde_json::json!({"frame": frame, "result": result}),
            CaptureSelection::Area(area) => serde_json::json!({"area": area, "result": result}),
        })
        .collect();
    serde_json::json!({collection: records})
}

fn report_completion(selection: CaptureSelection, plan: &ScanPlan, output: &serde_json::Value) {
    for (n, pass) in plan.passes.iter().enumerate() {
        let image = &output[pass.kind.name()];
        let mut written: Vec<_> = ["tiff", "payload"]
            .into_iter()
            .filter_map(|field| image[field].as_str())
            .collect();
        for field in ["raw_tiff_file", "signal_png_file"] {
            if let Some(path) = image["metadata"]["banding"][field].as_str() {
                written.push(path);
            }
        }
        info!(
            pass = n + 1,
            "{} x {} at {} dpi, wrote {}",
            pass.pixels[2],
            pass.pixels[3],
            pass.settings.dpi,
            written.join(", ")
        );
        let scores = &image["metadata"]["sharpness"];
        if let (Some(tenengrad), Some(laplacian)) = (
            scores["tenengrad"].as_f64(),
            scores["variance_of_laplacian"].as_f64(),
        ) {
            let label = selection.label();
            info!(
                "{label}: measured sharpness: Tenengrad {tenengrad:.12}, variance of Laplacian {laplacian:.12} (higher is better)"
            );
        }
    }
    if let Some(manifest) = output["manifest"].as_str() {
        info!("wrote {manifest}");
    }
}

#[cfg(test)]
#[path = "scan_tests.rs"]
mod tests;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Preparing,
    Settling,
    Transfer,
    Extracting,
    Analyzing,
    Banding,
    Saving,
}

/// Match the original fork's pass bars to Epson's byte-progress callbacks.
/// A steady spinner also stays alive while a scanner command blocks, including
/// confirmed startup warmup. Logs share the same terminal drawing coordinator.
struct ScanProgress {
    labels: Vec<&'static str>,
    export_tiff: bool,
    active: Option<(usize, Stage)>,
    bar: Option<ProgressBar>,
}

impl ScanProgress {
    fn new(plan: &ScanPlan, export_tiff: bool) -> Self {
        Self {
            labels: plan
                .passes
                .iter()
                .map(|pass| match pass.kind {
                    PassKind::Thumbnail => "Thumbnail",
                    PassKind::Ir => "IR",
                    PassKind::Rgb | PassKind::Gray if pass.settings.preview => "Preview",
                    PassKind::Rgb => "RGB",
                    PassKind::Gray => "Gray",
                })
                .collect(),
            export_tiff,
            active: None,
            bar: None,
        }
    }

    fn report(&mut self, progress: Progress<'_>) {
        let stage = match progress.phase {
            "setup" => Stage::Preparing,
            "settling" => Stage::Settling,
            "sharpness" | "analyzing" => Stage::Analyzing,
            "banding" => Stage::Banding,
            "extracting" => Stage::Extracting,
            "saving" => Stage::Saving,
            _ if progress.total > 0 && progress.done >= progress.total => Stage::Saving,
            _ => Stage::Transfer,
        };
        if self.active != Some((progress.pass, stage)) {
            if let Some(bar) = self.bar.take() {
                progress::done(bar);
            }
            let label = self.labels.get(progress.pass).copied().unwrap_or("Scan");
            let pass = format!("Pass {}/{} {label}", progress.pass + 1, self.labels.len());
            self.bar = Some(match stage {
                Stage::Preparing => spinner(format!("{pass}: preparing scanner")),
                Stage::Settling => spinner(format!("{pass}: waiting between passes")),
                Stage::Analyzing => spinner(format!("{pass}: measuring sharpness")),
                Stage::Banding => spinner(format!("{pass}: reducing vertical banding")),
                Stage::Transfer => pass_bar(
                    format!("{label} {}/{}", progress.pass + 1, self.labels.len()),
                    progress.total,
                ),
                Stage::Extracting => pass_bar(format!("{label} crop"), progress.total),
                Stage::Saving => spinner(format!(
                    "{pass}: saving {}",
                    if self.export_tiff {
                        "raw data and TIFF"
                    } else {
                        "raw data"
                    }
                )),
            });
            self.active = Some((progress.pass, stage));
        }
        if matches!(stage, Stage::Transfer | Stage::Extracting)
            && let Some(bar) = &self.bar
        {
            bar.report(progress);
        }
    }
}

impl Drop for ScanProgress {
    fn drop(&mut self) {
        if let Some(bar) = self.bar.take() {
            progress::done(bar);
        }
    }
}

fn spinner(prompt: impl Into<Cow<'static, str>>) -> ProgressBar {
    let spinner = ProgressBar::with_draw_target(None, ProgressDrawTarget::hidden());
    spinner.set_style(ProgressStyle::default_spinner());
    spinner.set_message(prompt);
    let spinner = crate::progress::add(spinner);
    spinner.enable_steady_tick(SPINNER_TICK);
    spinner
}

/// A bar for one pass
///
/// The length is not known until the first chunk arrives, so it starts empty
/// and learns. Hidden by indicatif when stderr is not a terminal, and drawn no
/// more than 20 times a second, which is what keeps the callback off the
/// scanner's back
fn pass_bar(label: impl Into<Cow<'static, str>>, total: u64) -> ProgressBar {
    // A bar built the usual way draws straight to stderr, so styling and naming
    // it here would leave that first line behind the moment `add` moves it onto
    // the shared draw target. Built hidden, it draws nothing until it is added
    let bar = ProgressBar::with_draw_target(Some(total), ProgressDrawTarget::hidden());
    bar.set_style(
        ProgressStyle::with_template(
            "{msg:<9} [{bar:30}] {bytes}/{total_bytes}  {bytes_per_sec}  eta {eta}",
        )
        .expect("a template of ours")
        .progress_chars("=> "),
    );
    bar.set_message(label.into());
    crate::progress::add(bar)
}

/// Moving a pass's progress onto a bar
trait Report {
    fn report(&self, progress: Progress<'_>);
}

impl Report for ProgressBar {
    fn report(&self, progress: Progress<'_>) {
        // Each Epson pass supplies its own total; RGB and IR differ in size.
        self.set_length(progress.total);
        self.set_position(progress.done);
    }
}

#[cfg(test)]
mod region_tests {
    use super::*;
    use crate::cli::{Action, Cli};
    use clap::Parser;

    #[test]
    fn explicit_region_lists_share_three_bounding_boxes_and_keep_area_names() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/v800-identity.json"))
                .unwrap();
        let text = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes: Vec<_> = (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect();
        let caps = Capabilities::parse(&bytes).unwrap();
        let rects = caps
            .scanner_model()
            .unwrap()
            .holder(epscan::Holder::V800Film35mm)
            .unwrap()
            .frames_mm;
        let rects = rects
            .iter()
            .map(|rect| {
                rect.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
            .join(";");
        let Action::Scan(args) = Cli::try_parse_from([
            "epscan",
            "scan",
            "--source",
            "film-holder",
            "--rect",
            &rects,
        ])
        .unwrap()
        .action
        else {
            panic!("scan command");
        };
        let settings = ScanSettings {
            dpi: 600,
            ..Default::default()
        };
        let jobs = prepare_jobs(&args.capture, &caps, &settings, &ScanOptions::default()).unwrap();
        let batches = prepare_batches(&args.capture, &jobs, &caps).unwrap();
        assert_eq!(jobs.len(), 18);
        assert_eq!(batches.len(), 3);
        for (index, batch) in batches.iter().enumerate() {
            assert_eq!(
                batch.targets,
                (index * 6..index * 6 + 6).collect::<Vec<_>>()
            );
            assert_eq!(batch.strip, Some(index as u32 + 1));
            assert!(!batch.capture.options.export_tiff);
            assert!(batch.capture.options.holder_selection.is_none());
        }
        assert!(jobs[0].basename.to_string_lossy().ends_with("_area01"));
        assert!(jobs[17].basename.to_string_lossy().ends_with("_area18"));
        let infrared = ScanOptions {
            infrared: true,
            ..Default::default()
        };
        let jobs = prepare_jobs(&args.capture, &caps, &settings, &infrared).unwrap();
        let batches = prepare_batches(&args.capture, &jobs, &caps).unwrap();
        assert_eq!(batches.len(), 18);
        assert!(
            batches
                .iter()
                .all(|batch| batch.strip.is_none() && batch.capture.plan.passes.len() == 2)
        );
    }
}
