pub(crate) mod flate {
    use crate::Limit;
    use crate::filter::predictor;
    use crate::object::Dict;
    use alloc::vec::Vec;

    pub(crate) fn decode(data: &[u8], params: &Dict<'_>, limit: Limit<usize>) -> Option<Vec<u8>> {
        let data = zlib_body(data).unwrap_or(data);
        // Broken encoders can refer to data before the start, which should read as zeros.
        let decoded = match inflate(data, false, limit) {
            Ok(decoded) => decoded,
            Err(InflateError::Limit) => return None,
            Err(InflateError::Partial(_)) => match inflate(data, true, limit) {
                Ok(decoded) | Err(InflateError::Partial(decoded)) => decoded,
                Err(InflateError::Limit) => return None,
            },
        };
        predictor::apply(decoded, params)
    }

    const WINDOW_SIZE: usize = 32 * 1024;

    enum InflateError {
        Limit,
        Partial(Vec<u8>),
    }

    // One extra byte distinguishes a stream at the limit from an overflowing one.
    fn grow_output(out: &mut Vec<u8>, limit: Limit<usize>, prefix: usize) {
        let max = limit
            .bound()
            .map_or(usize::MAX, |n| n.saturating_add(1).saturating_add(prefix));
        out.resize(out.len().saturating_mul(2).max(WINDOW_SIZE).min(max), 0);
    }

    fn zlib_body(data: &[u8]) -> Option<&[u8]> {
        let [cmf, flg, body @ ..] = data else {
            return None;
        };

        ((cmf & 0x0f) == 0x08
            && (u16::from(*cmf) << 8 | u16::from(*flg)).is_multiple_of(31)
            && (flg & 0x20) == 0)
            .then_some(body)
    }

    #[cfg(feature = "unsafe")]
    fn inflate(
        mut data: &[u8],
        zero_window: bool,
        limit: Limit<usize>,
    ) -> Result<Vec<u8>, InflateError> {
        use zlib_rs::{Inflate, InflateFlush, Status};

        static ZEROS: [u8; WINDOW_SIZE] = [0; WINDOW_SIZE];

        let mut inflate = Inflate::new(false, 15);
        if zero_window {
            inflate.set_dictionary(&ZEROS).ok();
        }
        let mut out = Vec::new();
        let mut written = 0;

        loop {
            grow_output(&mut out, limit, 0);

            let (total_in, total_out) = (inflate.total_in(), inflate.total_out());
            let result = inflate.decompress(data, &mut out[written..], InflateFlush::Finish);
            let consumed = (inflate.total_in() - total_in) as usize;
            let produced = (inflate.total_out() - total_out) as usize;
            written += produced;
            if !limit.permits(written) {
                return Err(InflateError::Limit);
            }
            data = data.get(consumed..).unwrap_or_default();

            match result {
                Ok(Status::Ok | Status::BufError) if consumed != 0 || produced != 0 => {}
                Ok(_) => {
                    out.truncate(written);
                    return Ok(out);
                }
                Err(_) => {
                    out.truncate(written);
                    return Err(InflateError::Partial(out));
                }
            }
        }
    }

    #[cfg(not(feature = "unsafe"))]
    fn inflate(
        mut data: &[u8],
        zero_window: bool,
        limit: Limit<usize>,
    ) -> Result<Vec<u8>, InflateError> {
        use alloc::boxed::Box;
        use alloc::vec;
        use miniz_oxide::inflate::TINFLStatus;
        use miniz_oxide::inflate::core::inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        use miniz_oxide::inflate::core::{DecompressorOxide, decompress};

        let start = if zero_window { WINDOW_SIZE } else { 0 };
        let mut decompressor = Box::<DecompressorOxide>::default();
        let mut out = vec![0; start];
        let mut written = start;

        loop {
            grow_output(&mut out, limit, start);

            let (status, consumed, produced) = decompress(
                &mut decompressor,
                data,
                &mut out,
                written,
                TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF,
            );
            written += produced;
            if !limit.permits(written - start) {
                return Err(InflateError::Limit);
            }
            data = data.get(consumed..).unwrap_or_default();

            if status != TINFLStatus::HasMoreOutput {
                out.truncate(written);
                out.drain(..start);

                return match status {
                    TINFLStatus::Done | TINFLStatus::FailedCannotMakeProgress => Ok(out),
                    _ => Err(InflateError::Partial(out)),
                };
            }
        }
    }
}

pub(crate) mod lzw {
    use crate::Limit;
    use crate::bit_reader::BitReader;
    use crate::filter::predictor;
    use crate::object::Dict;
    use crate::object::dict::keys::EARLY_CHANGE;
    use alloc::vec;
    use alloc::vec::Vec;

    pub(crate) fn decode(data: &[u8], params: &Dict<'_>, limit: Limit<usize>) -> Option<Vec<u8>> {
        let early_change = params
            .get::<u8>(EARLY_CHANGE)
            .map(|e| e != 0)
            .unwrap_or(true);
        let decoded = decode_impl(data, early_change, limit)?;

        predictor::apply(decoded, params)
    }

    const CLEAR_TABLE: usize = 256;
    const EOD: usize = 257;
    const MAX_ENTRIES: usize = 4096;
    const INITIAL_SIZE: u16 = 258;

    fn decode_impl(data: &[u8], early_change: bool, limit: Limit<usize>) -> Option<Vec<u8>> {
        let mut table = Table::new(early_change);
        let mut bit_size = table.code_length();
        let mut reader = BitReader::new(data);
        let mut decoded = vec![];
        let mut prev = None;

        loop {
            let next = match reader.read(bit_size) {
                Some(code) => code as usize,
                None => {
                    return Some(decoded);
                }
            };

            match next {
                CLEAR_TABLE => {
                    table.clear();
                    prev = None;
                    bit_size = table.code_length();
                }
                EOD => return Some(decoded),
                new => {
                    if new > table.size() {
                        return None;
                    }

                    if new < table.size() {
                        let entry = table.get(new)?;
                        let first_byte = entry[0];
                        if !limit.permits(decoded.len().checked_add(entry.len())?) {
                            return None;
                        }
                        decoded.extend_from_slice(entry);

                        if let Some(prev_code) = prev {
                            table.register(prev_code, first_byte);
                        }
                    } else if new == table.size() && prev.is_some() {
                        let prev_code = prev.unwrap();
                        let prev_entry = table.get(prev_code)?;
                        let first_byte = prev_entry[0];

                        let new_entry = table.register(prev_code, first_byte)?;
                        if !limit.permits(decoded.len().checked_add(new_entry.len())?) {
                            return None;
                        }
                        decoded.extend_from_slice(new_entry);
                    } else {
                        return None;
                    }

                    bit_size = table.code_length();
                    prev = Some(new);
                }
            }
        }
    }

    struct Table {
        early_change: bool,
        entries: Vec<Option<Vec<u8>>>,
    }

    impl Table {
        fn new(early_change: bool) -> Self {
            let mut entries: Vec<_> = (0..=255).map(|b| Some(vec![b])).collect();

            entries.push(None);
            entries.push(None);

            Self {
                early_change,
                entries,
            }
        }

        fn push(&mut self, entry: Vec<u8>) -> Option<&[u8]> {
            if self.entries.len() >= MAX_ENTRIES {
                None
            } else {
                self.entries.push(Some(entry));
                self.entries.last()?.as_ref().map(|v| &**v)
            }
        }

        fn register(&mut self, prev: usize, new_byte: u8) -> Option<&[u8]> {
            let prev_entry = self.get(prev)?;

            let mut new_entry = Vec::with_capacity(prev_entry.len() + 1);
            new_entry.extend(prev_entry);
            new_entry.push(new_byte);
            self.push(new_entry)
        }

        fn get(&self, index: usize) -> Option<&[u8]> {
            self.entries.get(index)?.as_ref().map(|v| &**v)
        }

        fn clear(&mut self) {
            self.entries.truncate(INITIAL_SIZE as usize);
        }

        fn size(&self) -> usize {
            self.entries.len()
        }

        fn code_length(&self) -> u8 {
            const TEN: usize = 512;
            const ELEVEN: usize = 1024;
            const TWELVE: usize = 2048;

            let adjusted = self.entries.len() + (if self.early_change { 1 } else { 0 });

            if adjusted >= TWELVE {
                12
            } else if adjusted >= ELEVEN {
                11
            } else if adjusted >= TEN {
                10
            } else {
                9
            }
        }
    }
}

#[cfg(test)]
#[rustfmt::skip]
mod tests {
    use crate::filter::lzw_flate::{flate, lzw};
    use crate::object::Dict;
    use crate::Limit;

    #[test]
    fn decode_lzw() {
        let input = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        let decoded = lzw::decode(&input, &Dict::default(), crate::Limits::DEFAULT.max_decoded_stream_size).unwrap();

        assert_eq!(decoded, vec![45, 45, 45, 45, 45, 65, 45, 45, 45, 66]);
    }

    #[test]
    fn rejects_flate_expansion_at_configured_limit() {
        let input = miniz_oxide::deflate::compress_to_vec_zlib(&vec![0; 128 * 1024], 6);
        assert!(flate::decode(&input, &Dict::default(), Limit::AtMost(64 * 1024)).is_none());
        assert_eq!(
            flate::decode(&input, &Dict::default(), Limit::AtMost(128 * 1024)).map(|d| d.len()),
            Some(128 * 1024),
        );
        assert_eq!(
            flate::decode(&input, &Dict::default(), Limit::Unlimited).map(|d| d.len()),
            Some(128 * 1024),
        );
    }

    #[test]
    fn lzw_expansion_limit() {
        let input = [0x80, 0x0B, 0x60, 0x50, 0x22, 0x0C, 0x0C, 0x85, 0x01];
        assert!(lzw::decode(&input, &Dict::default(), Limit::AtMost(9)).is_none());
        assert_eq!(lzw::decode(&input, &Dict::default(), Limit::AtMost(10)).unwrap().len(), 10);
    }

    #[test]
    fn decode_flate_zlib() {
        let input = [
            0x78, 0x9c, 0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x7, 0x0, 0x5, 0x8c, 0x1, 0xf5,
        ];

        let decoded = flate::decode(&input, &Dict::default(), crate::Limits::DEFAULT.max_decoded_stream_size).unwrap();
        assert_eq!(decoded, b"Hello");
    }

    #[test]
    fn decode_flate() {
        let input = [0xf3, 0x48, 0xcd, 0xc9, 0xc9, 0x7, 0x0];

        let decoded = flate::decode(&input, &Dict::default(), crate::Limits::DEFAULT.max_decoded_stream_size).unwrap();
        assert_eq!(decoded, b"Hello");
    }
}
