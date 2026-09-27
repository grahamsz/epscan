// SPDX-License-Identifier: MIT
//! Spatial sharpness measures over unchanged, full-resolution captured samples.
//!
//! Samples are normalized by their full-scale integer value; RGB intensity uses
//! Rec.709 weights without gamma conversion. The outer one-pixel border is
//! excluded. Tenengrad is the mean squared magnitude of the unscaled 3x3 Sobel
//! gradient, without a threshold. Variance of Laplacian is the population
//! variance of the four-neighbor kernel with center weight -4. No resizing,
//! automatic levels, or image-dependent normalization is applied.

use crate::{Error, Result, session::image::ImageResult};
use serde::Serialize;
use std::{
    fs::File,
    io::{BufReader, Read},
    sync::atomic::{AtomicBool, Ordering},
};

/// Version of the normalization, kernels, border policy, and aggregation rules.
pub const SHARPNESS_METHOD_VERSION: u32 = 1;
/// The RGB sample weights, applied after full-scale normalization.
pub const REC709_WEIGHTS: [f64; 3] = [0.2126, 0.7152, 0.0722];

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SharpnessScores {
    pub method_version: u32,
    pub sample_normalization: &'static str,
    pub luminance: &'static str,
    pub tenengrad: f64,
    pub variance_of_laplacian: f64,
    pub evaluated_pixels: u64,
}

/// Measure packed RGB or gray, 8-bit or little-endian 16-bit raw image data.
///
/// Uses three intensity rows and one packed input row, requiring O(width)
/// memory. Cancellation is checked between rows. Images smaller than 3x3,
/// unsupported shapes/depths, and payload-size mismatches are rejected.
pub fn measure(image: &ImageResult, cancel: &AtomicBool) -> Result<SharpnessScores> {
    check_cancel(cancel)?;
    let shape = Shape::from_image(image)?;
    let file = File::open(&image.payload)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != shape.expected_bytes {
        return Err(Error::Protocol(format!(
            "Sharpness requires {} packed image bytes; payload has {}",
            shape.expected_bytes,
            metadata.len()
        )));
    }
    measure_rows(&mut BufReader::new(file), shape, cancel)
}

#[derive(Clone, Copy)]
struct Shape {
    width: usize,
    height: u32,
    channels: usize,
    sample_bytes: usize,
    row_bytes: usize,
    expected_bytes: u64,
}

impl Shape {
    fn from_image(image: &ImageResult) -> Result<Self> {
        if image.width < 3 || image.height < 3 {
            return Err(Error::Invalid(
                "Sharpness requires an image at least 3 x 3 pixels".into(),
            ));
        }
        if ![1, 3].contains(&image.channels) || ![8, 16].contains(&image.depth) {
            return Err(Error::Invalid(
                "Sharpness requires packed gray or RGB samples at 8 or 16 bits".into(),
            ));
        }
        let channels = usize::from(image.channels);
        let sample_bytes = usize::from(image.depth / 8);
        let width = usize::try_from(image.width)
            .map_err(|_| Error::Invalid("Sharpness row width exceeds address space".into()))?;
        let row_bytes = width
            .checked_mul(channels)
            .and_then(|size| size.checked_mul(sample_bytes))
            .ok_or_else(|| Error::Invalid("Sharpness row size overflow".into()))?;
        let expected_bytes = u64::try_from(row_bytes)
            .ok()
            .and_then(|size| size.checked_mul(u64::from(image.height)))
            .ok_or_else(|| Error::Invalid("Sharpness image size overflow".into()))?;
        Ok(Self {
            width,
            height: image.height,
            channels,
            sample_bytes,
            row_bytes,
            expected_bytes,
        })
    }
}

fn zeroed<T: Default + Clone>(length: usize) -> Result<Vec<T>> {
    let mut values = Vec::new();
    values.try_reserve_exact(length).map_err(|error| {
        Error::Invalid(format!("Cannot allocate sharpness row buffer: {error}"))
    })?;
    values.resize(length, T::default());
    Ok(values)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn read_row(
    reader: &mut impl Read,
    packed: &mut [u8],
    intensity: &mut [f64],
    shape: Shape,
    cancel: &AtomicBool,
) -> Result<()> {
    check_cancel(cancel)?;
    reader.read_exact(packed)?;
    let pixel_bytes = shape.channels * shape.sample_bytes;
    for (destination, pixel) in intensity.iter_mut().zip(packed.chunks_exact(pixel_bytes)) {
        let sample = |channel: usize| {
            if shape.sample_bytes == 1 {
                f64::from(pixel[channel]) / 255.0
            } else {
                let offset = channel * 2;
                f64::from(u16::from_le_bytes([pixel[offset], pixel[offset + 1]])) / 65535.0
            }
        };
        *destination = if shape.channels == 1 {
            sample(0)
        } else {
            REC709_WEIGHTS[0] * sample(0)
                + REC709_WEIGHTS[1] * sample(1)
                + REC709_WEIGHTS[2] * sample(2)
        };
    }
    Ok(())
}

fn measure_rows(
    reader: &mut impl Read,
    shape: Shape,
    cancel: &AtomicBool,
) -> Result<SharpnessScores> {
    check_cancel(cancel)?;
    let mut packed = zeroed::<u8>(shape.row_bytes)?;
    let mut previous = zeroed::<f64>(shape.width)?;
    let mut current = zeroed::<f64>(shape.width)?;
    let mut next = zeroed::<f64>(shape.width)?;
    read_row(reader, &mut packed, &mut previous, shape, cancel)?;
    read_row(reader, &mut packed, &mut current, shape, cancel)?;
    let mut evaluated_pixels = 0u64;
    let mut gradient_sum = 0.0;
    let mut laplacian_mean = 0.0;
    let mut laplacian_m2 = 0.0;
    for _ in 1..shape.height - 1 {
        read_row(reader, &mut packed, &mut next, shape, cancel)?;
        check_cancel(cancel)?;
        for x in 1..shape.width - 1 {
            let gx = (previous[x + 1] + 2.0 * current[x + 1] + next[x + 1])
                - (previous[x - 1] + 2.0 * current[x - 1] + next[x - 1]);
            let gy = (next[x - 1] + 2.0 * next[x] + next[x + 1])
                - (previous[x - 1] + 2.0 * previous[x] + previous[x + 1]);
            gradient_sum += gx * gx + gy * gy;
            let laplacian =
                (previous[x] + next[x]) + (current[x - 1] + current[x + 1]) - 4.0 * current[x];
            evaluated_pixels += 1;
            // Welford's update avoids subtracting two nearly equal moments.
            let delta = laplacian - laplacian_mean;
            laplacian_mean += delta / evaluated_pixels as f64;
            laplacian_m2 += delta * (laplacian - laplacian_mean);
        }
        std::mem::swap(&mut previous, &mut current);
        std::mem::swap(&mut current, &mut next);
    }
    check_cancel(cancel)?;
    // Detect trailing data even if the file grew after the metadata check.
    if reader.read(&mut [0])? != 0 {
        return Err(Error::Protocol(
            "Sharpness payload has data beyond the declared image".into(),
        ));
    }
    check_cancel(cancel)?;
    Ok(SharpnessScores {
        method_version: SHARPNESS_METHOD_VERSION,
        sample_normalization: "full-scale-0..1",
        luminance: if shape.channels == 3 {
            "rec709"
        } else {
            "gray"
        },
        tenengrad: gradient_sum / evaluated_pixels as f64,
        variance_of_laplacian: (laplacian_m2 / evaluated_pixels as f64).max(0.0),
        evaluated_pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Cursor};

    fn image(width: u32, height: u32, channels: u8, depth: u8) -> ImageResult {
        ImageResult {
            payload: "unused.bin".into(),
            tiff: None,
            width,
            height,
            channels,
            depth,
            dpi: 300,
            metadata: serde_json::Value::Null,
        }
    }

    fn measure_samples(image: &mut ImageResult, samples: &[u8]) -> SharpnessScores {
        let directory = tempfile::tempdir().unwrap();
        image.payload = directory.path().join("samples.bin");
        fs::write(&image.payload, samples).unwrap();
        measure(image, &AtomicBool::new(false)).unwrap()
    }

    fn close(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() <= 1e-12 * expected.abs().max(1.0),
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn constant_and_linear_ramp_have_analytic_scores() {
        let mut image = image(5, 5, 1, 8);
        let constant = measure_samples(&mut image, &[85; 25]);
        close(constant.tenengrad, 0.0);
        close(constant.variance_of_laplacian, 0.0);
        assert_eq!(constant.evaluated_pixels, 9);
        let ramp: Vec<u8> = (0..25).map(|pixel| (pixel % 5) * 32).collect();
        let ramp = measure_samples(&mut image, &ramp);
        close(ramp.tenengrad, 64.0 * (32.0f64 / 255.0).powi(2));
        close(ramp.variance_of_laplacian, 0.0);
    }

    #[test]
    fn centered_impulse_has_unscaled_sobel_and_population_laplacian_scores() {
        let mut samples = vec![0; 25];
        samples[12] = 255;
        let scores = measure_samples(&mut image(5, 5, 1, 8), &samples);
        close(scores.tenengrad, 24.0 / 9.0);
        close(scores.variance_of_laplacian, 20.0 / 9.0);
        assert_eq!(scores.evaluated_pixels, 9);
        assert_eq!(scores.method_version, SHARPNESS_METHOD_VERSION);
        assert_eq!(scores.sample_normalization, "full-scale-0..1");
        assert_eq!(scores.luminance, "gray");
    }

    #[test]
    fn rgb_impulse_uses_rec709_weights() {
        let mut samples = vec![0; 25 * 3];
        samples[36..39].copy_from_slice(&[120, 80, 30]);
        let scores = measure_samples(&mut image(5, 5, 3, 8), &samples);
        let intensity = (120.0 * 0.2126 + 80.0 * 0.7152 + 30.0 * 0.0722) / 255.0;
        close(scores.tenengrad, (24.0 / 9.0) * intensity * intensity);
        close(
            scores.variance_of_laplacian,
            (20.0 / 9.0) * intensity * intensity,
        );
        assert_eq!(scores.luminance, "rec709");
    }

    #[test]
    fn blurred_edge_reduces_both_scores() {
        let sharp: Vec<u8> = (0..81)
            .map(|pixel| if pixel % 9 < 4 { 0 } else { 255 })
            .collect();
        let blurred_profile = [0, 0, 0, 64, 128, 191, 255, 255, 255];
        let blurred: Vec<u8> = (0..81).map(|pixel| blurred_profile[pixel % 9]).collect();
        let sharp = measure_samples(&mut image(9, 9, 1, 8), &sharp);
        let blurred = measure_samples(&mut image(9, 9, 1, 8), &blurred);
        assert!(sharp.tenengrad > blurred.tenengrad);
        assert!(sharp.variance_of_laplacian > blurred.variance_of_laplacian);
    }

    #[test]
    fn expanded_16_bit_samples_match_8_bit_and_rgb_gray_matches_gray() {
        let gray: Vec<u8> = (0..42)
            .map(|pixel| ((pixel % 7) * 19 + (pixel / 7) * 7) as u8)
            .collect();
        let rgb: Vec<u8> = gray.iter().flat_map(|sample| [*sample; 3]).collect();
        let gray16: Vec<u8> = gray
            .iter()
            .flat_map(|sample| (u16::from(*sample) * 257).to_le_bytes())
            .collect();
        let rgb16: Vec<u8> = rgb
            .iter()
            .flat_map(|sample| (u16::from(*sample) * 257).to_le_bytes())
            .collect();
        let reference = measure_samples(&mut image(7, 6, 1, 8), &gray);
        for (channels, depth, samples) in [(3, 8, &rgb), (1, 16, &gray16), (3, 16, &rgb16)] {
            let scores = measure_samples(&mut image(7, 6, channels, depth), samples);
            close(scores.tenengrad, reference.tenengrad);
            close(
                scores.variance_of_laplacian,
                reference.variance_of_laplacian,
            );
            assert_eq!(scores.evaluated_pixels, 20);
        }
    }

    #[test]
    fn invalid_shapes_and_mismatched_payload_lengths_are_rejected() {
        let cancel = AtomicBool::new(false);
        for shape in [
            (2, 3, 1, 8),
            (3, 2, 1, 8),
            (3, 3, 2, 8),
            (3, 3, 3, 12),
            (u32::MAX, u32::MAX, 3, 16),
        ] {
            assert!(matches!(
                measure(&image(shape.0, shape.1, shape.2, shape.3), &cancel),
                Err(Error::Invalid(_))
            ));
        }
        let directory = tempfile::tempdir().unwrap();
        let mut image = image(3, 3, 1, 8);
        image.payload = directory.path().join("samples.bin");
        for bytes in [8, 10] {
            fs::write(&image.payload, vec![0; bytes]).unwrap();
            assert!(matches!(measure(&image, &cancel), Err(Error::Protocol(_))));
        }
    }

    #[test]
    fn stream_truncation_and_trailing_bytes_are_rejected() {
        let shape = Shape::from_image(&image(3, 3, 1, 8)).unwrap();
        let cancel = AtomicBool::new(false);
        let truncated = measure_rows(&mut Cursor::new([0; 8]), shape, &cancel);
        assert!(
            matches!(truncated, Err(Error::Io(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof)
        );
        assert!(matches!(
            measure_rows(&mut Cursor::new([0; 10]), shape, &cancel),
            Err(Error::Protocol(_))
        ));
    }

    #[test]
    fn cancellation_is_checked_before_opening_and_between_rows() {
        assert!(matches!(
            measure(&image(3, 3, 1, 8), &AtomicBool::new(true)),
            Err(Error::Cancelled)
        ));
        struct CancellingReader<'a> {
            samples: Cursor<[u8; 9]>,
            reads: usize,
            cancel: &'a AtomicBool,
        }
        impl Read for CancellingReader<'_> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                self.reads += 1;
                let count = self.samples.read(buffer)?;
                if self.reads == 2 {
                    self.cancel.store(true, Ordering::Relaxed);
                }
                Ok(count)
            }
        }
        let cancel = AtomicBool::new(false);
        let mut reader = CancellingReader {
            samples: Cursor::new([0; 9]),
            reads: 0,
            cancel: &cancel,
        };
        let shape = Shape::from_image(&image(3, 3, 1, 8)).unwrap();
        assert!(matches!(
            measure_rows(&mut reader, shape, &cancel),
            Err(Error::Cancelled)
        ));
        assert_eq!(reader.reads, 2);
    }
}
