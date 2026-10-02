// SPDX-License-Identifier: MIT
use clap::{Args, Parser, Subcommand, ValueEnum};
use epscan::scan::{HolderSelection, banding::BandingOptions};
use epscan::{
    Backend, Capabilities, Error, FrameFormat, Gamma, Holder, Result, ScanMode, Source,
    error::unsupported,
};
use std::{collections::HashSet, path::PathBuf, str::FromStr};

#[derive(Parser)]
#[command(
    version,
    about = "Direct USB acquisition for supported Epson ESC/I scanners"
)]
pub struct Cli {
    #[command(subcommand)]
    pub action: Action,
    /// Log verbosity
    #[arg(long, global = true, default_value = "info", value_parser = ["trace", "debug", "info", "warn", "error", "off"])]
    pub log: String,
    /// USB transport; auto keeps the installed Epson driver on Windows
    #[arg(long, global = true, value_enum, default_value = "auto")]
    pub backend: Backend,
    /// Per-response/block timeout in seconds
    #[arg(long, global = true, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=214))]
    pub io_timeout: u64,
}

#[derive(Subcommand)]
pub enum Action {
    /// List connected supported scanners without issuing scan commands
    #[command(alias = "discover")]
    List,
    /// Acquire rectangles or selected holder frames, with optional infrared passes
    #[command(
        alias = "capture",
        after_help = "Examples:\n  epscan scan --source film-holder --rect \"10,30,10,10\" --dpi 300\n  epscan scan --source film-holder --rect \"10,30,10,10;40,60,10,10\"\n  epscan scan --holder v800-35mm --frame \"1-5,8-12\" --overage 5\n  epscan scan --holder v800-4x5 --frame 1 --dpi 1200"
    )]
    Scan(Scan),
    /// Read identity, advertised capabilities and status without moving the scanner
    #[command(alias = "diagnostics")]
    Dump(Dump),
    /// Acquire low-resolution, 8-bit previews of rectangles or selected frames
    #[command(
        after_help = "Examples:\n  epscan preview --source film-holder --rect \"0,0,149,246\"\n  epscan preview --source film-holder --rect \"10,30,10,10;40,60,10,10\"\n  epscan preview --holder v800-35mm --frame \"1-5,8-12\"\n  epscan preview --holder v800-4x5 --frame 1"
    )]
    Preview(Preview),
}

#[derive(Args)]
pub struct Dump {
    /// Exact device location from list; optional if only one scanner is found
    pub device: Option<String>,
    /// Also save diagnostics to a new JSON file
    #[arg(long)]
    pub output: Option<PathBuf>,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum Film {
    Positive,
    Negative,
    Kodachrome,
    Mono,
}

impl Film {
    pub fn name(self) -> &'static str {
        match self {
            Self::Positive => "positive",
            Self::Negative => "negative",
            Self::Kodachrome => "kodachrome",
            Self::Mono => "mono",
        }
    }
}

/// Explicit regions in request order. Repeated rectangles remain separate scans.
#[derive(Clone, Debug, PartialEq)]
pub struct RectangleSelection {
    rectangles: Vec<[f64; 4]>,
}

impl FromStr for RectangleSelection {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let rectangles = text
            .split(';')
            .enumerate()
            .map(|(index, entry)| {
                let mut fields = entry.split(',');
                let mut rectangle = [0.0; 4];
                let field_count_error = || {
                    format!(
                        "rectangle {} needs four comma-separated values: x,y,width,height",
                        index + 1
                    )
                };
                for coordinate in &mut rectangle {
                    let field = fields.next().ok_or_else(field_count_error)?;
                    *coordinate = field.trim().parse::<f64>().map_err(|_| {
                        format!(
                            "rectangle {} needs numeric x,y,width,height values",
                            index + 1
                        )
                    })?;
                }
                if fields.next().is_some() {
                    return Err(field_count_error());
                }
                validate_rectangle(rectangle)
                    .map_err(|message| format!("rectangle {}: {message}", index + 1))?;
                Ok(rectangle)
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        Ok(Self { rectangles })
    }
}

impl RectangleSelection {
    fn validate(&self) -> Result<()> {
        if self.rectangles.is_empty() {
            return Err(Error::Invalid("--rect needs at least one rectangle".into()));
        }
        for (index, &rectangle) in self.rectangles.iter().enumerate() {
            validate_rectangle(rectangle)
                .map_err(|message| Error::Invalid(format!("rectangle {}: {message}", index + 1)))?;
        }
        Ok(())
    }
}

fn validate_rectangle([x, y, width, height]: [f64; 4]) -> std::result::Result<(), String> {
    if ![x, y, width, height].iter().all(|value| value.is_finite())
        || x < 0.0
        || y < 0.0
        || width <= 0.0
        || height <= 0.0
    {
        return Err("--rect needs finite nonnegative x/y and positive width/height".into());
    }
    if !(x + width).is_finite() || !(y + height).is_finite() {
        return Err("--rect endpoints must be finite".into());
    }
    Ok(())
}

/// Ordered inclusive ranges. Parsing never expands a user-supplied range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameSelection {
    ranges: Vec<(u32, u32)>,
}

impl FromStr for FrameSelection {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        fn number(text: &str) -> std::result::Result<u32, String> {
            let text = text.trim();
            if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(
                    "frames must be positive integers or ascending ranges, such as 1-5,8-12".into(),
                );
            }
            text.parse::<u32>()
                .ok()
                .filter(|number| *number > 0)
                .ok_or_else(|| "frame numbers must be in 1..=4294967295".into())
        }
        let ranges = text
            .split(',')
            .map(|part| {
                let (first, last) = if let Some((first, last)) = part.trim().split_once('-') {
                    (number(first)?, number(last)?)
                } else {
                    let single = number(part)?;
                    (single, single)
                };
                if first > last {
                    return Err("frame ranges must be ascending, such as 1-5".into());
                }
                Ok((first, last))
            })
            .collect::<std::result::Result<Vec<_>, String>>()?;
        Ok(Self { ranges })
    }
}

impl FrameSelection {
    /// Check every range against the actual holder before expanding any range.
    /// Duplicates are omitted, preserving each frame's first requested position.
    fn resolve(&self, frame_count: usize) -> Result<Vec<u32>> {
        let maximum = u32::try_from(frame_count).map_err(|_| {
            Error::Invalid("Holder frame count exceeds the frame-number range".into())
        })?;
        if self
            .ranges
            .iter()
            .any(|&(first, last)| first == 0 || first > last || last > maximum)
        {
            return Err(Error::Invalid(format!(
                "Selected frames must be in 1..={maximum} for this holder"
            )));
        }
        let mut frames = Vec::new();
        let mut seen = HashSet::new();
        for &(first, last) in &self.ranges {
            for frame in first..=last {
                if seen.insert(frame) {
                    frames.push(frame);
                }
            }
        }
        Ok(frames)
    }
}

#[derive(Clone, Debug)]
pub struct ResolvedArea {
    pub source: Source,
    pub rect_mm: [f64; 4],
    pub holder_selection: Option<HolderSelection>,
}

/// Half-open pixel region in the unrotated output image: x0,x1,y0,y1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandingRoi(pub [u32; 4]);

impl FromStr for BandingRoi {
    type Err = String;

    fn from_str(text: &str) -> std::result::Result<Self, Self::Err> {
        let values = text
            .split(',')
            .map(|field| field.trim().parse::<u32>())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| "banding ROI needs four nonnegative pixel coordinates: X0,X1,Y0,Y1")?;
        let [x0, x1, y0, y1]: [u32; 4] = values
            .try_into()
            .map_err(|_| "banding ROI needs four pixel coordinates: X0,X1,Y0,Y1")?;
        if x0 >= x1 || y0 >= y1 {
            return Err("banding ROI requires X0 < X1 and Y0 < Y1".into());
        }
        Ok(Self([x0, x1, y0, y1]))
    }
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("area").args(["rect", "holder"]).required(true).multiple(false)))]
pub struct Capture {
    /// Exact device location from list; optional if only one scanner is found
    pub device: Option<String>,
    /// Output prefix; a free numbered suffix is added without replacing captures
    #[arg(long, default_value = "scan")]
    pub basename: PathBuf,
    /// Quoted x,y,width,height rectangles in mm, separated by semicolons
    #[arg(long, requires = "source", conflicts_with_all = ["holder", "frame", "frame_format", "overage"], value_name = "RECTANGLES", allow_hyphen_values = true)]
    pub rect: Option<RectangleSelection>,
    /// Select an approximate holder layout instead of an explicit rectangle
    #[arg(long, value_enum, requires = "frame", conflicts_with = "rect")]
    pub holder: Option<Holder>,
    /// Nominal exposure preset (defaults: 35mm or 6x6); camera gates and spacing vary
    #[arg(long, value_enum, requires = "holder", conflicts_with = "rect")]
    pub frame_format: Option<FrameFormat>,
    /// One-based frames or inclusive ranges, e.g. "1-5,8-12"; duplicates omitted
    #[arg(
        long,
        value_name = "FRAMES",
        requires = "holder",
        conflicts_with = "rect"
    )]
    pub frame: Option<FrameSelection>,
    /// Centered frame-size change in percent: positive expands, negative crops (>-100; default: 0)
    #[arg(long, value_parser = parse_overage, requires = "holder", conflicts_with = "rect", allow_hyphen_values = true)]
    pub overage: Option<f64>,
    /// Select placement and optical path; required with --rect, implied by --holder
    #[arg(long, value_enum)]
    pub source: Option<Source>,
    /// Scanner color mode: RGB or single-channel grayscale (alias: mono)
    #[arg(long, value_enum, default_value = "rgb")]
    pub mode: ScanMode,
    /// Film metadata only; samples stay un-inverted and unprofiled
    #[arg(long, value_enum)]
    pub film: Option<Film>,
    /// Measure Tenengrad and variance of Laplacian for each frame or area
    #[arg(long)]
    pub measure_sharpness: bool,
    /// Retain raw payload and metadata without exporting TIFF
    #[arg(long)]
    pub raw_only: bool,
    /// Reduce vertical bands in the exported visible-image TIFF
    #[arg(long, conflicts_with = "raw_only")]
    pub reduce_banding: bool,
    /// Fraction of the estimated band correction to apply
    #[arg(long, default_value_t = 1.0, value_parser = parse_fraction, requires = "reduce_banding")]
    pub banding_strength: f64,
    /// Full correction mask below this original brightness (fraction of full scale)
    #[arg(long, default_value_t = 0.1, value_parser = parse_fraction, requires = "reduce_banding")]
    pub banding_dark_full: f64,
    /// Fade correction to zero at this original brightness (fraction of full scale)
    #[arg(long, default_value_t = 0.6, value_parser = parse_fraction, requires = "reduce_banding")]
    pub banding_dark_off: f64,
    /// Detect bands within this half-open region of unrotated output pixels
    #[arg(long, value_name = "X0,X1,Y0,Y1", requires = "reduce_banding")]
    pub banding_roi: Option<BandingRoi>,
    /// Also save an uncorrected TIFF beside the corrected TIFF
    #[arg(long, requires = "reduce_banding")]
    pub save_raw_tiff: bool,
    /// Also save a PNG of the applied signed band correction
    #[arg(long, requires = "reduce_banding")]
    pub save_band_signal: bool,
    /// Keep raw .bin files and the protocol trace after successful TIFF export;
    /// with --raw-only, also keep the trace. Failures always retain diagnostics.
    #[arg(long)]
    pub keep_intermediates: bool,
    /// Print the complete scan result as JSON instead of the completion summary
    #[arg(long)]
    pub json: bool,
    /// Image-transfer deadline in seconds per pass, checked at block boundaries
    #[arg(long, default_value_t = 3600, value_parser = clap::value_parser!(u64).range(1..))]
    pub scan_timeout: u64,
}

impl Capture {
    pub fn film_name(&self) -> &'static str {
        self.film.map(Film::name).unwrap_or_else(|| {
            self.holder
                .and_then(|holder| {
                    epscan::capabilities::V800_FAMILY
                        .holder(holder)
                        .ok()
                        .and_then(|layout| layout.default_film_type)
                })
                .unwrap_or("negative")
        })
    }
    /// Validate option relationships and numbers before opening the device.
    pub fn validate_area(&self) -> Result<()> {
        self.banding_options()?;
        if let Some(holder) = self.holder {
            if self.rect.is_some() || self.frame.is_none() {
                return Err(Error::Invalid(
                    "--holder requires a positive --frame and conflicts with --rect".into(),
                ));
            }
            // All current holder IDs belong to this family. Reject incompatible
            // presets and capacities before opening USB; resolve again against
            // the connected model before any acquisition.
            let layout = epscan::capabilities::V800_FAMILY
                .holder(holder)?
                .for_format(self.frame_format)?;
            self.frame
                .as_ref()
                .expect("validated frames")
                .resolve(layout.frames_mm.len())?;
            if self
                .overage
                .is_some_and(|value| !value.is_finite() || value <= -100.0)
            {
                return Err(Error::Invalid(
                    "--overage must be finite and greater than -100".into(),
                ));
            }
        } else {
            if self.frame.is_some() || self.frame_format.is_some() || self.overage.is_some() {
                return Err(Error::Invalid(
                    "--frame, --frame-format and --overage require --holder".into(),
                ));
            }
            if self.source.is_none() {
                return Err(Error::Invalid(
                    "--rect requires an explicit --source".into(),
                ));
            }
            self.rect
                .as_ref()
                .ok_or_else(|| Error::Invalid("--rect needs at least one rectangle".into()))?
                .validate()?;
        }
        Ok(())
    }

    pub fn banding_options(&self) -> Result<Option<BandingOptions>> {
        if !self.reduce_banding {
            if self.save_raw_tiff || self.save_band_signal || self.banding_roi.is_some() {
                return Err(Error::Invalid(
                    "--save-raw-tiff, --save-band-signal and --banding-roi require --reduce-banding"
                        .into(),
                ));
            }
            return Ok(None);
        }
        if self.raw_only {
            return Err(Error::Invalid(
                "--reduce-banding requires TIFF export and conflicts with --raw-only".into(),
            ));
        }
        let options = BandingOptions {
            strength: self.banding_strength,
            dark_full: self.banding_dark_full,
            dark_off: self.banding_dark_off,
            detection_roi: self.banding_roi.map(|roi| roi.0),
            save_raw: self.save_raw_tiff,
            save_signal: self.save_band_signal,
            ..BandingOptions::default()
        };
        options.validate()?;
        Ok(Some(options))
    }

    /// Resolve every requested frame or rectangle after the scanner model is known.
    /// This returns no partial result when any later area is invalid.
    pub fn resolve_areas(&self, caps: &Capabilities) -> Result<Vec<ResolvedArea>> {
        self.validate_area()?;
        if let Some(holder) = self.holder {
            let model = caps.scanner_model()?;
            let layout = model.holder(holder)?.for_format(self.frame_format)?;
            let source = layout.source;
            if self.source.is_some_and(|explicit| explicit != source) {
                return Err(Error::Invalid(format!(
                    "Selected holder requires source {source:?}; remove the incompatible --source"
                )));
            }
            let frames = self
                .frame
                .as_ref()
                .expect("validated frames")
                .resolve(layout.frames_mm.len())?;
            let overage_percent = self.overage.unwrap_or(0.0);
            frames
                .into_iter()
                .map(|frame| {
                    let rect_mm = layout.frame_rect(frame, overage_percent)?;
                    validate_source_rectangle(caps, source, rect_mm)?;
                    Ok(ResolvedArea {
                        source,
                        rect_mm,
                        holder_selection: Some(HolderSelection {
                            holder,
                            frame_format: self.frame_format,
                            frame,
                            overage_percent,
                        }),
                    })
                })
                .collect()
        } else {
            let source = self.source.expect("validated source");
            self.rect
                .as_ref()
                .expect("validated rectangles")
                .rectangles
                .iter()
                .enumerate()
                .map(|(index, &rect_mm)| {
                    validate_source_rectangle(caps, source, rect_mm).map_err(
                        |error| match error {
                            Error::Invalid(message) => {
                                Error::Invalid(format!("rectangle {}: {message}", index + 1))
                            }
                            other => other,
                        },
                    )?;
                    Ok(ResolvedArea {
                        source,
                        rect_mm,
                        holder_selection: None,
                    })
                })
                .collect()
        }
    }
}

fn validate_source_rectangle(caps: &Capabilities, source: Source, rect: [f64; 4]) -> Result<()> {
    caps.scanner_model()?.source(source)?;
    let [max_x, max_y] = caps.area_mm(source);
    let [x, y, width, height] = rect;
    if !rect.iter().all(|value| value.is_finite())
        || !max_x.is_finite()
        || !max_y.is_finite()
        || max_x <= 0.0
        || max_y <= 0.0
        || x < 0.0
        || y < 0.0
        || width <= 0.0
        || height <= 0.0
        || x + width > max_x + 1e-6
        || y + height > max_y + 1e-6
    {
        return Err(Error::Invalid(format!(
            "Selected rectangle exceeds source area {max_x} x {max_y} mm"
        )));
    }
    Ok(())
}

#[derive(Args)]
#[command(group(clap::ArgGroup::new("infrared").args(["ir", "ir_only"]).multiple(false)))]
#[command(group(clap::ArgGroup::new("multipass").args(["ir", "thumbnail"]).multiple(true)))]
pub struct Scan {
    #[command(flatten)]
    pub capture: Capture,
    /// Requested output resolution (not measured optical resolution)
    #[arg(long, default_value_t = 300, value_parser = clap::value_parser!(u32).range(1..))]
    pub dpi: u32,
    /// Average N carriage-axis samples into each output row (limited to hardware Y DPI)
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=16))]
    pub y_oversampling: u32,
    /// Bits per channel for RGB or grayscale
    #[arg(long, default_value_t = 16, value_parser = parse_depth, conflicts_with = "ir_only")]
    pub depth: u8,
    /// Tone-table selection; identity does not establish calibrated linearity
    #[arg(
        long,
        value_enum,
        default_value = "device-default",
        conflicts_with = "ir_only"
    )]
    pub gamma: Gamma,
    /// Add an experimental, separate 8-bit infrared pass after the main image
    #[arg(long)]
    pub ir: bool,
    /// Acquire only an experimental 8-bit infrared pass
    #[arg(long, conflicts_with_all = ["thumbnail", "measure_sharpness", "mode", "reduce_banding"])]
    pub ir_only: bool,
    /// Infrared tone-table selection (default: device-default)
    #[arg(long, value_enum, requires = "infrared")]
    pub ir_gamma: Option<Gamma>,
    /// Save an additional low-resolution preview in the selected color mode
    #[arg(long)]
    pub thumbnail: bool,
    /// Host delay between passes; this does not guarantee device readiness
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u64).range(0..=120), requires = "multipass")]
    pub settle_seconds: u64,
}

impl Scan {
    /// Validate argument relationships before connecting to the scanner.
    pub fn validate_options(&self) -> Result<()> {
        self.capture.validate_area()?;
        if self.ir_only && self.capture.reduce_banding {
            return Err(Error::Invalid(
                "--reduce-banding requires an RGB or grayscale pass and conflicts with --ir-only"
                    .into(),
            ));
        }
        if self.ir_only && self.capture.measure_sharpness {
            return Err(Error::Invalid(
                "--measure-sharpness requires an RGB or grayscale pass and conflicts with --ir-only".into(),
            ));
        }
        if (self.ir || self.ir_only) && matches!(self.capture.film, Some(Film::Mono)) {
            return Err(unsupported(
                "IR for mono film",
                "silver-bearing film is unsuitable",
            ));
        }
        Ok(())
    }
}

fn parse_depth(value: &str) -> std::result::Result<u8, String> {
    match value {
        "8" => Ok(8),
        "16" => Ok(16),
        _ => Err("depth must be 8 or 16".into()),
    }
}

fn parse_overage(value: &str) -> std::result::Result<f64, String> {
    let percent: f64 = value
        .parse()
        .map_err(|_| "overage must be a percentage".to_string())?;
    if !percent.is_finite() || percent <= -100.0 {
        return Err("overage must be finite and greater than -100".into());
    }
    Ok(percent)
}

fn parse_fraction(value: &str) -> std::result::Result<f64, String> {
    let fraction: f64 = value
        .parse()
        .map_err(|_| "value must be a fraction between 0 and 1".to_string())?;
    if !fraction.is_finite() || !(0.0..=1.0).contains(&fraction) {
        return Err("value must be finite and between 0 and 1".into());
    }
    Ok(fraction)
}

#[derive(Args)]
pub struct Preview {
    #[command(flatten)]
    pub capture: Capture,
    /// Preview tone-table selection
    #[arg(long, value_enum, default_value = "device-default")]
    pub gamma: Gamma,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_capture(command: &str, arguments: &[&str]) -> std::result::Result<Cli, clap::Error> {
        Cli::try_parse_from(
            [
                "epscan",
                command,
                "--source",
                "film-holder",
                "--rect",
                "0,0,10,10",
            ]
            .into_iter()
            .chain(arguments.iter().copied()),
        )
    }

    #[test]
    fn scan_deadline_defaults_to_one_hour_with_separate_block_timeout() {
        for command in ["scan", "preview"] {
            let cli = parse_capture(command, &[]).unwrap();
            assert_eq!(cli.io_timeout, 60);
            assert_eq!(capture_from(cli).scan_timeout, 3600);
            let custom = parse_capture(command, &["--scan-timeout", "1200"]).unwrap();
            assert_eq!(capture_from(custom).scan_timeout, 1200);
        }
    }

    #[test]
    fn carriage_sampling_has_explicit_factor_and_bounded_integer_parser() {
        let Action::Scan(defaults) = parse_capture("scan", &[]).unwrap().action else {
            panic!("expected scan")
        };
        assert_eq!(defaults.y_oversampling, 1);
        let Action::Scan(sampled) =
            parse_capture("scan", &["--dpi", "3200", "--y-oversampling", "3"])
                .unwrap()
                .action
        else {
            panic!("expected scan")
        };
        assert_eq!((sampled.dpi, sampled.y_oversampling), (3200, 3));
        for invalid in ["0", "17", "1.5", "-1"] {
            assert!(parse_capture("scan", &["--y-oversampling", invalid]).is_err());
        }
    }

    #[test]
    fn gamma_defaults_to_device_default_and_identity_remains_explicit() {
        for command in ["scan", "capture", "preview"] {
            for (arguments, expected) in [
                (vec![], Gamma::DeviceDefault),
                (vec!["--gamma", "identity-lut"], Gamma::IdentityLut),
                (vec!["--gamma", "device-default"], Gamma::DeviceDefault),
            ] {
                let gamma = match parse_capture(command, &arguments).unwrap().action {
                    Action::Scan(scan) => scan.gamma,
                    Action::Preview(preview) => preview.gamma,
                    _ => unreachable!(),
                };
                assert_eq!(gamma, expected, "{command}");
            }
        }
        for command in ["scan", "capture"] {
            assert!(parse_capture(command, &["--ir-only"]).is_ok());
            for gamma in ["device-default", "identity-lut"] {
                assert!(parse_capture(command, &["--ir-only", "--gamma", gamma]).is_err());
            }
        }
    }

    fn capture_from(cli: Cli) -> Capture {
        match cli.action {
            Action::Scan(scan) => scan.capture,
            Action::Preview(preview) => preview.capture,
            _ => unreachable!(),
        }
    }

    #[test]
    fn banding_is_opt_in_for_scan_and_preview_with_matching_defaults() {
        for command in ["scan", "capture", "preview"] {
            let plain = capture_from(parse_capture(command, &[]).unwrap());
            assert!(plain.banding_options().unwrap().is_none());
            let capture = capture_from(
                parse_capture(
                    command,
                    &["--reduce-banding", "--save-raw-tiff", "--save-band-signal"],
                )
                .unwrap(),
            );
            capture.validate_area().unwrap();
            let config = capture.banding_options().unwrap().unwrap();
            assert_eq!(config.strength, 1.0);
            assert_eq!(config.dark_full, 0.1);
            assert_eq!(config.dark_off, 0.6);
            assert!(config.save_raw && config.save_signal);
            assert!(config.detection_roi.is_none());
        }
    }

    #[test]
    fn banding_adjustments_and_pixel_roi_are_preserved() {
        let capture = capture_from(
            parse_capture(
                "scan",
                &[
                    "--reduce-banding",
                    "--banding-strength",
                    "0.75",
                    "--banding-dark-full",
                    "0.05",
                    "--banding-dark-off",
                    "0.55",
                    "--banding-roi",
                    "100,2000,300,1000",
                ],
            )
            .unwrap(),
        );
        let config = capture.banding_options().unwrap().unwrap();
        assert_eq!(config.strength, 0.75);
        assert_eq!(config.dark_full, 0.05);
        assert_eq!(config.dark_off, 0.55);
        assert_eq!(config.detection_roi, Some([100, 2000, 300, 1000]));
    }

    #[test]
    fn banding_invalid_or_unused_options_fail_before_scanner_io() {
        for command in ["scan", "preview"] {
            for arguments in [
                vec!["--save-raw-tiff"],
                vec!["--save-band-signal"],
                vec!["--banding-strength", "0.8"],
                vec!["--banding-dark-full", "0.1"],
                vec!["--banding-dark-off", "0.6"],
                vec!["--banding-roi", "0,100,0,100"],
                vec!["--reduce-banding", "--raw-only"],
            ] {
                assert!(
                    parse_capture(command, &arguments).is_err(),
                    "{command} {arguments:?}"
                );
            }
            for flag in [
                "--banding-strength",
                "--banding-dark-full",
                "--banding-dark-off",
            ] {
                for value in ["NaN", "inf", "-inf", "-0.1", "1.01"] {
                    assert!(parse_capture(command, &["--reduce-banding", flag, value]).is_err());
                }
            }
            for value in [
                "",
                "0,1,0",
                "0,1,0,1,2",
                "-1,10,0,10",
                "1,1,0,10",
                "0,10,20,10",
            ] {
                assert!(
                    parse_capture(command, &["--reduce-banding", "--banding-roi", value]).is_err()
                );
            }
            let mut capture = capture_from(parse_capture(command, &["--reduce-banding"]).unwrap());
            capture.banding_dark_full = capture.banding_dark_off;
            assert!(capture.validate_area().is_err());
            capture.banding_dark_full = 0.1;
            capture.banding_strength = f64::NAN;
            assert!(capture.validate_area().is_err());
        }
        assert!(parse_capture("scan", &["--reduce-banding", "--ir-only"]).is_err());
        assert!(parse_capture("scan", &["--reduce-banding", "--ir"]).is_ok());
    }

    #[test]
    fn grayscale_modes_apply_to_scan_and_preview_without_changing_film_defaults() {
        for command in ["scan", "capture", "preview"] {
            let defaults = capture_from(parse_capture(command, &[]).unwrap());
            assert_eq!(defaults.mode, ScanMode::Rgb);
            for spelling in ["gray", "mono"] {
                let capture = capture_from(
                    parse_capture(command, &["--mode", spelling, "--measure-sharpness"]).unwrap(),
                );
                assert_eq!(capture.mode, ScanMode::Gray);
                assert_eq!(capture.film_name(), "negative");
                assert!(capture.measure_sharpness);
            }
            let film = capture_from(parse_capture(command, &["--film", "mono"]).unwrap());
            assert_eq!(film.mode, ScanMode::Rgb);
            assert!(parse_capture(command, &["--mode", "invalid"]).is_err());
        }
        for mode in ["rgb", "gray", "mono"] {
            assert!(parse_capture("scan", &["--mode", mode, "--ir-only"]).is_err());
        }
        assert!(parse_capture("scan", &["--ir-only"]).is_ok());
        assert!(parse_capture("scan", &["--mode", "gray", "--ir"]).is_ok());
    }

    #[test]
    fn mounted_slide_film_default_allows_explicit_override() {
        for command in ["scan", "preview"] {
            for (extra, expected) in [(None, "positive"), (Some("negative"), "negative")] {
                let mut args = vec![
                    "epscan",
                    command,
                    "--holder",
                    "v800-slides",
                    "--frame",
                    "1-12",
                ];
                if let Some(film) = extra {
                    args.extend(["--film", film]);
                }
                let capture = capture_from(Cli::try_parse_from(args).unwrap());
                assert_eq!(capture.film_name(), expected);
                assert_eq!(capture.resolve_areas(&capabilities()).unwrap().len(), 12);
            }
        }
    }

    fn capabilities() -> Capabilities {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/v800-identity.json"))
                .unwrap();
        let hex = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        Capabilities::parse(&bytes).unwrap()
    }

    #[test]
    fn holder_scan_and_preview_imply_source_and_preserve_selection() {
        let caps = capabilities();
        let model = caps.scanner_model().unwrap();
        for (holder_arg, holder) in [
            ("v800-35mm", Holder::V800Film35mm),
            ("v800-4x5", Holder::V800Film4x5),
        ] {
            for command in ["scan", "capture", "preview"] {
                for (extra, expected_overage) in [
                    (vec![], 0.0),
                    (vec!["--overage", "5"], 5.0),
                    (vec!["--overage", "-10"], -10.0),
                ] {
                    let capture = capture_from(
                        Cli::try_parse_from(
                            ["epscan", command, "--holder", holder_arg, "--frame", "1"]
                                .into_iter()
                                .chain(extra),
                        )
                        .unwrap(),
                    );
                    assert!(capture.source.is_none());
                    assert!(capture.validate_area().is_ok());
                    let areas = capture.resolve_areas(&caps).unwrap();
                    assert_eq!(areas.len(), 1);
                    assert_eq!(areas[0].source, Source::Transparency);
                    assert_eq!(
                        areas[0].rect_mm,
                        model.holder_frame(holder, 1, expected_overage).unwrap()
                    );
                    let selection = areas[0].holder_selection.unwrap();
                    assert_eq!(selection.holder, holder);
                    assert_eq!(selection.frame, 1);
                    assert_eq!(selection.overage_percent, expected_overage);
                }
            }
        }
    }

    #[test]
    fn holder_dependencies_conflicts_and_bad_numbers_fail_during_parsing() {
        for command in ["scan", "preview"] {
            for arguments in [
                vec!["--holder", "v800-35mm"],
                vec!["--frame", "1"],
                vec!["--overage", "5"],
                vec!["--holder", "v800-35mm", "--frame", "0"],
                vec!["--holder", "unknown", "--frame", "1"],
                vec!["--holder", "35mm", "--frame", "1"],
                vec!["--holder", "v800-35mm", "--frame", "1", "--overage", "-100"],
                vec!["--holder", "v800-35mm", "--frame", "1", "--overage", "-101"],
                vec!["--holder", "v800-35mm", "--frame", "1", "--overage", "NaN"],
                vec!["--holder", "v800-35mm", "--frame", "1", "--overage", "inf"],
                vec!["--holder", "v800-35mm", "--frame", "1", "--overage", "-inf"],
            ] {
                assert!(
                    Cli::try_parse_from(["epscan", command].into_iter().chain(arguments.clone()))
                        .is_err(),
                    "{command} {arguments:?}"
                );
            }
            for arguments in [
                vec!["--holder", "v800-35mm", "--frame", "1"],
                vec!["--frame", "1"],
                vec!["--overage", "0"],
            ] {
                assert!(
                    parse_capture(command, &arguments).is_err(),
                    "{command} {arguments:?}"
                );
            }
        }
    }

    #[test]
    fn holder_resolves_only_compatible_sources_and_existing_frames() {
        let caps = capabilities();
        for (holder_arg, holder) in [
            ("v800-35mm", Holder::V800Film35mm),
            ("v800-4x5", Holder::V800Film4x5),
        ] {
            for command in ["scan", "preview"] {
                for (source, compatible) in [
                    ("film-holder", true),
                    ("transparency", true),
                    ("flatbed", false),
                    ("film-area-guide", false),
                ] {
                    let capture = capture_from(
                        Cli::try_parse_from([
                            "epscan", command, "--holder", holder_arg, "--frame", "1", "--source",
                            source,
                        ])
                        .unwrap(),
                    );
                    assert_eq!(
                        capture.resolve_areas(&caps).is_ok(),
                        compatible,
                        "{holder_arg} {source}"
                    );
                }
                let mut capture = capture_from(
                    Cli::try_parse_from([
                        "epscan", command, "--holder", holder_arg, "--frame", "1",
                    ])
                    .unwrap(),
                );
                let missing_frame = caps
                    .scanner_model()
                    .unwrap()
                    .holder(holder)
                    .unwrap()
                    .frames_mm
                    .len()
                    + 1;
                capture.frame = Some(missing_frame.to_string().parse().unwrap());
                assert!(capture.resolve_areas(&caps).is_err());
            }
        }
    }

    #[test]
    fn holder_formats_resolve_geometry_and_metadata_for_scan_and_preview() {
        let caps = capabilities();
        for command in ["scan", "preview"] {
            for (holder, format, frames, dimensions, expected) in [
                ("v800-35mm", "35mm-half", "1-36", [24.0, 18.0], 36),
                ("v800-medium-format", "6x4.5", "1-4", [56.0, 41.5], 4),
                ("v800-medium-format", "6x6", "1-3", [56.0, 56.0], 3),
                ("v800-medium-format", "6x7", "1-2", [56.0, 69.0], 2),
                ("v800-medium-format", "6x8", "1-2", [56.0, 76.0], 2),
                ("v800-medium-format", "6x9", "1-2", [56.0, 84.0], 2),
                ("v800-medium-format", "6x12", "1", [56.0, 112.0], 1),
                ("v800-medium-format", "6x17", "1", [56.0, 168.0], 1),
            ] {
                let capture = capture_from(
                    Cli::try_parse_from([
                        "epscan",
                        command,
                        "--holder",
                        holder,
                        "--frame-format",
                        format,
                        "--frame",
                        frames,
                    ])
                    .unwrap(),
                );
                capture.validate_area().unwrap();
                let areas = capture.resolve_areas(&caps).unwrap();
                assert_eq!(areas.len(), expected);
                for area in areas {
                    assert_eq!(area.rect_mm[2..], dimensions);
                    let selection = area.holder_selection.unwrap();
                    assert_eq!(
                        serde_json::to_value(selection.frame_format).unwrap(),
                        format
                    );
                    let settings = epscan::ScanSettings {
                        source: area.source,
                        rect_mm: area.rect_mm,
                        ..Default::default()
                    };
                    let options = epscan::ScanOptions {
                        holder_selection: Some(selection),
                        ..Default::default()
                    };
                    options.plan(&settings, &caps).unwrap();
                }
            }
        }
    }

    #[test]
    fn incompatible_formats_and_capacity_are_rejected_before_connection() {
        for command in ["scan", "preview"] {
            for (holder, format, frames) in [
                ("v800-35mm", "6x6", "1"),
                ("v800-4x5", "35mm", "1"),
                ("v800-medium-format", "35mm-half", "1"),
                ("v800-medium-format", "6x6", "4"),
                ("v800-medium-format", "6x9", "1-3"),
                ("v800-medium-format", "6x17", "2"),
                ("v800-35mm", "35mm-half", "37"),
            ] {
                let capture = capture_from(
                    Cli::try_parse_from([
                        "epscan",
                        command,
                        "--holder",
                        holder,
                        "--frame-format",
                        format,
                        "--frame",
                        frames,
                    ])
                    .unwrap(),
                );
                assert!(
                    capture.validate_area().is_err(),
                    "{holder} {format} {frames}"
                );
            }
            assert!(
                Cli::try_parse_from([
                    "epscan",
                    command,
                    "--source",
                    "film-holder",
                    "--rect",
                    "10,10,10,10",
                    "--frame-format",
                    "6x6"
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn explicit_rectangles_retain_selected_source_without_holder_metadata() {
        let caps = capabilities();
        for command in ["scan", "capture", "preview"] {
            let capture = capture_from(parse_capture(command, &[]).unwrap());
            let areas = capture.resolve_areas(&caps).unwrap();
            assert_eq!(areas.len(), 1);
            assert_eq!(areas[0].source, Source::Transparency);
            assert_eq!(areas[0].rect_mm, [0., 0., 10., 10.]);
            assert!(areas[0].holder_selection.is_none());
        }
    }

    #[test]
    fn rectangle_lists_preserve_order_duplicates_and_selected_source() {
        let caps = capabilities();
        let selection = " 40,60,10,10 ; 10,30,8.5,9.25 ; 40,60,10,10 ";
        let expected = [
            [40., 60., 10., 10.],
            [10., 30., 8.5, 9.25],
            [40., 60., 10., 10.],
        ];
        for command in ["scan", "capture", "preview"] {
            for (source, expected_source) in [
                ("film-holder", Source::Transparency),
                ("flatbed", Source::Flatbed),
                ("film-area-guide", Source::Transparency8x10),
            ] {
                let capture = capture_from(
                    Cli::try_parse_from([
                        "epscan", command, "--source", source, "--rect", selection,
                    ])
                    .unwrap(),
                );
                assert!(capture.validate_area().is_ok());
                let areas = capture.resolve_areas(&caps).unwrap();
                assert_eq!(areas.len(), 3);
                assert_eq!(
                    areas.iter().map(|area| area.rect_mm).collect::<Vec<_>>(),
                    expected
                );
                assert!(
                    areas
                        .iter()
                        .all(|area| area.source == expected_source
                            && area.holder_selection.is_none())
                );
            }
        }
    }

    #[test]
    fn malformed_rectangle_lists_and_old_positional_syntax_fail_during_parsing() {
        for command in ["scan", "capture", "preview"] {
            for selection in [
                "",
                " ",
                ";",
                ";0,0,10,10",
                "0,0,10,10;",
                "0,0,10,10;;1,1,10,10",
                "0,0,10",
                "0,0,10,10,20",
                "0,,10,10",
                "0,0,10,",
                "text,0,10,10",
                "NaN,0,10,10",
                "0,inf,10,10",
                "0,0,10,-inf",
                "-1,0,10,10",
                "0,-1,10,10",
                "0,0,0,10",
                "0,0,10,0",
                "0,0,-1,10",
                "0,0,10,-1",
                "1e308,0,1e308,1",
                "0,1e308,1,1e308",
                "0,0,10,10;0,0,NaN,10",
            ] {
                assert!(
                    Cli::try_parse_from([
                        "epscan",
                        command,
                        "--source",
                        "film-holder",
                        "--rect",
                        selection
                    ])
                    .is_err(),
                    "{command} {selection:?}"
                );
            }
            assert!(
                Cli::try_parse_from([
                    "epscan",
                    command,
                    "--source",
                    "film-holder",
                    "--rect",
                    "0",
                    "0",
                    "10",
                    "10"
                ])
                .is_err()
            );
            let error = Cli::try_parse_from([
                "epscan",
                command,
                "--source",
                "film-holder",
                "--rect",
                "0,0,10,10;0,0,NaN,10",
            ])
            .err()
            .unwrap();
            assert!(error.to_string().contains("rectangle 2"));
        }
    }

    #[test]
    fn rectangle_lists_require_source_and_conflict_with_holder_selection() {
        let selection = "0,0,10,10;20,30,10,10";
        for command in ["scan", "capture", "preview"] {
            assert!(Cli::try_parse_from(["epscan", command, "--rect", selection]).is_err());
            for extra in [
                vec!["--holder", "v800-35mm", "--frame", "1"],
                vec!["--frame", "1"],
                vec!["--overage", "5"],
            ] {
                assert!(
                    Cli::try_parse_from(
                        [
                            "epscan",
                            command,
                            "--source",
                            "film-holder",
                            "--rect",
                            selection
                        ]
                        .into_iter()
                        .chain(extra)
                    )
                    .is_err()
                );
            }
        }
    }

    #[test]
    fn later_rectangle_outside_source_rejects_the_complete_list() {
        let caps = capabilities();
        for command in ["scan", "capture", "preview"] {
            for selection in ["0,0,10,10;170,0,10,10", "0,0,10,10;0,246,10,10"] {
                let capture = capture_from(
                    Cli::try_parse_from([
                        "epscan",
                        command,
                        "--source",
                        "film-holder",
                        "--rect",
                        selection,
                    ])
                    .unwrap(),
                );
                assert!(capture.validate_area().is_ok());
                let error = capture.resolve_areas(&caps).unwrap_err();
                assert!(error.to_string().contains("rectangle 2"));
            }
            let wide_source = capture_from(
                Cli::try_parse_from([
                    "epscan",
                    command,
                    "--source",
                    "flatbed",
                    "--rect",
                    "0,0,10,10;170,0,10,10",
                ])
                .unwrap(),
            );
            assert_eq!(wide_source.resolve_areas(&caps).unwrap().len(), 2);
        }
    }

    #[test]
    fn negative_overage_crops_each_dimension_about_the_same_center() {
        let caps = capabilities();
        for command in ["scan", "capture", "preview"] {
            let capture = capture_from(
                Cli::try_parse_from([
                    "epscan",
                    command,
                    "--holder",
                    "v800-35mm",
                    "--frame",
                    "1",
                    "--overage",
                    "-10",
                ])
                .unwrap(),
            );
            let area = capture.resolve_areas(&caps).unwrap().remove(0);
            for (actual, expected) in area.rect_mm.into_iter().zip([122.7, 18.3, 21.6, 32.4]) {
                assert!((actual - expected).abs() < 1e-9);
            }
            assert_eq!(area.holder_selection.unwrap().overage_percent, -10.0);
            let mut invalid = capture;
            invalid.overage = Some(-100.0);
            assert!(invalid.validate_area().is_err());
        }
    }

    #[test]
    fn sharpness_is_optional_for_rectangle_and_holder_captures_in_every_rgb_command() {
        let caps = capabilities();
        for command in ["scan", "capture", "preview"] {
            let default = capture_from(parse_capture(command, &[]).unwrap());
            assert!(!default.measure_sharpness);
            let rectangle = capture_from(parse_capture(command, &["--measure-sharpness"]).unwrap());
            assert!(rectangle.measure_sharpness);
            assert!(rectangle.validate_area().is_ok());
            assert_eq!(
                rectangle.resolve_areas(&caps).unwrap()[0].rect_mm,
                [0., 0., 10., 10.]
            );
            let holder = capture_from(
                Cli::try_parse_from([
                    "epscan",
                    command,
                    "--holder",
                    "v800-35mm",
                    "--frame",
                    "1-3",
                    "--measure-sharpness",
                ])
                .unwrap(),
            );
            assert!(holder.measure_sharpness);
            assert_eq!(holder.resolve_areas(&caps).unwrap().len(), 3);
        }
    }

    #[test]
    fn sharpness_requires_rgb_but_allows_an_additional_infrared_pass() {
        for command in ["scan", "capture"] {
            assert!(parse_capture(command, &["--measure-sharpness", "--ir-only"]).is_err());
            let Action::Scan(mut scan) = parse_capture(command, &["--measure-sharpness", "--ir"])
                .unwrap()
                .action
            else {
                panic!()
            };
            assert!(scan.validate_options().is_ok());
            scan.ir = false;
            scan.ir_only = true;
            assert!(scan.validate_options().is_err());
        }
    }

    #[test]
    fn frame_lists_expand_inclusive_ranges_and_preserve_first_request_order() {
        let caps = capabilities();
        for command in ["scan", "capture", "preview"] {
            for (selection, expected) in [
                ("1-5,8-12", vec![1, 2, 3, 4, 5, 8, 9, 10, 11, 12]),
                ("3,1-3,8,3,8-10", vec![3, 1, 2, 8, 9, 10]),
                (" 3, 1 - 3 ,8 ", vec![3, 1, 2, 8]),
                ("5-5,5", vec![5]),
                ("18", vec![18]),
            ] {
                let capture = capture_from(
                    Cli::try_parse_from([
                        "epscan",
                        command,
                        "--holder",
                        "v800-35mm",
                        "--frame",
                        selection,
                    ])
                    .unwrap(),
                );
                let areas = capture.resolve_areas(&caps).unwrap();
                let actual: Vec<_> = areas
                    .iter()
                    .map(|area| area.holder_selection.unwrap().frame)
                    .collect();
                assert_eq!(actual, expected, "{command} {selection}");
                for area in areas {
                    let selected = area.holder_selection.unwrap();
                    assert_eq!(area.source, Source::Transparency);
                    assert_eq!(
                        area.rect_mm,
                        caps.scanner_model()
                            .unwrap()
                            .holder_frame(Holder::V800Film35mm, selected.frame, 0.0)
                            .unwrap()
                    );
                }
            }
        }
    }

    #[test]
    fn malformed_frame_lists_are_rejected_before_connection() {
        for selection in [
            "",
            ",",
            "1,",
            ",1",
            "1,,2",
            "0",
            "0-2",
            "2-0",
            "5-1",
            "1-2-3",
            "1--3",
            "-1",
            "+1",
            "1.0",
            "1..3",
            "1-",
            "4294967296",
            "1-4294967296",
        ] {
            assert!(
                Cli::try_parse_from([
                    "epscan",
                    "scan",
                    "--holder",
                    "v800-35mm",
                    "--frame",
                    selection
                ])
                .is_err(),
                "{selection}"
            );
        }
    }

    #[test]
    fn huge_valid_u32_ranges_stay_compact_and_fail_model_bounds_before_expansion() {
        let selection: FrameSelection = "1-4294967295".parse().unwrap();
        assert_eq!(selection.ranges, vec![(1, u32::MAX)]);
        assert!(selection.resolve(18).is_err());
        let caps = capabilities();
        for selection in ["1-4294967295", "1-18,4294967295", "1-5,19"] {
            let capture = capture_from(
                Cli::try_parse_from([
                    "epscan",
                    "scan",
                    "--holder",
                    "v800-35mm",
                    "--frame",
                    selection,
                ])
                .unwrap(),
            );
            assert!(capture.resolve_areas(&caps).is_err());
        }
    }

    #[test]
    fn invalid_later_frame_geometry_rejects_the_complete_selection() {
        let caps = capabilities();
        // Frame 7 can grow 25% within the source, but frame 13 then crosses its
        // left edge. Resolving must not return the valid first frame alone.
        let capture = capture_from(
            Cli::try_parse_from([
                "epscan",
                "scan",
                "--holder",
                "v800-35mm",
                "--frame",
                "7,13",
                "--overage",
                "25",
            ])
            .unwrap(),
        );
        assert!(capture.resolve_areas(&caps).is_err());
    }

    #[test]
    fn rectangle_path_requires_explicit_source_and_area() {
        for command in ["scan", "capture", "preview"] {
            for arguments in [
                vec!["epscan", command],
                vec!["epscan", command, "--source", "film-holder"],
                vec!["epscan", command, "--rect", "0,0,10,10"],
                vec!["epscan", command, "usb:1:1"],
            ] {
                let error = Cli::try_parse_from(arguments)
                    .err()
                    .expect("missing capture settings must fail");
                assert_eq!(
                    error.kind(),
                    clap::error::ErrorKind::MissingRequiredArgument
                );
            }
            assert!(parse_capture(command, &[]).is_ok());
        }
    }

    #[test]
    fn epson_scan_options_form_a_valid_job() {
        let cli = Cli::try_parse_from([
            "epscan",
            "scan",
            "--source",
            "film-holder",
            "--dpi",
            "300",
            "--depth",
            "16",
            "--rect",
            "0,0,10,10",
            "--ir",
            "--ir-gamma",
            "device-default",
            "--thumbnail",
            "--raw-only",
            "--log",
            "debug",
        ])
        .unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!()
        };
        assert!(scan.validate_options().is_ok());
        assert!(scan.ir && scan.thumbnail && scan.capture.raw_only);
    }

    #[test]
    fn successful_capture_intermediates_are_opt_in_for_scan_and_preview() {
        for command in ["scan", "preview"] {
            for (arguments, keep, raw_only) in [
                (vec![], false, false),
                (vec!["--keep-intermediates"], true, false),
                (vec!["--raw-only"], false, true),
                (vec!["--raw-only", "--keep-intermediates"], true, true),
            ] {
                let capture = match parse_capture(command, &arguments).unwrap().action {
                    Action::Scan(scan) => scan.capture,
                    Action::Preview(preview) => preview.capture,
                    _ => unreachable!(),
                };
                assert_eq!(capture.keep_intermediates, keep);
                assert_eq!(capture.raw_only, raw_only);
            }
        }
    }

    #[test]
    fn scan_and_preview_json_output_is_opt_in() {
        for command in ["scan", "preview"] {
            for (arguments, json) in [(vec![], false), (vec!["--json"], true)] {
                let capture = match parse_capture(command, &arguments).unwrap().action {
                    Action::Scan(scan) => scan.capture,
                    Action::Preview(preview) => preview.capture,
                    _ => unreachable!(),
                };
                assert_eq!(capture.json, json);
            }
        }
    }

    #[test]
    fn removed_nikon_options_and_unimplemented_controls_are_not_accepted() {
        for flag in [
            "--lock-wb",
            "--unlock-wb",
            "--lock-ae",
            "--clean",
            "--superfine",
            "--no-eject",
            "--samples",
            "--frames",
            "--format",
            "--ir-depth",
            "--lens",
            "--exposure",
        ] {
            assert!(parse_capture("scan", &[flag]).is_err(), "{flag}");
        }
        assert!(Cli::try_parse_from(["epscan", "eject"]).is_err());
    }

    #[test]
    fn contradictory_or_unused_infrared_options_fail() {
        for arguments in [
            vec!["--ir", "--ir-only"],
            vec!["--ir-only", "--thumbnail"],
            vec!["--ir-only", "--depth", "16"],
            vec!["--ir-only", "--gamma", "identity-lut"],
            vec!["--ir-gamma", "identity-lut"],
            vec!["--settle-seconds", "0"],
        ] {
            assert!(parse_capture("scan", &arguments).is_err());
        }
        let cli = parse_capture("scan", &["--ir-only"]).unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!()
        };
        assert!(scan.validate_options().is_ok());
    }

    #[test]
    fn silver_film_with_infrared_fails_before_connect() {
        let cli = parse_capture("scan", &["--ir", "--film", "mono"]).unwrap();
        let Action::Scan(scan) = cli.action else {
            panic!()
        };
        assert!(scan.validate_options().is_err());
    }

    #[test]
    fn preview_does_not_accept_ignored_capture_options() {
        for flag in [
            "--dpi",
            "--depth",
            "--ir",
            "--ir-only",
            "--thumbnail",
            "--settle-seconds",
        ] {
            assert!(parse_capture("preview", &[flag]).is_err(), "{flag}");
        }
        assert!(
            Cli::try_parse_from([
                "epscan",
                "preview",
                "--raw-only",
                "--source",
                "film-area-guide",
                "--rect",
                "0,0,10,10"
            ])
            .is_ok()
        );
    }

    #[test]
    fn invalid_geometry_is_rejected_before_connect() {
        for rect in ["nan,0,10,10", "-1,0,10,10", "0,0,0,10"] {
            assert!(
                Cli::try_parse_from(["epscan", "scan", "--source", "film-holder", "--rect", rect])
                    .is_err()
            );
        }
        assert!(parse_capture("scan", &["--depth", "12"]).is_err());
    }
}
