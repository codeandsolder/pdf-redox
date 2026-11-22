use crate::{OwnedDictionary, OwnedObject, Result};
use std::collections::BTreeMap;

mod ccitt;
use ccitt::encode_ccitt_group4;

const CCITT_EXTRA_DICTIONARY_BYTES: usize = 96;

pub const fn estimated_bilevel_stream_cost(data_len: usize, codec: BilevelCodec) -> usize {
    data_len.saturating_add(match codec {
        BilevelCodec::Flate => 0,
        BilevelCodec::CcittGroup4 => CCITT_EXTRA_DICTIONARY_BYTES,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BilevelCodec {
    Flate,
    CcittGroup4,
}

#[derive(Debug)]
pub struct BilevelEncodedData {
    pub data: Vec<u8>,
    pub codec: BilevelCodec,
}

#[derive(Debug)]
pub struct BilevelImagePayload {
    pub data: Vec<u8>,
    pub dictionary: OwnedDictionary,
    pub codec: BilevelCodec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BilevelRaster {
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
        Some(Self {
            width,
            height,
            packed: pack_binary_samples::<true>(data, width, height, 1)?,
        })
    }

    pub(crate) fn from_binary_gray_samples(data: &[u8], width: u32, height: u32) -> Option<Self> {
        Some(Self {
            width,
            height,
            packed: pack_binary_samples::<false>(data, width, height, 1)?,
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

    pub(crate) fn encode(&self, flate_level: crate::FlateLevel) -> Result<BilevelEncodedData> {
        encode_best_bilevel_data(&self.packed, self.width, self.height, flate_level)
    }

    pub(crate) fn encode_image_mask(
        &self,
        flate_level: crate::FlateLevel,
    ) -> Result<BilevelImagePayload> {
        let encoded = self.encode(flate_level)?;
        Ok(BilevelImagePayload {
            dictionary: image_mask_dictionary(self.width, self.height, encoded.codec),
            codec: encoded.codec,
            data: encoded.data,
        })
    }
}

/// Validate and collect the low bits of eight binary bytes in one word.
/// Multiplication by 255 expands each spaced low bit into its own byte;
/// equality proves that every original sample was either 0 or 255.
fn binary_byte(samples: [u8; 8]) -> Option<u8> {
    let word = u64::from_le_bytes(samples);
    let low_bits = word & 0x0101_0101_0101_0101;
    if low_bits * 255 != word {
        return None;
    }
    // The diagonal products put sample i into bit 63-i. All cross
    // products are below the top byte or wrap out, without carries.
    Some(low_bits.wrapping_mul(0x8040_2010_0804_0201).to_le_bytes()[7])
}

/// Pack one bit per sample, preserving row boundaries for arbitrary
/// component counts. The stencil polarity inverts only active samples,
/// never the unused low padding bits of a row's final byte.
pub(crate) fn pack_binary_samples<const INVERT: bool>(
    data: &[u8],
    width: u32,
    height: u32,
    components: usize,
) -> Option<Vec<u8>> {
    let samples_per_row = usize::try_from(width).ok()?.checked_mul(components)?;
    let height = usize::try_from(height).ok()?;
    if components == 0 || data.len() != samples_per_row.checked_mul(height)? {
        return None;
    }
    if data.is_empty() {
        return Some(Vec::new());
    }
    let mut packed = Vec::with_capacity(samples_per_row.div_ceil(8).checked_mul(height)?);
    let polarity = if INVERT { u8::MAX } else { 0 };
    for row in data.chunks_exact(samples_per_row) {
        let mut groups = row.chunks_exact(8);
        for group in &mut groups {
            packed.push(binary_byte(group.try_into().ok()?)? ^ polarity);
        }
        let tail = groups.remainder();
        if !tail.is_empty() {
            let mut samples = [0; 8];
            samples[..tail.len()].copy_from_slice(tail);
            let active = u8::MAX << (8 - tail.len());
            packed.push((binary_byte(samples)? ^ polarity) & active);
        }
    }
    Some(packed)
}

/// Admission needs only a binary alpha proof, not an allocated mask.
pub(crate) fn is_binary_sample_plane(data: &[u8], width: u32, height: u32) -> bool {
    usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(usize::try_from(height).ok()?))
        == Some(data.len())
        && data.iter().all(|&sample| matches!(sample, 0 | 255))
}

pub fn set_bilevel_filter(
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

pub fn encode_best_bilevel_data(
    packed: &[u8],
    width: u32,
    height: u32,
    flate_level: crate::FlateLevel,
) -> Result<BilevelEncodedData> {
    let flate = crate::stream_codec::encode_flate(packed, flate_level)?;
    // Preserve the extra dictionary cost and strict tie-break for Flate.
    let budget = flate.len().saturating_sub(CCITT_EXTRA_DICTIONARY_BYTES + 1);
    Ok(encode_ccitt_group4(packed, width, height, budget)?.map_or(
        BilevelEncodedData {
            data: flate,
            codec: BilevelCodec::Flate,
        },
        |data| BilevelEncodedData {
            data,
            codec: BilevelCodec::CcittGroup4,
        },
    ))
}

#[cfg(test)]
pub fn encode_best_image_mask(
    packed: &[u8],
    width: u32,
    height: u32,
    flate_level: crate::FlateLevel,
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
    use crate::Error;
    use fax::{
        Color, VecWriter,
        decoder::{decode_g4, pels},
        encoder::Encoder,
    };
    use std::convert::Infallible;

    fn infallible<T>(value: std::result::Result<T, Infallible>) -> T {
        match value {
            Ok(value) => value,
            Err(never) => match never {},
        }
    }

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
    fn binary_word_projection_is_exact_for_every_pattern() {
        for pattern in 0..=255u8 {
            let samples = std::array::from_fn(|i| if pattern & (0x80 >> i) == 0 { 0 } else { 255 });
            assert_eq!(binary_byte(samples), Some(pattern));
            for i in 0..8 {
                let mut invalid = samples;
                invalid[i] = 127;
                assert_eq!(binary_byte(invalid), None);
            }
        }
    }

    #[test]
    fn binary_packing_preserves_polarity_components_and_row_tail() {
        for width in 0..=20u32 {
            for height in 0..=3u32 {
                for components in 1..=4 {
                    let row_samples = usize::try_from(width).unwrap_or_default() * components;
                    let data = (0..row_samples * usize::try_from(height).unwrap_or_default())
                        .map(|i| if (i ^ (i >> 2)) % 3 == 0 { 255 } else { 0 })
                        .collect::<Vec<_>>();
                    for invert in [false, true] {
                        let row_bytes = row_samples.div_ceil(8);
                        let mut expected =
                            vec![0; row_bytes * usize::try_from(height).unwrap_or_default()];
                        if row_samples != 0 {
                            for (y, row) in data.chunks_exact(row_samples).enumerate() {
                                for (x, &sample) in row.iter().enumerate() {
                                    if (sample == 255) != invert {
                                        expected[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
                                    }
                                }
                            }
                        }
                        let actual = if invert {
                            pack_binary_samples::<true>(&data, width, height, components)
                        } else {
                            pack_binary_samples::<false>(&data, width, height, components)
                        };
                        assert_eq!(actual, Some(expected));
                    }
                }
            }
        }
        assert_eq!(pack_binary_samples::<false>(&[], 1, 1, 0), None);
        assert_eq!(pack_binary_samples::<false>(&[0, 127], 2, 1, 1), None);
        assert_eq!(pack_binary_samples::<true>(&[0, 127], 2, 1, 1), None);
        assert_eq!(pack_binary_samples::<true>(&[0], 2, 1, 1), None);
        assert!(is_binary_sample_plane(&[0, 255], 2, 1));
        assert!(!is_binary_sample_plane(&[0, 127], 2, 1));
        assert!(!is_binary_sample_plane(&[0], 2, 1));
    }

    #[test]
    fn group4_roundtrips_all_two_row_five_bit_patterns() -> Result<()> {
        for first in 0u8..32 {
            for second in 0u8..32 {
                let packed = [packed_row(first, 5), packed_row(second, 5)].concat();
                let encoded = encode_ccitt_group4(&packed, 5, 2, usize::MAX)?.ok_or_else(|| {
                    Error::Invalid("uncapped CCITT encoding rejected fixture".into())
                })?;
                assert_eq!(
                    encode_ccitt_group4(&packed, 5, 2, encoded.len())?.as_ref(),
                    Some(&encoded)
                );
                assert!(encode_ccitt_group4(&packed, 5, 2, encoded.len() - 1)?.is_none());
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
        Ok(())
    }

    #[test]
    fn capped_group4_preserves_original_bytes_and_codec_selection() -> Result<()> {
        for width in [1u32, 7, 8, 9, 31, 63, 64, 65, 127, 128, 129, 257, 2625] {
            let height = 32;
            let stride = usize::try_from(width.div_ceil(8)).unwrap_or(0);
            let width_usize = usize::try_from(width).unwrap_or(0);
            for pattern in [0u8, 0xff, 0x55, 0xaa, 0x31] {
                let mut packed = vec![pattern; stride * 32];
                if pattern == 0x31 {
                    for (index, value) in packed.iter_mut().enumerate() {
                        *value = (index.wrapping_mul(97) ^ index.rotate_left(3)).to_le_bytes()[0];
                    }
                }
                let mut reference = Encoder::new(VecWriter::new());
                for row in packed.chunks_exact(stride) {
                    infallible(reference.encode_line(
                        (0..width_usize).map(|x| {
                            if row[x / 8] & (0x80 >> (x % 8)) == 0 {
                                Color::Black
                            } else {
                                Color::White
                            }
                        }),
                        width,
                    ));
                }
                let expected = infallible(reference.finish()).finish();
                assert_eq!(
                    encode_ccitt_group4(&packed, width, height, expected.len())?,
                    Some(expected.clone())
                );
                assert!(encode_ccitt_group4(&packed, width, height, expected.len() - 1)?.is_none());
                let flate = crate::stream_codec::encode_flate(&packed, crate::FlateLevel::SIZE)?;
                let best =
                    encode_best_bilevel_data(&packed, width, height, crate::FlateLevel::SIZE)?;
                if estimated_bilevel_stream_cost(expected.len(), BilevelCodec::CcittGroup4)
                    < flate.len()
                {
                    assert_eq!(best.codec, BilevelCodec::CcittGroup4);
                    assert_eq!(best.data, expected);
                } else {
                    assert_eq!(best.codec, BilevelCodec::Flate);
                    assert_eq!(best.data, flate);
                }
            }
        }
        assert!(encode_ccitt_group4(&[], 0, 1, 0).is_err());
        assert!(encode_ccitt_group4(&[], 8, 1, 0).is_err());
        Ok(())
    }

    #[test]
    fn moving_thin_feature_prefers_group4() -> Result<()> {
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
        let payload = encode_best_image_mask(&packed, width, height, crate::FlateLevel::SIZE)?;
        assert!(matches!(
            payload.dictionary.get(b"Filter".as_slice()),
            Some(OwnedObject::Name(name)) if name == b"CCITTFaxDecode"
        ));
        Ok(())
    }
}
