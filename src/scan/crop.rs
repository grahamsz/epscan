// SPDX-License-Identifier: MIT
//! Exact, bounded-memory extraction of frames from completed or committed captures.
use super::{
    PassKind, PlannedPass, Progress, ScanOptions, ScanPlan, ScanResult, banding,
    io::{publish_payload, reserve, suffix, write_manifest},
    sharpness,
};
use crate::{
    Capabilities, Error, Result, ScanSettings,
    protocol::hex,
    session::{image::ImageResult, now},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
};

/// One frame to extract, preserving the requested plan's exact pixel rectangle.
#[derive(Clone, Debug)]
pub struct FrameExtraction {
    pub settings: ScanSettings,
    pub options: ScanOptions,
    pub basename: PathBuf,
}

/// One shared visible-pass fit for all crops from a completed strip.
pub(crate) struct PreparedFrameBanding {
    kind: PassKind,
    fitted: banding::PreparedBanding,
}

impl PreparedFrameBanding {
    pub(crate) fn prepare(
        source: &ScanResult,
        source_plan: &ScanPlan,
        options: &ScanOptions,
        cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        let Some(options) = &options.banding else {
            return Ok(None);
        };
        // Explicit detection ROIs retain the existing frame-local contract.
        // Such frames are fitted independently after their strip completes.
        if options.detection_roi.is_some() {
            return Ok(None);
        }
        let mut visible = source_plan
            .passes
            .iter()
            .filter(|pass| matches!(pass.kind, PassKind::Rgb | PassKind::Gray));
        let pass = visible
            .next()
            .ok_or_else(|| Error::Invalid("Banding source has no visible pass".into()))?;
        if visible.next().is_some() {
            return Err(Error::Invalid(
                "Banding source has multiple visible passes".into(),
            ));
        }
        let image = if pass.kind == PassKind::Rgb {
            source.rgb.as_ref()
        } else {
            source.gray.as_ref()
        }
        .ok_or_else(|| Error::Invalid("Banding source has no visible image".into()))?;
        if image.metadata["complete"] != true
            || image.width != pass.pixels[2]
            || image.height != pass.pixels[3]
            || image.channels != pass.channels
            || image.depth != pass.settings.depth
            || image.dpi != pass.settings.dpi
        {
            return Err(Error::Invalid(
                "Banding source is incomplete or does not match its plan".into(),
            ));
        }
        Ok(Some(Self {
            kind: pass.kind,
            fitted: banding::PreparedBanding::prepare(image, options, cancel)?,
        }))
    }
}

#[derive(Clone, Copy)]
struct AvailableInput<'a> {
    file: &'a File,
    bytes: u64,
}

struct Input<'a> {
    file: File,
    image: &'a ImageResult,
    pass: &'a PlannedPass,
    relative: [u32; 4],
    source_stride: u64,
    pixel_bytes: u64,
    row_bytes: usize,
    available_bytes: Option<u64>,
}

fn cancelled(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn report(
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    phase: &str,
    pass: usize,
    done: u64,
    total: u64,
) -> Result<()> {
    cancelled(cancel)?;
    if !progress(Progress {
        phase,
        pass,
        done,
        total,
    }) {
        return Err(Error::Cancelled);
    }
    cancelled(cancel)
}

fn prepare_input<'a>(
    source: &'a ScanResult,
    source_plan: &'a ScanPlan,
    target: &PlannedPass,
    caps: &Capabilities,
    available: Option<AvailableInput<'_>>,
) -> Result<Input<'a>> {
    let mut matching = source_plan
        .passes
        .iter()
        .filter(|pass| pass.kind == target.kind);
    let pass = matching
        .next()
        .ok_or_else(|| Error::Invalid(format!("Source plan has no {} pass", target.kind.name())))?;
    if matching.next().is_some() {
        return Err(Error::Invalid(
            "Source plan contains duplicate pass kinds".into(),
        ));
    }
    let image = match target.kind {
        PassKind::Rgb => source.rgb.as_ref(),
        PassKind::Gray => source.gray.as_ref(),
        PassKind::Ir => source.ir.as_ref(),
        PassKind::Thumbnail => source.thumbnail.as_ref(),
    }
    .ok_or_else(|| Error::Invalid(format!("Source has no {} image", target.kind.name())))?;
    pass.settings.validate(caps, pass.kind == PassKind::Ir)?;
    if !pass.kind.matches_mode(pass.settings.mode)
        || pass.pixels != pass.settings.pixels_for(caps.scanner_model()?)?
        || pass.channels != target.channels
        || pass.settings.source != target.settings.source
        || pass.settings.mode != target.settings.mode
        || pass.settings.dpi != target.settings.dpi
        || pass.settings.y_oversampling != target.settings.y_oversampling
        || pass.settings.depth != target.settings.depth
        || pass.settings.gamma != target.settings.gamma
        || pass.settings.preview != target.settings.preview
    {
        return Err(Error::Invalid(
            "Crop source and target acquisition settings do not match".into(),
        ));
    }
    let [source_x, source_y, source_width, source_height] = pass.pixels;
    let [x, y, width, height] = target.pixels;
    let relative_x = x.checked_sub(source_x);
    let relative_y = y.checked_sub(source_y);
    let inside = relative_x.zip(relative_y).filter(|(x, y)| {
        x.checked_add(width).is_some_and(|end| end <= source_width)
            && y.checked_add(height)
                .is_some_and(|end| end <= source_height)
    });
    let (relative_x, relative_y) = inside.ok_or_else(|| {
        Error::Invalid(format!(
            "{} crop lies outside source pixels",
            target.kind.name()
        ))
    })?;
    if image.width != source_width
        || image.height != source_height
        || image.channels != pass.channels
        || image.depth != pass.settings.depth
        || image.dpi != pass.settings.dpi
        || (available.is_none() && image.metadata["complete"] != true)
    {
        return Err(Error::Invalid(
            "Source image is incomplete or does not match its plan".into(),
        ));
    }
    let hash = image.metadata["sha256"].as_str().unwrap_or_default();
    if available.is_none()
        && (hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        return Err(Error::Invalid("Source image has no recorded SHA256".into()));
    }
    let pixel_bytes = u64::from(pass.channels) * u64::from(pass.settings.depth / 8);
    let source_stride = u64::from(source_width)
        .checked_mul(pixel_bytes)
        .ok_or_else(|| Error::Invalid("Source row size overflow".into()))?;
    let expected = source_stride
        .checked_mul(u64::from(source_height))
        .ok_or_else(|| Error::Invalid("Source image size overflow".into()))?;
    let file = if let Some(available) = available {
        available.file.try_clone()?
    } else {
        File::open(&image.payload)?
    };
    let info = file.metadata()?;
    let length_valid = if let Some(available) = available {
        let frame_end = u64::from(relative_y + height) * source_stride;
        frame_end <= available.bytes && available.bytes <= info.len() && info.len() <= expected
    } else {
        info.len() == expected
    };
    if expected != pass.expected_bytes || !info.is_file() || !length_valid {
        return Err(Error::Invalid(
            "Source payload size does not match its packed pixel plan".into(),
        ));
    }
    let row_bytes = u64::from(width)
        .checked_mul(pixel_bytes)
        .and_then(|size| usize::try_from(size).ok())
        .ok_or_else(|| Error::Invalid("Crop row exceeds address space".into()))?;
    Ok(Input {
        file,
        image,
        pass,
        relative: [relative_x, relative_y, width, height],
        source_stride,
        pixel_bytes,
        row_bytes,
        available_bytes: available.map(|value| value.bytes),
    })
}

fn source_metadata(source: &ScanResult, input: &Input<'_>) -> Value {
    let mut recorded = input.image.metadata.clone();
    // These paths and retention flags describe mutable output bookkeeping,
    // rather than the acquisition. The parent manifest remains authoritative.
    if let Some(object) = recorded.as_object_mut() {
        for field in [
            "payload_file",
            "partial_payload_file",
            "tiff_file",
            "payload_retained",
        ] {
            object.remove(field);
        }
    }
    let mut metadata = json!({
        "manifest":source.manifest,
        "sha256":input.image.metadata["sha256"],
        "hash_basis":"recorded source acquisition; source must remain unchanged during extraction",
        "settings":input.pass.settings,
        "pixels":input.pass.pixels,
        "recorded_metadata":recorded,
    });
    if let Some(available) = input.available_bytes {
        metadata["sha256"] = Value::Null;
        metadata["hash_basis"] =
            "provisional acquisition snapshot; consult source manifest for final full-strip SHA256"
                .into();
        metadata["acquisition_complete"] = false.into();
        metadata["committed_prefix_bytes"] = available.into();
        metadata["recorded_metadata"]["complete"] = false.into();
        metadata["recorded_metadata"]["acquisition_complete"] = false.into();
        if let Some(recorded) = metadata["recorded_metadata"].as_object_mut() {
            recorded.remove("sha256");
        }
    }
    metadata
}

/// Extract unchanged packed samples from completed raw captures, without I/O to
/// the scanner. Source raw files must remain unchanged for the entire call.
///
/// Every requested pass is checked before output reservation. Cropping uses one
/// output row of memory, checks cancellation between rows, and performs no
/// resampling or color conversion. A failure keeps source files, partial crops,
/// and any completed earlier passes. Source SHA256 values are acquisition
/// records, not a new full-source verification for each extracted frame.
pub fn extract_frame(
    source: &ScanResult,
    source_plan: &ScanPlan,
    target: &FrameExtraction,
    caps: &Capabilities,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
) -> Result<ScanResult> {
    extract_frame_with_banding(source, source_plan, target, caps, cancel, progress, None)
}

/// Reuse a completed strip's correction model for the requested frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_frame_with_banding(
    source: &ScanResult,
    source_plan: &ScanPlan,
    target: &FrameExtraction,
    caps: &Capabilities,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    prepared: Option<&PreparedFrameBanding>,
) -> Result<ScanResult> {
    extract_frame_inner(
        source,
        source_plan,
        target,
        caps,
        cancel,
        progress,
        prepared,
        None,
    )
}

/// Extract one visible frame from an immutable, committed prefix while later
/// rows are still arriving. `file` must be opened separately from the writer,
/// and `available_bytes` must include only successfully acquired blocks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_available_frame(
    source: &ScanResult,
    source_plan: &ScanPlan,
    target: &FrameExtraction,
    caps: &Capabilities,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    file: &File,
    available_bytes: u64,
) -> Result<ScanResult> {
    extract_frame_inner(
        source,
        source_plan,
        target,
        caps,
        cancel,
        progress,
        None,
        Some(AvailableInput {
            file,
            bytes: available_bytes,
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn extract_frame_inner(
    source: &ScanResult,
    source_plan: &ScanPlan,
    target: &FrameExtraction,
    caps: &Capabilities,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress<'_>) -> bool,
    prepared: Option<&PreparedFrameBanding>,
    available: Option<AvailableInput<'_>>,
) -> Result<ScanResult> {
    cancelled(cancel)?;
    let plan = target.options.plan(&target.settings, caps)?;
    if available.is_some()
        && (target.options.banding.is_some()
            || plan.passes.len() != 1
            || !matches!(plan.passes[0].kind, PassKind::Rgb | PassKind::Gray))
    {
        return Err(Error::Invalid(
            "Early frame extraction requires one visible pass without banding correction".into(),
        ));
    }
    let mut inputs = plan
        .passes
        .iter()
        .map(|pass| prepare_input(source, source_plan, pass, caps, available))
        .collect::<Result<Vec<_>>>()?;
    if let Some(prepared) = prepared {
        let valid = inputs.iter().any(|input| {
            input.pass.kind == prepared.kind
                && target
                    .options
                    .banding
                    .as_ref()
                    .is_some_and(|options| prepared.fitted.matches_source(input.image, options))
        });
        if !valid {
            return Err(Error::Invalid(
                "Prepared banding model does not match crop source or options".into(),
            ));
        }
    }
    let holder = target
        .options
        .holder_selection
        .map(|selection| selection.metadata(caps.scanner_model()?))
        .transpose()?;
    cancelled(cancel)?;
    if let Some(parent) = target
        .basename
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let (stem, mut manifest_file, manifest_path) = reserve(&target.basename)?;
    let mut manifest = json!({
        "schema":1,"implementation":"epscan","version":env!("CARGO_PKG_VERSION"),
        "complete":false,"derived_crop":true,"started_unix":now(),
        "source_capture":{"manifest":source.manifest},
        "requested":target.settings,"film":target.options.film,
        "export_tiff":target.options.export_tiff,"plan":plan,"passes":[],
    });
    if let Some(holder) = &holder {
        manifest["holder_selection"] = holder.clone();
    }
    if target.options.measure_sharpness {
        manifest["measure_sharpness"] = true.into();
    }
    if let Some(banding) = &target.options.banding {
        manifest["banding_requested"] = serde_json::to_value(banding)?;
    }
    write_manifest(&mut manifest_file, &manifest)?;
    let outcome: Result<ScanResult> = (|| {
        let mut result = ScanResult {
            rgb: None,
            gray: None,
            ir: None,
            thumbnail: None,
            manifest: manifest_path.clone(),
        };
        for (index, (pass, input)) in plan.passes.iter().zip(inputs.iter_mut()).enumerate() {
            let [x, y, width, height] = pass.pixels;
            let tail = pass.kind.suffix();
            let partial = suffix(&stem, &format!("{tail}.partial.bin"));
            let payload = suffix(&stem, &format!("{tail}.bin"));
            let tiff = suffix(&stem, &format!("{tail}.tiff"));
            let mut meta = json!({
                "pass":pass.kind.name(),"complete":false,"derived_crop":true,
                "settings":pass.settings,"width":width,"height":height,
                "channels":pass.channels,"depth":pass.settings.depth,"dpi":pass.settings.dpi,
                "stride_bytes":input.row_bytes,"byteorder":"little",
                "channel_layout":if pass.kind == PassKind::Ir { "single_IR_candidate" } else if pass.channels == 1 { "gray" } else { "RGB_interleaved" },
                "requested_rect_mm":pass.settings.rect_mm,
                "effective_rect_mm":([x,y,width,height].map(|v|f64::from(v)*25.4/f64::from(pass.settings.dpi))),
                "crop_pixels":input.relative,"effective_pixels":pass.pixels,
                "sample_transform":if pass.settings.y_oversampling > 1 {
                    "exact crop of Y-row-averaged packed samples"
                } else { "none; exact packed samples" },
                "source_capture":source_metadata(source,input),
                "extracted_bytes":0,
            });
            if let Some(holder) = &holder {
                meta["holder_selection"] = holder.clone();
            }
            manifest["passes"]
                .as_array_mut()
                .unwrap()
                .push(meta.clone());
            write_manifest(&mut manifest_file, &manifest)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&partial)?;
            meta["partial_payload_file"] = serde_json::to_value(&partial)?;
            manifest["passes"][index] = meta.clone();
            write_manifest(&mut manifest_file, &manifest)?;
            report(
                cancel,
                progress,
                "extracting",
                index,
                0,
                pass.expected_bytes,
            )?;
            let mut row = Vec::new();
            row.try_reserve_exact(input.row_bytes)
                .map_err(|error| Error::Invalid(format!("Cannot allocate crop row: {error}")))?;
            row.resize(input.row_bytes, 0);
            let mut hasher = Sha256::new();
            for row_index in 0..height {
                cancelled(cancel)?;
                let offset = u64::from(input.relative[1] + row_index)
                    .checked_mul(input.source_stride)
                    .and_then(|offset| {
                        offset.checked_add(u64::from(input.relative[0]) * input.pixel_bytes)
                    })
                    .ok_or_else(|| Error::Invalid("Crop file offset overflow".into()))?;
                input.file.seek(SeekFrom::Start(offset))?;
                input.file.read_exact(&mut row)?;
                output.write_all(&row)?;
                hasher.update(&row);
                let done = u64::from(row_index + 1) * input.row_bytes as u64;
                manifest["passes"][index]["extracted_bytes"] = done.into();
                report(
                    cancel,
                    progress,
                    "extracting",
                    index,
                    done,
                    pass.expected_bytes,
                )?;
            }
            output.sync_all()?;
            drop(output);
            meta["extracted_bytes"] = pass.expected_bytes.into();
            meta["extraction_complete"] = true.into();
            meta["sha256"] = hex(&hasher.finalize()).into();
            manifest["passes"][index] = meta.clone();
            write_manifest(&mut manifest_file, &manifest)?;
            publish_payload(&partial, &payload)?;
            meta["payload_file"] = serde_json::to_value(&payload)?;
            meta["complete"] = true.into();
            meta.as_object_mut().unwrap().remove("partial_payload_file");
            manifest["passes"][index] = meta.clone();
            write_manifest(&mut manifest_file, &manifest)?;
            let mut image = ImageResult {
                payload,
                tiff: None,
                width,
                height,
                channels: pass.channels,
                depth: pass.settings.depth,
                dpi: pass.settings.dpi,
                metadata: meta.clone(),
            };
            if target.options.measure_sharpness
                && matches!(pass.kind, PassKind::Rgb | PassKind::Gray)
            {
                report(cancel, progress, "analyzing", index, 0, 0)?;
                meta["sharpness"] = serde_json::to_value(sharpness::measure(&image, cancel)?)?;
                image.metadata = meta.clone();
                manifest["passes"][index] = meta.clone();
                write_manifest(&mut manifest_file, &manifest)?;
            }
            if target.options.export_tiff {
                if matches!(pass.kind, PassKind::Rgb | PassKind::Gray)
                    && let Some(banding_options) = &target.options.banding
                {
                    report(cancel, progress, "banding", index, 0, 0)?;
                    if let Some(prepared) = prepared {
                        prepared.fitted.export_crop(
                            &mut image,
                            [input.relative[0], input.relative[1]],
                            &tiff,
                            cancel,
                        )?;
                    } else {
                        banding::export(&mut image, &tiff, banding_options, cancel)?;
                    }
                    meta["banding"] = image.metadata["banding"].clone();
                } else {
                    report(cancel, progress, "saving", index, 0, 0)?;
                    image.save_tiff(&tiff)?;
                }
                meta["tiff_file"] = serde_json::to_value(&tiff)?;
                image.tiff = Some(tiff);
            }
            cancelled(cancel)?;
            image.metadata = meta.clone();
            manifest["passes"][index] = meta;
            write_manifest(&mut manifest_file, &manifest)?;
            match pass.kind {
                PassKind::Rgb => result.rgb = Some(image),
                PassKind::Gray => result.gray = Some(image),
                PassKind::Ir => result.ir = Some(image),
                PassKind::Thumbnail => result.thumbnail = Some(image),
            }
        }
        manifest["complete"] = true.into();
        Ok(result)
    })();
    if let Err(error) = &outcome {
        manifest["error"] = error.to_string().into();
    }
    manifest["finished_unix"] = now().into();
    match (outcome, write_manifest(&mut manifest_file, &manifest)) {
        (Err(error), Err(manifest_error)) => {
            log::error!(
                "Could not record extraction failure in {}: {manifest_error}",
                manifest_path.display()
            );
            Err(error)
        }
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(result), Ok(())) => Ok(result),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::HolderSelection;
    use std::path::Path;
    use tiff::decoder::{Decoder, DecodingResult};

    fn capabilities() -> Capabilities {
        let fixture: Value =
            serde_json::from_str(include_str!("../../tests/fixtures/v800-identity.json")).unwrap();
        let text = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes: Vec<_> = (0..text.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&text[offset..offset + 2], 16).unwrap())
            .collect();
        Capabilities::parse(&bytes).unwrap()
    }

    fn settings(pixels: [u32; 4], depth: u8) -> ScanSettings {
        ScanSettings {
            rect_mm: pixels.map(|value| f64::from(value) * 25.4 / 100.0),
            dpi: 100,
            depth,
            ..Default::default()
        }
    }

    fn source_fixture(
        directory: &Path,
        settings: &ScanSettings,
        options: &ScanOptions,
    ) -> (ScanResult, ScanPlan) {
        let plan = options.plan(settings, &capabilities()).unwrap();
        let mut result = ScanResult {
            rgb: None,
            gray: None,
            ir: None,
            thumbnail: None,
            manifest: directory.join("source.json"),
        };
        let mut passes = Vec::new();
        for (index, pass) in plan.passes.iter().enumerate() {
            let [_, _, width, height] = pass.pixels;
            let mut bytes = Vec::new();
            for y in 0..height {
                for x in 0..width {
                    for channel in 0..pass.channels {
                        let value =
                            ((x * 17 + y * 23 + u32::from(channel) * 47 + index as u32 * 31) % 256)
                                as u8;
                        if pass.settings.depth == 8 {
                            bytes.push(value);
                        } else {
                            bytes.extend((u16::from(value) * 257).to_le_bytes());
                        }
                    }
                }
            }
            let payload = directory.join(format!("source_{}.bin", pass.kind.name()));
            fs::write(&payload, &bytes).unwrap();
            let metadata = json!({"pass":pass.kind.name(),"complete":true,"acquisition_complete":true,
                "settings":pass.settings,"payload_file":payload,"payload_retained":true,"tiff_file":"source.tiff",
                "parameters_sent_hex":"acquisition-only","parameters_readback_hex":"acquisition-only",
                "transfer":{"start_attempts":1},"sharpness":{"tenengrad":999},
                "sha256":hex(&Sha256::digest(&bytes))});
            passes.push(metadata.clone());
            let image = ImageResult {
                payload,
                tiff: None,
                width,
                height,
                channels: pass.channels,
                depth: pass.settings.depth,
                dpi: pass.settings.dpi,
                metadata,
            };
            match pass.kind {
                PassKind::Rgb => result.rgb = Some(image),
                PassKind::Gray => result.gray = Some(image),
                PassKind::Ir => result.ir = Some(image),
                PassKind::Thumbnail => result.thumbnail = Some(image),
            }
        }
        fs::write(
            &result.manifest,
            serde_json::to_vec(&json!({"complete":true,"passes":passes})).unwrap(),
        )
        .unwrap();
        (result, plan)
    }

    fn raw_slice(image: &ImageResult, relative: [u32; 4]) -> Vec<u8> {
        let bytes = fs::read(&image.payload).unwrap();
        let pixel_bytes = usize::from(image.channels) * usize::from(image.depth / 8);
        let [x, y, width, height] = relative.map(|value| value as usize);
        let mut cropped = Vec::new();
        for row in y..y + height {
            let offset = (row * image.width as usize + x) * pixel_bytes;
            cropped.extend_from_slice(&bytes[offset..offset + width * pixel_bytes]);
        }
        cropped
    }

    #[test]
    fn shared_banding_fit_retains_full_strip_provenance_and_frame_roi_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let (source, plan) = source_fixture(
            directory.path(),
            &ScanSettings {
                mode: crate::ScanMode::Gray,
                ..settings([40, 50, 256, 160], 16)
            },
            &ScanOptions::default(),
        );
        let cancel = AtomicBool::new(false);
        let mut options = ScanOptions {
            banding: Some(banding::BandingOptions::default()),
            ..Default::default()
        };
        let prepared = PreparedFrameBanding::prepare(&source, &plan, &options, &cancel)
            .unwrap()
            .unwrap();
        let target = FrameExtraction {
            settings: ScanSettings {
                mode: crate::ScanMode::Gray,
                ..settings([72, 66, 128, 96], 16)
            },
            options: options.clone(),
            basename: directory.path().join("shared"),
        };
        let result = extract_frame_with_banding(
            &source,
            &plan,
            &target,
            &capabilities(),
            &cancel,
            &mut |_| true,
            Some(&prepared),
        )
        .unwrap();
        let image = result.gray.unwrap();
        assert_eq!(
            fs::read(&image.payload).unwrap(),
            raw_slice(source.gray.as_ref().unwrap(), [32, 16, 128, 96])
        );
        assert_eq!(
            image.metadata["banding"]["analysis_scope"],
            "shared_capture"
        );
        assert_eq!(
            image.metadata["banding"]["analysis_source"]["sha256"],
            source.gray.as_ref().unwrap().metadata["sha256"]
        );
        assert_eq!(
            image.metadata["banding"]["crop_pixels"],
            json!([32, 16, 128, 96])
        );
        options.banding.as_mut().unwrap().detection_roi = Some([10, 110, 4, 80]);
        assert!(
            PreparedFrameBanding::prepare(&source, &plan, &options, &cancel)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn committed_prefix_extracts_exact_frame_without_claiming_complete_source() {
        for (mode, depth) in [(crate::ScanMode::Gray, 16), (crate::ScanMode::Rgb, 8)] {
            let directory = tempfile::tempdir().unwrap();
            let (mut source, plan) = source_fixture(
                directory.path(),
                &ScanSettings {
                    mode,
                    ..settings([40, 50, 32, 64], depth)
                },
                &ScanOptions::default(),
            );
            let original = if mode == crate::ScanMode::Gray {
                source.gray.as_mut().unwrap()
            } else {
                source.rgb.as_mut().unwrap()
            };
            let relative = [3, 10, 16, 7];
            let expected = raw_slice(original, relative);
            let row_bytes = u64::from(original.width)
                * u64::from(original.channels)
                * u64::from(original.depth / 8);
            let committed = 17 * row_bytes;
            let bytes = fs::read(&original.payload).unwrap();
            fs::write(&original.payload, &bytes[..committed as usize]).unwrap();
            original.metadata["complete"] = false.into();
            original.metadata["acquisition_complete"] = false.into();
            original.metadata.as_object_mut().unwrap().remove("sha256");
            let file = File::open(&original.payload).unwrap();
            fs::rename(
                &original.payload,
                original.payload.with_extension("published.bin"),
            )
            .unwrap();
            let target = FrameExtraction {
                settings: ScanSettings {
                    mode,
                    ..settings([43, 60, 16, 7], depth)
                },
                options: ScanOptions::default(),
                basename: directory.path().join("streamed"),
            };
            let result = extract_available_frame(
                &source,
                &plan,
                &target,
                &capabilities(),
                &AtomicBool::new(false),
                &mut |_| true,
                &file,
                committed,
            )
            .unwrap();
            let image = result.gray.or(result.rgb).unwrap();
            assert_eq!(fs::read(&image.payload).unwrap(), expected);
            assert_eq!(image.metadata["complete"], true);
            assert_eq!(image.metadata["sha256"], hex(&Sha256::digest(&expected)));
            assert!(image.tiff.unwrap().is_file());
            assert_eq!(image.metadata["source_capture"]["sha256"], Value::Null);
            assert_eq!(
                image.metadata["source_capture"]["acquisition_complete"],
                false
            );
            assert_eq!(
                image.metadata["source_capture"]["recorded_metadata"]["complete"],
                false
            );
            assert_eq!(
                image.metadata["source_capture"]["committed_prefix_bytes"],
                committed
            );
        }
    }

    #[test]
    fn committed_prefix_rejects_uncommitted_rows_even_if_already_on_disk() {
        let directory = tempfile::tempdir().unwrap();
        let (source, plan) = source_fixture(
            directory.path(),
            &settings([40, 50, 32, 64], 8),
            &ScanOptions::default(),
        );
        let image = source.rgb.as_ref().unwrap();
        let file = File::open(&image.payload).unwrap();
        let target = FrameExtraction {
            settings: settings([43, 60, 16, 7], 8),
            options: ScanOptions::default(),
            basename: directory.path().join("not-ready"),
        };
        let row_bytes = u64::from(image.width) * u64::from(image.channels);
        for watermark in [0, 17 * row_bytes - 1, plan.passes[0].expected_bytes + 1] {
            assert!(
                extract_available_frame(
                    &source,
                    &plan,
                    &target,
                    &capabilities(),
                    &AtomicBool::new(false),
                    &mut |_| true,
                    &file,
                    watermark
                )
                .is_err()
            );
        }
        assert!(!directory.path().join("not-ready_1.json").exists());
    }

    #[test]
    fn derived_frame_exports_banding_outputs_and_keeps_exact_crop_payload() {
        let directory = tempfile::tempdir().unwrap();
        let (source, plan) = source_fixture(
            directory.path(),
            &ScanSettings {
                mode: crate::ScanMode::Gray,
                ..settings([40, 50, 256, 160], 16)
            },
            &ScanOptions::default(),
        );
        let original = source.gray.as_ref().unwrap();
        let expected = raw_slice(original, [32, 16, 128, 96]);
        let target = FrameExtraction {
            settings: ScanSettings {
                mode: crate::ScanMode::Gray,
                ..settings([72, 66, 128, 96], 16)
            },
            options: ScanOptions {
                banding: Some(banding::BandingOptions {
                    save_raw: true,
                    save_signal: true,
                    detection_roi: Some([10, 110, 4, 80]),
                    ..Default::default()
                }),
                ..Default::default()
            },
            basename: directory.path().join("frame"),
        };
        let mut phases = Vec::new();
        let result = extract_frame(
            &source,
            &plan,
            &target,
            &capabilities(),
            &AtomicBool::new(false),
            &mut |p| {
                phases.push(p.phase.to_owned());
                true
            },
        )
        .unwrap();
        let image = result.gray.unwrap();
        assert_eq!(fs::read(&image.payload).unwrap(), expected);
        assert_eq!(
            image.metadata["sample_transform"],
            "none; exact packed samples"
        );
        assert_eq!(
            image.metadata["banding"]["config"]["detection_roi"],
            json!([10, 110, 4, 80])
        );
        assert!(phases.iter().any(|phase| phase == "banding"));
        for file in ["frame_1.tiff", "frame_1_raw.tiff", "frame_1_banding.png"] {
            assert!(directory.path().join(file).metadata().unwrap().len() > 0);
        }
        let mut decoder = tiff::decoder::Decoder::new(
            fs::File::open(directory.path().join("frame_1_raw.tiff")).unwrap(),
        )
        .unwrap();
        assert_eq!(decoder.dimensions().unwrap(), (128, 96));
        let tiff::decoder::DecodingResult::U16(samples) = decoder.read_image().unwrap() else {
            panic!("16-bit raw crop");
        };
        assert_eq!(
            samples,
            crate::session::image::decode_u16_le(&expected).unwrap()
        );
        let manifest: Value = serde_json::from_slice(&fs::read(result.manifest).unwrap()).unwrap();
        assert_eq!(manifest["passes"][0]["banding"], image.metadata["banding"]);
    }

    #[test]
    fn exact_nonzero_rgb_gray_ir_and_thumbnail_crops_at_both_visible_depths() {
        for depth in [8, 16] {
            for mode in [crate::ScanMode::Rgb, crate::ScanMode::Gray] {
                let directory = tempfile::tempdir().unwrap();
                let options = ScanOptions {
                    infrared: true,
                    thumbnail: true,
                    measure_sharpness: true,
                    ..Default::default()
                };
                let (source, plan) = source_fixture(
                    directory.path(),
                    &ScanSettings {
                        mode,
                        ..settings([40, 50, 64, 40], depth)
                    },
                    &options,
                );
                let target = FrameExtraction {
                    settings: ScanSettings {
                        mode,
                        ..settings([48, 53, 24, 12], depth)
                    },
                    options,
                    basename: directory.path().join("film_frame02"),
                };
                let mut phases = Vec::new();
                let result = extract_frame(
                    &source,
                    &plan,
                    &target,
                    &capabilities(),
                    &AtomicBool::new(false),
                    &mut |update| {
                        phases.push(update.phase.to_owned());
                        true
                    },
                )
                .unwrap();
                let manifest: Value =
                    serde_json::from_slice(&fs::read(&result.manifest).unwrap()).unwrap();
                assert_eq!(manifest["complete"], true);
                assert_eq!(manifest["derived_crop"], true);
                assert_eq!(result.manifest.file_name().unwrap(), "film_frame02_1.json");
                let original_main = if mode == crate::ScanMode::Rgb {
                    source.rgb.as_ref()
                } else {
                    source.gray.as_ref()
                }
                .unwrap();
                let cropped_main = if mode == crate::ScanMode::Rgb {
                    result.rgb.as_ref()
                } else {
                    result.gray.as_ref()
                }
                .unwrap();
                for (original, cropped, index) in [
                    (
                        source.thumbnail.as_ref().unwrap(),
                        result.thumbnail.as_ref().unwrap(),
                        0,
                    ),
                    (original_main, cropped_main, 1),
                    (source.ir.as_ref().unwrap(), result.ir.as_ref().unwrap(), 2),
                ] {
                    let expected = raw_slice(original, [8, 3, 24, 12]);
                    assert_eq!(
                        (
                            cropped.width,
                            cropped.height,
                            cropped.channels,
                            cropped.depth
                        ),
                        (24, 12, original.channels, original.depth)
                    );
                    assert_eq!(fs::read(&cropped.payload).unwrap(), expected);
                    let meta = &cropped.metadata;
                    assert_eq!(meta["sha256"], hex(&Sha256::digest(&expected)));
                    assert_eq!(
                        meta["stride_bytes"],
                        24 * u64::from(cropped.channels) * u64::from(cropped.depth / 8)
                    );
                    assert_eq!(meta["crop_pixels"], json!([8, 3, 24, 12]));
                    assert_eq!(meta["effective_pixels"], json!([48, 53, 24, 12]));
                    assert_eq!(
                        meta["source_capture"]["manifest"],
                        serde_json::to_value(&source.manifest).unwrap()
                    );
                    assert_eq!(
                        meta["source_capture"]["sha256"],
                        original.metadata["sha256"]
                    );
                    assert_eq!(
                        meta["source_capture"]["recorded_metadata"]["transfer"]["start_attempts"],
                        1
                    );
                    for field in [
                        "parameters_sent_hex",
                        "parameters_readback_hex",
                        "transfer",
                        "acquisition_complete",
                    ] {
                        assert!(meta.get(field).is_none());
                    }
                    for field in [
                        "payload_file",
                        "tiff_file",
                        "partial_payload_file",
                        "payload_retained",
                    ] {
                        assert!(
                            meta["source_capture"]["recorded_metadata"]
                                .get(field)
                                .is_none()
                        );
                    }
                    assert_eq!(manifest["passes"][index]["sha256"], meta["sha256"]);
                    let mut decoder =
                        Decoder::new(File::open(cropped.tiff.as_ref().unwrap()).unwrap()).unwrap();
                    assert_eq!(decoder.dimensions().unwrap(), (24, 12));
                    if cropped.channels == 1 {
                        assert_eq!(
                            decoder.colortype().unwrap(),
                            tiff::ColorType::Gray(cropped.depth)
                        );
                        assert_eq!(
                            meta["channel_layout"],
                            if index == 2 {
                                "single_IR_candidate"
                            } else {
                                "gray"
                            }
                        );
                    }
                    let description = decoder
                        .get_tag_ascii_string(tiff::tags::Tag::ImageDescription)
                        .unwrap();
                    let embedded: Value =
                        serde_json::from_str(description.trim_end_matches('\0')).unwrap();
                    assert_eq!(
                        embedded["source_capture"]["sha256"],
                        original.metadata["sha256"]
                    );
                    match decoder.read_image().unwrap() {
                        DecodingResult::U8(samples) => assert_eq!(samples, expected),
                        DecodingResult::U16(samples) => assert_eq!(
                            samples,
                            crate::session::image::decode_u16_le(&expected).unwrap()
                        ),
                        other => panic!("Unexpected TIFF samples: {other:?}"),
                    }
                    if index == 1 {
                        assert_eq!(
                            meta["sharpness"],
                            serde_json::to_value(
                                sharpness::measure(cropped, &AtomicBool::new(false)).unwrap()
                            )
                            .unwrap()
                        );
                        assert_eq!(meta["sharpness"]["evaluated_pixels"], 220);
                        assert_ne!(meta["sharpness"]["tenengrad"], 999);
                        for metric in ["tenengrad", "variance_of_laplacian"] {
                            assert!(
                                (embedded["sharpness"][metric].as_f64().unwrap()
                                    - meta["sharpness"][metric].as_f64().unwrap())
                                .abs()
                                    < 1e-12
                            );
                        }
                        assert_eq!(embedded["sharpness"]["evaluated_pixels"], 220);
                    } else {
                        assert!(meta.get("sharpness").is_none());
                    }
                }
                assert_eq!(
                    phases.iter().filter(|phase| *phase == "analyzing").count(),
                    1
                );
                assert_eq!(phases.iter().filter(|phase| *phase == "saving").count(), 3);
                assert!(!result.manifest.with_extension("protocol.jsonl").exists());
                assert!(original_main.payload.exists());
            }
        }
    }

    #[test]
    fn holder_provenance_and_scores_cover_only_the_selected_frame() {
        let directory = tempfile::tempdir().unwrap();
        let caps = capabilities();
        let selection = HolderSelection {
            holder: crate::capabilities::Holder::V800Film35mm,
            frame_format: None,
            frame: 1,
            overage_percent: 0.0,
        };
        let target_settings = ScanSettings {
            rect_mm: caps
                .scanner_model()
                .unwrap()
                .holder_frame(selection.holder, 1, 0.0)
                .unwrap(),
            dpi: 25,
            depth: 8,
            ..Default::default()
        };
        let source_settings = ScanSettings {
            rect_mm: [0.0, 0.0, 40.0, 60.0],
            ..target_settings.clone()
        };
        let (source, plan) =
            source_fixture(directory.path(), &source_settings, &ScanOptions::default());
        let target_pixels = target_settings.pixels().unwrap();
        // The selected frame is constant while the surrounding strip remains
        // textured, so scores from the parent image cannot pass this check.
        let image = source.rgb.as_ref().unwrap();
        let mut bytes = fs::read(&image.payload).unwrap();
        for y in target_pixels[1]..target_pixels[1] + target_pixels[3] {
            let offset = ((y * image.width + target_pixels[0]) * 3) as usize;
            bytes[offset..offset + (target_pixels[2] * 3) as usize].fill(77);
        }
        fs::write(&image.payload, &bytes).unwrap();
        let mut source = source;
        source.rgb.as_mut().unwrap().metadata["sha256"] = hex(&Sha256::digest(&bytes)).into();
        let target = FrameExtraction {
            settings: target_settings,
            options: ScanOptions {
                holder_selection: Some(selection),
                measure_sharpness: true,
                ..Default::default()
            },
            basename: directory.path().join("holder_frame01"),
        };
        let result = extract_frame(
            &source,
            &plan,
            &target,
            &caps,
            &AtomicBool::new(false),
            &mut |_| true,
        )
        .unwrap();
        let manifest: Value = serde_json::from_slice(&fs::read(&result.manifest).unwrap()).unwrap();
        let metadata = result.rgb.unwrap().metadata;
        assert_eq!(manifest["holder_selection"]["frame"], 1);
        assert_eq!(manifest["holder_selection"], metadata["holder_selection"]);
        assert_eq!(metadata["sharpness"]["tenengrad"], 0.0);
        assert_eq!(metadata["sharpness"]["variance_of_laplacian"], 0.0);
        assert!(
            sharpness::measure(source.rgb.as_ref().unwrap(), &AtomicBool::new(false))
                .unwrap()
                .tenengrad
                > 0.0
        );
    }

    #[test]
    fn invalid_geometry_settings_and_truncated_later_pass_fail_before_output() {
        let directory = tempfile::tempdir().unwrap();
        let options = ScanOptions {
            infrared: true,
            ..Default::default()
        };
        let (source, plan) =
            source_fixture(directory.path(), &settings([40, 50, 64, 40], 8), &options);
        let mut target = FrameExtraction {
            settings: settings([48, 53, 24, 12], 8),
            options,
            basename: directory.path().join("output").join("frame"),
        };
        for invalid in [
            settings([32, 53, 24, 12], 8),
            settings([96, 53, 24, 12], 8),
            settings([48, 85, 24, 12], 8),
            settings([48, 53, 24, 12], 16),
            ScanSettings {
                mode: crate::ScanMode::Gray,
                ..settings([48, 53, 24, 12], 8)
            },
        ] {
            target.settings = invalid;
            assert!(
                extract_frame(
                    &source,
                    &plan,
                    &target,
                    &capabilities(),
                    &AtomicBool::new(false),
                    &mut |_| true
                )
                .is_err()
            );
            assert!(!directory.path().join("output").exists());
        }
        target.settings = settings([48, 53, 24, 12], 8);
        let mut bad_plan = plan.clone();
        bad_plan.passes[0].pixels[2] += 8;
        assert!(
            extract_frame(
                &source,
                &bad_plan,
                &target,
                &capabilities(),
                &AtomicBool::new(false),
                &mut |_| true
            )
            .is_err()
        );
        let mut bad_plan = plan.clone();
        bad_plan.passes[0].settings.mode = crate::ScanMode::Gray;
        assert!(
            extract_frame(
                &source,
                &bad_plan,
                &target,
                &capabilities(),
                &AtomicBool::new(false),
                &mut |_| true,
            )
            .is_err()
        );
        fs::write(&source.ir.as_ref().unwrap().payload, b"truncated").unwrap();
        assert!(
            extract_frame(
                &source,
                &plan,
                &target,
                &capabilities(),
                &AtomicBool::new(false),
                &mut |_| true
            )
            .is_err()
        );
        assert!(!directory.path().join("output").exists());
    }

    #[test]
    fn cancellation_before_output_and_during_rows_preserves_sources_and_partial() {
        let directory = tempfile::tempdir().unwrap();
        let (source, plan) = source_fixture(
            directory.path(),
            &settings([40, 50, 64, 40], 8),
            &ScanOptions::default(),
        );
        let target = FrameExtraction {
            settings: settings([48, 53, 24, 12], 8),
            options: ScanOptions::default(),
            basename: directory.path().join("frame"),
        };
        assert!(matches!(
            extract_frame(
                &source,
                &plan,
                &target,
                &capabilities(),
                &AtomicBool::new(true),
                &mut |_| true
            ),
            Err(Error::Cancelled)
        ));
        assert!(!directory.path().join("frame_1.json").exists());
        for use_atomic in [false, true] {
            let cancel = AtomicBool::new(false);
            let error = extract_frame(
                &source,
                &plan,
                &target,
                &capabilities(),
                &cancel,
                &mut |update| {
                    if update.phase == "extracting" && update.done > 0 {
                        if use_atomic {
                            cancel.store(true, Ordering::Relaxed);
                            true
                        } else {
                            false
                        }
                    } else {
                        true
                    }
                },
            );
            assert!(matches!(error, Err(Error::Cancelled)));
            let index = if use_atomic { 2 } else { 1 };
            let partial = directory.path().join(format!("frame_{index}.partial.bin"));
            assert_eq!(fs::metadata(partial).unwrap().len(), 72);
            let manifest: Value = serde_json::from_slice(
                &fs::read(directory.path().join(format!("frame_{index}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(manifest["complete"], false);
            assert_eq!(manifest["passes"][0]["extracted_bytes"], 72);
            assert!(!directory.path().join(format!("frame_{index}.bin")).exists());
        }
        assert_eq!(
            fs::metadata(&source.rgb.as_ref().unwrap().payload)
                .unwrap()
                .len(),
            64 * 40 * 3
        );
    }

    #[test]
    fn late_export_collision_preserves_completed_crop_and_existing_tiff() {
        let directory = tempfile::tempdir().unwrap();
        let (source, plan) = source_fixture(
            directory.path(),
            &settings([40, 50, 64, 40], 8),
            &ScanOptions::default(),
        );
        let target = FrameExtraction {
            settings: settings([48, 53, 24, 12], 8),
            options: ScanOptions::default(),
            basename: directory.path().join("frame"),
        };
        let tiff = directory.path().join("frame_1.tiff");
        let result = extract_frame(
            &source,
            &plan,
            &target,
            &capabilities(),
            &AtomicBool::new(false),
            &mut |update| {
                if update.phase == "saving" {
                    fs::write(&tiff, b"existing").unwrap();
                }
                true
            },
        );
        assert!(result.is_err());
        assert_eq!(fs::read(tiff).unwrap(), b"existing");
        assert_eq!(
            fs::read(directory.path().join("frame_1.bin")).unwrap(),
            raw_slice(source.rgb.as_ref().unwrap(), [8, 3, 24, 12])
        );
        let manifest: Value =
            serde_json::from_slice(&fs::read(directory.path().join("frame_1.json")).unwrap())
                .unwrap();
        assert_eq!(manifest["complete"], false);
        assert_eq!(manifest["passes"][0]["complete"], true);
        assert_eq!(manifest["passes"][0]["extraction_complete"], true);
        assert!(manifest["passes"][0].get("tiff_file").is_none());
        assert!(source.rgb.unwrap().payload.exists());
    }
}
