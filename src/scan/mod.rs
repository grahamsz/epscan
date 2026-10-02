// SPDX-License-Identifier: MIT
//! High-level pass planning, acquisition results and bounded file output.
pub mod banding;
pub mod crop;
pub mod frame;
pub mod holder;
pub mod io;
pub mod region_batch;
pub mod regions;
mod sampling;
pub mod sharpness;
mod stream;
use crate::{
    Capabilities, Error, Gamma, Result, ScanMode, ScanSettings,
    capabilities::{FrameFormat, Holder, ScannerModel},
    error::unsupported,
    session::image::ImageResult,
};
use serde::Serialize;
use std::{path::PathBuf, time::Duration};

/// A numbered frame in a scanner-specific holder layout.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HolderSelection {
    pub holder: Holder,
    /// Optional compatible exposure preset; omission uses the holder default.
    pub frame_format: Option<FrameFormat>,
    /// One-based frame number in the selected holder layout.
    pub frame: u32,
    /// Signed total percentage change to width and height, centered on the
    /// frame; positive expands, negative crops, and values must exceed -100.
    pub overage_percent: f64,
}

impl HolderSelection {
    fn validate(self, settings: &ScanSettings, model: &ScannerModel) -> Result<()> {
        let layout = model.holder(self.holder)?.for_format(self.frame_format)?;
        let rectangle = layout.frame_rect(self.frame, self.overage_percent)?;
        if settings.source != layout.source {
            return Err(Error::Invalid(format!(
                "Holder selection requires source {:?}, but scan settings use {:?}",
                layout.source, settings.source
            )));
        }
        // This tolerance only absorbs floating-point arithmetic differences;
        // a shifted or resized region must not claim a different holder frame.
        if settings
            .rect_mm
            .iter()
            .zip(rectangle)
            .any(|(actual, expected)| (*actual - expected).abs() > 1e-6)
        {
            return Err(Error::Invalid(
                "Scan rectangle does not match the selected holder frame and overage".into(),
            ));
        }
        Ok(())
    }

    fn metadata(self, model: &ScannerModel) -> Result<serde_json::Value> {
        let layout = model.holder(self.holder)?.for_format(self.frame_format)?;
        let mut metadata = serde_json::to_value(self)?;
        metadata["frame_format"] =
            serde_json::to_value(self.frame_format.or(layout.default_format))?;
        metadata["layout"] = layout.name.into();
        metadata["source"] = serde_json::to_value(layout.source)?;
        metadata["nominal_rect_mm"] = serde_json::to_value(layout.frame_rect(self.frame, 0.0)?)?;
        metadata["rect_mm"] =
            serde_json::to_value(layout.frame_rect(self.frame, self.overage_percent)?)?;
        metadata["overage_semantics"] =
            "signed total width and height change, centered; positive expands and negative crops equally on both edges".into();
        Ok(metadata)
    }
}

#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Add a separate experimental infrared acquisition after the visible image.
    pub infrared: bool,
    /// Acquire infrared without a visible or thumbnail pass.
    pub infrared_only: bool,
    /// Depth for infrared passes only; visible depth is in `ScanSettings`.
    pub ir_depth: u8,
    /// Tone table for infrared passes only; overrides `ScanSettings::gamma`.
    pub ir_gamma: Gamma,
    pub thumbnail: bool,
    /// Disable to retain only raw payloads and metadata; TIFF can be exported later.
    pub export_tiff: bool,
    pub film: String,
    /// Optional holder provenance; its source and region must match `ScanSettings`.
    pub holder_selection: Option<HolderSelection>,
    /// Measure both sharpness metrics on the main visible pass before TIFF export.
    pub measure_sharpness: bool,
    /// Optional host-side vertical band correction for visible TIFF exports.
    /// Captured payloads stay unchanged; extra raw TIFF/PNG outputs are opt-in.
    pub banding: Option<banding::BandingOptions>,
    pub pass_timeout: Duration,
    /// Optional host delay between scans; this did not resolve the observed IR failure.
    pub settle_time: Duration,
}
impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            infrared: false,
            infrared_only: false,
            ir_depth: 8,
            ir_gamma: Gamma::DeviceDefault,
            thumbnail: false,
            export_tiff: true,
            film: "negative".into(),
            holder_selection: None,
            measure_sharpness: false,
            banding: None,
            pass_timeout: Duration::from_secs(3600),
            settle_time: Duration::from_secs(10),
        }
    }
}

/// The role of one acquisition in a scan job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PassKind {
    Rgb,
    Gray,
    Ir,
    Thumbnail,
}

impl PassKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rgb => "rgb",
            Self::Gray => "gray",
            Self::Ir => "ir",
            Self::Thumbnail => "thumbnail",
        }
    }

    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Self::Rgb | Self::Gray => "",
            Self::Ir => "_IR",
            Self::Thumbnail => "_thumbnail",
        }
    }

    pub(crate) fn matches_mode(self, mode: ScanMode) -> bool {
        match self {
            Self::Rgb => mode == ScanMode::Rgb,
            Self::Gray => mode == ScanMode::Gray,
            Self::Ir | Self::Thumbnail => true,
        }
    }
}

/// One fully validated acquisition, including its effective wire geometry.
#[derive(Clone, Debug, Serialize)]
pub struct PlannedPass {
    pub kind: PassKind,
    pub settings: ScanSettings,
    pub pixels: [u32; 4],
    pub channels: u8,
    pub expected_bytes: u64,
}

/// A host-side plan. Building this value does not communicate with a scanner.
#[derive(Clone, Debug, Serialize)]
pub struct ScanPlan {
    pub passes: Vec<PlannedPass>,
}

impl ScanOptions {
    /// Validate the complete job before reserving output or starting any pass.
    pub fn plan(&self, settings: &ScanSettings, caps: &Capabilities) -> Result<ScanPlan> {
        if let Some(options) = &self.banding {
            options.validate()?;
            if !self.export_tiff || self.infrared_only {
                return Err(Error::Invalid(
                    "Band reduction requires visible TIFF export; it cannot be used with raw-only or infrared-only capture".into(),
                ));
            }
        }
        if self.measure_sharpness && self.infrared_only {
            return Err(Error::Invalid(
                "Sharpness requires an RGB or grayscale pass".into(),
            ));
        }
        if self.infrared_only && (self.infrared || self.thumbnail) {
            return Err(Error::Invalid(
                "Infrared-only cannot be combined with an additional IR pass or thumbnail".into(),
            ));
        }
        if settings.preview && (self.infrared || self.infrared_only || self.thumbnail) {
            return Err(Error::Invalid("Preview is a single visible pass".into()));
        }
        if self.pass_timeout.is_zero() {
            return Err(Error::Invalid("Pass timeout must be positive".into()));
        }
        if std::time::Instant::now()
            .checked_add(self.pass_timeout)
            .is_none()
        {
            return Err(Error::Invalid("Pass timeout is too large".into()));
        }
        if (self.thumbnail || self.infrared)
            && std::time::Instant::now()
                .checked_add(self.settle_time)
                .is_none()
        {
            return Err(Error::Invalid("Settle time is too large".into()));
        }
        if (self.infrared || self.infrared_only) && self.film.eq_ignore_ascii_case("mono") {
            return Err(unsupported(
                "infrared on monochrome film",
                "silver-bearing film is unsuitable",
            ));
        }
        let model = caps.scanner_model()?;
        // The base settings describe a visible image even for an IR-only job. Validate them
        // before applying the explicitly separate infrared depth/tone controls,
        // so an invalid public input never gets silently repaired by an override.
        settings.validate(caps, false)?;
        if let Some(selection) = self.holder_selection {
            selection.validate(settings, model)?;
        }
        let mut requested = Vec::new();
        if self.thumbnail {
            let mut preview = settings.clone();
            preview.dpi = model.preview_dpi;
            preview.y_oversampling = 1;
            preview.depth = model.preview_depth;
            preview.preview = true;
            requested.push((PassKind::Thumbnail, preview));
        }
        if !self.infrared_only {
            let kind = match settings.mode {
                ScanMode::Rgb => PassKind::Rgb,
                ScanMode::Gray => PassKind::Gray,
            };
            requested.push((kind, settings.clone()));
        }
        if self.infrared || self.infrared_only {
            let mut infrared = settings.clone();
            infrared.mode = ScanMode::Gray;
            infrared.depth = self.ir_depth;
            infrared.gamma = self.ir_gamma;
            requested.push((PassKind::Ir, infrared));
        }
        let mut passes = Vec::with_capacity(requested.len());
        for (kind, settings) in requested {
            settings.validate(caps, kind == PassKind::Ir)?;
            let pixels = settings.pixels_for(model)?;
            if matches!(kind, PassKind::Rgb | PassKind::Gray)
                && let Some(options) = &self.banding
            {
                options.validate_shape(pixels[2], pixels[3])?;
            }
            if self.measure_sharpness
                && matches!(kind, PassKind::Rgb | PassKind::Gray)
                && (pixels[2] < 3 || pixels[3] < 3)
            {
                return Err(Error::Invalid(
                    "Sharpness requires an effective image of at least 3 by 3 pixels".into(),
                ));
            }
            let channels = model
                .mode_for(settings.mode, kind == PassKind::Ir)?
                .channels;
            let expected_bytes = u64::from(pixels[2])
                .checked_mul(u64::from(pixels[3]))
                .and_then(|n| n.checked_mul(u64::from(channels)))
                .and_then(|n| n.checked_mul(u64::from(settings.depth / 8)))
                .ok_or_else(|| Error::Invalid("Capture size overflow".into()))?;
            expected_bytes
                .checked_mul(u64::from(settings.y_oversampling))
                .ok_or_else(|| Error::Invalid("Y acquisition size overflow".into()))?;
            passes.push(PlannedPass {
                kind,
                settings,
                pixels,
                channels,
                expected_bytes,
            });
        }
        Ok(ScanPlan { passes })
    }
}
#[derive(Debug)]
pub struct Progress<'a> {
    pub phase: &'a str,
    pub pass: usize,
    pub done: u64,
    pub total: u64,
}
#[derive(Debug, Serialize)]
pub struct ScanResult {
    pub rgb: Option<ImageResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gray: Option<ImageResult>,
    pub ir: Option<ImageResult>,
    pub thumbnail: Option<ImageResult>,
    pub manifest: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Source;

    fn capabilities() -> Capabilities {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/v800-identity.json")).unwrap();
        let hex = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        Capabilities::parse(&bytes).unwrap()
    }

    #[test]
    fn extra_sampling_preserves_ir_registration_and_resets_automatic_thumbnails() {
        let options = ScanOptions {
            infrared: true,
            thumbnail: true,
            banding: Some(banding::BandingOptions::default()),
            ..Default::default()
        };
        let settings = ScanSettings {
            dpi: 3200,
            y_oversampling: 3,
            ..Default::default()
        };
        let plan = options.plan(&settings, &capabilities()).unwrap();
        assert_eq!(plan.passes[0].settings.y_oversampling, 1);
        assert_eq!(plan.passes[1].settings.acquisition_y_dpi().unwrap(), 9600);
        assert_eq!(plan.passes[2].settings.acquisition_y_dpi().unwrap(), 9600);
        assert_eq!(plan.passes[1].pixels, plan.passes[2].pixels);
        assert_eq!(
            plan.passes[1]
                .settings
                .acquisition_pixels_for(&crate::capabilities::V800_FAMILY)
                .unwrap(),
            plan.passes[2]
                .settings
                .acquisition_pixels_for(&crate::capabilities::V800_FAMILY)
                .unwrap()
        );
    }

    #[test]
    fn complete_plan_retains_source_and_records_effective_geometry() {
        let caps = capabilities();
        let options = ScanOptions {
            thumbnail: true,
            infrared: true,
            ..Default::default()
        };
        let settings = ScanSettings::default();
        let plan = options.plan(&settings, &caps).unwrap();
        assert_eq!(
            plan.passes.iter().map(|p| p.kind).collect::<Vec<_>>(),
            vec![PassKind::Thumbnail, PassKind::Rgb, PassKind::Ir]
        );
        assert!(
            plan.passes
                .iter()
                .all(|p| p.settings.source == settings.source)
        );
        assert_eq!(
            plan.passes[0].settings.dpi,
            caps.scanner_model().unwrap().preview_dpi
        );
        assert_eq!(plan.passes[1].pixels, [0, 0, 112, 118]);
        assert_eq!(plan.passes[1].expected_bytes, 112 * 118 * 6);
        assert_eq!(plan.passes[2].expected_bytes, 112 * 118);
        assert_eq!(plan.passes[2].settings.gamma, Gamma::DeviceDefault);
    }

    #[test]
    fn grayscale_plans_one_channel_at_both_depths_and_preserves_thumbnail_mode() {
        let caps = capabilities();
        for depth in [8, 16] {
            let settings = ScanSettings {
                mode: ScanMode::Gray,
                depth,
                ..Default::default()
            };
            let options = ScanOptions {
                thumbnail: true,
                infrared: true,
                measure_sharpness: true,
                ..Default::default()
            };
            let plan = options.plan(&settings, &caps).unwrap();
            assert_eq!(
                plan.passes.iter().map(|pass| pass.kind).collect::<Vec<_>>(),
                [PassKind::Thumbnail, PassKind::Gray, PassKind::Ir]
            );
            assert!(plan.passes.iter().all(|pass| pass.channels == 1));
            assert!(
                plan.passes
                    .iter()
                    .all(|pass| pass.settings.mode == ScanMode::Gray)
            );
            assert_eq!(plan.passes[0].settings.depth, 8);
            assert_eq!(
                plan.passes[1].expected_bytes,
                112 * 118 * u64::from(depth / 8)
            );
            assert_eq!(plan.passes[2].expected_bytes, 112 * 118);
            let preview = ScanSettings {
                preview: true,
                ..settings
            };
            let preview_plan = ScanOptions::default().plan(&preview, &caps).unwrap();
            assert_eq!(preview_plan.passes.len(), 1);
            assert_eq!(preview_plan.passes[0].kind, PassKind::Gray);
        }
    }

    #[test]
    fn invalid_later_pass_rejects_entire_job() {
        let caps = capabilities();
        let options = ScanOptions {
            infrared: true,
            ir_depth: 16,
            ..Default::default()
        };
        assert!(options.plan(&ScanSettings::default(), &caps).is_err());
        let options = ScanOptions {
            infrared: true,
            ..Default::default()
        };
        let settings = ScanSettings {
            source: Source::Flatbed,
            ..Default::default()
        };
        assert!(options.plan(&settings, &caps).is_err());
    }

    #[test]
    fn thumbnail_geometry_is_preflighted_at_its_own_resolution() {
        let caps = capabilities();
        let settings = ScanSettings {
            rect_mm: [0., 0., 1., 1.],
            ..Default::default()
        };
        assert!(ScanOptions::default().plan(&settings, &caps).is_ok());
        let options = ScanOptions {
            thumbnail: true,
            ..Default::default()
        };
        assert!(options.plan(&settings, &caps).is_err());
    }

    #[test]
    fn infrared_only_uses_ir_settings_and_rejects_invalid_base_settings() {
        let caps = capabilities();
        let mut options = ScanOptions {
            infrared_only: true,
            ..Default::default()
        };
        let invalid = ScanSettings {
            depth: 12,
            ..Default::default()
        };
        assert!(options.plan(&invalid, &caps).is_err());
        let settings = ScanSettings::default();
        let plan = options.plan(&settings, &caps).unwrap();
        assert_eq!(plan.passes.len(), 1);
        assert_eq!(plan.passes[0].kind, PassKind::Ir);
        assert_eq!(plan.passes[0].settings.depth, 8);
        options.thumbnail = true;
        assert!(options.plan(&settings, &caps).is_err());
    }

    #[test]
    fn overflowing_pass_deadline_is_rejected_in_plan() {
        let options = ScanOptions {
            pass_timeout: Duration::MAX,
            ..Default::default()
        };
        assert!(matches!(
            options.plan(&ScanSettings::default(), &capabilities()),
            Err(Error::Invalid(_))
        ));
        let options = ScanOptions {
            thumbnail: true,
            settle_time: Duration::MAX,
            ..Default::default()
        };
        assert!(matches!(
            options.plan(&ScanSettings::default(), &capabilities()),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn preview_is_always_a_single_rgb_pass() {
        let caps = capabilities();
        let settings = ScanSettings {
            preview: true,
            ..Default::default()
        };
        for options in [
            ScanOptions {
                infrared: true,
                ..Default::default()
            },
            ScanOptions {
                infrared_only: true,
                ..Default::default()
            },
            ScanOptions {
                thumbnail: true,
                ..Default::default()
            },
        ] {
            assert!(options.plan(&settings, &caps).is_err());
        }
    }

    #[test]
    fn sharpness_requires_rgb_without_changing_acquisition_settings() {
        let caps = capabilities();
        let settings = ScanSettings::default();
        let mut options = ScanOptions {
            measure_sharpness: true,
            infrared_only: true,
            ..Default::default()
        };
        assert!(matches!(
            options.plan(&settings, &caps),
            Err(Error::Invalid(_))
        ));
        options.infrared_only = false;
        assert_eq!(
            serde_json::to_value(options.plan(&settings, &caps).unwrap()).unwrap(),
            serde_json::to_value(ScanOptions::default().plan(&settings, &caps).unwrap()).unwrap()
        );
    }

    #[test]
    fn sharpness_preflights_the_effective_main_rgb_size() {
        let caps = capabilities();
        let settings = ScanSettings {
            rect_mm: [0., 0., 1., 0.1],
            ..Default::default()
        };
        assert!(ScanOptions::default().plan(&settings, &caps).is_ok());
        let options = ScanOptions {
            measure_sharpness: true,
            ..Default::default()
        };
        assert!(
            matches!(options.plan(&settings, &caps), Err(Error::Invalid(message)) if message.contains("at least 3 by 3"))
        );
        let preview = ScanSettings {
            preview: true,
            ..Default::default()
        };
        assert!(options.plan(&preview, &caps).is_ok());
        let options = ScanOptions {
            measure_sharpness: true,
            infrared: true,
            thumbnail: true,
            ..Default::default()
        };
        assert_eq!(
            options
                .plan(&ScanSettings::default(), &caps)
                .unwrap()
                .passes
                .len(),
            3
        );
    }

    #[test]
    fn holder_provenance_requires_the_registered_source_and_rectangle() {
        let caps = capabilities();
        let model = caps.scanner_model().unwrap();
        let selection = HolderSelection {
            holder: Holder::V800Film35mm,
            frame_format: None,
            frame: 1,
            overage_percent: 5.0,
        };
        let mut settings = ScanSettings {
            source: model.holder(selection.holder).unwrap().source,
            rect_mm: model
                .holder_frame(selection.holder, selection.frame, selection.overage_percent)
                .unwrap(),
            ..Default::default()
        };
        let options = ScanOptions {
            holder_selection: Some(selection),
            ..Default::default()
        };
        assert!(options.plan(&settings, &caps).is_ok());
        settings.rect_mm[0] += 0.5e-6;
        assert!(options.plan(&settings, &caps).is_ok());
        settings.rect_mm[0] += 0.01;
        assert!(matches!(
            options.plan(&settings, &caps),
            Err(Error::Invalid(message)) if message.contains("rectangle does not match")
        ));
        settings.rect_mm = model
            .holder_frame(selection.holder, selection.frame, selection.overage_percent)
            .unwrap();
        settings.source = Source::Flatbed;
        assert!(matches!(
            options.plan(&settings, &caps),
            Err(Error::Invalid(message)) if message.contains("requires source")
        ));
    }

    #[test]
    fn invalid_holder_selection_cannot_be_attached_to_a_valid_rectangle() {
        let caps = capabilities();
        for (frame, overage_percent) in [
            (0, 0.0),
            (u32::MAX, 0.0),
            (1, f64::NAN),
            (1, -100.0),
            (1, -101.0),
        ] {
            let options = ScanOptions {
                holder_selection: Some(HolderSelection {
                    holder: Holder::V800Film35mm,
                    frame_format: None,
                    frame,
                    overage_percent,
                }),
                ..Default::default()
            };
            assert!(matches!(
                options.plan(&ScanSettings::default(), &caps),
                Err(Error::Invalid(_))
            ));
        }
    }

    #[test]
    fn format_metadata_records_resolved_preset_and_its_geometry() {
        let model = crate::capabilities::V800_FAMILY;
        for (holder, frame_format, expected) in [
            (Holder::V800Film35mm, None, "35mm"),
            (
                Holder::V800Film35mm,
                Some(FrameFormat::Film35mmHalf),
                "35mm-half",
            ),
            (Holder::V800MediumFormat, None, "6x6"),
            (
                Holder::V800MediumFormat,
                Some(FrameFormat::Film6x45),
                "6x4.5",
            ),
        ] {
            let selection = HolderSelection {
                holder,
                frame_format,
                frame: 1,
                overage_percent: -10.0,
            };
            let metadata = selection.metadata(&model).unwrap();
            assert_eq!(metadata["frame_format"], expected);
            assert_eq!(
                metadata["nominal_rect_mm"],
                serde_json::to_value(
                    model
                        .holder_frame_with_format(holder, frame_format, 1, 0.0)
                        .unwrap()
                )
                .unwrap()
            );
            assert_eq!(
                metadata["rect_mm"],
                serde_json::to_value(
                    model
                        .holder_frame_with_format(holder, frame_format, 1, -10.0)
                        .unwrap()
                )
                .unwrap()
            );
        }
    }

    #[test]
    fn negative_holder_overage_is_valid_and_recorded_as_a_centered_crop() {
        let caps = capabilities();
        let model = caps.scanner_model().unwrap();
        let selection = HolderSelection {
            holder: Holder::V800Film35mm,
            frame_format: None,
            frame: 1,
            overage_percent: -10.0,
        };
        let settings = ScanSettings {
            source: model.holder(selection.holder).unwrap().source,
            rect_mm: model
                .holder_frame(selection.holder, selection.frame, -10.0)
                .unwrap(),
            ..Default::default()
        };
        let options = ScanOptions {
            holder_selection: Some(selection),
            ..Default::default()
        };
        assert!(options.plan(&settings, &caps).is_ok());
        let metadata = selection.metadata(model).unwrap();
        assert_eq!(metadata["overage_percent"], -10.0);
        assert_eq!(
            metadata["rect_mm"],
            serde_json::to_value(settings.rect_mm).unwrap()
        );
        assert_eq!(
            metadata["nominal_rect_mm"],
            serde_json::json!([121.5, 16.5, 24.0, 36.0])
        );
        assert!(
            metadata["overage_semantics"]
                .as_str()
                .unwrap()
                .contains("negative crops")
        );
    }

    #[test]
    fn banding_is_host_only_and_preflights_visible_export_and_geometry() {
        let caps = capabilities();
        let settings = ScanSettings::default();
        let mut options = ScanOptions {
            banding: Some(banding::BandingOptions::default()),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(options.plan(&settings, &caps).unwrap()).unwrap(),
            serde_json::to_value(ScanOptions::default().plan(&settings, &caps).unwrap()).unwrap()
        );
        options.export_tiff = false;
        assert!(options.plan(&settings, &caps).is_err());
        options.export_tiff = true;
        options.infrared_only = true;
        assert!(options.plan(&settings, &caps).is_err());
        options.infrared_only = false;
        options.banding.as_mut().unwrap().strength = f64::NAN;
        assert!(options.plan(&settings, &caps).is_err());
        options.banding.as_mut().unwrap().strength = 0.8;
        options.banding.as_mut().unwrap().detection_roi = Some([0, 10_000, 0, 32]);
        assert!(options.plan(&settings, &caps).is_err());
        options.banding.as_mut().unwrap().detection_roi = Some([0, 64, 0, 32]);
        assert!(options.plan(&settings, &caps).is_ok());
        options.banding.as_mut().unwrap().detection_roi = None;
        let tiny = ScanSettings {
            rect_mm: [0., 0., 1., 0.1],
            ..settings
        };
        assert!(options.plan(&tiny, &caps).is_err());
    }
}
