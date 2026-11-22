use std::convert::TryFrom;

use crate::{
    maps::{black, mode, white, Mode, EDFB_HALF},
    BitWriter, Color, Transitions,
};

fn absdiff(a: u32, b: u32) -> u32 {
    if a > b {
        a - b
    } else {
        b - a
    }
}

/// Find color changes directly in 64 packed pixels. Each set bit of
/// word XOR (word shifted right with prior color) marks an edge.
struct PackedTransitions<'a> {
    words: std::slice::Chunks<'a, u8>,
    width: u32,
    next_base: u32,
    base: u32,
    previous_white: u64,
    edges: u64,
}

impl<'a> PackedTransitions<'a> {
    fn new(row: &'a [u8], width: u32) -> Self {
        Self {
            words: row.chunks(8),
            width,
            next_base: 0,
            base: 0,
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
                self.edges &= !(1u64 << (63 - index));
                return Some(self.base + index);
            }
            let bytes = self.words.next()?;
            let mut padded = [0u8; 8];
            padded[..bytes.len()].copy_from_slice(bytes);
            let word = u64::from_be_bytes(padded);
            self.base = self.next_base;
            let valid = (self.width - self.base).min(64);
            self.next_base += valid;
            self.edges = (word ^ ((word >> 1) | (self.previous_white << 63)))
                & (u64::MAX << (64 - valid));
            self.previous_white = word & 1;
        }
    }
}

/// Failure to encode a packed row.
#[derive(Debug)]
pub enum PackedLineError<E> {
    InvalidRowLength,
    Write(E),
}

pub struct Encoder<W> {
    writer: W,
    reference: Vec<u32>,
    current: Vec<u32>,
}
fn encode_color<W: BitWriter>(writer: &mut W, color: Color, mut n: u32) -> Result<(), W::Error> {
    let table = match color {
        Color::White => &white::ENTRIES,
        Color::Black => &black::ENTRIES,
    };
    let mut write = |n: u32| {
        let idx = if n >= 64 { 63 + n / 64 } else { n } as usize;
        let (v, bits) = table[idx];
        assert_eq!(v as u32, n);
        //debug!("{}", n);
        writer.write(bits)
    };

    while n >= 2560 {
        write(2560)?;
        n -= 2560;
    }
    if n >= 64 {
        let d = n & !63;
        write(d)?;
        n -= d;
    }

    write(n)
}
impl<W: BitWriter> Encoder<W> {
    pub fn new(writer: W) -> Self {
        Encoder {
            writer,
            reference: vec![],
            current: vec![],
        }
    }
    pub fn encode_line(
        &mut self,
        pels: impl Iterator<Item = Color>,
        width: u32,
    ) -> Result<(), W::Error> {
        let transitions = pels
            .enumerate()
            .scan(Color::White, |state, (i, c)| {
                Some(if c != *state {
                    debug!("  {i} {c:?}");
                    *state = c;
                    Some(i as u32)
                } else {
                    None
                })
            })
            .filter_map(|x| x);
        self.encode_transitions(transitions, width)
    }

    /// Encode a line of MSB-first packed pixels (one is white).
    /// Padding bits after width are ignored. The row must contain exactly
    /// ceil(width / 8) bytes.
    pub fn encode_packed_line(
        &mut self,
        row: &[u8],
        width: u32,
    ) -> Result<(), PackedLineError<W::Error>> {
        let expected = usize::try_from(width / 8 + u32::from(width % 8 != 0))
            .map_err(|_| PackedLineError::InvalidRowLength)?;
        if row.len() != expected {
            return Err(PackedLineError::InvalidRowLength);
        }
        self.encode_transitions(PackedTransitions::new(row, width), width)
            .map_err(PackedLineError::Write)
    }

    fn encode_transitions(
        &mut self,
        mut pels: impl Iterator<Item = u32>,
        width: u32,
    ) -> Result<(), W::Error> {
        let mut color = Color::White;
        let mut transitions = Transitions::new(&self.reference);
        let mut a0 = 0;
        let mut start_of_line = true;
        let writer = &mut self.writer;
        self.current.clear();

        while a0 < width {
            let a1;
            if let Some(a1_) = pels.next() {
                self.current.push(a1_);
                a1 = a1_;
            } else {
                a1 = width;
            }
            loop {
                transitions.seek_back(a0);
                let b1 = transitions
                    .next_color(a0, !color, start_of_line)
                    .unwrap_or(width);
                let b2 = transitions.peek();
                start_of_line = false;
                debug!("  a0={a0}, a1={a1}, b1={:?}, b2={:?}", b1, b2);
                match (b1, b2) {
                    (_b1, Some(b2)) if b2 < a1 => {
                        debug!("  Pass");
                        let bits = mode::encode(Mode::Pass).unwrap();
                        writer.write(bits)?;
                        transitions.skip(1);
                        a0 = b2;
                        continue;
                    }
                    (b1, _) if absdiff(a1, b1) <= 3 => {
                        let delta = a1 as i16 - b1 as i16;
                        debug!("  Vertical({})", delta);
                        let bits = mode::encode(Mode::Vertical(delta as i8)).unwrap();
                        writer.write(bits)?;
                        a0 = a1;
                        color = !color;
                    }
                    _ => {
                        let a2 = match pels.next() {
                            Some(a2) => {
                                self.current.push(a2);
                                a2
                            }
                            None => width,
                        };
                        let a0a1 = a1.saturating_sub(a0);
                        let a1a2 = a2.saturating_sub(a1);
                        debug!("  Horizontal({}, {}) color={color:?}", a0a1, a1a2);
                        let bits = mode::encode(Mode::Horizontal).unwrap();
                        writer.write(bits)?;
                        let c = if a0 + a1 == 0 { Color::White } else { color };
                        encode_color(writer, c, a0a1)?;
                        encode_color(writer, !c, a1a2)?;
                        a0 = a2;
                    }
                }
                break;
            }
        }

        std::mem::swap(&mut self.reference, &mut self.current);
        debug!("next line");
        Ok(())
    }
    pub fn finish(mut self) -> Result<W, W::Error> {
        self.writer.write(EDFB_HALF)?;
        self.writer.write(EDFB_HALF)?;
        Ok(self.writer)
    }
}
