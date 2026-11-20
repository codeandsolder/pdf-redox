// Keep in sync with `hayro-postscript/src/string/ascii_hex.rs`.

use crate::Limit;
use crate::trivia::is_white_space_character;
use crate::util::ByteBuf;
use alloc::vec::Vec;

pub(crate) fn decode(data: &[u8], limit: Limit<usize>) -> Option<Vec<u8>> {
    decode_with_limit(data, limit)
}

pub(crate) fn decode_into<B: ByteBuf>(data: &[u8]) -> Option<B> {
    decode_with_limit(data, Limit::Unlimited)
}

fn decode_with_limit<B: ByteBuf>(data: &[u8], limit: Limit<usize>) -> Option<B> {
    let mut has_whitespace = false;
    let mut end = data.len();
    let mut digits = 0usize;

    for (idx, byte) in data.iter().enumerate() {
        let whitespace = is_white_space_character(*byte);
        has_whitespace |= whitespace;

        if *byte == b'>' {
            end = idx;
            break;
        }
        digits += usize::from(!whitespace);
    }

    let data = &data[..end];
    let len = digits.div_ceil(2);
    if !limit.permits(len) {
        return None;
    }
    let mut decoded = B::with_capacity(len);

    if !has_whitespace {
        let (chunks, remainder) = data.as_chunks::<2>();

        for &[hi, lo] in chunks {
            decoded.push(decode_hex_digit(hi)? << 4 | decode_hex_digit(lo)?);
        }

        if let [hi] = remainder {
            decoded.push(decode_hex_digit(*hi)? << 4);
        }
    } else {
        let mut iter = data.iter().copied();
        let mut read_byte = || -> Option<u8> {
            loop {
                let b = iter.next()?;
                if !is_white_space_character(b) {
                    return Some(b);
                }
            }
        };

        loop {
            match (read_byte(), read_byte()) {
                (Some(hi), Some(lo)) => {
                    decoded.push(decode_hex_digit(hi)? << 4 | decode_hex_digit(lo)?);
                }
                (Some(hi), None) => {
                    decoded.push(decode_hex_digit(hi)? << 4);

                    break;
                }
                (None, _) => break,
            }
        }
    }

    Some(decoded)
}

#[inline(always)]
pub(crate) fn decode_hex_digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'A'..=b'F' => Some(c - b'A' + 10),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::{Limit, filter::ascii_hex::decode};

    #[test]
    fn ascii_hex_limit_counts_digits_not_whitespace() {
        assert!(decode(b"  41 42  >", Limit::AtMost(1)).is_none());
        assert_eq!(decode(b"  41 42  >", Limit::AtMost(2)).unwrap(), b"AB");
    }

    #[test]
    fn decode_simple() {
        let input = b"AF3E2901>";
        assert_eq!(
            decode(input, Limit::Unlimited).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x01]
        );
    }

    #[test]
    fn decode_whitespaces() {
        let input = b"AF3   E2   901>";
        assert_eq!(
            decode(input, Limit::Unlimited).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x01]
        );
    }

    #[test]
    fn decode_without_gt() {
        let input = b"AF3E2901";
        assert_eq!(
            decode(input, Limit::Unlimited).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x01]
        );
    }

    #[test]
    fn decode_with_padding() {
        let input = b"AF3E291>";
        assert_eq!(
            decode(input, Limit::Unlimited).unwrap(),
            vec![0xaf, 0x3e, 0x29, 0x10]
        );
    }
}
