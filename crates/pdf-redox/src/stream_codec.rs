//! Stream filter decoding and encoding for the copy-on-write document layer.

use crate::{EditDocument, Error, OwnedDictionary, OwnedObject, Result};

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
    if dictionary.contains_key(b"F".as_slice())
        || dictionary.contains_key(b"FFilter".as_slice())
        || dictionary.contains_key(b"FDecodeParms".as_slice())
    {
        return Err(Error::Invalid(
            "external-file stream filters are unsupported".to_owned(),
        ));
    }

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

/// Decode bytes using the stream's current `/Filter` and `/DecodeParms`.
pub fn decode_stream(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    encoded: &[u8],
) -> Result<Vec<u8>> {
    if !dictionary.contains_key(b"Filter".as_slice()) {
        return Ok(encoded.to_vec());
    }
    let filter_dictionary = standalone_filter_dictionary(document, dictionary)?;
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
    if !dictionary.contains_key(b"Filter".as_slice()) {
        return Ok(encoded.to_vec());
    }
    let filter_dictionary = standalone_filter_dictionary(document, dictionary)?;
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
pub(crate) struct FlateBatchEncoder {
    #[cfg(not(target_family = "wasm"))]
    compressor: Option<zlib_rs::BatchCompressor>,
}

impl FlateBatchEncoder {
    pub(crate) fn new(workers: std::num::NonZeroUsize) -> Result<Self> {
        #[cfg(target_family = "wasm")]
        {
            let _ = workers;
            Ok(Self {})
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let compressor = if workers.get() > 1 {
                Some(zlib_rs::BatchCompressor::new(workers).map_err(|error| {
                    Error::Compression(format!("failed to create Flate worker pool: {error}"))
                })?)
            } else {
                None
            };
            Ok(Self { compressor })
        }
    }

    pub(crate) fn encode_many(
        &self,
        inputs: &[&[u8]],
        level: crate::FlateLevel,
    ) -> Result<Vec<Vec<u8>>> {
        #[cfg(not(target_family = "wasm"))]
        if inputs.len() > 1
            && let Some(compressor) = &self.compressor
        {
            let mut outputs = inputs
                .iter()
                .map(|input| vec![0_u8; zlib_rs::compress_bound(input.len())])
                .collect::<Vec<_>>();
            let mut jobs = inputs
                .iter()
                .zip(&mut outputs)
                .map(|(input, output)| zlib_rs::BatchCompressJob::new(input, output))
                .collect::<Vec<_>>();
            let results = compressor.compress(&mut jobs, flate_config(level));
            drop(jobs);

            for (output, result) in outputs.iter_mut().zip(results) {
                if result.code != zlib_rs::ReturnCode::Ok {
                    return Err(compression_code_error(result.code));
                }
                output.truncate(result.written);
            }
            return Ok(outputs);
        }

        inputs
            .iter()
            .map(|input| encode_flate(input, level))
            .collect()
    }
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

        for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
            assert_eq!(
                parallel.encode_many(&inputs, level)?,
                serial.encode_many(&inputs, level)?
            );
        }
        Ok(())
    }
}
