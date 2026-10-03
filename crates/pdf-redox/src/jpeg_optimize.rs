use crate::{EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData};
use flpdf::ObjectHandle as FlObjectHandle;
use libjpeg_turbo_rs::{
    MarkerCopyMode, TransformOp, TransformOptions, transform_jpeg_with_options,
};

const MIN_JPEG_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JpegEntropyStats {
    pub streams_considered: usize,
    pub streams_optimized: usize,
    pub original_encoded_bytes: usize,
    pub optimized_encoded_bytes: usize,
}

impl JpegEntropyStats {
    pub(crate) const fn saved_bytes(self) -> usize {
        self.original_encoded_bytes
            .saturating_sub(self.optimized_encoded_bytes)
    }
}

fn is_safe_lone_dct(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<bool> {
    if dictionary.contains_key(b"F".as_slice()) {
        return Ok(false);
    }

    let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
        return Ok(false);
    };
    if !matches!(
        document.resolve_owned_value(filter)?,
        Some(OwnedObject::Name(name)) if matches!(name.as_slice(), b"DCTDecode" | b"DCT")
    ) {
        return Ok(false);
    }

    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(false);
    };
    Ok(matches!(
        document.resolve_owned_value(subtype)?,
        Some(OwnedObject::Name(name)) if name == b"Image"
    ))
}

fn qpdf_compatible_decode(data: &[u8]) -> Option<Vec<u8>> {
    let dictionary = FlObjectHandle::dictionary(vec![(
        b"/Filter".to_vec(),
        FlObjectHandle::name(b"DCTDecode".to_vec()),
    )]);
    flpdf::filters::decode_stream_data(&dictionary, data).ok()
}

fn is_app_or_com_marker(marker: u8) -> bool {
    (0xe0..=0xef).contains(&marker) || marker == 0xfe
}

fn source_app_com_markers(data: &[u8]) -> Option<Vec<Vec<u8>>> {
    if data.get(..2) != Some(&[0xff, 0xd8]) {
        return None;
    }
    let mut position = 2usize;
    let mut markers = Vec::new();
    while position < data.len() {
        let marker_start = position;
        if data[position] != 0xff {
            return None;
        }
        while data.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *data.get(position)?;
        position += 1;
        match marker {
            0xda | 0xd9 => return Some(markers),
            0x00 | 0xff => return None,
            0x01 | 0xd0..=0xd8 => {}
            _ => {
                let length_bytes = data.get(position..position.checked_add(2)?)?;
                let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
                if length < 2 {
                    return None;
                }
                let end = position.checked_add(length)?;
                if end > data.len() {
                    return None;
                }
                if is_app_or_com_marker(marker) {
                    markers.push(data[marker_start..end].to_vec());
                }
                position = end;
            }
        }
    }
    None
}

fn restore_source_app_com_markers(source: &[u8], transformed: &[u8]) -> Option<Vec<u8>> {
    let source_markers = source_app_com_markers(source)?;
    if transformed.get(..2) != Some(&[0xff, 0xd8]) {
        return None;
    }

    let mut output = Vec::with_capacity(transformed.len());
    output.extend_from_slice(&[0xff, 0xd8]);
    for marker in source_markers {
        output.extend_from_slice(&marker);
    }

    let mut position = 2usize;
    while position < transformed.len() {
        let marker_start = position;
        if transformed[position] != 0xff {
            return None;
        }
        while transformed.get(position) == Some(&0xff) {
            position += 1;
        }
        let marker = *transformed.get(position)?;
        position += 1;
        match marker {
            0xda | 0xd9 => {
                output.extend_from_slice(&transformed[marker_start..]);
                return Some(output);
            }
            0x00 | 0xff => return None,
            0x01 | 0xd0..=0xd8 => {
                output.extend_from_slice(&transformed[marker_start..position]);
            }
            _ => {
                let length_bytes = transformed.get(position..position.checked_add(2)?)?;
                let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
                if length < 2 {
                    return None;
                }
                let end = position.checked_add(length)?;
                if end > transformed.len() {
                    return None;
                }
                if !is_app_or_com_marker(marker) {
                    output.extend_from_slice(&transformed[marker_start..end]);
                }
                position = end;
            }
        }
    }
    None
}

fn optimized_jpeg_bytes(data: &[u8]) -> Option<Vec<u8>> {
    let optimized = transform_jpeg_with_options(
        data,
        &TransformOptions {
            op: TransformOp::None,
            optimize: true,
            // The coefficient writer synthesizes JFIF APP0. Strip all of its
            // APP/COM output and restore the source marker sequence exactly below.
            copy_markers: MarkerCopyMode::None,
            ..Default::default()
        },
    )
    .ok()?;
    let optimized = restore_source_app_com_markers(data, &optimized)?;

    // libjpeg-turbo-rs's coefficient reader accepts some entropy streams that
    // qpdf's Pl_DCT compatibility path rejects. Require the before/after JPEGs
    // to decode through the qpdf-compatible flpdf pipeline and to produce the
    // exact same pixels before considering the rewrite.
    let before = qpdf_compatible_decode(data)?;
    let after = qpdf_compatible_decode(&optimized)?;
    (before == after).then_some(optimized)
}

pub fn optimize_jpeg_entropy_hayro(
    document: &mut EditDocument,
    min_savings_bytes: usize,
    min_savings_percent: u8,
) -> Result<JpegEntropyStats> {
    let mut stats = JpegEntropyStats::default();
    for handle in document.reachable_streams_with_subtype(b"Image")? {
        let Some(OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        if !is_safe_lone_dct(document, &dictionary)? {
            continue;
        }

        let raw = data.bytes(document.source())?;
        if raw.len() < MIN_JPEG_BYTES {
            continue;
        }
        stats.streams_considered += 1;

        let Some(optimized) = optimized_jpeg_bytes(raw.as_ref()) else {
            continue;
        };
        let saving = raw.len().saturating_sub(optimized.len());
        if saving == 0
            || saving < min_savings_bytes
            || saving.saturating_mul(100)
                < raw.len().saturating_mul(usize::from(min_savings_percent))
        {
            continue;
        }

        let original_len = raw.len();
        let optimized_len = optimized.len();
        let object = match handle {
            ObjectHandle::Existing(id) => document.edit_object(id)?,
            ObjectHandle::New(id) => document.edit_added_object(id)?,
        };
        if let OwnedObject::Stream { data, .. } = object {
            *data = StreamData::Owned(optimized);
            stats.streams_optimized += 1;
            stats.original_encoded_bytes += original_len;
            stats.optimized_encoded_bytes += optimized_len;
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use libjpeg_turbo_rs::{PixelFormat, Subsampling, compress, read_coefficients};

    #[test]
    fn huffman_optimization_preserves_source_app_markers_without_synthesizing_jfif()
    -> crate::Result<()> {
        let width = 32usize;
        let height = 32usize;
        let pixels = vec![96u8; width * height * 3];
        let generated = compress(
            &pixels,
            width,
            height,
            PixelFormat::Rgb,
            88,
            Subsampling::S444,
        )
        .map_err(|error| crate::Error::Invalid(error.to_string()))?;
        let stripped = restore_source_app_com_markers(&[0xff, 0xd8, 0xff, 0xd9], &generated)
            .ok_or_else(|| {
                crate::Error::Invalid("generated JPEG marker parse failed".to_owned())
            })?;
        let adobe = [
            0xff, 0xee, 0x00, 0x0e, b'A', b'd', b'o', b'b', b'e', 0x00, 0x64, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let mut source = Vec::with_capacity(stripped.len() + adobe.len());
        source.extend_from_slice(&[0xff, 0xd8]);
        source.extend_from_slice(&adobe);
        source.extend_from_slice(&stripped[2..]);

        let optimized = optimized_jpeg_bytes(&source).ok_or_else(|| {
            crate::Error::Invalid("Adobe-only JPEG should be transformable".to_owned())
        })?;
        assert_eq!(
            source_app_com_markers(&optimized),
            source_app_com_markers(&source)
        );
        assert_eq!(
            source_app_com_markers(&optimized),
            Some(vec![adobe.to_vec()])
        );
        Ok(())
    }

    #[test]
    fn huffman_optimization_preserves_quantized_dct_coefficients() -> crate::Result<()> {
        let width = 96usize;
        let height = 80usize;
        let mut pixels = Vec::with_capacity(width * height * 3);
        let mut state = 0x1234_5678u32;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let x_gradient = u8::try_from(x * 255 / width)
                    .map_err(|_| crate::Error::Invalid("x gradient exceeds u8".to_owned()))?;
                let y_gradient = u8::try_from(y * 255 / height)
                    .map_err(|_| crate::Error::Invalid("y gradient exceeds u8".to_owned()))?;
                let diagonal = u8::try_from(x + y)
                    .map_err(|_| crate::Error::Invalid("diagonal fixture exceeds u8".to_owned()))?;
                pixels.push(x_gradient.wrapping_add((state >> 27) as u8));
                pixels.push(y_gradient.wrapping_add((state >> 24) as u8));
                pixels.push(diagonal.wrapping_add((state >> 29) as u8));
            }
        }

        let original = compress(
            &pixels,
            width,
            height,
            PixelFormat::Rgb,
            88,
            Subsampling::S420,
        )
        .map_err(|error| crate::Error::Invalid(error.to_string()))?;
        let optimized = optimized_jpeg_bytes(&original).ok_or_else(|| {
            crate::Error::Invalid("generated JPEG should be transformable".to_owned())
        })?;

        let before = read_coefficients(&original)
            .map_err(|error| crate::Error::Invalid(error.to_string()))?;
        let after = read_coefficients(&optimized)
            .map_err(|error| crate::Error::Invalid(error.to_string()))?;
        assert_eq!(before.width, after.width);
        assert_eq!(before.height, after.height);
        assert_eq!(before.components.len(), after.components.len());
        for (left, right) in before.components.iter().zip(&after.components) {
            assert_eq!(left.h_sampling, right.h_sampling);
            assert_eq!(left.v_sampling, right.v_sampling);
            assert_eq!(left.blocks, right.blocks);
        }
        assert!(optimized.len() < original.len());
        Ok(())
    }
}
