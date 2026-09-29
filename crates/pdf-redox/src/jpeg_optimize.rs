use crate::{EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData};
use flpdf::ObjectHandle as FlObjectHandle;
use libjpeg_turbo_rs::{
    MarkerCopyMode, TransformOp, TransformOptions, transform_jpeg_with_options,
};

const MIN_JPEG_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct JpegEntropyStats {
    pub streams_considered: usize,
    pub streams_optimized: usize,
    pub original_encoded_bytes: usize,
    pub optimized_encoded_bytes: usize,
}

impl JpegEntropyStats {
    pub(crate) fn saved_bytes(self) -> usize {
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

fn optimized_jpeg_bytes(data: &[u8]) -> Option<Vec<u8>> {
    let optimized = transform_jpeg_with_options(
        data,
        &TransformOptions {
            op: TransformOp::None,
            optimize: true,
            copy_markers: MarkerCopyMode::All,
            ..Default::default()
        },
    )
    .ok()?;

    // libjpeg-turbo-rs's coefficient reader accepts some entropy streams that
    // qpdf's Pl_DCT compatibility path rejects. Require the before/after JPEGs
    // to decode through the qpdf-compatible flpdf pipeline and to produce the
    // exact same pixels before considering the rewrite.
    let before = qpdf_compatible_decode(data)?;
    let after = qpdf_compatible_decode(&optimized)?;
    (before == after).then_some(optimized)
}

pub(crate) fn optimize_jpeg_entropy_hayro(
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
    fn huffman_optimization_preserves_quantized_dct_coefficients() {
        let width = 96usize;
        let height = 80usize;
        let mut pixels = Vec::with_capacity(width * height * 3);
        let mut state = 0x1234_5678u32;
        for y in 0..height {
            for x in 0..width {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                pixels.push(((x * 255 / width) as u8).wrapping_add((state >> 27) as u8));
                pixels.push(((y * 255 / height) as u8).wrapping_add((state >> 24) as u8));
                pixels.push(((x + y) as u8).wrapping_add((state >> 29) as u8));
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
        .unwrap_or_else(|error| panic!("{error}"));
        let optimized = optimized_jpeg_bytes(&original)
            .unwrap_or_else(|| panic!("generated JPEG should be transformable"));

        let before = read_coefficients(&original).unwrap_or_else(|error| panic!("{error}"));
        let after = read_coefficients(&optimized).unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(before.width, after.width);
        assert_eq!(before.height, after.height);
        assert_eq!(before.components.len(), after.components.len());
        for (left, right) in before.components.iter().zip(&after.components) {
            assert_eq!(left.h_sampling, right.h_sampling);
            assert_eq!(left.v_sampling, right.v_sampling);
            assert_eq!(left.blocks, right.blocks);
        }
        assert!(optimized.len() < original.len());
    }
}
