// SPDX-License-Identifier: MIT
use super::*;
use std::f64::consts::TAU;
use tiff::decoder::{Decoder, DecodingResult};

fn fixture(directory: &Path, channels: u8, depth: u8, periodic: bool) -> (ImageResult, Vec<u16>) {
    let width = 256;
    let height = 384;
    let maximum = if depth == 16 { 65535.0 } else { 255.0 };
    let mut values = Vec::new();
    let mut packed = Vec::new();
    for y in 0..height {
        for x in 0..width {
            for channel in 0..channels {
                let base = if y >= 320 || (channels == 3 && x == 7 && channel == 2) {
                    0.8
                } else {
                    0.12 + 0.02 * f64::from(y) / f64::from(height)
                };
                let gain = if periodic && y < 320 {
                    (0.08 * (TAU * f64::from(x) / 32.0 + 0.3).cos()).exp()
                } else {
                    1.0
                };
                let value = if x == 0 {
                    0
                } else {
                    (maximum * base * gain).round() as u16
                };
                values.push(value);
                if depth == 16 {
                    packed.extend_from_slice(&value.to_le_bytes());
                } else {
                    packed.push(value as u8);
                }
            }
        }
    }
    let payload = directory.join("capture.raw");
    std::fs::write(&payload, packed).unwrap();
    (
        ImageResult {
            payload,
            tiff: None,
            width,
            height,
            channels,
            depth,
            dpi: 3200,
            metadata: serde_json::json!({"sample_transform": "none; exact packed samples", "test_source": true}),
        },
        values,
    )
}

fn decode(path: &Path) -> (Vec<u16>, serde_json::Value, [u32; 2]) {
    let mut decoder = Decoder::new(File::open(path).unwrap()).unwrap();
    let (width, height) = decoder.dimensions().unwrap();
    let description = decoder
        .get_tag_ascii_string(tiff::tags::Tag::ImageDescription)
        .unwrap();
    let metadata = serde_json::from_str(&description).unwrap();
    let samples = match decoder.read_image().unwrap() {
        DecodingResult::U8(samples) => samples.into_iter().map(u16::from).collect(),
        DecodingResult::U16(samples) => samples,
        _ => panic!("unexpected TIFF sample type"),
    };
    (samples, metadata, [width, height])
}

#[test]
fn shared_fit_exports_match_full_corrected_pixels_at_nonzero_crop_origins() {
    for (channels, depth) in [(1, 16), (3, 8)] {
        let directory = tempfile::tempdir().unwrap();
        let (mut source, original) = fixture(directory.path(), channels, depth, true);
        let source_bytes = std::fs::read(&source.payload).unwrap();
        let options = BandingOptions {
            save_raw: true,
            save_signal: true,
            ..Default::default()
        };
        let cancel = AtomicBool::new(false);
        let fitted = PreparedBanding::prepare(&source, &options, &cancel).unwrap();
        let full_path = directory.path().join("full.tiff");
        export_prepared(&mut source, &full_path, &fitted, [0, 0], false, &cancel).unwrap();
        let full = decode(&full_path).0;
        assert_ne!(full, original);
        for (index, [x, y, width, height]) in [[31, 43, 173, 127], [12, 246, 180, 119]]
            .into_iter()
            .enumerate()
        {
            let slice = |values: &[u16]| {
                let mut cropped = Vec::new();
                for row in y..y + height {
                    let start = (row * source.width + x) as usize * channels as usize;
                    cropped.extend_from_slice(
                        &values[start..start + width as usize * channels as usize],
                    );
                }
                cropped
            };
            let raw_crop = slice(&original);
            let packed: Vec<u8> = if depth == 16 {
                raw_crop
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect()
            } else {
                raw_crop.iter().map(|value| *value as u8).collect()
            };
            let payload = directory.path().join(format!("crop{index}.bin"));
            std::fs::write(&payload, &packed).unwrap();
            let original_metadata =
                serde_json::json!({"sample_transform":"none; exact packed samples"});
            let mut crop = ImageResult {
                payload,
                tiff: None,
                width,
                height,
                channels,
                depth,
                dpi: source.dpi,
                metadata: original_metadata.clone(),
            };
            let output = directory.path().join(format!("crop{index}.tiff"));
            fitted
                .export_crop(&mut crop, [x, y], &output, &cancel)
                .unwrap();
            let (corrected, metadata, dimensions) = decode(&output);
            assert_eq!(dimensions, [width, height]);
            assert_eq!(corrected, slice(&full));
            assert_eq!(metadata["banding"]["analysis_scope"], "shared_capture");
            assert_eq!(
                metadata["banding"]["analysis_source"]["width"],
                source.width
            );
            assert_eq!(
                metadata["banding"]["analysis_source"]["height"],
                source.height
            );
            assert_eq!(
                metadata["banding"]["crop_pixels"],
                serde_json::json!([x, y, width, height])
            );
            assert_eq!(
                metadata["banding"]["signal_preview"]["model_origin"],
                serde_json::json!([x, y])
            );
            let (raw, raw_metadata, _) =
                decode(&directory.path().join(format!("crop{index}_raw.tiff")));
            assert_eq!(raw, raw_crop);
            assert_eq!(raw_metadata, original_metadata);
            assert_eq!(std::fs::read(&crop.payload).unwrap(), packed);
            assert!(
                directory
                    .path()
                    .join(format!("crop{index}_banding.png"))
                    .is_file()
            );
        }
        assert_eq!(std::fs::read(&source.payload).unwrap(), source_bytes);
    }
}

#[test]
fn exports_preserve_raw_and_protected_samples_with_truthful_metadata() {
    for (channels, depth) in [(1, 16), (3, 8)] {
        let directory = tempfile::tempdir().unwrap();
        let (mut image, original) = fixture(directory.path(), channels, depth, true);
        let original_payload = std::fs::read(&image.payload).unwrap();
        let original_metadata = image.metadata.clone();
        let options = BandingOptions {
            save_raw: true,
            save_signal: true,
            detection_roi: Some([0, image.width, 0, 320]),
            ..Default::default()
        };
        let output = directory.path().join("corrected.tiff");
        export(&mut image, &output, &options, &AtomicBool::new(false)).unwrap();
        assert_eq!(image.tiff.as_ref(), Some(&output));
        assert_eq!(image.metadata["banding"]["status"], "corrected");
        assert_eq!(image.metadata["banding"]["config"]["strength"], 1.0);
        assert_eq!(image.metadata["banding"]["method_version"], 2);
        assert_eq!(
            image.metadata["banding"]["model"],
            "residual_refined_linear_sine"
        );
        let refinement = image.metadata["banding"]["residual_refinement"]
            .as_array()
            .unwrap();
        assert_eq!(refinement.len(), channels as usize);
        for channel in refinement {
            assert!(
                channel["residual_rms_after"].as_f64().unwrap()
                    <= channel["residual_rms_before"].as_f64().unwrap()
            );
        }
        assert_eq!(
            image.metadata["sample_transform"],
            original_metadata["sample_transform"]
        );
        let (corrected, corrected_metadata, dimensions) = decode(&output);
        assert_eq!(dimensions, [image.width, image.height]);
        assert!(
            corrected_metadata["sample_transform"]
                .as_str()
                .unwrap()
                .contains("derived")
        );
        let serialized: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&image.metadata["banding"]).unwrap())
                .unwrap();
        assert_eq!(corrected_metadata["banding"], serialized);
        let (raw, raw_metadata, _) = decode(&directory.path().join("corrected_raw.tiff"));
        assert_eq!(raw, original);
        assert_eq!(raw_metadata, original_metadata);
        assert_eq!(std::fs::read(&image.payload).unwrap(), original_payload);
        let maximum = if depth == 16 { 65535.0 } else { 255.0 };
        let mut changed = 0;
        for (source, output) in original
            .chunks(channels as usize)
            .zip(corrected.chunks(channels as usize))
        {
            if f64::from(*source.iter().max().unwrap()) / maximum >= options.dark_off {
                assert_eq!(output, source);
            }
            for (&before, &after) in source.iter().zip(output) {
                if before == 0 {
                    assert_eq!(after, 0);
                }
                changed += usize::from(before != after);
            }
        }
        assert!(changed > original.len() / 5);
        let decoder =
            png::Decoder::new(File::open(directory.path().join("corrected_banding.png")).unwrap());
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buffer).unwrap();
        assert_eq!(info.width, image.width * u32::from(channels));
        assert_eq!(info.height, image.height);
        assert_eq!(info.color_type, png::ColorType::Rgb);
        assert!(
            reader
                .info()
                .uncompressed_latin1_text
                .iter()
                .any(|text| text.text.contains("source orientation preserved"))
        );
        let stride = info.width as usize * 3;
        // A carrier varies along columns: signs agree between adjacent dark rows,
        // reverse a half-period across x, and protected rows are exactly white.
        let pixel = |x: usize, y: usize| &buffer[y * stride + x * 3..y * stride + x * 3 + 3];
        assert!(pixel(30, 60)[0] > pixel(30, 60)[2]);
        assert!(pixel(46, 60)[2] > pixel(46, 60)[0]);
        assert!(pixel(30, 61)[0] > pixel(30, 61)[2]);
        assert_eq!(pixel(30, 340), [255, 255, 255]);
    }
}

#[test]
fn unsupported_flat_signal_exports_unchanged_and_reports_no_frequency() {
    let directory = tempfile::tempdir().unwrap();
    let (mut image, _) = fixture(directory.path(), 1, 16, false);
    let original = vec![8000u16; image.width as usize * image.height as usize];
    let packed: Vec<_> = original
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect();
    std::fs::write(&image.payload, packed).unwrap();
    let output = directory.path().join("flat.tiff");
    export(
        &mut image,
        &output,
        &BandingOptions::default(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        image.metadata["banding"]["status"],
        "no_supported_frequency"
    );
    assert_eq!(decode(&output).0, original);
}

#[test]
fn preflight_rejects_any_existing_companion_and_cancel_creates_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let (mut image, _) = fixture(directory.path(), 1, 16, true);
    let output = directory.path().join("final.tiff");
    let companion = directory.path().join("final_raw.tiff");
    let options = BandingOptions {
        save_raw: true,
        save_signal: true,
        ..Default::default()
    };
    std::fs::write(&companion, b"keep").unwrap();
    assert!(export(&mut image, &output, &options, &AtomicBool::new(false)).is_err());
    assert_eq!(std::fs::read(&companion).unwrap(), b"keep");
    assert!(!output.exists());
    assert!(!directory.path().join("final_banding.png").exists());
    assert!(image.tiff.is_none());
    assert!(matches!(
        export(&mut image, &output, &options, &AtomicBool::new(true)),
        Err(Error::Cancelled)
    ));
    assert!(!output.exists());
}

#[test]
fn cancelled_strip_export_removes_its_partial_file_and_preserves_payload() {
    let directory = tempfile::tempdir().unwrap();
    let (image, _) = fixture(directory.path(), 1, 16, true);
    let before = std::fs::read(&image.payload).unwrap();
    let output = directory.path().join("partial.tiff");
    let result = image.save_tiff_transformed(&output, &image.metadata, |y, _| {
        if y > 0 { Err(Error::Cancelled) } else { Ok(()) }
    });
    assert!(matches!(result, Err(Error::Cancelled)));
    assert!(!output.exists());
    assert_eq!(std::fs::read(&image.payload).unwrap(), before);
}

#[test]
fn options_reject_bad_numeric_values_and_insufficient_detection_roi() {
    for strength in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
        assert!(
            BandingOptions {
                strength,
                ..Default::default()
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        BandingOptions {
            dark_full: 0.6,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        BandingOptions {
            grid_y: usize::MAX,
            ..Default::default()
        }
        .validate()
        .is_err()
    );
    assert!(BandingOptions::default().validate_shape(40, 100).is_err());
    assert!(
        BandingOptions {
            detection_roi: Some([0, 200, 0, 8]),
            ..Default::default()
        }
        .validate_shape(256, 384)
        .is_err()
    );
    assert!(
        BandingOptions {
            detection_roi: Some([0, 200, 0, 500]),
            ..Default::default()
        }
        .validate_shape(256, 384)
        .is_err()
    );
    assert!(
        BandingOptions {
            max_period: Some(100.0),
            ..Default::default()
        }
        .validate_shape(256, 384)
        .is_err()
    );
}
