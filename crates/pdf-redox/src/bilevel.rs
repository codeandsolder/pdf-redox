use crate::{Error, OwnedDictionary, OwnedObject, Result};
use fax::{Color, VecWriter, encoder::Encoder};
use flate2::{Compression, write::ZlibEncoder};
use std::{collections::BTreeMap, convert::Infallible, io::Write as _};

const CCITT_EXTRA_DICTIONARY_BYTES: usize = 96;

pub(crate) fn estimated_bilevel_stream_cost(data_len: usize, codec: BilevelCodec) -> usize {
    data_len.saturating_add(match codec {
        BilevelCodec::Flate => 0,
        BilevelCodec::CcittGroup4 => CCITT_EXTRA_DICTIONARY_BYTES,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BilevelCodec {
    Flate,
    CcittGroup4,
}

#[derive(Debug)]
pub(crate) struct BilevelEncodedData {
    pub data: Vec<u8>,
    pub codec: BilevelCodec,
}

#[derive(Debug)]
pub(crate) struct BilevelImagePayload {
    pub data: Vec<u8>,
    pub dictionary: OwnedDictionary,
    pub codec: BilevelCodec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BilevelRaster {
    width: u32,
    height: u32,
    // PDF ImageMask sample semantics used throughout pdf-redox:
    // zero paints with the current nonstroking color; one is transparent.
    packed: Vec<u8>,
}

impl BilevelRaster {
    pub(crate) fn transparent(width: u32, height: u32) -> Option<Self> {
        let row_bytes = usize::try_from(width.div_ceil(8)).ok()?;
        let height_usize = usize::try_from(height).ok()?;
        let len = row_bytes.checked_mul(height_usize)?;
        let mut packed = vec![0xff; len];
        let active_tail_bits = width % 8;
        if active_tail_bits != 0 {
            let tail_mask = u8::MAX << (8 - active_tail_bits);
            for row in packed.chunks_exact_mut(row_bytes) {
                if let Some(last) = row.last_mut() {
                    *last &= tail_mask;
                }
            }
        }
        Some(Self {
            width,
            height,
            packed,
        })
    }

    pub(crate) fn from_image_mask_alpha(data: &[u8], width: u32, height: u32) -> Option<Self> {
        let width_usize = usize::try_from(width).ok()?;
        let height_usize = usize::try_from(height).ok()?;
        if data.len() != width_usize.checked_mul(height_usize)?
            || !data.iter().all(|&value| matches!(value, 0 | 255))
        {
            return None;
        }

        let mut raster = Self::transparent(width, height)?;
        for y in 0..height_usize {
            for x in 0..width_usize {
                if data[y * width_usize + x] != 0 {
                    raster.paint(x, y);
                }
            }
        }
        Some(raster)
    }

    pub(crate) fn from_binary_gray_samples(data: &[u8], width: u32, height: u32) -> Option<Self> {
        let width_usize = usize::try_from(width).ok()?;
        let height_usize = usize::try_from(height).ok()?;
        if data.len() != width_usize.checked_mul(height_usize)?
            || !data.iter().all(|&value| matches!(value, 0 | 255))
        {
            return None;
        }
        let row_bytes = usize::try_from(width.div_ceil(8)).ok()?;
        let mut packed = vec![0u8; row_bytes.checked_mul(height_usize)?];
        for y in 0..height_usize {
            for x in 0..width_usize {
                if data[y * width_usize + x] == 255 {
                    packed[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                }
            }
        }
        Some(Self {
            width,
            height,
            packed,
        })
    }

    #[cfg(test)]
    pub(crate) fn packed(&self) -> &[u8] {
        &self.packed
    }

    pub(crate) fn paint(&mut self, x: usize, y: usize) {
        let Ok(row_bytes) = usize::try_from(self.width.div_ceil(8)) else {
            return;
        };
        let Some(byte) = self
            .packed
            .get_mut(y.saturating_mul(row_bytes).saturating_add(x / 8))
        else {
            return;
        };
        *byte &= !(0x80 >> (x % 8));
    }

    pub(crate) fn encode(&self, flate_level: i32) -> Result<BilevelEncodedData> {
        encode_best_bilevel_data(&self.packed, self.width, self.height, flate_level)
    }

    pub(crate) fn encode_image_mask(&self, flate_level: i32) -> Result<BilevelImagePayload> {
        let encoded = self.encode(flate_level)?;
        Ok(BilevelImagePayload {
            dictionary: image_mask_dictionary(self.width, self.height, encoded.codec),
            codec: encoded.codec,
            data: encoded.data,
        })
    }
}

fn infallible<T>(value: std::result::Result<T, Infallible>) -> T {
    match value {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

pub(crate) fn compress_flate(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let level = u32::try_from(level.clamp(0, 9)).unwrap_or(9);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn encode_ccitt_group4(packed: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let stride = usize::try_from(width.div_ceil(8))
        .map_err(|_| Error::Invalid("bilevel image row is too wide".to_owned()))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| Error::Invalid("bilevel image is too tall".to_owned()))?;
    let expected = stride
        .checked_mul(height_usize)
        .ok_or_else(|| Error::Invalid("bilevel image dimensions overflow".to_owned()))?;
    if packed.len() != expected {
        return Err(Error::Invalid(format!(
            "bilevel image payload has {} bytes, expected {expected}",
            packed.len()
        )));
    }

    let writer = VecWriter::with_capacity(packed.len().saturating_mul(8));
    let mut encoder = Encoder::new(writer);
    for row in packed.chunks_exact(stride) {
        let pixels = (0..usize::try_from(width).unwrap_or(usize::MAX)).map(|x| {
            let bit = (row[x >> 3] >> (7 - (x & 7))) & 1;
            if bit == 0 { Color::Black } else { Color::White }
        });
        infallible(encoder.encode_line(pixels, width));
    }
    Ok(infallible(encoder.finish()).finish())
}

pub(crate) fn set_bilevel_filter(
    dictionary: &mut OwnedDictionary,
    width: u32,
    height: u32,
    codec: BilevelCodec,
) {
    dictionary.remove(b"DecodeParms".as_slice());
    match codec {
        BilevelCodec::Flate => {
            dictionary.insert(
                b"Filter".to_vec(),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            );
        }
        BilevelCodec::CcittGroup4 => {
            dictionary.insert(
                b"Filter".to_vec(),
                OwnedObject::Name(b"CCITTFaxDecode".to_vec()),
            );
            dictionary.insert(
                b"DecodeParms".to_vec(),
                OwnedObject::Dictionary(BTreeMap::from([
                    (b"K".to_vec(), OwnedObject::Integer(-1)),
                    (b"Columns".to_vec(), OwnedObject::Integer(i64::from(width))),
                    (b"Rows".to_vec(), OwnedObject::Integer(i64::from(height))),
                    (b"BlackIs1".to_vec(), OwnedObject::Boolean(false)),
                ])),
            );
        }
    }
}

fn image_mask_dictionary(width: u32, height: u32, codec: BilevelCodec) -> OwnedDictionary {
    let mut dictionary = BTreeMap::from([
        (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
        (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
        (b"Width".to_vec(), OwnedObject::Integer(i64::from(width))),
        (b"Height".to_vec(), OwnedObject::Integer(i64::from(height))),
        (b"ImageMask".to_vec(), OwnedObject::Boolean(true)),
        (b"BitsPerComponent".to_vec(), OwnedObject::Integer(1)),
    ]);
    set_bilevel_filter(&mut dictionary, width, height, codec);
    dictionary
}

pub(crate) fn encode_best_bilevel_data(
    packed: &[u8],
    width: u32,
    height: u32,
    flate_level: i32,
) -> Result<BilevelEncodedData> {
    let flate = compress_flate(packed, flate_level)?;
    let ccitt = encode_ccitt_group4(packed, width, height)?;

    let codec =
        if estimated_bilevel_stream_cost(ccitt.len(), BilevelCodec::CcittGroup4) < flate.len() {
            BilevelCodec::CcittGroup4
        } else {
            BilevelCodec::Flate
        };
    let data = match codec {
        BilevelCodec::Flate => flate,
        BilevelCodec::CcittGroup4 => ccitt,
    };
    Ok(BilevelEncodedData { data, codec })
}

#[cfg(test)]
pub(crate) fn encode_best_image_mask(
    packed: &[u8],
    width: u32,
    height: u32,
    flate_level: i32,
) -> Result<BilevelImagePayload> {
    let encoded = encode_best_bilevel_data(packed, width, height, flate_level)?;
    Ok(BilevelImagePayload {
        dictionary: image_mask_dictionary(width, height, encoded.codec),
        codec: encoded.codec,
        data: encoded.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fax::decoder::{decode_g4, pels};

    fn packed_row(value: u8, width: u32) -> Vec<u8> {
        let stride = usize::try_from(width.div_ceil(8)).unwrap_or(0);
        let mut row = vec![0u8; stride];
        for x in 0..width {
            if value & (1 << (width - 1 - x)) != 0 {
                let index = usize::try_from(x / 8).unwrap_or(0);
                row[index] |= 1 << (7 - (x & 7));
            }
        }
        row
    }

    #[test]
    fn group4_roundtrips_all_two_row_five_bit_patterns() {
        for first in 0u8..32 {
            for second in 0u8..32 {
                let packed = [packed_row(first, 5), packed_row(second, 5)].concat();
                let encoded =
                    encode_ccitt_group4(&packed, 5, 2).unwrap_or_else(|error| panic!("{error}"));
                let mut decoded = Vec::new();
                assert!(
                    decode_g4(encoded.into_iter(), 5, Some(2), |line| {
                        let mut value = 0u8;
                        for color in pels(line, 5) {
                            value <<= 1;
                            if color == Color::White {
                                value |= 1;
                            }
                        }
                        decoded.push(value);
                    })
                    .is_some()
                );
                assert_eq!(decoded, [first, second]);
            }
        }
    }

    #[test]
    fn moving_thin_feature_prefers_group4() {
        let width = 1024u32;
        let height = 1024u32;
        let stride = usize::try_from(width.div_ceil(8)).unwrap_or(0);
        let mut packed = vec![0xff; stride * usize::try_from(height).unwrap_or(0)];
        for y in 0..height {
            let x0 = 64 + y / 4;
            for x in x0..x0 + 16 {
                let row = usize::try_from(y).unwrap_or(0) * stride;
                let byte = usize::try_from(x / 8).unwrap_or(0);
                packed[row + byte] &= !(1 << (7 - (x & 7)));
            }
        }
        let payload = encode_best_image_mask(&packed, width, height, 9)
            .unwrap_or_else(|error| panic!("{error}"));
        assert!(matches!(
            payload.dictionary.get(b"Filter".as_slice()),
            Some(OwnedObject::Name(name)) if name == b"CCITTFaxDecode"
        ));
    }
}
