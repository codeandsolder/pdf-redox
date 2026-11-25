//! Stream filter decoding and encoding for the copy-on-write document layer.

use crate::{
    EditDocument, Error, OwnedDictionary, OwnedObject, Result,
    source::{CurrentValue, NumericValue},
};
#[cfg(not(target_family = "wasm"))]
use std::sync::Arc;
use std::{
    borrow::Cow,
    sync::{Mutex, PoisonError},
};

const EXTERNAL_STREAM_KEYS: [&[u8]; 3] = [b"F", b"FFilter", b"FDecodeParms"];

/// Whether the stream dictionary declares payload from an external file.
///
/// `/F` supplies an external file specification and `/FFilter` plus
/// `/FDecodeParms` describe its independent filter chain. The inline bytes
/// cannot be treated as the authoritative decoded stream in this case.
pub fn has_external_file_stream_entries(dictionary: &OwnedDictionary) -> bool {
    EXTERNAL_STREAM_KEYS
        .iter()
        .any(|key| dictionary.contains_key(*key))
}

fn write_resolved_value(
    document: &EditDocument,
    value: CurrentValue<'_>,
    output: &mut Vec<u8>,
    depth: usize,
) -> Result<()> {
    if depth > 256 {
        return Err(Error::Invalid(
            "stream filter dictionary nesting exceeds supported depth".to_owned(),
        ));
    }
    let Some(value) = document.resolve_current_value(value)? else {
        output.extend_from_slice(b"null");
        return Ok(());
    };

    if value.is_stream() {
        return Err(Error::Invalid(
            "stream cannot be embedded in stream filter parameters".to_owned(),
        ));
    }
    if value.is_null() {
        output.extend_from_slice(b"null");
    } else if let Some(value) = value.boolean() {
        output.extend_from_slice(if value { b"true" } else { b"false" });
    } else if let Some(number) = value.number() {
        match number {
            NumericValue::Integer(value) => output.extend_from_slice(value.to_string().as_bytes()),
            NumericValue::Real(value) => crate::writer::write_pdf_real(output, value)?,
        }
    } else if let Some(name) = value.name() {
        crate::writer::write_pdf_name(output, name);
    } else if let Some(string) = value.string() {
        crate::writer::write_pdf_string(output, string);
    } else if let Some(values) = value.array_items() {
        output.push(b'[');
        for (index, value) in values.enumerate() {
            if index != 0 {
                output.push(b' ');
            }
            write_resolved_value(document, value, output, depth + 1)?;
        }
        output.push(b']');
    } else if let Some(entries) = value.dictionary_entries() {
        output.extend_from_slice(b"<<");
        for (name, value) in entries {
            output.push(b' ');
            crate::writer::write_pdf_name(output, name.as_ref());
            output.push(b' ');
            write_resolved_value(document, value, output, depth + 1)?;
        }
        output.extend_from_slice(b" >>");
    } else {
        return Err(Error::Invalid(
            "unresolved stream filter parameter".to_owned(),
        ));
    }
    Ok(())
}

/// The only COS entries needed to decode an inline payload. Read them once
/// and retain the original source/overlay views while serializing the
/// detached filter dictionary.
pub struct InlineFilterConfiguration<'a> {
    filter: Option<CurrentValue<'a>>,
    parameters: Option<CurrentValue<'a>>,
}

impl<'a> InlineFilterConfiguration<'a> {
    fn from_lookup(lookup: impl Fn(&[u8]) -> Option<CurrentValue<'a>>) -> Result<Self> {
        if EXTERNAL_STREAM_KEYS
            .into_iter()
            .any(|key| lookup(key).is_some())
        {
            return Err(Error::Invalid(
                "external-file stream data cannot be decoded as inline payload".to_owned(),
            ));
        }
        Ok(Self {
            filter: lookup(b"Filter"),
            parameters: lookup(b"DecodeParms"),
        })
    }

    pub fn from_stream(stream: &CurrentValue<'a>) -> Result<Self> {
        if !stream.is_stream() {
            return Err(Error::Invalid("object is not a stream".to_owned()));
        }
        Self::from_lookup(|key| stream.dictionary_entry(key))
    }

    fn from_dictionary(dictionary: &'a OwnedDictionary) -> Result<Self> {
        Self::from_lookup(|key| dictionary.get(key).map(CurrentValue::Owned))
    }

    pub const fn is_unfiltered(&self) -> bool {
        self.filter.is_none()
    }

    pub fn is_direct(&self) -> bool {
        self.filter
            .iter()
            .chain(&self.parameters)
            .all(|value| !value.contains_indirect_reference())
    }

    fn serialize(self, document: &EditDocument) -> Result<Option<Vec<u8>>> {
        let Some(filter) = self.filter else {
            return Ok(None);
        };
        let mut output = Vec::new();
        output.extend_from_slice(b"<<");
        for (key, value) in [
            (b"Filter".as_slice(), Some(filter)),
            (b"DecodeParms", self.parameters),
        ] {
            let Some(value) = value else {
                continue;
            };
            output.push(b' ');
            crate::writer::write_pdf_name(&mut output, key);
            output.push(b' ');
            write_resolved_value(document, value, &mut output, 0)?;
        }
        output.extend_from_slice(b" >>");
        Ok(Some(output))
    }

    pub fn decode(self, document: &EditDocument, encoded: Cow<'_, [u8]>) -> Result<Vec<u8>> {
        let Some(filter_dictionary) = self.serialize(document)? else {
            return Ok(encoded.into_owned());
        };
        hayro_syntax::object::stream::decode_standalone_stream(&filter_dictionary, encoded.as_ref())
            .map_err(|error| Error::Invalid(format!("failed to decode stream filters: {error:?}")))
    }
}

/// Decode bytes using the stream's current `/Filter` and `/DecodeParms`.
pub fn decode_stream(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    encoded: &[u8],
) -> Result<Vec<u8>> {
    InlineFilterConfiguration::from_dictionary(dictionary)?.decode(document, Cow::Borrowed(encoded))
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
    let Some(filter_dictionary) =
        InlineFilterConfiguration::from_dictionary(dictionary)?.serialize(document)?
    else {
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
    compressor: Option<Arc<zlib_rs::BatchCodec>>,
    #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
    gpu: Option<Arc<GpuFlateExecutor>>,
    stats: Option<Mutex<crate::FlateBatchStats>>,
}

#[cfg(all(feature = "gpu", not(target_family = "wasm")))]
struct GpuFlateExecutor {
    searcher:
        std::sync::OnceLock<std::result::Result<zlib_rs::GpuMatchSearcher, zlib_rs::GpuMatchError>>,
    config: crate::FlateGpuConfig,
}

impl FlateBatchEncoder {
    pub fn from_config(config: &crate::Config) -> Result<Self> {
        let Some(gpu) = &config.flate_gpu else {
            return Self::new(config.flate_workers);
        };
        #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
        {
            let mut encoder = Self::new(config.flate_workers)?;
            encoder.gpu = Some(Arc::new(GpuFlateExecutor {
                searcher: std::sync::OnceLock::new(),
                config: gpu.clone(),
            }));
            Ok(encoder)
        }
        #[cfg(not(all(feature = "gpu", not(target_family = "wasm"))))]
        {
            let _ = gpu;
            Err(Error::Compression(
                "GPU Flate requires a native build with the gpu feature".to_owned(),
            ))
        }
    }

    pub fn new(workers: std::num::NonZeroUsize) -> Result<Self> {
        #[cfg(target_family = "wasm")]
        {
            let _ = workers;
            Ok(Self { stats: None })
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let compressor = if workers.get() > 1 {
                Some(Arc::new(zlib_rs::BatchCodec::new(workers).map_err(
                    |error| {
                        Error::Compression(format!("failed to create Flate worker pool: {error}"))
                    },
                )?))
            } else {
                None
            };
            Ok(Self {
                compressor,
                #[cfg(feature = "gpu")]
                gpu: None,
                stats: None,
            })
        }
    }

    /// Share the immutable executor resources but start a fresh document's counters.
    pub fn for_document(&self) -> Self {
        Self {
            #[cfg(not(target_family = "wasm"))]
            compressor: self.compressor.clone(),
            #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
            gpu: self.gpu.clone(),
            stats: Some(Mutex::default()),
        }
    }

    pub fn stats(&self) -> crate::FlateBatchStats {
        self.stats
            .as_ref()
            .map_or_else(crate::FlateBatchStats::default, |stats| {
                *stats.lock().unwrap_or_else(PoisonError::into_inner)
            })
    }

    fn record_jobs(&self, jobs: &[zlib_rs::BatchJob<'_>], gpu: bool) {
        let Some(stats) = &self.stats else {
            return;
        };
        if jobs.is_empty() {
            return;
        }
        let mut stats = stats.lock().unwrap_or_else(PoisonError::into_inner);
        let work = if gpu {
            &mut stats.gpu_assisted
        } else {
            &mut stats.cpu
        };
        work.batches = work.batches.saturating_add(1);
        work.streams = work.streams.saturating_add(jobs.len());
        work.decoded_bytes = jobs.iter().fold(work.decoded_bytes, |bytes, job| {
            bytes.saturating_add(job.input.len())
        });
    }

    fn compress_cpu(
        &self,
        jobs: &mut [zlib_rs::BatchJob<'_>],
        config: zlib_rs::DeflateConfig,
    ) -> Vec<zlib_rs::BatchCompressResult> {
        self.record_jobs(jobs, false);
        #[cfg(not(target_family = "wasm"))]
        if let Some(compressor) = &self.compressor {
            return compressor.compress(jobs, config);
        }
        zlib_rs::compress_slices_batch(jobs, config)
    }

    fn compress_jobs(
        &self,
        mut jobs: Vec<zlib_rs::BatchJob<'_>>,
        config: zlib_rs::DeflateConfig,
    ) -> Result<Vec<zlib_rs::BatchCompressResult>> {
        #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
        if let Some(gpu) = &self.gpu
            && jobs
                .iter()
                .any(|job| job.input.len() >= gpu.config.min_stream_bytes)
        {
            let searcher = gpu
                .searcher
                .get_or_init(|| {
                    let start = std::time::Instant::now();
                    let result = zlib_rs::GpuMatchSearcher::new(gpu.config.adapter.as_deref());
                    if let Some(stats) = &self.stats {
                        stats
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .gpu_initialization_ms = start.elapsed().as_secs_f64() * 1000.0;
                    }
                    result
                })
                .as_ref()
                .map_err(|error| {
                    Error::Compression(format!("GPU Flate initialization: {error}"))
                })?;
            let search_budget = gpu.config.search_budget.get();
            let count = jobs.len();
            let (cpu_jobs, gpu_jobs): (Vec<_>, Vec<_>) = jobs
                .into_iter()
                .enumerate()
                .partition(|(_, job)| job.input.len() < gpu.config.min_stream_bytes);
            let (cpu_indices, mut cpu_jobs): (Vec<_>, Vec<_>) = cpu_jobs.into_iter().unzip();
            let (gpu_indices, mut gpu_jobs): (Vec<_>, Vec<_>) = gpu_jobs.into_iter().unzip();
            let cpu_results = self.compress_cpu(&mut cpu_jobs, config);
            self.record_jobs(&gpu_jobs, true);
            let gpu_results = match &self.compressor {
                Some(pool) => pool.compress_gpu(&mut gpu_jobs, config, searcher, search_budget),
                None => zlib_rs::compress_slices_batch_gpu(
                    &mut gpu_jobs,
                    config,
                    searcher,
                    search_budget,
                ),
            }
            .map_err(|error| Error::Compression(format!("GPU Flate search: {error}")))?;
            let mut results = vec![
                zlib_rs::BatchCompressResult {
                    written: 0,
                    code: zlib_rs::ReturnCode::StreamError
                };
                count
            ];
            for (index, result) in cpu_indices
                .into_iter()
                .zip(cpu_results)
                .chain(gpu_indices.into_iter().zip(gpu_results))
            {
                results[index] = result;
            }
            return Ok(results);
        }
        Ok(self.compress_cpu(&mut jobs, config))
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
        let jobs = inputs
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
        let results = self.compress_jobs(jobs, flate_config(level))?;
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
    Ok(encode_flate_capped(decoded, maximum_length, level)?.map(|bytes| bytes.len()))
}

/// The ordinary zlib encoding, only if it fits the exact byte budget.
pub fn encode_flate_capped(
    decoded: &[u8],
    maximum_length: usize,
    level: crate::FlateLevel,
) -> Result<Option<Vec<u8>>> {
    let mut output = vec![0_u8; maximum_length.min(zlib_rs::compress_bound(decoded.len()))];
    let (encoded, code) = zlib_rs::compress_slice(&mut output, decoded, flate_config(level));
    let Some(length) = flate_length_with_limit(encoded.len(), code)? else {
        return Ok(None);
    };
    output.truncate(length);
    Ok(Some(output))
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

    #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
    #[test]
    #[ignore = "requires a Vulkan adapter"]
    fn gpu_batch_preserves_order_caps_and_decoded_bytes() -> Result<()> {
        let mut config = crate::Config {
            flate_gpu: Some(crate::FlateGpuConfig {
                adapter: None,
                search_budget: std::num::NonZeroU32::new(32)
                    .ok_or_else(|| Error::Invalid("zero budget".into()))?,
                min_stream_bytes: 1024,
            }),
            ..crate::Config::default()
        };
        let large = b"BT /F1 12 Tf (GPU compression) Tj ET\n".repeat(4096);
        let other = (0_u8..=255).cycle().take(100_000).collect::<Vec<_>>();
        let inputs = [
            b"small".as_slice(),
            large.as_slice(),
            other.as_slice(),
            b"tail",
        ];
        let full_caps = inputs.map(|input| zlib_rs::compress_bound(input.len()));
        for workers in [1, 4] {
            config.flate_workers = std::num::NonZeroUsize::new(workers)
                .ok_or_else(|| Error::Invalid("zero workers".into()))?;
            let executor = FlateBatchEncoder::from_config(&config)?;
            let batcher = executor.for_document();
            for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
                let encoded = batcher
                    .encode_many_capped(&inputs, &full_caps, level)?
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .ok_or_else(|| Error::Compression("full bound rejected".into()))?;
                for (input, bytes) in inputs.iter().zip(&encoded) {
                    let mut decoded = vec![0; input.len()];
                    let (decoded, code) = zlib_rs::decompress_slice(
                        &mut decoded,
                        bytes,
                        zlib_rs::InflateConfig::default(),
                    );
                    assert_eq!(code, zlib_rs::ReturnCode::Ok);
                    assert_eq!(decoded, *input);
                }
                let mut caps = [0; 4];
                for (cap, bytes) in caps.iter_mut().zip(&encoded) {
                    *cap = bytes.len();
                }
                caps[1] -= 1;
                caps[3] -= 1;
                assert_eq!(
                    batcher.encode_many_capped(&inputs, &caps, level)?,
                    vec![
                        Some(encoded[0].clone()),
                        None,
                        Some(encoded[2].clone()),
                        None
                    ]
                );
                assert_eq!(encoded[0], encode_flate(inputs[0], level)?);
                assert_eq!(encoded[3], encode_flate(inputs[3], level)?);
            }
            let stats = batcher.stats();
            assert_eq!(stats.cpu.streams, 8);
            assert_eq!(stats.gpu_assisted.streams, 8);
            assert_eq!(
                stats.gpu_assisted.decoded_bytes,
                4 * (large.len() + other.len())
            );
            assert!(stats.gpu_initialization_ms > 0.0);
            let next = executor.for_document();
            assert_eq!(next.stats(), crate::FlateBatchStats::default());
            let _ = next.encode_many_capped(&inputs, &full_caps, crate::FlateLevel::SIZE)?;
            assert_eq!(next.stats().gpu_assisted.streams, 2);
            assert!(next.stats().gpu_initialization_ms <= 0.0);
            assert_eq!(batcher.stats(), stats);
        }
        Ok(())
    }

    #[cfg(not(all(feature = "gpu", not(target_family = "wasm"))))]
    #[test]
    fn unsupported_gpu_configuration_is_explicit() {
        let config = crate::Config {
            flate_gpu: Some(crate::FlateGpuConfig {
                adapter: None,
                search_budget: std::num::NonZeroU32::MIN,
                min_stream_bytes: 0,
            }),
            ..crate::Config::default()
        };
        assert!(FlateBatchEncoder::from_config(&config).is_err());
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
