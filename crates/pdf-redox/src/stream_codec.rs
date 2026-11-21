//! Stream filter decoding and encoding for the copy-on-write document layer.

use crate::{EditDocument, Error, OwnedDictionary, OwnedObject, Result};

/// Whether the stream dictionary declares payload from an external file.
///
/// `/F` supplies an external file specification and `/FFilter` plus
/// `/FDecodeParms` describe its independent filter chain. The inline bytes
/// cannot be treated as the authoritative decoded stream in this case.
pub fn has_external_file_stream_entries(dictionary: &OwnedDictionary) -> bool {
    [b"F".as_slice(), b"FFilter", b"FDecodeParms"]
        .iter()
        .any(|key| dictionary.contains_key(*key))
}

fn write_resolved_value(
    document: &EditDocument,
    value: &OwnedObject,
    output: &mut Vec<u8>,
    depth: usize,
) -> Result<()> {
    if depth > 256 {
        return Err(Error::Invalid(
            "stream filter dictionary nesting exceeds supported depth".to_owned(),
        ));
    }
    let resolved;
    let value = if matches!(value, OwnedObject::Reference(_)) {
        let Some(value) = document.resolve_owned_value(value)? else {
            output.extend_from_slice(b"null");
            return Ok(());
        };
        resolved = value;
        &resolved
    } else {
        value
    };

    match value {
        OwnedObject::Null => output.extend_from_slice(b"null"),
        OwnedObject::Boolean(value) => {
            output.extend_from_slice(if *value { b"true" } else { b"false" });
        }
        OwnedObject::Integer(value) => output.extend_from_slice(value.to_string().as_bytes()),
        OwnedObject::Real(value) => crate::writer::write_pdf_real(output, *value)?,
        OwnedObject::Name(value) => crate::writer::write_pdf_name(output, value),
        OwnedObject::String(value) => crate::writer::write_pdf_string(output, value),
        OwnedObject::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b' ');
                }
                write_resolved_value(document, value, output, depth + 1)?;
            }
            output.push(b']');
        }
        OwnedObject::Dictionary(dictionary) => {
            output.extend_from_slice(b"<<");
            for (name, value) in dictionary {
                output.push(b' ');
                crate::writer::write_pdf_name(output, name);
                output.push(b' ');
                write_resolved_value(document, value, output, depth + 1)?;
            }
            output.extend_from_slice(b" >>");
        }
        OwnedObject::Reference(_) => unreachable!("references are resolved above"),
        OwnedObject::Stream { .. } => {
            return Err(Error::Invalid(
                "stream cannot be embedded in stream filter parameters".to_owned(),
            ));
        }
    }
    Ok(())
}

fn standalone_filter_dictionary(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    output.extend_from_slice(b"<<");
    for key in [b"Filter".as_slice(), b"DecodeParms".as_slice()] {
        let Some(value) = dictionary.get(key) else {
            continue;
        };
        output.push(b' ');
        crate::writer::write_pdf_name(&mut output, key);
        output.push(b' ');
        write_resolved_value(document, value, &mut output, 0)?;
    }
    output.extend_from_slice(b" >>");
    Ok(output)
}

/// Prepare the inline filter chain once for both ordinary and image streams.
/// External-file entries take precedence even when no inline `/Filter` exists.
fn inline_filter_dictionary(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<Vec<u8>>> {
    if has_external_file_stream_entries(dictionary) {
        return Err(Error::Invalid(
            "external-file stream data cannot be decoded as inline payload".to_owned(),
        ));
    }
    dictionary
        .contains_key(b"Filter".as_slice())
        .then(|| standalone_filter_dictionary(document, dictionary))
        .transpose()
}

/// Decode bytes using the stream's current `/Filter` and `/DecodeParms`.
pub fn decode_stream(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    encoded: &[u8],
) -> Result<Vec<u8>> {
    let Some(filter_dictionary) = inline_filter_dictionary(document, dictionary)? else {
        return Ok(encoded.to_vec());
    };
    hayro_syntax::object::stream::decode_standalone_stream(&filter_dictionary, encoded)
        .map_err(|error| Error::Invalid(format!("failed to decode stream filters: {error:?}")))
}

/// Decode image-stream bytes with the image metadata required by DCT/JPX/etc.
pub fn decode_image_stream(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    encoded: &[u8],
    width: u32,
    height: u32,
    bits_per_component: u8,
    components: u8,
) -> Result<Vec<u8>> {
    let Some(filter_dictionary) = inline_filter_dictionary(document, dictionary)? else {
        return Ok(encoded.to_vec());
    };
    let params = hayro_syntax::object::stream::ImageDecodeParams {
        is_indexed: false,
        bpc: Some(bits_per_component),
        num_components: Some(components),
        target_dimension: None,
        width,
        height,
    };
    hayro_syntax::object::stream::decode_standalone_image_stream(
        &filter_dictionary,
        encoded,
        &params,
    )
    .map_err(|error| Error::Invalid(format!("failed to decode image stream filters: {error:?}")))
}

fn flate_config(level: crate::FlateLevel) -> zlib_rs::DeflateConfig {
    zlib_rs::DeflateConfig::new(level.value())
}

fn compression_code_error(code: zlib_rs::ReturnCode) -> Error {
    Error::Compression(format!("zlib-rs returned {code:?}"))
}

/// Encode ordinary zlib/Flate data at an explicit PDF compression level.
pub fn encode_flate(decoded: &[u8], level: crate::FlateLevel) -> Result<Vec<u8>> {
    let mut output = vec![0_u8; zlib_rs::compress_bound(decoded.len())];
    let (encoded, code) = zlib_rs::compress_slice(&mut output, decoded, flate_config(level));
    if code != zlib_rs::ReturnCode::Ok {
        return Err(compression_code_error(code));
    }
    let encoded_len = encoded.len();
    output.truncate(encoded_len);
    Ok(output)
}

/// Persistent whole-stream Flate executor used by document-level batch phases.
pub struct FlateBatchEncoder {
    #[cfg(not(target_family = "wasm"))]
    compressor: Option<zlib_rs::BatchCodec>,
}

impl FlateBatchEncoder {
    pub fn new(workers: std::num::NonZeroUsize) -> Result<Self> {
        #[cfg(target_family = "wasm")]
        {
            let _ = workers;
            Ok(Self {})
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let compressor = if workers.get() > 1 {
                Some(zlib_rs::BatchCodec::new(workers).map_err(|error| {
                    Error::Compression(format!("failed to create Flate worker pool: {error}"))
                })?)
            } else {
                None
            };
            Ok(Self { compressor })
        }
    }

    // Encode directly into bounded outputs. A failed cap is a normal rejection
    // of a candidate, not a compression error. For unrestricted callers the
    // zlib compress_bound provides an output size guaranteed to succeed.
    fn encode_many_with_limits(
        &self,
        inputs: &[&[u8]],
        maximum_lengths: &[usize],
        level: crate::FlateLevel,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        if inputs.len() != maximum_lengths.len() {
            return Err(Error::Invalid(
                "Flate input and output-limit counts differ".to_owned(),
            ));
        }
        let mut outputs = vec![Vec::new(); inputs.len()];
        let mut jobs = inputs
            .iter()
            .zip(maximum_lengths)
            .zip(&mut outputs)
            .map(|((input, &limit), output)| {
                zlib_rs::BatchJob::with_vec(
                    input,
                    output,
                    limit.min(zlib_rs::compress_bound(input.len())),
                )
                .map_err(|error| {
                    Error::Compression(format!("failed to reserve Flate output: {error}"))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let config = flate_config(level);
        #[cfg(not(target_family = "wasm"))]
        let results = match &self.compressor {
            Some(compressor) => compressor.compress(&mut jobs, config),
            None => zlib_rs::compress_slices_batch(&mut jobs, config),
        };
        #[cfg(target_family = "wasm")]
        let results = zlib_rs::compress_slices_batch(&mut jobs, config);
        drop(jobs);
        outputs
            .into_iter()
            .zip(results)
            .map(|(output, result)| {
                Ok(flate_length_with_limit(result.written, result.code)?.map(|_| output))
            })
            .collect()
    }

    /// Compress into at most each requested output limit. None indicates
    /// the candidate cannot fit; successful outputs are the exact zlib bytes,
    /// without recompressing the input.
    pub fn encode_many_capped(
        &self,
        inputs: &[&[u8]],
        maximum_lengths: &[usize],
        level: crate::FlateLevel,
    ) -> Result<Vec<Option<Vec<u8>>>> {
        self.encode_many_with_limits(inputs, maximum_lengths, level)
    }

    /// Exact encoded lengths, provided they fit within the corresponding output
    /// limit. None means that compression exceeded that limit, without needing
    /// to allocate the worst-case compressed bound for the decoded input.
    pub fn encode_many_capped_lengths(
        &self,
        inputs: &[&[u8]],
        maximum_lengths: &[usize],
        level: crate::FlateLevel,
    ) -> Result<Vec<Option<usize>>> {
        Ok(self
            .encode_many_with_limits(inputs, maximum_lengths, level)?
            .into_iter()
            .map(|output| output.map(|bytes| bytes.len()))
            .collect())
    }
}

fn flate_length_with_limit(written: usize, code: zlib_rs::ReturnCode) -> Result<Option<usize>> {
    match code {
        zlib_rs::ReturnCode::Ok => Ok(Some(written)),
        zlib_rs::ReturnCode::BufError => Ok(None),
        other => Err(compression_code_error(other)),
    }
}

/// Exact Flate length when it fits in the provided output byte limit.
///
/// A full output would be larger when the output buffer fills, so no exact-size compression
/// candidate can be lost by rejecting it.
pub fn flate_capped_encoded_len(
    decoded: &[u8],
    maximum_length: usize,
    level: crate::FlateLevel,
) -> Result<Option<usize>> {
    let mut output = vec![0_u8; maximum_length];
    let (encoded, code) = zlib_rs::compress_slice(&mut output, decoded, flate_config(level));
    flate_length_with_limit(encoded.len(), code)
}

/// Return the encoded byte length of ordinary zlib/Flate data.
pub fn flate_encoded_len(decoded: &[u8], level: crate::FlateLevel) -> Result<usize> {
    Ok(encode_flate(decoded, level)?.len())
}

/// Return true when the stream is unfiltered or has exactly one Flate filter.
///
/// This intentionally accepts `/Filter /FlateDecode`, `/Filter /Fl`, and
/// one-element arrays containing either name.
pub fn is_unfiltered_or_lone_flate(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<bool> {
    if has_external_file_stream_entries(dictionary) {
        return Ok(false);
    }
    let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
        return Ok(true);
    };
    let Some(filter) = document.resolve_owned_value(filter)? else {
        return Ok(false);
    };
    match filter {
        OwnedObject::Name(name) => Ok(matches!(name.as_slice(), b"FlateDecode" | b"Fl")),
        OwnedObject::Array(values) if values.len() == 1 => {
            let Some(value) = values.first() else {
                return Ok(false);
            };
            let Some(value) = document.resolve_owned_value(value)? else {
                return Ok(false);
            };
            Ok(
                matches!(value, OwnedObject::Name(name) if matches!(name.as_slice(), b"FlateDecode" | b"Fl")),
            )
        }
        _ => Ok(false),
    }
}

/// Normalize a rewritten stream to plain Flate with no predictor parameters.
pub fn set_plain_flate(dictionary: &mut OwnedDictionary) {
    dictionary.insert(
        b"Filter".to_vec(),
        OwnedObject::Name(b"FlateDecode".to_vec()),
    );
    dictionary.remove(b"DecodeParms".as_slice());
    dictionary.remove(b"F".as_slice());
    dictionary.remove(b"FFilter".as_slice());
    dictionary.remove(b"FDecodeParms".as_slice());
    dictionary.remove(b"Length".as_slice());
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use super::*;

    #[test]
    fn external_file_streams_are_not_decoded_or_used_as_flate_inputs() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        )?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let payload = b"inline fallback data";
        let mut ordinary = OwnedDictionary::new();
        assert_eq!(decode_stream(&document, &ordinary, payload)?, payload);
        assert_eq!(
            decode_image_stream(&document, &ordinary, payload, 1, 1, 8, 3)?,
            payload
        );
        assert!(is_unfiltered_or_lone_flate(&document, &ordinary)?);

        for key in [b"F".as_slice(), b"FFilter", b"FDecodeParms"] {
            ordinary.insert(key.to_vec(), OwnedObject::String(b"external".to_vec()));
            for filtered in [false, true] {
                if filtered {
                    ordinary.insert(
                        b"Filter".to_vec(),
                        OwnedObject::Name(b"FlateDecode".to_vec()),
                    );
                }
                assert!(
                    !is_unfiltered_or_lone_flate(&document, &ordinary)?,
                    "external entry {key:?}, filtered={filtered} admitted for compression"
                );
                assert!(
                    decode_stream(&document, &ordinary, payload).is_err(),
                    "external entry {key:?}, filtered={filtered} decoded as inline stream"
                );
                assert!(
                    decode_image_stream(&document, &ordinary, payload, 1, 1, 8, 3).is_err(),
                    "external entry {key:?}, filtered={filtered} decoded as inline image"
                );
                ordinary.remove(b"Filter".as_slice());
            }
            ordinary.remove(key);
        }
        Ok(())
    }

    #[test]
    fn shared_filter_preparation_resolves_indirect_filters_and_parameters() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        )?;
        pdf.object(4, b"/FlateDecode")?;
        pdf.object(5, b"<< /Predictor 1 >>")?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let raw = b"x";
        let encoded = encode_flate(raw, crate::FlateLevel::PROCESSING)?;
        let filter =
            OwnedObject::Reference(crate::ObjectHandle::Existing(crate::ObjectId::new(4, 0)));
        let parms =
            OwnedObject::Reference(crate::ObjectHandle::Existing(crate::ObjectId::new(5, 0)));
        for (filter, parms) in [
            (filter.clone(), parms.clone()),
            (
                OwnedObject::Array(vec![filter]),
                OwnedObject::Array(vec![parms]),
            ),
        ] {
            let dictionary = OwnedDictionary::from([
                (b"Filter".to_vec(), filter),
                (b"DecodeParms".to_vec(), parms),
            ]);
            assert_eq!(decode_stream(&document, &dictionary, &encoded)?, raw);
            assert_eq!(
                decode_image_stream(&document, &dictionary, &encoded, 1, 1, 8, 1)?,
                raw
            );
        }
        Ok(())
    }

    #[test]
    fn capped_lengths_match_scalar_exact_at_overflow_boundaries() -> Result<()> {
        let mut random = vec![0_u8; 2 * 1024 * 1024];
        let mut state = 0x0123_4567_89ab_cdef_u64;
        for byte in &mut random {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = state.to_le_bytes()[0];
        }
        let buffers = [
            Vec::new(),
            b"short hello".to_vec(),
            b"BT /F1 12 Tf (hello) Tj ET".repeat(100),
            vec![0_u8; 4 * 1024 * 1024],
            random,
            (0_u8..=255).cycle().take(128 * 1024).collect(),
        ];
        let inputs = buffers.iter().map(Vec::as_slice).collect::<Vec<_>>();

        for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
            let canonical = inputs
                .iter()
                .map(|input| encode_flate(input, level))
                .collect::<Result<Vec<_>>>()?;
            let exact = canonical.iter().map(Vec::len).collect::<Vec<_>>();
            for adjustment in [-1_i64, 0, 1, 1024] {
                let caps = exact
                    .iter()
                    .map(|&size| match adjustment {
                        -1 => size.saturating_sub(1),
                        0 => size,
                        1 => size.saturating_add(1),
                        1024 => size.saturating_add(1024),
                        _ => unreachable!("test adjustment is from fixed cases"),
                    })
                    .collect::<Vec<_>>();
                let expected = exact
                    .iter()
                    .zip(&caps)
                    .map(|(&size, &cap)| (size <= cap).then_some(size))
                    .collect::<Vec<_>>();
                for workers in [1, 4] {
                    let encoder = FlateBatchEncoder::new(
                        std::num::NonZeroUsize::new(workers).unwrap_or(std::num::NonZeroUsize::MIN),
                    )?;
                    let capped = encoder.encode_many_capped(&inputs, &caps, level)?;
                    let expected_bytes = canonical
                        .iter()
                        .zip(&caps)
                        .map(|(bytes, &cap)| (bytes.len() <= cap).then(|| bytes.clone()))
                        .collect::<Vec<_>>();
                    assert_eq!(
                        capped,
                        expected_bytes,
                        "capped bytes: level={}, adjustment={adjustment}, workers={workers}",
                        level.value()
                    );
                    assert_eq!(
                        encoder.encode_many_capped_lengths(&inputs, &caps, level)?,
                        expected,
                        "capped lengths: level={}, adjustment={adjustment}, workers={workers}",
                        level.value()
                    );
                }
                let scalar = inputs
                    .iter()
                    .zip(&caps)
                    .map(|(&input, &cap)| flate_capped_encoded_len(input, cap, level))
                    .collect::<Result<Vec<_>>>()?;
                assert_eq!(scalar, expected);
            }
        }
        Ok(())
    }

    #[test]
    fn batch_encoder_is_byte_exact_across_worker_counts() -> Result<()> {
        let buffers = [
            b"BT /F1 12 Tf (hello) Tj ET".repeat(64),
            (0_u8..=255).cycle().take(256 * 1024).collect::<Vec<_>>(),
            vec![0_u8; 1024 * 1024],
            b"pdf-redox-zlib-batch/".repeat(40_000),
        ];
        let inputs = buffers.iter().map(Vec::as_slice).collect::<Vec<_>>();
        let serial = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
        let parallel = FlateBatchEncoder::new(
            std::num::NonZeroUsize::new(4).unwrap_or(std::num::NonZeroUsize::MIN),
        )?;

        let limits = inputs
            .iter()
            .map(|input| zlib_rs::compress_bound(input.len()))
            .collect::<Vec<_>>();
        for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
            assert_eq!(
                parallel.encode_many_capped(&inputs, &limits, level)?,
                serial.encode_many_capped(&inputs, &limits, level)?
            );
        }
        Ok(())
    }
}
