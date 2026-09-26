// SPDX-License-Identifier: MIT OR Apache-2.0
//! ESC/I wire data defined by docs/protocol.md and checked against device traces.
//! Protocol facts were researched using SANE; this module is original Rust code.
pub use crate::capabilities::{OpticsProfile, ScanMode, Source};
use crate::{
    Error, Result,
    capabilities::{ScannerModel, V800_FAMILY, model_by_identity, pixel_rectangle},
    error::unsupported,
};
use serde::{Deserialize, Serialize};

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Capabilities {
    pub model: String,
    pub command_level: String,
    pub firmware: String,
    pub basic_dpi: u32,
    pub min_dpi: u32,
    pub max_dpi: u32,
    pub max_width_pixels: u32,
    pub input_depth: u8,
    pub max_output_depth: u8,
    pub flatbed_pixels: [u32; 2],
    pub transparency_pixels: [u32; 2],
    pub transparency_8x10_pixels: [u32; 2],
    pub infrared_advertised: bool,
}
impl Capabilities {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 80 {
            return Err(Error::Protocol("FS I needs 80 bytes".into()));
        }
        let c = Self {
            model: String::from_utf8_lossy(&bytes[46..62])
                .trim_matches([' ', '\0'])
                .to_owned(),
            command_level: String::from_utf8_lossy(&bytes[..2]).into(),
            firmware: String::from_utf8_lossy(&bytes[62..66]).into(),
            basic_dpi: u32_at(bytes, 4),
            min_dpi: u32_at(bytes, 8),
            max_dpi: u32_at(bytes, 12),
            max_width_pixels: u32_at(bytes, 16),
            input_depth: bytes[66],
            max_output_depth: bytes[67],
            flatbed_pixels: [u32_at(bytes, 20), u32_at(bytes, 24)],
            transparency_pixels: [u32_at(bytes, 36), u32_at(bytes, 40)],
            transparency_8x10_pixels: [u32_at(bytes, 68), u32_at(bytes, 72)],
            infrared_advertised: bytes[44] & 2 != 0,
        };
        if c.basic_dpi == 0 || c.min_dpi == 0 || c.min_dpi > c.max_dpi || c.max_width_pixels == 0 {
            return Err(Error::Protocol("Invalid identity geometry".into()));
        }
        Ok(c)
    }
    pub fn area_mm(&self, source: Source) -> [f64; 2] {
        let p = match source {
            Source::Flatbed => self.flatbed_pixels,
            Source::Transparency => self.transparency_pixels,
            Source::Transparency8x10 => self.transparency_8x10_pixels,
        };
        p.map(|v| f64::from(v) * 25.4 / f64::from(self.basic_dpi))
    }

    /// Match both the advertised identity and command level before selecting
    /// model-specific command policy. USB discovery alone is insufficient.
    pub fn scanner_model(&self) -> Result<&'static ScannerModel> {
        let model = model_by_identity(&self.model).ok_or_else(|| {
            unsupported(
                "scanner identity",
                format!("{} has no model profile", self.model),
            )
        })?;
        model.validate_identity(self)?;
        Ok(model)
    }

    pub fn optics(&self, source: Source) -> Result<OpticsProfile> {
        Ok(self.scanner_model()?.source(source)?.optics)
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "cli", derive(clap::ValueEnum))]
#[serde(rename_all = "kebab-case")]
pub enum Gamma {
    IdentityLut,
    #[default]
    DeviceDefault,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanSettings {
    pub rect_mm: [f64; 4],
    pub dpi: u32,
    pub depth: u8,
    pub mode: ScanMode,
    pub source: Source,
    pub gamma: Gamma,
    pub preview: bool,
}
impl Default for ScanSettings {
    fn default() -> Self {
        Self {
            rect_mm: [0., 0., 10., 10.],
            dpi: 300,
            depth: 16,
            mode: ScanMode::default(),
            source: Source::Transparency,
            gamma: Gamma::default(),
            preview: false,
        }
    }
}
impl ScanSettings {
    /// V800-family geometry convenience method. Connected scanners should use
    /// [`Self::pixels_for`] with their registered model.
    pub fn pixels(&self) -> Result<[u32; 4]> {
        self.pixels_for(&V800_FAMILY)
    }

    pub fn pixels_for(&self, model: &ScannerModel) -> Result<[u32; 4]> {
        pixel_rectangle(self.rect_mm, self.dpi, model.transfer.width_alignment)
    }

    pub fn validate(&self, caps: &Capabilities, ir: bool) -> Result<()> {
        let model = caps.scanner_model()?;
        let source = model.source(self.source)?;
        let mode = model.mode_for(self.mode, ir)?;
        let pixels = self.pixels_for(model)?;
        if !mode.depths.contains(&self.depth) || self.depth > caps.max_output_depth {
            return Err(unsupported(
                "depth",
                format!(
                    "{} supports {:?}-bit output, further limited by the device's {}-bit maximum",
                    if ir {
                        "experimental infrared"
                    } else {
                        match self.mode {
                            ScanMode::Rgb => "RGB",
                            ScanMode::Gray => "grayscale",
                        }
                    },
                    mode.depths,
                    caps.max_output_depth
                ),
            ));
        }
        let minimum = source.min_dpi.max(caps.min_dpi);
        let maximum = source.max_dpi.min(caps.max_dpi);
        if !(minimum..=maximum).contains(&self.dpi) {
            return Err(Error::Invalid(format!(
                "DPI outside supported range {minimum}..={maximum}"
            )));
        }
        let [max_x, max_y] = caps.area_mm(self.source);
        if !max_x.is_finite() || !max_y.is_finite() || max_x <= 0. || max_y <= 0. {
            return Err(unsupported(
                "source",
                "device does not advertise a usable area",
            ));
        }
        let [x, y, w, h] = self.rect_mm;
        // Check the requested physical area and the independently rounded wire
        // endpoints. Rounding x and width separately can add one column at an edge.
        let bounds = [max_x, max_y].map(|value| (value * f64::from(self.dpi) / 25.4 + 0.5).floor());
        if x + w > max_x + 1e-6
            || y + h > max_y + 1e-6
            || f64::from(pixels[0]) + f64::from(pixels[2]) > bounds[0]
            || f64::from(pixels[1]) + f64::from(pixels[3]) > bounds[1]
            || pixels[2] > caps.max_width_pixels
        {
            return Err(Error::Invalid(format!(
                "Rectangle exceeds source area {max_x} x {max_y} mm"
            )));
        }
        if ir && (source.infrared_option.is_none() || !caps.infrared_advertised) {
            return Err(unsupported(
                "infrared",
                "requires a supported transparency source and advertised IR capability",
            ));
        }
        Ok(())
    }

    /// Encode the V800-family wire packet without connected-device validation.
    /// This low-level diagnostic API can encode IR16, which the validated scan
    /// API rejects because only IR8 candidate transfers have succeeded.
    pub fn parameters(&self, ir: bool) -> Result<[u8; 64]> {
        self.parameters_for_model(&V800_FAMILY, ir)
    }

    pub fn parameters_with_capabilities(&self, caps: &Capabilities, ir: bool) -> Result<[u8; 64]> {
        self.validate(caps, ir)?;
        self.parameters_for_model(caps.scanner_model()?, ir)
    }

    fn parameters_for_model(&self, model: &ScannerModel, ir: bool) -> Result<[u8; 64]> {
        if ![8, 16].contains(&self.depth) {
            return Err(Error::Invalid("Depth must be 8 or 16".into()));
        }
        let rectangle = self.pixels_for(model)?;
        let source = model.source(self.source)?;
        let mode = model.mode_for(self.mode, ir)?;
        let mut packet = ParameterPacket::default();
        packet.words([self.dpi, self.dpi].into_iter().chain(rectangle));
        let option = if ir {
            source
                .infrared_option
                .ok_or_else(|| unsupported("IR source", "source has no infrared option"))?
        } else {
            source.option
        };
        packet.bytes(&[
            (24, mode.wire_mode),
            (25, self.depth),
            (26, option),
            (27, u8::from(self.preview)),
            (
                28,
                model
                    .transfer
                    .block_lines(rectangle[2], mode.channels, self.depth)?,
            ),
            (
                29,
                match self.gamma {
                    Gamma::IdentityLut => 3,
                    Gamma::DeviceDefault => 2,
                },
            ),
            (32, 1),
            (33, 128),
        ]);
        Ok(packet.0)
    }
}

struct ParameterPacket([u8; 64]);
impl Default for ParameterPacket {
    fn default() -> Self {
        Self([0; 64])
    }
}
impl ParameterPacket {
    fn words(&mut self, values: impl Iterator<Item = u32>) {
        for (destination, value) in self.0[..24].as_chunks_mut::<4>().0.iter_mut().zip(values) {
            destination.copy_from_slice(&value.to_le_bytes());
        }
    }
    fn bytes(&mut self, fields: &[(usize, u8)]) {
        for &(offset, value) in fields {
            self.0[offset] = value;
        }
    }
}

pub fn infrared_token(parameters: &[u8]) -> Result<[u8; 32]> {
    if parameters.len() != 64 {
        return Err(Error::Protocol(
            "IR token needs 64-byte FS S readback".into(),
        ));
    }
    let key = [
        0xca, 0xfb, 0x77, 0x71, 0x20, 0x16, 0xda, 0x09, 0x5f, 0x57, 0x09, 0x12, 0x04, 0x83, 0x76,
        0x77, 0x3c, 0x73, 0x9c, 0xbe, 0x7a, 0xe0, 0x52, 0xe2, 0x90, 0x0d, 0xff, 0x9a, 0xef, 0x4c,
        0x2c, 0x81,
    ];
    Ok(std::array::from_fn(|i| key[i] ^ parameters[i]))
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub raw_hex: String,
    pub fatal: bool,
    pub busy: bool,
    pub warming_up: bool,
    pub transparency_installed: bool,
    pub transparency_error: bool,
    pub lid_open: bool,
}
impl Status {
    pub fn parse(b: &[u8]) -> Result<Self> {
        if b.len() != 16 {
            return Err(Error::Protocol("FS F needs 16 bytes".into()));
        }
        Ok(Self {
            raw_hex: hex(b),
            fatal: b[0] & 128 != 0,
            busy: b[0] & 64 != 0,
            warming_up: b[0] & 2 != 0,
            transparency_installed: b[2] & 128 != 0,
            transparency_error: b[2] & 32 != 0,
            lid_open: b[2] & 2 != 0,
        })
    }
}
#[derive(Debug, Serialize)]
pub struct TransferHeader {
    pub block_size: u32,
    pub full_blocks: u32,
    pub tail_size: u32,
}
impl TransferHeader {
    pub fn parse(b: &[u8], expected: u64) -> Result<Self> {
        if b.len() != 14 || b[0] != 2 {
            return Err(Error::Protocol("FS G requires 14-byte STX header".into()));
        }
        if b[1] & 128 != 0 {
            return Err(Error::Protocol(format!(
                "Scanner rejected scan start (FS G status 0x{:02x}, fatal flag set); no image data received; no automatic retry",
                b[1]
            )));
        }
        if b[1] & 64 != 0 {
            return Err(Error::Busy("FS G not ready; no automatic retry".into()));
        }
        let h = Self {
            block_size: u32_at(b, 2),
            full_blocks: u32_at(b, 6),
            tail_size: u32_at(b, 10),
        };
        let total = u64::from(h.block_size) * u64::from(h.full_blocks) + u64::from(h.tail_size);
        if h.full_blocks > 0 && h.block_size == 0
            || h.block_size.max(h.tail_size) > 64 * 1024 * 1024
            || total != expected
            || expected == 0
        {
            return Err(Error::Protocol(format!(
                "Transfer geometry {total} bytes vs {expected}; refusing unknown stride/layout"
            )));
        }
        Ok(h)
    }
}
#[derive(Debug, Serialize)]
pub struct StartStatusObservation {
    pub elapsed_seconds: f64,
    pub status: Status,
}

#[derive(Debug, Serialize)]
pub struct StartRecovery {
    pub rejected_header_hex: String,
    pub status_observations: Vec<StartStatusObservation>,
    pub warmup_seconds: f64,
}

#[derive(Debug, Serialize)]
pub struct TransferResult {
    pub header_hex: String,
    pub block_status: Vec<u8>,
    pub received_bytes: u64,
    /// Includes the initial start and, when confirmed warmup required it, the
    /// single resumed start. Strict acquisition always reports one.
    pub start_attempts: u8,
    pub start_recovery: Option<StartRecovery>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // The 80-byte FS I response captured from the reference GT-X980/B8 device.
    fn identity() -> Vec<u8> {
        let hex = "42380000c01200001900000000320000609f0000609f000060db00000000000000000000a06e0000e0b50000871047542d58393830202020202020202020312e313010100096000080bb000000000000";
        (0..hex.len())
            .step_by(2)
            .map(|offset| u8::from_str_radix(&hex[offset..offset + 2], 16).unwrap())
            .collect()
    }

    fn caps() -> Capabilities {
        Capabilities::parse(&identity()).unwrap()
    }

    fn header(block: u32, count: u32, tail: u32) -> Vec<u8> {
        let mut bytes = vec![2, 0x12];
        for value in [block, count, tail] {
            bytes.extend(value.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn captured_identity_selects_profile_and_preserves_advertised_area() {
        let caps = caps();
        assert_eq!(caps.model, "GT-X980");
        assert_eq!(caps.command_level, "B8");
        assert_eq!(caps.scanner_model().unwrap().pid, 0x0151);
        assert_eq!(caps.area_mm(Source::Transparency), [149.86, 246.38]);
        assert_eq!(caps.area_mm(Source::Transparency8x10), [203.2, 254.]);
        assert_eq!(caps.basic_dpi, 4800);
        assert_eq!(caps.max_dpi, 12800);
        assert_eq!(
            caps.optics(Source::Transparency)
                .unwrap()
                .manufacturer_optical_dpi,
            6400
        );
    }

    #[test]
    fn malformed_identity_and_unregistered_protocol_are_rejected() {
        assert!(Capabilities::parse(&[0; 79]).is_err());
        assert!(Capabilities::parse(&[0; 80]).is_err());
        let mut caps = caps();
        caps.command_level = "F5".into();
        assert!(ScanSettings::default().validate(&caps, false).is_err());
        caps.command_level = "B8".into();
        caps.model = "unregistered Epson".into();
        assert!(ScanSettings::default().validate(&caps, false).is_err());
    }

    #[test]
    fn rgb_packet_matches_captured_contract() {
        let settings = ScanSettings {
            gamma: Gamma::IdentityLut, // the historical capture explicitly used custom gamma
            ..Default::default()
        };
        let packet = settings
            .parameters_with_capabilities(&caps(), false)
            .unwrap();
        assert_eq!(settings.pixels().unwrap(), [0, 0, 112, 118]);
        let words: Vec<_> = packet[..24]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|word| u32::from_le_bytes(*word))
            .collect();
        assert_eq!(words, [300, 300, 0, 0, 112, 118]);
        assert_eq!(
            &packet[24..38],
            &[0x13, 16, 1, 0, 32, 3, 0, 0, 1, 128, 0, 0, 0, 0]
        );
        assert!(packet[38..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn grayscale_packet_uses_one_visible_channel_at_both_depths() {
        let caps = caps();
        for depth in [8, 16] {
            let settings = ScanSettings {
                mode: ScanMode::Gray,
                depth,
                ..Default::default()
            };
            let packet = settings.parameters_with_capabilities(&caps, false).unwrap();
            assert_eq!(&packet[24..30], &[0, depth, 1, 0, 32, 2]);
            let mode = caps
                .scanner_model()
                .unwrap()
                .mode_for(settings.mode, false)
                .unwrap();
            assert_eq!(mode.channels, 1);
            let [_, _, width, height] = settings.pixels().unwrap();
            let expected = u64::from(width) * u64::from(height) * u64::from(depth / 8);
            assert_eq!(expected, 112 * 118 * u64::from(depth / 8));
            assert!(TransferHeader::parse(&header(expected as u32, 1, 0), expected).is_ok());
            assert!(TransferHeader::parse(&header(expected as u32 * 3, 1, 0), expected).is_err());
        }

        let settings = ScanSettings {
            mode: ScanMode::Gray,
            ..Default::default()
        };
        let mut limited_caps = caps;
        limited_caps.max_output_depth = 8;
        let error = settings
            .validate(&limited_caps, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains("grayscale"));
        assert!(settings.validate(&limited_caps, true).is_err());
    }

    #[test]
    fn source_selection_does_not_silently_widen_holder_area() {
        let settings = ScanSettings {
            rect_mm: [0., 0., 170., 10.],
            ..Default::default()
        };
        assert!(settings.validate(&caps(), false).is_err());
        let guide = ScanSettings {
            source: Source::Transparency8x10,
            ..settings
        };
        assert_eq!(
            guide.parameters_with_capabilities(&caps(), false).unwrap()[26],
            5
        );
        let mut caps = caps();
        caps.transparency_8x10_pixels = [0, 0];
        assert!(guide.validate(&caps, false).is_err());
    }

    #[test]
    fn dpi_and_depth_intersect_profile_with_advertised_limits() {
        let mut caps = caps();
        let mut settings = ScanSettings {
            dpi: 6400,
            ..Default::default()
        };
        assert!(settings.validate(&caps, false).is_ok());
        caps.max_dpi = 3200;
        assert!(settings.validate(&caps, false).is_err());
        caps.max_dpi = 25600;
        settings.dpi = 12801;
        assert!(settings.validate(&caps, false).is_err());
        settings.dpi = 24;
        assert!(settings.validate(&caps, false).is_err());
        settings.dpi = 300;
        caps.max_output_depth = 8;
        assert!(settings.validate(&caps, false).is_err());
        settings.depth = 8;
        assert!(settings.validate(&caps, false).is_ok());
    }

    #[test]
    fn ir_requires_supported_source_advertisement_and_candidate_depth() {
        let mut caps = caps();
        let mut settings = ScanSettings {
            depth: 8,
            ..Default::default()
        };
        let packet = settings.parameters_with_capabilities(&caps, true).unwrap();
        assert_eq!(&packet[24..27], &[0, 8, 3]);
        settings.depth = 16;
        assert!(settings.validate(&caps, true).is_err());
        // The explicit low-level diagnostic encoder still represents IR16.
        assert_eq!(settings.parameters(true).unwrap()[25], 16);
        settings.depth = 8;
        for source in [Source::Flatbed, Source::Transparency8x10] {
            settings.source = source;
            assert!(settings.parameters_with_capabilities(&caps, true).is_err());
        }
        settings.source = Source::Transparency;
        caps.infrared_advertised = false;
        assert!(settings.validate(&caps, true).is_err());
    }

    #[test]
    fn geometry_rejects_nonfinite_empty_and_overflowing_rectangles() {
        for rect in [
            [-1., 0., 10., 10.],
            [0., 0., 0., 10.],
            [0., 0., 0.01, 10.],
            [0., 0., f64::NAN, 10.],
            [0., 0., 10., f64::INFINITY],
            [f64::from(u32::MAX), 0., 10., 10.],
            [
                f64::from(u32::MAX) / 2.,
                0.,
                f64::from(u32::MAX) / 2. + 10.,
                10.,
            ],
        ] {
            let settings = ScanSettings {
                dpi: 254,
                rect_mm: rect,
                ..Default::default()
            };
            assert!(settings.pixels().is_err(), "{rect:?}");
        }
        // Individual fields fit in u32 but the rounded endpoint does not.
        let rect = [
            f64::from(u32::MAX) / 2.,
            0.,
            f64::from(u32::MAX) / 2. + 10.,
            10.,
        ];
        assert!(pixel_rectangle(rect, 25, 8).is_ok());
        assert!(pixel_rectangle(rect.map(|value| value * 25.4), 1, 8).is_err());
    }

    #[test]
    fn physical_edge_can_be_exceeded_by_independent_rounding() {
        let settings = ScanSettings {
            dpi: 302,
            // Height and origin each round up: 119 + 2811 = 2930, whereas
            // the source height rounds to 2929 rows.
            rect_mm: [0., 10., 10., 236.38],
            ..Default::default()
        };
        let caps = caps();
        let pixels = settings.pixels().unwrap();
        let rounded_height = (caps.area_mm(Source::Transparency)[1] * 302. / 25.4 + 0.5).floor();
        assert!(f64::from(pixels[1] + pixels[3]) > rounded_height);
        assert!(settings.validate(&caps, false).is_err());
    }

    #[test]
    fn settings_partial_deserialization_defaults_and_rejects_typos() {
        let settings: ScanSettings = serde_json::from_str(r#"{"dpi":600}"#).unwrap();
        assert_eq!(settings.dpi, 600);
        assert_eq!(settings.depth, 16);
        assert_eq!(settings.mode, ScanMode::Rgb);
        assert_eq!(settings.source, Source::Transparency);
        let gray: ScanSettings = serde_json::from_str(r#"{"mode":"gray"}"#).unwrap();
        assert_eq!(gray.mode, ScanMode::Gray);
        assert_eq!(serde_json::to_value(gray).unwrap()["mode"], "gray");
        assert!(serde_json::from_str::<ScanSettings>(r#"{"mode":"greyish"}"#).is_err());
        assert!(serde_json::from_str::<ScanSettings>(r#"{"dppi":600}"#).is_err());
    }

    #[test]
    fn transfer_header_validates_status_and_exact_byte_count() {
        assert!(TransferHeader::parse(&header(6, 2, 3), 15).is_ok());
        assert!(TransferHeader::parse(&header(6, 2, 3), 16).is_err());
        assert!(TransferHeader::parse(&header(0, 1, 0), 0).is_err());
        assert!(
            TransferHeader::parse(&header(64 * 1024 * 1024 + 1, 1, 0), 64 * 1024 * 1024 + 1)
                .is_err()
        );
        let mut bytes = header(6, 2, 3);
        bytes[1] = 0x92;
        assert!(TransferHeader::parse(&bytes, 15).is_err());
        bytes[1] = 0x40;
        assert!(matches!(
            TransferHeader::parse(&bytes, 15),
            Err(Error::Busy(_))
        ));
        assert!(TransferHeader::parse(&bytes[..13], 15).is_err());
    }

    #[test]
    fn transfer_line_count_tracks_packed_row_size_without_changing_geometry() {
        let mut settings = ScanSettings {
            rect_mm: [8.3, 27.5, 12.0, 208.0],
            dpi: 3200,
            ..Default::default()
        };
        let rgb16 = settings
            .parameters_with_capabilities(&caps(), false)
            .unwrap();
        assert_eq!(settings.pixels().unwrap(), [1046, 3465, 1512, 26205]);
        assert_eq!(rgb16[28], 7);
        assert_eq!(u32::from(rgb16[28]) * 1512 * 6 + 1, 63505);
        settings.depth = 8;
        let rgb8 = settings
            .parameters_with_capabilities(&caps(), false)
            .unwrap();
        assert_eq!(rgb8[28], 14);
        assert_eq!(&rgb16[..24], &rgb8[..24]);
        assert_eq!(
            settings
                .parameters_with_capabilities(&caps(), true)
                .unwrap()[28],
            32
        );
        settings.mode = ScanMode::Gray;
        settings.depth = 16;
        let gray16 = settings
            .parameters_with_capabilities(&caps(), false)
            .unwrap();
        assert_eq!(&gray16[..24], &rgb16[..24]);
        assert_eq!(gray16[28], 21);
        assert_eq!(u32::from(gray16[28]) * 1512 * 2 + 1, 63505);
        settings.depth = 8;
        assert_eq!(
            settings
                .parameters_with_capabilities(&caps(), false)
                .unwrap()[28],
            32
        );
        assert_eq!(ScanSettings::default().parameters(false).unwrap()[28], 32);

        let policy = V800_FAMILY.transfer;
        assert_eq!(policy.block_lines(16384, 1, 16).unwrap(), 1);
        assert_eq!(policy.block_lines(32768, 3, 16).unwrap(), 1);
        assert!(policy.block_lines(0, 3, 16).is_err());
        assert!(policy.block_lines(1512, 3, 12).is_err());
    }

    #[test]
    fn infrared_challenge_matches_known_vector() {
        assert_eq!(
            hex(&infrared_token(&(0..64).collect::<Vec<_>>()).unwrap()),
            "cafa75722413dc0e575e0319088e78782c628ead6ef544f58814e581f351329e"
        );
        assert!(infrared_token(&[0; 32]).is_err());
    }
}
