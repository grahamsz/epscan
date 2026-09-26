// SPDX-License-Identifier: MIT OR Apache-2.0
//! Optional correction of coherent vertical bands in uninverted negative samples.
//! The original packed acquisition stays immutable. TIFFs are streamed in strips;
//! fitting retains only disjoint sets of at most 768 full-width source rows.

mod model;
#[cfg(test)]
mod tests;

use crate::{Error, Result, session::image::ImageResult};
use serde::Serialize;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// The accepted experimental negative-film preset. Brightness is original
/// sample/full-scale, with a common maximum-channel mask for RGB.
#[derive(Clone, Debug, Serialize)]
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
            strength: 0.8,
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

fn sample_rows(height: usize) -> (Vec<usize>, Vec<usize>) {
    let stride = 2 * height.div_ceil(2 * 768).max(1);
    (
        (0..height).step_by(stride).collect(),
        (stride / 2..height).step_by(stride).collect(),
    )
}

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
) -> Result<model::Samples> {
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
    Ok(model::Samples {
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
    options.validate_shape(image.width, image.height)?;
    let (row_bytes, expected) = packed_shape(image)?;
    let source = image.payload.metadata()?;
    if !source.is_file() || source.len() != expected {
        return Err(Error::Protocol(
            "Banding payload size does not match packed row stride".into(),
        ));
    }
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
    let samples = sample_image(image, row_bytes, cancel)?;
    let fitted = model::analyze(&samples, options, cancel)?;
    drop(samples);
    let preview = if options.save_signal {
        Some(signal_preview(image, &fitted, options, row_bytes, cancel)?)
    } else {
        None
    };
    let mut banding = serde_json::json!({
        "method_version": 1,
        "config": options,
        "status": if fitted.has_components() { "corrected" } else { "no_supported_frequency" },
        "model": "multiplicative_log_gain",
        "axis": "vertical stripes; carrier varies across source columns x",
        "diagnostics": fitted,
        "corrected_tiff_file": final_path,
        "source_payload_unchanged": true,
        "signal_semantics": "strength * original darkness mask * fitted log-gain; corrected = raw * exp(-signal), before integer quantization/clipping"
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
        "derived multiplicative periodic band correction; original payload unchanged".into();
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
                    fitted.correction_row(channel, first_row as usize + dy, correction);
                }
                for x in 0..width {
                    let brightness = (0..channels)
                        .map(|channel| sample_value(row, x * channels + channel, image.depth))
                        .max()
                        .unwrap();
                    let mask = model::dark_weight(
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
                        let corrected = model::corrected_sample(
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
    channels: u8,
    range: f64,
    pixels: Vec<u8>,
}

impl SignalPreview {
    fn metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "width": self.width, "height": self.height, "panel_width": self.panel_width,
            "source_width": self.source_width, "source_height": self.source_height,
            "panels": if self.channels == 3 { "red, green, blue channel panels, left to right" } else { "gray channel" },
            "coordinates": "source orientation preserved; nearest samples at floor(preview_coordinate * source_extent / panel_extent)",
            "range_log_gain": [-self.range, self.range],
            "legend": "red = positive log signal removed (output darker); white = zero; blue = negative (output brighter)",
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
    fitted: &model::Model,
    options: &BandingOptions,
    row_bytes: usize,
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
            let mask = model::dark_weight(
                f64::from(brightness) / max_value,
                options.dark_full,
                options.dark_off,
            );
            for channel in 0..channels {
                values[py * width + channel * panel_width + px] =
                    options.strength * mask * fitted.correction(channel, x, y);
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
        channels: image.channels,
        range,
        pixels,
    })
}
