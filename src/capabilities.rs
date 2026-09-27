// SPDX-License-Identifier: MIT
//! Epson model policy. Add a profile here before enabling another scanner.
//!
//! Device-advertised dimensions and DPI remain in [`Capabilities`]; this registry
//! describes the commands and modes this implementation supports. The V800/V850
//! family profile is based on the locally tested GT-X980/B8 identity, Epson's
//! optical-path documentation, and the protocol research credited in docs/sources.md.
use crate::{Error, Result, error::unsupported, protocol::Capabilities};
use serde::{Deserialize, Serialize};

/// Visible-light acquisition mode, separate from an optional infrared pass.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "kebab-case")]
pub enum ScanMode {
    /// Three interleaved red, green, and blue channels.
    #[default]
    Rgb,
    /// One grayscale channel acquired directly from the scanner.
    #[cfg_attr(feature = "cli", value(alias = "mono"))]
    Gray,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Reflective originals on the glass.
    Flatbed,
    /// Film placed in holders.
    #[default]
    #[cfg_attr(feature = "cli", value(alias = "film-holder"))]
    Transparency,
    /// Film on the glass, using the film area guide.
    #[cfg_attr(
        feature = "cli",
        value(name = "transparency-8x10", alias = "film-area-guide")
    )]
    #[serde(rename = "transparency-8x10")]
    Transparency8x10,
}

/// Film-holder layouts explicitly registered for a scanner model.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
pub enum Holder {
    /// Approximate three-strip, eighteen-frame layout for the V800 family.
    #[serde(rename = "v800-35mm")]
    #[cfg_attr(feature = "cli", value(name = "v800-35mm"))]
    V800Film35mm,
    /// Measured single-sheet opening in the V800-family 4 × 5 inch holder.
    #[serde(rename = "v800-4x5")]
    #[cfg_attr(feature = "cli", value(name = "v800-4x5"))]
    V800Film4x5,
    /// Measured continuous opening in the V800-family medium-format holder.
    #[serde(rename = "v800-medium-format")]
    #[cfg_attr(feature = "cli", value(name = "v800-medium-format"))]
    V800MediumFormat,
    /// Twelve mounted slides, numbered left to right and top to bottom.
    #[serde(rename = "v800-slides")]
    #[cfg_attr(feature = "cli", value(name = "v800-slides"))]
    V800Slides,
}

/// Nominal exposure sizes; camera gates, spacing, and film placement vary.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
pub enum FrameFormat {
    #[serde(rename = "35mm")]
    #[cfg_attr(feature = "cli", value(name = "35mm"))]
    Film35mm,
    #[serde(rename = "35mm-half")]
    #[cfg_attr(feature = "cli", value(name = "35mm-half"))]
    Film35mmHalf,
    #[serde(rename = "6x4.5")]
    #[cfg_attr(feature = "cli", value(name = "6x4.5"))]
    Film6x45,
    #[serde(rename = "6x6")]
    #[cfg_attr(feature = "cli", value(name = "6x6"))]
    Film6x6,
    #[serde(rename = "6x7")]
    #[cfg_attr(feature = "cli", value(name = "6x7"))]
    Film6x7,
    #[serde(rename = "6x8")]
    #[cfg_attr(feature = "cli", value(name = "6x8"))]
    Film6x8,
    #[serde(rename = "6x9")]
    #[cfg_attr(feature = "cli", value(name = "6x9"))]
    Film6x9,
    #[serde(rename = "6x12")]
    #[cfg_attr(feature = "cli", value(name = "6x12"))]
    Film6x12,
    #[serde(rename = "6x17")]
    #[cfg_attr(feature = "cli", value(name = "6x17"))]
    Film6x17,
}

/// Starter crop positions for a film format within its physical holder.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HolderFormatLayout {
    pub format: FrameFormat,
    pub name: &'static str,
    pub frames_mm: &'static [[f64; 4]],
    pub strip_groups: &'static [&'static [u32]],
}

/// Nominal frame positions in source millimetres, in device-order preview
/// orientation. Array order defines the one-based frame numbers.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct HolderLayout {
    pub default_film_type: Option<&'static str>,
    pub holder: Holder,
    pub name: &'static str,
    pub source: Source,
    pub frames_mm: &'static [[f64; 4]],
    /// Continuous film strips, each listing its one-based frame numbers.
    /// An empty list or a frame absent from every group means separate captures.
    pub strip_groups: &'static [&'static [u32]],
    /// Default exposure format; fixed sheet holders have no format selector.
    pub default_format: Option<FrameFormat>,
    pub formats: &'static [HolderFormatLayout],
    /// Physical continuous openings, before preview padding. Their order matches
    /// `strip_groups`; they do not shrink when a shorter format is selected.
    pub strip_rects_mm: &'static [[f64; 4]],
}

impl HolderLayout {
    /// Resolve compatible exposure presets while retaining the physical holder.
    pub fn for_format(&self, format: Option<FrameFormat>) -> Result<Self> {
        let Some(format) = format.or(self.default_format) else {
            return Ok(*self);
        };
        let preset = self
            .formats
            .iter()
            .find(|preset| preset.format == format)
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "Frame format {format:?} is not supported by {}",
                    self.name
                ))
            })?;
        Ok(Self {
            frames_mm: preset.frames_mm,
            strip_groups: preset.strip_groups,
            ..*self
        })
    }

    /// Return the one-based continuous-strip identifier for a registered frame.
    /// Reject ambiguous or invalid registry entries rather than guessing a group.
    pub fn strip_for_frame(&self, frame: u32) -> Result<Option<u32>> {
        if frame == 0 || frame as usize > self.frames_mm.len() {
            return Err(Error::Invalid(format!(
                "Frame must be in 1..={} for {}",
                self.frames_mm.len(),
                self.name
            )));
        }
        let mut seen = vec![false; self.frames_mm.len()];
        let mut selected = None;
        for (index, group) in self.strip_groups.iter().enumerate() {
            for &member in *group {
                let occupied = member
                    .checked_sub(1)
                    .and_then(|position| seen.get_mut(position as usize))
                    .ok_or_else(|| {
                        Error::Invalid("Invalid registered holder strip frame".into())
                    })?;
                if *occupied {
                    return Err(Error::Invalid(
                        "A registered holder frame belongs to more than one strip entry".into(),
                    ));
                }
                *occupied = true;
                if member == frame {
                    selected =
                        Some(u32::try_from(index + 1).map_err(|_| {
                            Error::Invalid("Holder strip identifier overflow".into())
                        })?);
                }
            }
        }
        Ok(selected)
    }

    /// Resolve a one-based frame number and change its total width and height
    /// by `overage_percent`, centered on the nominal frame. Positive percentages
    /// expand and negative percentages crop; values must be finite and greater
    /// than -100. Source-area and minimum-pixel validation remain the scan plan's
    /// responsibility; this never clamps a requested rectangle.
    pub fn frame_rect(&self, frame: u32, overage_percent: f64) -> Result<[f64; 4]> {
        if !overage_percent.is_finite() || overage_percent <= -100.0 {
            return Err(Error::Invalid(
                "Frame overage must be finite and greater than -100 percent".into(),
            ));
        }
        let &[x, y, width, height] = frame
            .checked_sub(1)
            .and_then(|index| self.frames_mm.get(index as usize))
            .ok_or_else(|| {
                Error::Invalid(format!(
                    "Frame must be in 1..={} for {}",
                    self.frames_mm.len(),
                    self.name
                ))
            })?;
        if ![x, y, width, height].iter().all(|value| value.is_finite())
            || x < 0.0
            || y < 0.0
            || width <= 0.0
            || height <= 0.0
        {
            return Err(Error::Invalid("Invalid registered holder rectangle".into()));
        }
        let scale = 1.0 + overage_percent / 100.0;
        let adjusted_width = width * scale;
        let adjusted_height = height * scale;
        let adjusted = [
            x + (width - adjusted_width) / 2.0,
            y + (height - adjusted_height) / 2.0,
            adjusted_width,
            adjusted_height,
        ];
        if !adjusted.iter().all(|value| value.is_finite())
            || !(adjusted[0] + adjusted[2]).is_finite()
            || !(adjusted[1] + adjusted[3]).is_finite()
            || adjusted_width <= 0.0
            || adjusted_height <= 0.0
        {
            return Err(Error::Invalid(
                "Adjusted holder rectangle needs finite coordinates and positive dimensions".into(),
            ));
        }
        Ok(adjusted)
    }
}

/// Requested source/focus and manufacturer ratings are not lens readback or
/// measured resolving power.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct OpticsProfile {
    pub mount: &'static str,
    pub intended_lens: &'static str,
    pub manufacturer_optical_dpi: u32,
    pub focus_command_position: u8,
    pub physical_lens_verified: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct SourceCapabilities {
    pub source: Source,
    pub optics: OpticsProfile,
    pub option: u8,
    pub infrared_option: Option<u8>,
    /// Supported requested output range, distinct from nominal optical DPI.
    pub min_dpi: u32,
    pub max_dpi: u32,
    /// Manufacturer's carriage-axis hardware sampling limit, excluding interpolation.
    pub max_y_dpi: u32,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ModeCapabilities {
    pub wire_mode: u8,
    pub channels: u8,
    pub depths: &'static [u8],
    pub experimental: bool,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TransferQuirks {
    /// Width is truncated to this multiple after nearest-pixel conversion.
    pub width_alignment: u32,
    /// Maximum rows requested in one acknowledged image block.
    pub lines_per_block: u8,
    /// Target data-plus-status size. At least one complete row is requested,
    /// even when a very wide row exceeds this target.
    pub target_block_bytes: u32,
    /// A zero-length FS G 0x92 reply may initiate device preparation. Permit
    /// one more start only after FS F confirms warmup and then healthy readiness.
    pub allow_start_warmup_recovery: bool,
}

impl TransferQuirks {
    /// Derive the transfer line count from packed row size without changing
    /// image geometry or samples. The trailing block status uses one byte.
    pub fn block_lines(&self, width: u32, channels: u8, depth: u8) -> Result<u8> {
        if width == 0
            || ![1, 3].contains(&channels)
            || ![8, 16].contains(&depth)
            || self.lines_per_block == 0
            || self.target_block_bytes <= 1
        {
            return Err(Error::Invalid(
                "Invalid image block sizing policy or shape".into(),
            ));
        }
        let row_bytes = u64::from(width) * u64::from(channels) * u64::from(depth / 8);
        let fitting_rows = u64::from(self.target_block_bytes - 1) / row_bytes;
        Ok(fitting_rows.clamp(1, u64::from(self.lines_per_block)) as u8)
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct ScannerModel {
    pub name: &'static str,
    pub support_status: &'static str,
    pub vid: u16,
    pub pid: u16,
    pub identity_names: &'static [&'static str],
    pub command_level: &'static str,
    pub sources: &'static [SourceCapabilities],
    pub holders: &'static [HolderLayout],
    pub rgb_mode: ModeCapabilities,
    pub gray_mode: Option<ModeCapabilities>,
    pub infrared_mode: Option<ModeCapabilities>,
    pub transfer: TransferQuirks,
    pub preview_dpi: u32,
    pub preview_depth: u8,
    pub max_ready_wait_secs: u64,
}

impl ScannerModel {
    pub fn holder(&self, holder: Holder) -> Result<&HolderLayout> {
        self.holders
            .iter()
            .find(|layout| layout.holder == holder)
            .ok_or_else(|| {
                unsupported(
                    "holder layout",
                    format!("{holder:?} is not registered for {}", self.name),
                )
            })
    }

    pub fn holder_frame(
        &self,
        holder: Holder,
        frame: u32,
        overage_percent: f64,
    ) -> Result<[f64; 4]> {
        self.holder(holder)?.frame_rect(frame, overage_percent)
    }

    pub fn holder_frame_with_format(
        &self,
        holder: Holder,
        frame_format: Option<FrameFormat>,
        frame: u32,
        overage_percent: f64,
    ) -> Result<[f64; 4]> {
        self.holder(holder)?
            .for_format(frame_format)?
            .frame_rect(frame, overage_percent)
    }

    pub fn source(&self, source: Source) -> Result<&SourceCapabilities> {
        self.sources
            .iter()
            .find(|entry| entry.source == source)
            .ok_or_else(|| {
                unsupported(
                    "source",
                    format!("{source:?} is not implemented for {}", self.name),
                )
            })
    }

    pub fn validate_identity(&self, caps: &Capabilities) -> Result<()> {
        if !self.identity_names.contains(&caps.model.as_str())
            || caps.command_level != self.command_level
        {
            return Err(unsupported(
                "scanner identity",
                format!(
                    "{} / {} does not match the {} / {} profile",
                    caps.model, caps.command_level, self.name, self.command_level
                ),
            ));
        }
        Ok(())
    }

    pub fn mode(&self, infrared: bool) -> Result<&ModeCapabilities> {
        self.mode_for(ScanMode::Rgb, infrared)
    }

    pub fn mode_for(&self, mode: ScanMode, infrared: bool) -> Result<&ModeCapabilities> {
        if infrared {
            self.infrared_mode.as_ref().ok_or_else(|| {
                unsupported("infrared", format!("not implemented for {}", self.name))
            })
        } else {
            match mode {
                ScanMode::Rgb => Ok(&self.rgb_mode),
                ScanMode::Gray => self.gray_mode.as_ref().ok_or_else(|| {
                    unsupported("grayscale", format!("not implemented for {}", self.name))
                }),
            }
        }
    }
}

const V800_SOURCES: &[SourceCapabilities] = &[
    SourceCapabilities {
        source: Source::Flatbed,
        optics: OpticsProfile {
            mount: "reflective-on-glass",
            intended_lens: "glass-4800",
            manufacturer_optical_dpi: 4800,
            focus_command_position: 64,
            physical_lens_verified: false,
        },
        option: 0,
        infrared_option: None,
        min_dpi: 25,
        max_dpi: 12800,
        max_y_dpi: 9600,
    },
    SourceCapabilities {
        source: Source::Transparency,
        optics: OpticsProfile {
            mount: "film-holder",
            intended_lens: "film-6400",
            manufacturer_optical_dpi: 6400,
            focus_command_position: 89,
            physical_lens_verified: false,
        },
        option: 1,
        infrared_option: Some(3),
        min_dpi: 25,
        max_dpi: 12800,
        max_y_dpi: 9600,
    },
    SourceCapabilities {
        source: Source::Transparency8x10,
        optics: OpticsProfile {
            mount: "film-area-guide",
            intended_lens: "glass-4800",
            manufacturer_optical_dpi: 4800,
            focus_command_position: 64,
            physical_lens_verified: false,
        },
        option: 5,
        infrared_option: None,
        min_dpi: 25,
        max_dpi: 12800,
        max_y_dpi: 9600,
    },
];

// Approximate positions measured from the installed empty 3-strip holder in
// the 2026-09-25 device-order 100-DPI full-source preview (584 x 970 pixels),
// retained locally as captures/holder-calibration-20260925/full-holder_1.tiff.
// Horizontal opening positions are rounded to 0.1 mm. The nominal 24 x 36 mm
// frames start at y=16.5 mm with 38 mm pitch, aligned against the loaded-film
// previews in captures/negpy-holder-debug-20260925. Individual strip placement
// still varies. Strips are numbered left to right, frames top to bottom:
// left 1-6, middle 7-12, right 13-18. No image rotation is assumed.
const V800_35MM_FRAMES: &[[f64; 4]] = &[
    [2.3, 16.5, 24.0, 36.0],
    [2.3, 54.5, 24.0, 36.0],
    [2.3, 92.5, 24.0, 36.0],
    [2.3, 130.5, 24.0, 36.0],
    [2.3, 168.5, 24.0, 36.0],
    [2.3, 206.5, 24.0, 36.0],
    [62.1, 16.5, 24.0, 36.0],
    [62.1, 54.5, 24.0, 36.0],
    [62.1, 92.5, 24.0, 36.0],
    [62.1, 130.5, 24.0, 36.0],
    [62.1, 168.5, 24.0, 36.0],
    [62.1, 206.5, 24.0, 36.0],
    [121.5, 16.5, 24.0, 36.0],
    [121.5, 54.5, 24.0, 36.0],
    [121.5, 92.5, 24.0, 36.0],
    [121.5, 130.5, 24.0, 36.0],
    [121.5, 168.5, 24.0, 36.0],
    [121.5, 206.5, 24.0, 36.0],
];

// Visible opening measured with a loaded negative on 2026-09-26, from the
// 300-DPI, 1768 x 2910 device-order transparency preview retained locally as
// captures/holder-4x5-20260926/overview-300_1.tiff. The mask covers the edges
// of the nominal 101.6 x 127 mm sheet; placement and skew still vary slightly.
const V800_4X5_FRAMES: &[[f64; 4]] = &[[27.0, 48.0, 94.0, 119.0]];

const V800_35MM_GROUPS: &[&[u32]] = &[
    &[1, 2, 3, 4, 5, 6],
    &[7, 8, 9, 10, 11, 12],
    &[13, 14, 15, 16, 17, 18],
];

// Half-frame exposures run along the strip on half the calibrated full-frame
// pitch. These are starter crops; real camera gates and loading offsets vary.
const fn half_frames() -> [[f64; 4]; 36] {
    let mut frames = [[0.0; 4]; 36];
    let mut index = 0;
    while index < frames.len() {
        frames[index] = [
            [2.3, 62.1, 121.5][index / 12],
            16.5 + (index % 12) as f64 * 19.0,
            24.0,
            18.0,
        ];
        index += 1;
    }
    frames
}

const V800_35MM_HALF_FRAMES: &[[f64; 4]] = &half_frames();
const V800_35MM_HALF_GROUPS: &[&[u32]] = &[
    &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
    &[13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24],
    &[25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36],
];
const V800_35MM_FORMATS: &[HolderFormatLayout] = &[
    HolderFormatLayout {
        format: FrameFormat::Film35mm,
        name: "35 mm (24 x 36 mm)",
        frames_mm: V800_35MM_FRAMES,
        strip_groups: V800_35MM_GROUPS,
    },
    HolderFormatLayout {
        format: FrameFormat::Film35mmHalf,
        name: "35 mm half-frame (24 x 18 mm)",
        frames_mm: V800_35MM_HALF_FRAMES,
        strip_groups: V800_35MM_HALF_GROUPS,
    },
];

// Measured empty on 2026-09-26 in the full-source 300-DPI RGB8 preview:
// captures/holder-medium-20260926/overview-300_1.tiff (1768 x 2910).
// No film was available: this registers the aperture, not exposure positions.
const V800_MEDIUM_OPENING: [f64; 4] = [45.1, 33.5, 57.6, 200.0];

const fn medium_frames<const N: usize>(height: f64) -> [[f64; 4]; N] {
    let [x, y, width, length] = V800_MEDIUM_OPENING;
    let occupied = N as f64 * height + (N - 1) as f64 * 2.0;
    let mut frames = [[0.0; 4]; N];
    let mut index = 0;
    while index < N {
        frames[index] = [
            x + (width - 56.0) / 2.0,
            y + (length - occupied) / 2.0 + index as f64 * (height + 2.0),
            56.0,
            height,
        ];
        index += 1;
    }
    frames
}

const V800_MEDIUM_FORMATS: &[HolderFormatLayout] = &[
    HolderFormatLayout {
        format: FrameFormat::Film6x45,
        name: "6 x 4.5 (56 x 41.5 mm)",
        frames_mm: &medium_frames::<4>(41.5),
        strip_groups: &[&[1, 2, 3, 4]],
    },
    HolderFormatLayout {
        format: FrameFormat::Film6x6,
        name: "6 x 6 (56 x 56 mm)",
        frames_mm: &medium_frames::<3>(56.0),
        strip_groups: &[&[1, 2, 3]],
    },
    HolderFormatLayout {
        format: FrameFormat::Film6x7,
        name: "6 x 7 (56 x 69 mm)",
        frames_mm: &medium_frames::<2>(69.0),
        strip_groups: &[&[1, 2]],
    },
    HolderFormatLayout {
        format: FrameFormat::Film6x8,
        name: "6 x 8 (56 x 76 mm)",
        frames_mm: &medium_frames::<2>(76.0),
        strip_groups: &[&[1, 2]],
    },
    HolderFormatLayout {
        format: FrameFormat::Film6x9,
        name: "6 x 9 (56 x 84 mm)",
        frames_mm: &medium_frames::<2>(84.0),
        strip_groups: &[&[1, 2]],
    },
    HolderFormatLayout {
        format: FrameFormat::Film6x12,
        name: "6 x 12 (56 x 112 mm)",
        frames_mm: &medium_frames::<1>(112.0),
        strip_groups: &[&[1]],
    },
    HolderFormatLayout {
        format: FrameFormat::Film6x17,
        name: "6 x 17 (56 x 168 mm)",
        frames_mm: &medium_frames::<1>(168.0),
        strip_groups: &[&[1]],
    },
];

const V800_HOLDERS: &[HolderLayout] = &[
    HolderLayout {
        holder: Holder::V800Slides,
        default_film_type: Some("positive"),
        name: "Epson V800/V850 35 mm Slide Holder",
        source: Source::Transparency,
        frames_mm: &[
            [2.7, 33.1, 36.0, 36.0],
            [56.1, 33.1, 36.0, 36.0],
            [109.3, 33.1, 36.0, 36.0],
            [2.7, 91.1, 36.0, 36.0],
            [56.0, 91.1, 36.0, 36.0],
            [109.2, 91.1, 36.0, 36.0],
            [2.5, 151.1, 36.0, 36.0],
            [55.8, 151.1, 36.0, 36.0],
            [109.1, 151.1, 36.0, 36.0],
            [2.4, 209.1, 36.0, 36.0],
            [55.7, 209.1, 36.0, 36.0],
            [109.0, 209.1, 36.0, 36.0],
        ],
        strip_groups: &[],
        default_format: None,
        formats: &[],
        strip_rects_mm: &[],
    },
    HolderLayout {
        holder: Holder::V800Film35mm,
        default_film_type: None,
        name: "Epson V800/V850 35 mm Film Strip Holder",
        source: Source::Transparency,
        frames_mm: V800_35MM_FRAMES,
        strip_groups: V800_35MM_GROUPS,
        default_format: Some(FrameFormat::Film35mm),
        formats: V800_35MM_FORMATS,
        strip_rects_mm: &[
            [2.3, 16.5, 24.0, 227.0],
            [62.1, 16.5, 24.0, 227.0],
            [121.5, 16.5, 24.0, 227.0],
        ],
    },
    HolderLayout {
        holder: Holder::V800Film4x5,
        default_film_type: None,
        name: "Epson V800/V850 4 x 5 inch Film Holder",
        source: Source::Transparency,
        frames_mm: V800_4X5_FRAMES,
        strip_groups: &[],
        default_format: None,
        formats: &[],
        strip_rects_mm: &[],
    },
    HolderLayout {
        holder: Holder::V800MediumFormat,
        default_film_type: None,
        name: "Epson V800/V850 Medium Format Film Holder",
        source: Source::Transparency,
        frames_mm: V800_MEDIUM_FORMATS[1].frames_mm,
        strip_groups: V800_MEDIUM_FORMATS[1].strip_groups,
        default_format: Some(FrameFormat::Film6x6),
        formats: V800_MEDIUM_FORMATS,
        strip_rects_mm: &[V800_MEDIUM_OPENING],
    },
];

/// First supported family. Sharing a USB ID is not a guarantee of a tested model;
/// an exact identity and command-level match is required when opening a session.
pub const V800_FAMILY: ScannerModel = ScannerModel {
    name: "Epson Perfection V800/V850 / GT-X980",
    support_status: "GT-X980 hardware tested; V800/V850 family",
    vid: 0x04b8,
    pid: 0x0151,
    identity_names: &["GT-X980"],
    command_level: "B8",
    sources: V800_SOURCES,
    holders: V800_HOLDERS,
    rgb_mode: ModeCapabilities {
        wire_mode: 0x13,
        channels: 3,
        depths: &[8, 16],
        experimental: false,
    },
    // ESC/I monochrome mode uses the selected visible-light source. It does
    // not enable the separate infrared option or challenge sequence.
    gray_mode: Some(ModeCapabilities {
        wire_mode: 0,
        channels: 1,
        depths: &[8, 16],
        experimental: false,
    }),
    // Only 8-bit candidate transfers have succeeded. Repeated/paired starts,
    // spectral identity, and RGB/IR registration remain unverified.
    infrared_mode: Some(ModeCapabilities {
        wire_mode: 0,
        channels: 1,
        depths: &[8],
        experimental: true,
    }),
    transfer: TransferQuirks {
        width_alignment: 8,
        lines_per_block: 32,
        target_block_bytes: 64 * 1024,
        allow_start_warmup_recovery: true,
    },
    preview_dpi: 100,
    preview_depth: 8,
    max_ready_wait_secs: 180,
};

const V700_SOURCES: &[SourceCapabilities] = &[
    SourceCapabilities {
        min_dpi: 50,
        ..V800_SOURCES[0]
    },
    SourceCapabilities {
        min_dpi: 50,
        infrared_option: None,
        ..V800_SOURCES[1]
    },
    SourceCapabilities {
        min_dpi: 50,
        ..V800_SOURCES[2]
    },
];

/// SANE identifies this family as GT-X900. No local identity capture is available.
pub const V700_FAMILY: ScannerModel = ScannerModel {
    name: "Epson Perfection V700/V750 / GT-X900 (provisional)",
    support_status: "provisional; no local hardware verification",
    pid: 0x012c,
    identity_names: &["GT-X900"],
    sources: V700_SOURCES,
    holders: &[],
    infrared_mode: None,
    rgb_mode: ModeCapabilities {
        experimental: true,
        ..V800_FAMILY.rgb_mode
    },
    gray_mode: Some(ModeCapabilities {
        wire_mode: 0,
        channels: 1,
        depths: &[8, 16],
        experimental: true,
    }),
    transfer: TransferQuirks {
        allow_start_warmup_recovery: false,
        ..V800_FAMILY.transfer
    },
    ..V800_FAMILY
};

pub static MODELS: &[ScannerModel] = &[V800_FAMILY, V700_FAMILY];

/// Known USB devices whose interpreter protocol is not implemented by epscan.
pub const INTERPRETER_SCANNERS: &[(u16, &str)] = &[
    (0x0130, "Epson Perfection V500 Photo"),
    (0x013b, "Epson Perfection V550 Photo"),
    (0x013a, "Epson Perfection V600 Photo"),
];

pub fn interpreter_scanner(vid: u16, pid: u16) -> Option<&'static str> {
    (vid == 0x04b8).then_some(())?;
    INTERPRETER_SCANNERS
        .iter()
        .find(|(id, _)| *id == pid)
        .map(|(_, name)| *name)
}

pub fn discovery_name(vid: u16, pid: u16) -> Option<&'static str> {
    scanner_model(vid, pid)
        .map(|model| model.name)
        .or_else(|| interpreter_scanner(vid, pid))
}

pub fn require_native_protocol(vid: u16, pid: u16) -> Result<()> {
    if let Some(name) = interpreter_scanner(vid, pid) {
        return Err(unsupported(
            "scanner protocol",
            format!(
                "{name} ({vid:04x}:{pid:04x}) requires the epkowa interpreter; its protocol is not implemented in epscan. USB recognition is not scanning support."
            ),
        ));
    }
    Ok(())
}

/// USB discovery uses only registered IDs; identity is checked after connecting.
pub fn scanner_model(vid: u16, pid: u16) -> Option<&'static ScannerModel> {
    MODELS
        .iter()
        .find(|model| model.vid == vid && model.pid == pid)
}

pub fn model_by_identity(identity: &str) -> Option<&'static ScannerModel> {
    MODELS
        .iter()
        .find(|model| model.identity_names.contains(&identity))
}

impl Source {
    /// V800-family convenience method retained from the reference library.
    /// For a connected scanner use [`Capabilities::optics`] instead.
    pub fn optics(self) -> OpticsProfile {
        V800_FAMILY
            .source(self)
            .expect("V800 supports every Source variant")
            .optics
    }
}

/// Convert millimetres once, using the model's transfer alignment. Integer
/// extents must remain addressable even before device-specific area validation.
pub(crate) fn pixel_rectangle(rect_mm: [f64; 4], dpi: u32, alignment: u32) -> Result<[u32; 4]> {
    let [x, y, w, h] = rect_mm;
    if !rect_mm.iter().all(|v| v.is_finite()) || x < 0. || y < 0. || w <= 0. || h <= 0. || dpi == 0
    {
        return Err(Error::Invalid(
            "Need finite nonnegative x/y, positive width/height and DPI".into(),
        ));
    }
    if alignment == 0 {
        return Err(Error::Invalid(
            "Model width alignment must be positive".into(),
        ));
    }
    let values = rect_mm.map(|value| (value * f64::from(dpi) / 25.4 + 0.5).floor());
    if values.iter().any(|value| *value > f64::from(u32::MAX)) {
        return Err(Error::Invalid("Pixel geometry overflow".into()));
    }
    let mut pixels = values.map(|value| value as u32);
    pixels[2] -= pixels[2] % alignment;
    if pixels[2] < alignment || pixels[3] == 0 {
        return Err(Error::Invalid(format!(
            "Region needs at least {alignment} columns and one row at this DPI"
        )));
    }
    if pixels[0].checked_add(pixels[2]).is_none() || pixels[1].checked_add(pixels[3]).is_none() {
        return Err(Error::Invalid("Pixel rectangle endpoint overflow".into()));
    }
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grayscale_support_is_independent_of_infrared() {
        let model = ScannerModel {
            infrared_mode: None,
            ..V800_FAMILY
        };
        let gray = model.mode_for(ScanMode::Gray, false).unwrap();
        assert_eq!(gray.channels, 1);
        assert_eq!(gray.depths, &[8, 16]);
        assert!(model.mode_for(ScanMode::Gray, true).is_err());
        let model = ScannerModel {
            gray_mode: None,
            ..V800_FAMILY
        };
        assert!(model.mode_for(ScanMode::Gray, false).is_err());
        assert_eq!(model.mode(false).unwrap().channels, 3);
        assert_eq!(model.mode_for(ScanMode::Gray, true).unwrap().channels, 1);
    }

    #[cfg(feature = "cli")]
    #[test]
    fn grayscale_cli_name_accepts_mono_alias() {
        use clap::ValueEnum;
        assert_eq!(ScanMode::from_str("gray", false).unwrap(), ScanMode::Gray);
        assert_eq!(ScanMode::from_str("mono", false).unwrap(), ScanMode::Gray);
        assert_eq!(
            ScanMode::Gray.to_possible_value().unwrap().get_name(),
            "gray"
        );
    }

    #[test]
    fn registry_entries_are_consistent_and_unique() {
        for (index, model) in MODELS.iter().enumerate() {
            assert!(!model.identity_names.is_empty());
            assert!(!model.sources.is_empty());
            assert!(model.transfer.width_alignment > 0);
            assert!(model.transfer.lines_per_block > 0);
            for other in &MODELS[index + 1..] {
                assert_ne!((model.vid, model.pid), (other.vid, other.pid));
                assert!(
                    model
                        .identity_names
                        .iter()
                        .all(|name| !other.identity_names.contains(name))
                );
            }
            for (source_index, source) in model.sources.iter().enumerate() {
                assert!(source.min_dpi > 0 && source.min_dpi <= source.max_dpi);
                assert!((source.min_dpi..=source.max_dpi).contains(&model.preview_dpi));
                assert!(
                    model.sources[source_index + 1..]
                        .iter()
                        .all(|other| other.source != source.source)
                );
                assert!(source.infrared_option.is_none() || model.infrared_mode.is_some());
            }
            assert!(model.rgb_mode.depths.contains(&model.preview_depth));
            for (holder_index, holder) in model.holders.iter().enumerate() {
                assert!(model.source(holder.source).is_ok());
                assert!(!holder.frames_mm.is_empty());
                for frame in 1..=holder.frames_mm.len() as u32 {
                    assert!(holder.strip_for_frame(frame).is_ok());
                }
                assert!(
                    model.holders[holder_index + 1..]
                        .iter()
                        .all(|other| other.holder != holder.holder)
                );
            }
            assert_eq!(
                scanner_model(model.vid, model.pid).unwrap().name,
                model.name
            );
        }
        assert!(scanner_model(0x04b8, 0xffff).is_none());
        assert!(scanner_model(0xffff, 0x0151).is_none());
        assert!(model_by_identity("unregistered").is_none());
    }

    #[test]
    fn source_policy_distinguishes_holder_and_glass_optics() {
        let holder = V800_FAMILY.source(Source::Transparency).unwrap();
        let guide = V800_FAMILY.source(Source::Transparency8x10).unwrap();
        assert_eq!((holder.option, holder.infrared_option), (1, Some(3)));
        assert_eq!((guide.option, guide.infrared_option), (5, None));
        assert_eq!(holder.optics.manufacturer_optical_dpi, 6400);
        assert_eq!(guide.optics.manufacturer_optical_dpi, 4800);
        assert_eq!(holder.optics.focus_command_position, 89);
        assert_eq!(guide.optics.focus_command_position, 64);
        assert!(!holder.optics.physical_lens_verified);
        assert!(holder.max_dpi > holder.optics.manufacturer_optical_dpi);
    }

    const EXAMPLE_LAYOUT: HolderLayout = HolderLayout {
        default_film_type: None,
        holder: Holder::V800Film35mm,
        name: "geometry test fixture",
        source: Source::Transparency,
        frames_mm: &[[10.0, 20.0, 24.0, 36.0], [45.0, 58.0, 24.0, 36.0]],
        strip_groups: &[],
        default_format: None,
        formats: &[],
        strip_rects_mm: &[],
    };

    #[test]
    fn holder_strips_are_explicit_and_unlisted_frames_stay_separate() {
        let holder = V800_FAMILY.holder(Holder::V800Film35mm).unwrap();
        for frame in 1..=18 {
            assert_eq!(
                holder.strip_for_frame(frame).unwrap(),
                Some((frame - 1) / 6 + 1)
            );
        }
        assert!(holder.strip_for_frame(0).is_err());
        assert!(holder.strip_for_frame(19).is_err());
        assert_eq!(EXAMPLE_LAYOUT.strip_for_frame(1).unwrap(), None);
        let partial = HolderLayout {
            strip_groups: &[&[1]],
            ..EXAMPLE_LAYOUT
        };
        assert_eq!(partial.strip_for_frame(1).unwrap(), Some(1));
        assert_eq!(partial.strip_for_frame(2).unwrap(), None);
        const INVALID_GROUPS: &[&[&[u32]]] = &[&[&[0]], &[&[3]], &[&[1], &[1]]];
        for &groups in INVALID_GROUPS {
            let malformed = HolderLayout {
                strip_groups: groups,
                ..EXAMPLE_LAYOUT
            };
            assert!(malformed.strip_for_frame(1).is_err());
        }
    }

    #[test]
    fn holder_rectangles_use_one_based_frames_and_centered_total_expansion() {
        assert_eq!(
            EXAMPLE_LAYOUT.frame_rect(2, 0.0).unwrap(),
            [45.0, 58.0, 24.0, 36.0]
        );
        let nominal = EXAMPLE_LAYOUT.frame_rect(1, 0.0).unwrap();
        let expanded = EXAMPLE_LAYOUT.frame_rect(1, 5.0).unwrap();
        for (actual, expected) in expanded.into_iter().zip([9.4, 19.1, 25.2, 37.8]) {
            assert!((actual - expected).abs() < 1e-10);
        }
        for axis in 0..2 {
            assert!(
                (expanded[axis] + expanded[axis + 2] / 2.0
                    - nominal[axis]
                    - nominal[axis + 2] / 2.0)
                    .abs()
                    < 1e-10
            );
        }
    }

    #[test]
    fn holder_frame_validation_rejects_invalid_indices_and_overage() {
        for frame in [0, 3, u32::MAX] {
            assert!(EXAMPLE_LAYOUT.frame_rect(frame, 0.0).is_err());
        }
        for overage in [-100.0, -101.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(EXAMPLE_LAYOUT.frame_rect(1, overage).is_err());
        }
        let unregistered = ScannerModel {
            holders: &[],
            ..V800_FAMILY
        };
        assert!(
            unregistered
                .holder_frame(Holder::V800Film35mm, 1, 0.0)
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(Holder::V800Film35mm).unwrap(),
            "v800-35mm"
        );
        assert_eq!(
            serde_json::from_str::<Holder>("\"v800-35mm\"").unwrap(),
            Holder::V800Film35mm
        );
        assert!(serde_json::from_str::<Holder>("\"120\"").is_err());
        assert!(serde_json::from_str::<Holder>("\"35mm\"").is_err());
    }

    #[test]
    fn holder_expansion_never_clamps_and_rejects_arithmetic_overflow() {
        let expanded = EXAMPLE_LAYOUT.frame_rect(1, 200.0).unwrap();
        assert_eq!(expanded, [-14.0, -16.0, 72.0, 108.0]);
        let overflowing = HolderLayout {
            frames_mm: &[[0.0, 0.0, f64::MAX, 36.0]],
            ..EXAMPLE_LAYOUT
        };
        assert!(overflowing.frame_rect(1, 100.0).is_err());
        const UNDERFLOW_FRAMES: [[f64; 4]; 1] = [[0.0, 0.0, f64::from_bits(1), 36.0]];
        let underflowing = HolderLayout {
            frames_mm: &UNDERFLOW_FRAMES,
            ..EXAMPLE_LAYOUT
        };
        assert!(underflowing.frame_rect(1, -75.0).is_err());
    }

    #[test]
    fn negative_overage_crops_the_frame_without_moving_its_center() {
        let layout = V800_FAMILY.holder(Holder::V800Film35mm).unwrap();
        let nominal = layout.frame_rect(1, 0.0).unwrap();
        let cropped = layout.frame_rect(1, -10.0).unwrap();
        for (actual, expected) in cropped.into_iter().zip([3.5, 18.3, 21.6, 32.4]) {
            assert!((actual - expected).abs() < 1e-10);
        }
        for axis in 0..2 {
            assert!(
                (cropped[axis] + cropped[axis + 2] / 2.0 - nominal[axis] - nominal[axis + 2] / 2.0)
                    .abs()
                    < 1e-10
            );
        }
        let tiny = layout.frame_rect(1, -99.999999).unwrap();
        assert!(tiny[2] > 0.0 && tiny[3] > 0.0);
        assert!(pixel_rectangle(tiny, 300, V800_FAMILY.transfer.width_alignment).is_err());
    }

    #[test]
    fn installed_35mm_layout_is_numbered_strip_by_strip() {
        let layout = V800_FAMILY.holder(Holder::V800Film35mm).unwrap();
        assert_eq!(layout.source, Source::Transparency);
        assert_eq!(layout.frames_mm.len(), 18);
        assert_eq!(layout.frame_rect(1, 0.0).unwrap(), [2.3, 16.5, 24.0, 36.0]);
        assert_eq!(layout.frame_rect(6, 0.0).unwrap(), [2.3, 206.5, 24.0, 36.0]);
        assert_eq!(layout.frame_rect(7, 0.0).unwrap(), [62.1, 16.5, 24.0, 36.0]);
        assert_eq!(
            layout.frame_rect(18, 0.0).unwrap(),
            [121.5, 206.5, 24.0, 36.0]
        );
        let (strips, remainder) = layout.frames_mm.as_chunks::<6>();
        assert!(remainder.is_empty());
        assert_eq!(strips.len(), 3);
        for strip in strips {
            for pair in strip.windows(2) {
                assert_eq!(pair[0][0], pair[1][0]);
                assert_eq!(pair[1][1] - pair[0][1], 38.0);
            }
        }
        assert!(strips.windows(2).all(|pair| pair[0][0][0] < pair[1][0][0]));
        assert!(layout.frame_rect(19, 0.0).is_err());
    }

    #[test]
    fn half_frame_layout_preserves_holder_and_doubles_strip_capacity() {
        let holder = V800_FAMILY.holder(Holder::V800Film35mm).unwrap();
        assert_eq!(holder.for_format(None).unwrap().frames_mm, V800_35MM_FRAMES);
        let half = holder.for_format(Some(FrameFormat::Film35mmHalf)).unwrap();
        assert_eq!(half.frames_mm.len(), 36);
        assert_eq!(half.strip_rects_mm, holder.strip_rects_mm);
        for frame in 1..=36 {
            let rect = half.frame_rect(frame, 0.0).unwrap();
            assert_eq!(rect[2..], [24.0, 18.0]);
            assert_eq!(
                half.strip_for_frame(frame).unwrap(),
                Some((frame - 1) / 12 + 1)
            );
            if (frame - 1) % 12 != 0 {
                assert_eq!(rect[1] - half.frame_rect(frame - 1, 0.0).unwrap()[1], 19.0);
            }
        }
        assert_eq!(
            half.frame_rect(36, 0.0).unwrap(),
            [121.5, 225.5, 24.0, 18.0]
        );
        assert!(half.frame_rect(37, 0.0).is_err());
        assert!(holder.for_format(Some(FrameFormat::Film6x6)).is_err());
    }

    #[test]
    fn medium_formats_fit_measured_opening_and_keep_whole_strip_geometry() {
        let holder = V800_FAMILY.holder(Holder::V800MediumFormat).unwrap();
        assert_eq!(holder.default_format, Some(FrameFormat::Film6x6));
        assert_eq!(holder.for_format(None).unwrap().frames_mm.len(), 3);
        assert_eq!(holder.strip_rects_mm, &[[45.1, 33.5, 57.6, 200.0]]);
        let [x, y, width, height] = holder.strip_rects_mm[0];
        for (preset, count) in holder.formats.iter().zip([4, 3, 2, 2, 2, 1, 1]) {
            let layout = holder.for_format(Some(preset.format)).unwrap();
            assert_eq!(layout.frames_mm.len(), count);
            assert_eq!(layout.strip_rects_mm, holder.strip_rects_mm);
            assert!(preset.name.is_ascii());
            let first = layout.frames_mm.first().unwrap();
            let last = layout.frames_mm.last().unwrap();
            assert!((first[1] - y - (y + height - last[1] - last[3])).abs() < 1e-10);
            for (index, rect) in layout.frames_mm.iter().enumerate() {
                assert!(rect[0] >= x && rect[0] + rect[2] <= x + width);
                assert!(rect[1] >= y && rect[1] + rect[3] <= y + height);
                assert_eq!(rect[2], 56.0);
                assert_eq!(layout.strip_for_frame(index as u32 + 1).unwrap(), Some(1));
                if index > 0 {
                    let previous = layout.frames_mm[index - 1];
                    assert!((rect[1] - previous[1] - previous[3] - 2.0).abs() < 1e-10);
                }
            }
            assert!(layout.frame_rect(count as u32 + 1, 0.0).is_err());
        }
        assert!(holder.for_format(Some(FrameFormat::Film35mm)).is_err());
        assert!(
            V800_FAMILY
                .holder(Holder::V800Film4x5)
                .unwrap()
                .for_format(Some(FrameFormat::Film6x6))
                .is_err()
        );
        let serialized = serde_json::to_value(holder).unwrap();
        assert_eq!(serialized["holder"], "v800-medium-format");
        assert_eq!(serialized["default_format"], "6x6");
        assert_eq!(serialized["formats"][0]["format"], "6x4.5");
        assert_eq!(serialized["formats"][0]["frames_mm"][0][3], 41.5);
        assert_eq!(
            serialized["strip_rects_mm"],
            serde_json::json!([[45.1, 33.5, 57.6, 200.0]])
        );
    }

    #[test]
    fn sheet_holder_is_one_independent_frame_and_serializes_for_adapters() {
        let holder = V800_FAMILY.holder(Holder::V800Film4x5).unwrap();
        assert_eq!(holder.source, Source::Transparency);
        assert_eq!(holder.frames_mm, &[[27.0, 48.0, 94.0, 119.0]]);
        assert_eq!(holder.strip_for_frame(1).unwrap(), None);
        assert!(holder.frame_rect(0, 0.0).is_err());
        assert!(holder.frame_rect(2, 0.0).is_err());
        assert_eq!(
            serde_json::from_str::<Holder>("\"v800-4x5\"").unwrap(),
            Holder::V800Film4x5
        );
        let serialized = serde_json::to_value(holder).unwrap();
        assert_eq!(serialized["holder"], "v800-4x5");
        assert_eq!(
            serialized["frames_mm"][0],
            serde_json::json!([27.0, 48.0, 94.0, 119.0])
        );
        assert_eq!(serialized["strip_groups"], serde_json::json!([]));
    }

    #[test]
    fn every_35mm_frame_with_valid_crop_or_padding_fits_the_source() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/v800-identity.json")).unwrap();
        let hex = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect::<Vec<_>>();
        let caps = Capabilities::parse(&bytes).unwrap();
        let model = caps.scanner_model().unwrap();
        let layout = model.holder(Holder::V800Film35mm).unwrap();
        let area = caps.area_mm(layout.source);
        for overage in [-10.0, 0.0, 10.0] {
            for frame in 1..=18 {
                let rectangle = model
                    .holder_frame(Holder::V800Film35mm, frame, overage)
                    .unwrap();
                assert!(rectangle[0] >= 0.0 && rectangle[1] >= 0.0);
                assert!(rectangle[0] + rectangle[2] <= area[0]);
                assert!(rectangle[1] + rectangle[3] <= area[1]);
                let settings = crate::ScanSettings {
                    rect_mm: rectangle,
                    source: layout.source,
                    ..Default::default()
                };
                settings.validate(&caps, false).unwrap();
            }
        }
        let settings = crate::ScanSettings {
            rect_mm: model.holder_frame(Holder::V800Film35mm, 18, 30.0).unwrap(),
            source: layout.source,
            ..Default::default()
        };
        assert!(settings.validate(&caps, false).is_err());
    }
}
