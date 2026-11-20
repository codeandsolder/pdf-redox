use crate::Limit;
use crate::reader::Reader;
use alloc::vec;
use alloc::vec::Vec;

pub(crate) fn decode(data: &[u8], limit: Limit<usize>) -> Option<Vec<u8>> {
    let mut reader = Reader::new(data);
    let mut decoded = vec![];

    loop {
        let length = reader.read_byte()?;

        match length {
            128 => break,
            0..=127 => {
                // PDFBOX-3990, just abort early if stream is invalid.
                let Some(bytes) = reader.read_bytes(length as usize + 1) else {
                    break;
                };

                if !limit.permits(decoded.len().checked_add(bytes.len())?) {
                    return None;
                }
                decoded.extend(bytes);
            }
            _ => {
                let length = 257 - length as usize;
                if !limit.permits(decoded.len().checked_add(length)?) {
                    return None;
                }
                decoded.resize(decoded.len() + length, reader.read_byte()?);
            }
        }
    }

    Some(decoded)
}

#[cfg(test)]
mod tests {
    use crate::filter::run_length::decode;

    #[test]
    fn run_length_expansion_limit() {
        let input = [129, b'A', 128];
        assert!(decode(&input, crate::Limit::AtMost(127)).is_none());
        assert_eq!(
            decode(&input, crate::Limit::AtMost(128)).unwrap(),
            vec![b'A'; 128]
        );
    }

    #[test]
    fn run_length() {
        let input = vec![4, 10, 11, 12, 13, 14, 253, 3, 128];
        assert_eq!(
            decode(&input, crate::Limits::DEFAULT.max_decoded_stream_size).unwrap(),
            vec![10, 11, 12, 13, 14, 3, 3, 3, 3]
        );
    }
}
