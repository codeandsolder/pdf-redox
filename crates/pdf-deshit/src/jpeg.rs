/// Strip metadata-only JPEG marker segments without touching quantized DCT
/// coefficients or entropy-coded scan data. Returns None when the input is not
/// a structurally recognizable JPEG or no bytes would be removed.
pub(crate) fn strip_jpeg_metadata(data: &[u8], aggressive: bool) -> Option<(Vec<u8>, usize)> {
    if data.len() < 4 || data[0..2] != [0xff, 0xd8] {
        return None;
    }
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..2]);
    let mut p = 2usize;
    let mut removed = 0usize;

    while p < data.len() {
        if data[p] != 0xff {
            return None;
        }
        let marker_start = p;
        while p < data.len() && data[p] == 0xff {
            p += 1;
        }
        if p >= data.len() {
            return None;
        }
        let marker = data[p];
        p += 1;

        // SOS: marker segment plus the remainder of the JPEG is image payload;
        // copy it byte-for-byte and stop parsing metadata.
        if marker == 0xda {
            out.extend_from_slice(&data[marker_start..]);
            break;
        }
        // Standalone markers.
        if marker == 0xd9 || marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            out.extend_from_slice(&data[marker_start..p]);
            if marker == 0xd9 {
                break;
            }
            continue;
        }
        if p + 2 > data.len() {
            return None;
        }
        let len = u16::from_be_bytes([data[p], data[p + 1]]) as usize;
        if len < 2 || p + len > data.len() {
            return None;
        }
        let end = p + len;

        let safe_metadata = matches!(marker, 0xe1 | 0xed | 0xfe); // APP1, APP13, COM
        let aggressive_metadata =
            aggressive && (0xe0..=0xef).contains(&marker) && !matches!(marker, 0xe0 | 0xe2 | 0xee); // retain JFIF, ICC, Adobe transform
        if safe_metadata || aggressive_metadata {
            removed += end - marker_start;
        } else {
            out.extend_from_slice(&data[marker_start..end]);
        }
        p = end;
    }

    (removed > 0).then_some((out, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_exif_without_touching_scan() {
        let jpeg = [
            0xff, 0xd8, 0xff, 0xe1, 0x00, 0x06, b'E', b'X', b'I', b'F', 0xff, 0xda, 0x00, 0x02,
            0x11, 0x22, 0xff, 0xd9,
        ];
        let Some((out, n)) = strip_jpeg_metadata(&jpeg, false) else {
            panic!("test JPEG should contain removable EXIF metadata");
        };
        assert_eq!(n, 8);
        assert_eq!(
            out,
            [0xff, 0xd8, 0xff, 0xda, 0x00, 0x02, 0x11, 0x22, 0xff, 0xd9]
        );
    }
}
