// SPDX-License-Identifier: MIT
//! Bounded row averaging between USB acquisition and the square-pixel payload.
use std::{
    io::{self, Write},
    sync::atomic::{AtomicU64, Ordering},
};

pub(super) struct RowAverager<'a, W> {
    inner: W,
    factor: u32,
    depth: u8,
    stride: usize,
    row: Vec<u8>,
    sums: Vec<u32>,
    rows: u32,
    committed: &'a AtomicU64,
}

impl<'a, W: Write> RowAverager<'a, W> {
    pub(super) fn new(
        inner: W,
        stride: usize,
        depth: u8,
        factor: u32,
        committed: &'a AtomicU64,
    ) -> Self {
        Self {
            inner,
            factor,
            depth,
            stride,
            row: Vec::with_capacity(if factor > 1 { stride } else { 0 }),
            sums: if factor > 1 {
                vec![0; stride / usize::from(depth / 8)]
            } else {
                Vec::new()
            },
            rows: 0,
            committed,
        }
    }

    pub(super) fn finish(&mut self) -> io::Result<()> {
        if !self.row.is_empty() || self.rows != 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Incomplete Y oversampling row group",
            ));
        }
        self.flush()
    }

    fn complete_row(&mut self) -> io::Result<()> {
        if self.depth == 8 {
            for (sum, &value) in self.sums.iter_mut().zip(&self.row) {
                *sum += u32::from(value);
            }
        } else {
            for (sum, bytes) in self.sums.iter_mut().zip(self.row.as_chunks::<2>().0) {
                *sum += u32::from(u16::from_le_bytes([bytes[0], bytes[1]]));
            }
        }
        self.rows += 1;
        if self.rows == self.factor {
            // Round half upward once, after summing all unmodified integer samples.
            if self.depth == 8 {
                for (value, sum) in self.row.iter_mut().zip(&self.sums) {
                    *value = ((sum + self.factor / 2) / self.factor) as u8;
                }
            } else {
                for (bytes, sum) in self.row.as_chunks_mut::<2>().0.iter_mut().zip(&self.sums) {
                    bytes.copy_from_slice(
                        &(((sum + self.factor / 2) / self.factor) as u16).to_le_bytes(),
                    );
                }
            }
            self.inner.write_all(&self.row)?;
            self.committed
                .fetch_add(self.stride as u64, Ordering::Release);
            self.sums.fill(0);
            self.rows = 0;
        }
        self.row.clear();
        Ok(())
    }
}

impl<W: Write> Write for RowAverager<'_, W> {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        if self.factor == 1 {
            let written = self.inner.write(input)?;
            self.committed.fetch_add(written as u64, Ordering::Release);
            return Ok(written);
        }
        let mut remaining = input;
        while !remaining.is_empty() {
            let count = remaining.len().min(self.stride - self.row.len());
            self.row.extend_from_slice(&remaining[..count]);
            remaining = &remaining[count..];
            if self.row.len() == self.stride {
                self.complete_row()?;
            }
        }
        Ok(input.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgb16_averages_without_channel_or_byte_mixing_for_split_transfers() {
        let values: Vec<u16> = vec![
            0, 1000, 65535, 600, 5000, 1234, 2, 1004, 65535, 604, 5002, 1236,
        ];
        let bytes: Vec<u8> = values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect();
        for block_size in 1..=bytes.len() {
            let committed = AtomicU64::new(0);
            let mut output = Vec::new();
            let mut writer = RowAverager::new(&mut output, 12, 16, 2, &committed);
            for chunk in bytes.chunks(block_size) {
                writer.write_all(chunk).unwrap();
            }
            writer.finish().unwrap();
            assert_eq!(committed.load(Ordering::Acquire), 12);
            assert_eq!(
                output,
                [1_u16, 1002, 65535, 602, 5001, 1235]
                    .into_iter()
                    .flat_map(u16::to_le_bytes)
                    .collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn gray8_only_publishes_complete_average_groups_and_rejects_partial_end() {
        let committed = AtomicU64::new(0);
        let mut output = Vec::new();
        let mut writer = RowAverager::new(&mut output, 2, 8, 3, &committed);
        writer.write_all(&[0, 255, 1, 255]).unwrap();
        assert_eq!(committed.load(Ordering::Acquire), 0);
        assert!(writer.finish().is_err());
        writer.write_all(&[1, 255]).unwrap();
        writer.finish().unwrap();
        assert_eq!(committed.load(Ordering::Acquire), 2);
        assert_eq!(output, [1, 255]);
    }

    #[test]
    fn disabled_is_byte_exact_without_row_buffers() {
        let committed = AtomicU64::new(0);
        let mut output = Vec::new();
        let mut writer = RowAverager::new(&mut output, 2, 8, 1, &committed);
        writer.write_all(&[3, 1, 0, 255]).unwrap();
        assert!(writer.sums.is_empty());
        writer.finish().unwrap();
        assert_eq!(output, [3, 1, 0, 255]);
    }
}
