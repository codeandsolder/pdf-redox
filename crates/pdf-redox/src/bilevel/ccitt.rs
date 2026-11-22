//! Packed, budget-limited CCITT Group 4 encoding.
//!
//! The coding modes and run decomposition follow fax 0.3.0's MIT-licensed
//! encoder (see LICENSE-fax-MIT). Code tables and the bit writer remain in
//! that dependency; this implementation works directly with packed row edges.

use crate::{Error, Result};
use fax::{
    BitWriter, Bits, Color, VecWriter,
    maps::{EDFB_HALF, Mode, black, mode, white},
};

struct PackedTransitions<'a> {
    words: std::slice::Chunks<'a, u8>,
    width: u32,
    base: u32,
    next_base: u32,
    previous_white: u64,
    edges: u64,
}

impl<'a> PackedTransitions<'a> {
    fn new(row: &'a [u8], width: u32) -> Self {
        Self {
            words: row.chunks(8),
            width,
            base: 0,
            next_base: 0,
            previous_white: 1,
            edges: 0,
        }
    }
}

impl Iterator for PackedTransitions<'_> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        loop {
            if self.edges != 0 {
                let index = self.edges.leading_zeros();
                self.edges ^= 1_u64 << (63 - index);
                return Some(self.base + index);
            }
            let bytes = self.words.next()?;
            let mut padded = [0; 8];
            padded[..bytes.len()].copy_from_slice(bytes);
            let word = u64::from_be_bytes(padded);
            self.base = self.next_base;
            let valid = (self.width - self.base).min(64);
            self.next_base += valid;
            self.edges =
                (word ^ ((word >> 1) | (self.previous_white << 63))) & (u64::MAX << (64 - valid));
            self.previous_white = word & 1;
        }
    }
}

#[derive(Debug)]
enum EncodeError {
    Budget,
    InvalidCode,
}

type EncodeResult<T> = std::result::Result<T, EncodeError>;

struct PackedEncoder {
    writer: VecWriter,
    remaining_bits: usize,
    reference: Vec<u32>,
    current: Vec<u32>,
}

impl PackedEncoder {
    fn write_bits(&mut self, bits: Bits) -> EncodeResult<()> {
        self.remaining_bits = self
            .remaining_bits
            .checked_sub(usize::from(bits.len))
            .ok_or(EncodeError::Budget)?;
        match self.writer.write(bits) {
            Ok(()) => Ok(()),
            Err(never) => match never {},
        }
    }

    fn write_mode(&mut self, mode: Mode) -> EncodeResult<()> {
        self.write_bits(mode::encode(mode).ok_or(EncodeError::InvalidCode)?)
    }

    fn write_run(&mut self, color: Color, mut length: u32) -> EncodeResult<()> {
        let table = match color {
            Color::White => &white::ENTRIES,
            Color::Black => &black::ENTRIES,
        };
        while length >= 64 {
            let makeup = (length & !63).min(2560);
            let index = usize::try_from(63 + makeup / 64).map_err(|_| EncodeError::InvalidCode)?;
            let &(value, bits) = table.get(index).ok_or(EncodeError::InvalidCode)?;
            if u32::from(value) != makeup {
                return Err(EncodeError::InvalidCode);
            }
            self.write_bits(bits)?;
            length -= makeup;
        }
        let index = usize::try_from(length).map_err(|_| EncodeError::InvalidCode)?;
        let &(value, bits) = table.get(index).ok_or(EncodeError::InvalidCode)?;
        if u32::from(value) != length {
            return Err(EncodeError::InvalidCode);
        }
        self.write_bits(bits)
    }

    fn next_edge(&mut self, edges: &mut PackedTransitions<'_>, width: u32) -> u32 {
        edges.next().map_or(width, |edge| {
            self.current.push(edge);
            edge
        })
    }

    fn encode_row(&mut self, row: &[u8], width: u32) -> EncodeResult<()> {
        let mut edges = PackedTransitions::new(row, width);
        self.current.clear();
        let mut position = 0;
        let mut color = Color::White;
        let mut first = true;
        let mut reference_cursor = 0;
        while position < width {
            let next = self.next_edge(&mut edges, width);
            loop {
                // Only the initial query includes a reference edge at zero.
                // Later queries exclude all edges at or before the current
                // position. This cursor moves monotonically, independent of
                // whether the coding color changes.
                if !first {
                    while self
                        .reference
                        .get(reference_cursor)
                        .is_some_and(|&edge| edge <= position)
                    {
                        reference_cursor += 1;
                    }
                }
                first = false;
                let opposite = reference_cursor
                    + usize::from((reference_cursor % 2 == 0) != (color == Color::White));
                let b1 = self.reference.get(opposite).copied().unwrap_or(width);
                if let Some(&b2) = self.reference.get(opposite + 1)
                    && b2 < next
                {
                    self.write_mode(Mode::Pass)?;
                    position = b2;
                    continue;
                }
                if next.abs_diff(b1) <= 3 {
                    let delta = i8::try_from(i64::from(next) - i64::from(b1))
                        .map_err(|_| EncodeError::InvalidCode)?;
                    self.write_mode(Mode::Vertical(delta))?;
                    position = next;
                    color = !color;
                } else {
                    let second = self.next_edge(&mut edges, width);
                    self.write_mode(Mode::Horizontal)?;
                    self.write_run(color, next.saturating_sub(position))?;
                    self.write_run(!color, second.saturating_sub(next))?;
                    position = second;
                }
                break;
            }
        }
        std::mem::swap(&mut self.reference, &mut self.current);
        Ok(())
    }

    fn encode(&mut self, packed: &[u8], stride: usize, width: u32) -> EncodeResult<()> {
        for row in packed.chunks_exact(stride) {
            self.encode_row(row, width)?;
        }
        self.write_bits(EDFB_HALF)?;
        self.write_bits(EDFB_HALF)
    }
}

pub(super) fn encode_ccitt_group4(
    packed: &[u8],
    width: u32,
    height: u32,
    max_encoded_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    let stride = usize::try_from(width.div_ceil(8))
        .map_err(|_| Error::Invalid("bilevel image row is too wide".to_owned()))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| Error::Invalid("bilevel image is too tall".to_owned()))?;
    let expected = stride
        .checked_mul(height_usize)
        .ok_or_else(|| Error::Invalid("bilevel image dimensions overflow".to_owned()))?;
    if stride == 0 || packed.len() != expected {
        return Err(Error::Invalid(format!(
            "invalid bilevel dimensions {width}x{height}: {} bytes, expected {expected}",
            packed.len()
        )));
    }
    if max_encoded_bytes < 3 {
        return Ok(None);
    }
    let mut encoder = PackedEncoder {
        writer: VecWriter::new(),
        remaining_bits: max_encoded_bytes.saturating_mul(8),
        reference: Vec::new(),
        current: Vec::new(),
    };
    match encoder.encode(packed, stride, width) {
        Ok(()) => Ok(Some(encoder.writer.finish())),
        Err(EncodeError::Budget) => Ok(None),
        Err(EncodeError::InvalidCode) => Err(Error::Invalid("invalid CCITT code".into())),
    }
}
