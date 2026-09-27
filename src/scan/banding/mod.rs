// SPDX-License-Identifier: MIT
//! Optional correction of coherent vertical bands in uninverted negative samples.
//! The original packed acquisition stays immutable. TIFFs are streamed in strips;
//! fitting retains only disjoint sets of at most 768 full-width source rows.

use negative_banding as core;
#[cfg(test)]
mod tests;

use crate::{Error, Result, session::image::ImageResult};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// The accepted experimental negative-film preset. Brightness is original
/// sample/full-scale, with a common maximum-channel mask for RGB.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BandingOptions {
    pub strength: f64,
    pub dark_full: f64,
    pub dark_off: f64,
    pub max_frequencies: usize,
    pub min_period: f64,
    pub max_period: Option<f64>,
    pub window_cycles: f64,
    pub grid_y: usize,
    /// Half-open source coordinates: X0, X1, Y0, Y1. Detection only.
    pub detection_roi: Option<[u32; 4]>,
    pub save_raw: bool,
    pub save_signal: bool,
}

impl Default for BandingOptions {
    fn default() -> Self {
        Self {
            strength: 1.0,
            dark_full: 0.1,
            dark_off: 0.6,
            max_frequencies: 3,
            min_period: 8.0,
            max_period: None,
            window_cycles: 4.0,
            grid_y: 12,
            detection_roi: None,
            save_raw: false,
            save_signal: false,
        }
    }
}

impl BandingOptions {
    fn core_options(&self) -> core::BandingOptions {
        core::BandingOptions {
            strength: self.strength,
            dark_full: self.dark_full,
            dark_off: self.dark_off,
            max_frequencies: self.max_frequencies,
            min_period: self.min_period,
            max_period: self.max_period,
            window_cycles: self.window_cycles,
            grid_y: self.grid_y,
            detection_roi: self.detection_roi,
            save_raw: self.save_raw,
            save_signal: self.save_signal,
        }
    }

    pub fn validate(&self) -> Result<()> {
        if !self.strength.is_finite() || !(0.0..=1.0).contains(&self.strength) {
            return Err(Error::Invalid(
                "Banding strength must be between 0 and 1".into(),
            ));
        }
        if !self.dark_full.is_finite()
            || !self.dark_off.is_finite()
            || !(0.0 <= self.dark_full && self.dark_full < self.dark_off && self.dark_off <= 1.0)
        {
            return Err(Error::Invalid(
                "Banding darkness limits must satisfy 0 <= full < cutoff <= 1".into(),
            ));
        }
        if !self.min_period.is_finite()
            || self.min_period < 2.0
            || self
                .max_period
                .is_some_and(|value| !value.is_finite() || value <= self.min_period)
        {
            return Err(Error::Invalid(
                "Banding periods must be finite and satisfy 2 <= minimum < maximum".into(),
            ));
        }
        if !(1..=8).contains(&self.max_frequencies)
            || !self.window_cycles.is_finite()
            || self.window_cycles < 3.0
            || !(2..=128).contains(&self.grid_y)
        {
            return Err(Error::Invalid(
                "Banding needs 1..=8 frequencies, at least 3 window cycles and 2..=128 grid rows"
                    .into(),
            ));
        }
        if self
            .detection_roi
            .is_some_and(|[x0, x1, y0, y1]| x0 >= x1 || y0 >= y1)
        {
            return Err(Error::Invalid(
                "Banding ROI must have increasing X0 X1 Y0 Y1 bounds".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_shape(&self, width: u32, height: u32) -> Result<()> {
        self.validate()?;
        if height < 16 || f64::from(width) <= 5.0 * self.min_period {
            return Err(Error::Invalid(
                "Banding needs at least 16 rows and width greater than five minimum periods".into(),
            ));
        }
        let detection_width = if let Some([x0, x1, y0, y1]) = self.detection_roi {
            if x1 > width || y1 > height {
                return Err(Error::Invalid(
                    "Banding ROI extends outside the source image".into(),
                ));
            }
            let (rows, held) = sample_rows(height as usize);
            if [rows, held].iter().any(|set| {
                set.iter()
                    .filter(|&&y| y >= y0 as usize && y < y1 as usize)
                    .count()
                    < 8
            }) {
                return Err(Error::Invalid(
                    "Banding ROI needs at least eight fitting and held-out rows".into(),
                ));
            }
            x1 - x0
        } else {
            width
        };
        let maximum = self.max_period.unwrap_or(f64::from(detection_width) / 5.0);
        if maximum <= self.min_period || maximum > f64::from(detection_width) / 3.0 {
            return Err(Error::Invalid("Banding maximum period must exceed minimum and fit at least three cycles in detection width".into()));
        }
        Ok(())
    }
}

/// Paths reserved for the derived TIFF and requested companions, in that order.
pub fn artifact_paths(final_path: &Path, options: &BandingOptions) -> Vec<PathBuf> {
    let mut paths = vec![final_path.to_owned()];
    let stem = final_path.file_stem().unwrap_or_default();
    if options.save_raw {
        let mut name = stem.to_os_string();
        name.push("_raw.tiff");
        paths.push(final_path.with_file_name(name));
    }
    if options.save_signal {
        let mut name = stem.to_os_string();
        name.push("_banding.png");
        paths.push(final_path.with_file_name(name));
    }
    paths
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

use core::sample_rows;

fn packed_shape(image: &ImageResult) -> Result<(usize, u64)> {
    if ![1, 3].contains(&image.channels) || ![8, 16].contains(&image.depth) || image.dpi == 0 {
        return Err(Error::Invalid(
            "Banding needs packed 8/16-bit grayscale or RGB samples and positive DPI".into(),
        ));
    }
    let row_bytes = (image.width as usize)
        .checked_mul(image.channels as usize)
        .and_then(|n| n.checked_mul((image.depth / 8) as usize))
        .ok_or_else(|| Error::Invalid("Banding row size overflow".into()))?;
    let expected = (row_bytes as u64)
        .checked_mul(u64::from(image.height))
        .ok_or_else(|| Error::Invalid("Banding image size overflow".into()))?;
    Ok((row_bytes, expected))
}

fn sample_value(bytes: &[u8], sample: usize, depth: u8) -> u16 {
    if depth == 8 {
        u16::from(bytes[sample])
    } else {
        u16::from_le_bytes([bytes[2 * sample], bytes[2 * sample + 1]])
    }
}

fn sample_image(
    image: &ImageResult,
    row_bytes: usize,
    cancel: &AtomicBool,
) -> Result<core::Samples> {
    let mut file = File::open(&image.payload)?;
    let width = image.width as usize;
    let channels = image.channels as usize;
    let (rows, held_rows) = sample_rows(image.height as usize);
    let max_value = if image.depth == 16 { 65535.0 } else { 255.0 };
    let mut packed = vec![0; row_bytes];
    let mut read = |ys: &[usize]| -> Result<Vec<Vec<f64>>> {
        let count = ys
            .len()
            .checked_mul(width)
            .ok_or_else(|| Error::Invalid("Banding sample count overflow".into()))?;
        let mut samples = Vec::with_capacity(channels);
        for _ in 0..channels {
            let mut values = Vec::new();
            values
                .try_reserve_exact(count)
                .map_err(|e| Error::Invalid(format!("Cannot allocate banding samples: {e}")))?;
            samples.push(values);
        }
        for &y in ys {
            check_cancel(cancel)?;
            file.seek(SeekFrom::Start(y as u64 * row_bytes as u64))?;
            file.read_exact(&mut packed)?;
            for x in 0..width {
                for (channel, values) in samples.iter_mut().enumerate() {
                    values.push(
                        f64::from(sample_value(&packed, x * channels + channel, image.depth))
                            / max_value,
                    );
                }
            }
        }
        Ok(samples)
    };
    let training = read(&rows)?;
    let held = read(&held_rows)?;
    Ok(core::Samples {
        width,
        height: image.height as usize,
        channels,
        max_value,
        rows,
        held_rows,
        training,
        held,
    })
}

/// A fit retained after one completed acquisition so all derived frames use
/// the same source-coordinate correction field.
pub(crate) struct PreparedBanding {
    fitted: core::Analysis,
    options: BandingOptions,
    source_payload: PathBuf,
    source_sha256: serde_json::Value,
    channels: u8,
    depth: u8,
    dpi: u32,
}

impl PreparedBanding {
    pub(crate) fn prepare(
        image: &ImageResult,
        options: &BandingOptions,
        cancel: &AtomicBool,
    ) -> Result<Self> {
        check_cancel(cancel)?;
        options.validate_shape(image.width, image.height)?;
        let (row_bytes, expected) = packed_shape(image)?;
        let source = image.payload.metadata()?;
        if !source.is_file() || source.len() != expected {
            return Err(Error::Protocol(
                "Banding payload size does not match packed row stride".into(),
            ));
        }
        let samples = sample_image(image, row_bytes, cancel)?;
        let config = core::Config {
            width: image.width as usize,
            height: image.height as usize,
            channels: image.channels as usize,
            options: options.core_options(),
            carrier_boost: 1.5,
            compact_opacity: 1.0,
        };
        let fitted = core::analyze_samples(samples, config, cancel)?;
        Ok(Self {
            fitted,
            options: options.clone(),
            source_payload: image.payload.clone(),
            source_sha256: image.metadata["sha256"].clone(),
            channels: image.channels,
            depth: image.depth,
            dpi: image.dpi,
        })
    }

    pub(crate) fn matches_source(&self, image: &ImageResult, options: &BandingOptions) -> bool {
        self.source_payload == image.payload
            && self.source_sha256 == image.metadata["sha256"]
            && self.fitted.config.width == image.width as usize
            && self.fitted.config.height == image.height as usize
            && self.channels == image.channels
            && self.depth == image.depth
            && self.dpi == image.dpi
            && self.options == *options
    }

    pub(crate) fn export_crop(
        &self,
        image: &mut ImageResult,
        origin: [u32; 2],
        final_path: &Path,
        cancel: &AtomicBool,
    ) -> Result<()> {
        export_prepared(image, final_path, self, origin, true, cancel)
    }
}

fn check_output_paths(final_path: &Path, options: &BandingOptions) -> Result<Vec<PathBuf>> {
    let paths = artifact_paths(final_path, options);
    for path in &paths {
        match path.symlink_metadata() {
            Ok(_) => {
                return Err(Error::Invalid(format!(
                    "Banding output already exists: {}",
                    path.display()
                )));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(Error::Invalid(format!(
                "Banding output directory does not exist: {}",
                parent.display()
            )));
        }
    }
    Ok(paths)
}

/// Analyze the captured payload, then export a separate corrected TIFF and
/// optional unchanged TIFF and signed applied-signal PNG. Existing files are
/// never replaced. Failed exports clean up only files created by this call.
pub fn export(
    image: &mut ImageResult,
    final_path: &Path,
    options: &BandingOptions,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    check_output_paths(final_path, options)?;
    let prepared = PreparedBanding::prepare(image, options, cancel)?;
    export_prepared(image, final_path, &prepared, [0, 0], false, cancel)
}

fn export_prepared(
    image: &mut ImageResult,
    final_path: &Path,
    prepared: &PreparedBanding,
    origin: [u32; 2],
    shared_capture: bool,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    let options = &prepared.options;
    let fitted = &prepared.fitted;
    if image.channels != prepared.channels
        || image.depth != prepared.depth
        || image.dpi != prepared.dpi
        || origin[0]
            .checked_add(image.width)
            .is_none_or(|end| end as usize > fitted.config.width)
        || origin[1]
            .checked_add(image.height)
            .is_none_or(|end| end as usize > fitted.config.height)
    {
        return Err(Error::Invalid(
            "Banding crop does not match the fitted source image".into(),
        ));
    }
    let (row_bytes, expected) = packed_shape(image)?;
    let source = image.payload.metadata()?;
    if !source.is_file() || source.len() != expected {
        return Err(Error::Protocol(
            "Banding payload size does not match packed row stride".into(),
        ));
    }
    let paths = check_output_paths(final_path, options)?;
    let preview = if options.save_signal {
        Some(signal_preview(
            image, fitted, options, row_bytes, origin, cancel,
        )?)
    } else {
        None
    };
    let mut banding = serde_json::json!({
        "method_version": 2,
        "config": options,
        "status": if fitted.model.has_components() { "corrected" } else { "no_supported_frequency" },
        "model": "residual_refined_linear_sine",
        "axis": "vertical stripes; carrier varies across source columns x",
        "diagnostics": fitted.model,
        "residual_refinement": fitted.report()["residual_refinement"],
        "shared_engine_version": core::VERSION,
        "analysis_scope": if shared_capture { "shared_capture" } else { "output_image" },
        "analysis_source": {
            "payload_file": prepared.source_payload,
            "sha256": prepared.source_sha256,
            "width": fitted.config.width,
            "height": fitted.config.height,
        },
        "crop_pixels": [origin[0], origin[1], image.width, image.height],
        "corrected_tiff_file": final_path,
        "source_payload_unchanged": true,
        "signal_semantics": "signal PNG = (raw - corrected) / full_scale; corrected = clamp(raw * (1 - strength * darkness * refined_sine_sum)), before source-depth quantization"
    });
    let mut next = 1;
    let raw_path = if options.save_raw {
        banding["raw_tiff_file"] = serde_json::to_value(&paths[next])?;
        next += 1;
        Some(&paths[next - 1])
    } else {
        None
    };
    let signal_path = if options.save_signal {
        banding["signal_png_file"] = serde_json::to_value(&paths[next])?;
        banding["signal_preview"] = preview.as_ref().unwrap().metadata();
        Some(&paths[next])
    } else {
        None
    };
    let mut corrected_metadata = image.metadata.clone();
    if !corrected_metadata.is_object() {
        return Err(Error::Invalid(
            "Banding image metadata must be an object".into(),
        ));
    }
    corrected_metadata["sample_transform"] =
        "derived residual-refined periodic band correction; original payload unchanged".into();
    corrected_metadata["banding"] = banding.clone();
    let width = image.width as usize;
    let channels = image.channels as usize;
    let max_value = if image.depth == 16 { 65535 } else { 255 };
    let mut corrections = vec![vec![0.0; width]; channels];
    let mut created = Vec::new();
    let outcome: Result<()> = (|| {
        image.save_tiff_transformed(final_path, &corrected_metadata, |first_row, strip| {
            for (dy, row) in strip.chunks_exact_mut(row_bytes).enumerate() {
                check_cancel(cancel)?;
                for (channel, correction) in corrections.iter_mut().enumerate() {
                    fitted.correction_region_row(
                        channel,
                        origin[0] as usize,
                        origin[1] as usize + first_row as usize + dy,
                        correction,
                    )?;
                }
                for x in 0..width {
                    let brightness = (0..channels)
                        .map(|channel| sample_value(row, x * channels + channel, image.depth))
                        .max()
                        .unwrap();
                    let mask = core::dark_weight(
                        f64::from(brightness) / f64::from(max_value),
                        options.dark_full,
                        options.dark_off,
                    );
                    if mask == 0.0 || options.strength == 0.0 {
                        continue;
                    }
                    for (channel, correction) in corrections.iter().enumerate() {
                        let offset = x * channels + channel;
                        let original = sample_value(row, offset, image.depth);
                        let corrected = core::linear_corrected_sample(
                            original,
                            max_value,
                            correction[x],
                            mask,
                            options.strength,
                        );
                        if image.depth == 16 {
                            row[2 * offset..2 * offset + 2]
                                .copy_from_slice(&corrected.to_le_bytes());
                        } else {
                            row[offset] = corrected as u8;
                        }
                    }
                }
            }
            Ok(())
        })?;
        created.push(final_path.to_owned());
        if let Some(path) = raw_path {
            image.save_tiff_transformed(path, &image.metadata, |_, _| check_cancel(cancel))?;
            created.push(path.clone());
        }
        if let Some(path) = signal_path {
            preview.as_ref().unwrap().save(path, cancel)?;
            created.push(path.clone());
        }
        check_cancel(cancel)?;
        Ok(())
    })();
    if outcome.is_err() {
        for path in created {
            let _ = std::fs::remove_file(path);
        }
    }
    outcome?;
    image.tiff = Some(final_path.to_owned());
    image.metadata["banding"] = banding;
    Ok(())
}

struct SignalPreview {
    width: u32,
    height: u32,
    panel_width: u32,
    source_width: u32,
    source_height: u32,
    model_origin: [u32; 2],
    channels: u8,
    range: f64,
    pixels: Vec<u8>,
}

impl SignalPreview {
    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "width": self.width, "height": self.height, "panel_width": self.panel_width,
            "source_width": self.source_width, "source_height": self.source_height,
            "model_origin": self.model_origin,
            "panels": if self.channels == 3 { "red, green, blue channel panels, left to right" } else { "gray channel" },
            "coordinates": "source orientation preserved; nearest samples at floor(preview_coordinate * source_extent / panel_extent)",
            "range_fraction_full_scale": [-self.range, self.range],
            "legend": "red = positive intensity removed (output darker); white = zero; blue = negative (output brighter)",
            "normalization": "one common symmetric range across all channel panels; max absolute preview signal"
        })
    }

    fn save(&self, path: &Path, cancel: &AtomicBool) -> Result<()> {
        check_cancel(cancel)?;
        let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = (|| {
            let mut encoder = png::Encoder::new(&mut file, self.width, self.height);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            encoder
                .add_text_chunk("Banding signal".into(), self.metadata().to_string())
                .map_err(png_error)?;
            let mut writer = encoder.write_header().map_err(png_error)?;
            writer.write_image_data(&self.pixels).map_err(png_error)?;
            writer.finish().map_err(png_error)?;
            check_cancel(cancel)?;
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if result.is_err() {
            let _ = std::fs::remove_file(path);
        }
        result
    }
}

fn png_error(error: png::EncodingError) -> Error {
    Error::Io(std::io::Error::other(error))
}

fn signal_preview(
    image: &ImageResult,
    fitted: &core::Analysis,
    options: &BandingOptions,
    row_bytes: usize,
    origin: [u32; 2],
    cancel: &AtomicBool,
) -> Result<SignalPreview> {
    let source_width = image.width as usize;
    let source_height = image.height as usize;
    let channels = image.channels as usize;
    let scale = 1600.0 / (source_width * channels).max(source_height) as f64;
    let scale = scale.min(1.0);
    let panel_width = ((source_width as f64 * scale).floor() as usize).max(1);
    let height = ((source_height as f64 * scale).floor() as usize).max(1);
    let width = panel_width * channels;
    let max_value = if image.depth == 16 { 65535.0 } else { 255.0 };
    let mut values = vec![0.0f64; width * height];
    let mut packed = vec![0; row_bytes];
    let mut source = File::open(&image.payload)?;
    for py in 0..height {
        check_cancel(cancel)?;
        let y = py * source_height / height;
        source.seek(SeekFrom::Start(y as u64 * row_bytes as u64))?;
        source.read_exact(&mut packed)?;
        for px in 0..panel_width {
            let x = px * source_width / panel_width;
            let brightness = (0..channels)
                .map(|channel| sample_value(&packed, x * channels + channel, image.depth))
                .max()
                .unwrap();
            let mask = core::dark_weight(
                f64::from(brightness) / max_value,
                options.dark_full,
                options.dark_off,
            );
            for channel in 0..channels {
                let raw = f64::from(sample_value(&packed, x * channels + channel, image.depth))
                    / max_value;
                let signal =
                    fitted.correction(channel, origin[0] as usize + x, origin[1] as usize + y)?;
                let corrected = (raw * (1.0 - options.strength * mask * signal)).clamp(0.0, 1.0);
                values[py * width + channel * panel_width + px] = raw - corrected;
            }
        }
    }
    let range = values.iter().map(|value| value.abs()).fold(0.0, f64::max);
    let mut pixels = Vec::with_capacity(values.len() * 3);
    for value in values {
        let signed = if range == 0.0 {
            0.0
        } else {
            (value / range).clamp(-1.0, 1.0)
        };
        let pale = ((1.0 - signed.abs()) * 255.0).round() as u8;
        if signed >= 0.0 {
            pixels.extend_from_slice(&[255, pale, pale]);
        } else {
            pixels.extend_from_slice(&[pale, pale, 255]);
        }
    }
    Ok(SignalPreview {
        width: width as u32,
        height: height as u32,
        panel_width: panel_width as u32,
        source_width: image.width,
        source_height: image.height,
        model_origin: origin,
        channels: image.channels,
        range,
        pixels,
    })
}
