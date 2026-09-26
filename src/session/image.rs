// SPDX-License-Identifier: MIT OR Apache-2.0
use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{BufReader, Read, Seek, Write},
    path::{Path, PathBuf},
};
use tiff::{
    encoder::{Rational, TiffEncoder, TiffKind, colortype},
    tags::Tag,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageResult {
    pub payload: PathBuf,
    pub tiff: Option<PathBuf>,
    pub width: u32,
    pub height: u32,
    pub channels: u8,
    pub depth: u8,
    pub dpi: u32,
    pub metadata: serde_json::Value,
}
pub fn decode_u16_le(bytes: &[u8]) -> Result<Vec<u16>> {
    if !bytes.len().is_multiple_of(2) {
        return Err(Error::Protocol("Odd 16-bit sample buffer".into()));
    }
    Ok(bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .collect())
}
impl ImageResult {
    pub fn expected_bytes(&self) -> u64 {
        u64::from(self.width)
            * u64::from(self.height)
            * u64::from(self.channels)
            * u64::from(self.depth / 8)
    }
    /// Export unchanged samples in small strips; input stays on disk.
    pub fn save_tiff(&self, path: &Path) -> Result<()> {
        self.save_tiff_transformed(path, &self.metadata, |_, _| Ok(()))
    }

    /// Export a derived image without altering the captured packed payload.
    /// The callback receives the first source row and a packed, mutable strip.
    pub(crate) fn save_tiff_transformed<F>(
        &self,
        path: &Path,
        metadata: &serde_json::Value,
        mut transform: F,
    ) -> Result<()>
    where
        F: FnMut(u32, &mut [u8]) -> Result<()>,
    {
        if ![1, 3].contains(&self.channels)
            || ![8, 16].contains(&self.depth)
            || self.width == 0
            || self.height == 0
            || self.dpi == 0
        {
            return Err(Error::Invalid("Invalid output image shape/depth".into()));
        }
        let expected = u64::from(self.width)
            .checked_mul(u64::from(self.height))
            .and_then(|n| n.checked_mul(u64::from(self.channels)))
            .and_then(|n| n.checked_mul(u64::from(self.depth / 8)))
            .ok_or_else(|| Error::Invalid("Output image byte count exceeds u64".into()))?;
        if self.payload.metadata()?.len() != expected {
            return Err(Error::Protocol(
                "Payload size does not match packed row stride".into(),
            ));
        }
        let file = OpenOptions::new().write(true).create_new(true).open(path)?;
        let result = (|| {
            let writer = file.try_clone()?;
            if expected > u64::from(u32::MAX) - 32 * 1024 * 1024 {
                self.encode(TiffEncoder::new_big(writer)?, metadata, &mut transform)?;
            } else {
                self.encode(TiffEncoder::new(writer)?, metadata, &mut transform)?;
            }
            file.sync_all()?;
            Ok(())
        })();
        drop(file);
        if result.is_err() {
            // This path belongs to us: create_new above succeeded.
            let _ = std::fs::remove_file(path);
        }
        result
    }
    fn encode<W: Write + Seek, K: TiffKind, F>(
        &self,
        mut encoder: TiffEncoder<W, K>,
        metadata: &serde_json::Value,
        transform: &mut F,
    ) -> Result<()>
    where
        F: FnMut(u32, &mut [u8]) -> Result<()>,
    {
        let description = serde_json::to_string(metadata)?;
        let mut input = BufReader::new(File::open(&self.payload)?);
        macro_rules! encode {
            ($color:ty,$depth:expr) => {{
                let mut image = encoder.new_image::<$color>(self.width, self.height)?;
                image.rows_per_strip(32)?;
                image
                    .encoder()
                    .write_tag(Tag::ImageDescription, description.as_str())?;
                image
                    .encoder()
                    .write_tag(Tag::Software, concat!("epscan ", env!("CARGO_PKG_VERSION")))?;
                image.encoder().write_tag(Tag::ResolutionUnit, 2u16)?;
                image
                    .encoder()
                    .write_tag(Tag::XResolution, Rational { n: self.dpi, d: 1 })?;
                image
                    .encoder()
                    .write_tag(Tag::YResolution, Rational { n: self.dpi, d: 1 })?;
                let mut first_row = 0;
                while image.next_strip_sample_count() > 0 {
                    let count = image.next_strip_sample_count() as usize;
                    let mut bytes = vec![0u8; count * $depth];
                    input.read_exact(&mut bytes)?;
                    transform(first_row, &mut bytes)?;
                    first_row +=
                        (count / (self.width as usize * usize::from(self.channels))) as u32;
                    let samples = bytes
                        .chunks_exact($depth)
                        .map(|b| {
                            let mut value = [0u8; $depth];
                            value.copy_from_slice(b);
                            value
                        })
                        .map(|b| <$color as colortype::ColorType>::Inner::from_le_bytes(b))
                        .collect::<Vec<_>>();
                    image.write_strip(&samples)?;
                }
                image.finish()?;
                Ok(())
            }};
        }
        match (self.channels, self.depth) {
            (3, 16) => encode!(colortype::RGB16, 2),
            (3, 8) => encode!(colortype::RGB8, 1),
            (1, 16) => encode!(colortype::Gray16, 2),
            (1, 8) => encode!(colortype::Gray8, 1),
            _ => unreachable!(),
        }
    }
}
