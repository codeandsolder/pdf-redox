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

    pub(crate) fn detach(
        self,
        document: &EditDocument,
        encoded: Cow<'a, [u8]>,
    ) -> Result<DetachedStream<'a>> {
        Ok(DetachedStream {
            filter_dictionary: self.serialize(document)?,
            encoded,
        })
    }

    pub fn decode(self, document: &EditDocument, encoded: Cow<'_, [u8]>) -> Result<Vec<u8>> {
        let Some(filter_dictionary) = self.serialize(document)? else {
            return Ok(encoded.into_owned());
        };
        decode_detached(&filter_dictionary, encoded.as_ref())
    }
}

/// Resolved filter parameters and encoded bytes only: no non-Sync PDF
/// parser, object overlay, or cache references cross the worker boundary.
pub struct DetachedStream<'a> {
    filter_dictionary: Option<Vec<u8>>,
    pub(crate) encoded: Cow<'a, [u8]>,
}

impl DetachedStream<'_> {
    pub(crate) fn decode(&self) -> Result<Vec<u8>> {
        self.filter_dictionary.as_ref().map_or_else(
            || Ok(self.encoded.to_vec()),
            |dictionary| decode_detached(dictionary, &self.encoded),
        )
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        let encoded = match &self.encoded {
            Cow::Owned(bytes) => bytes.capacity(),
            Cow::Borrowed(_) => 0,
        };
        encoded.saturating_add(self.filter_dictionary.as_ref().map_or(0, Vec::capacity))
    }
}

fn decode_detached(dictionary: &[u8], encoded: &[u8]) -> Result<Vec<u8>> {
    hayro_syntax::object::stream::decode_standalone_stream(dictionary, encoded)
        .map_err(|error| Error::Invalid(format!("failed to decode stream filters: {error:?}")))
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
    encode_flate_capped(decoded, usize::MAX, level)?
        .ok_or_else(|| compression_code_error(zlib_rs::ReturnCode::BufError))
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

#[cfg(all(feature = "gpu", not(target_family = "wasm")))]
impl GpuFlateExecutor {
    fn eligible(&self, input_len: usize, output_limit: usize) -> bool {
        input_len >= self.config.min_stream_bytes
            && (output_limit as u128) * 1000
                >= (input_len as u128) * u128::from(self.config.min_cap_per_mille)
    }
}

#[cfg(all(feature = "cuda", not(target_family = "wasm")))]
pub enum FlateTranscode {
    NotRouted,
    Rejected,
    RetryCpu,
    Accepted(Vec<u8>),
}

#[cfg(all(feature = "gpu", not(target_family = "wasm")))]
impl GpuFlateExecutor {
    #[cfg(feature = "cuda")]
    fn classify_transcode(
        &self,
        result: zlib_rs::GpuTranscodeResult,
        limit: usize,
    ) -> Result<FlateTranscode> {
        // A rejected prefix can have an unknown decoded length. Respect a
        // nonzero ratio gate by falling back if eligibility cannot be proven.
        match result.code {
            zlib_rs::ReturnCode::Ok
                if result.decoded_len.is_some_and(|n| self.eligible(n, limit)) =>
            {
                Ok(FlateTranscode::Accepted(result.output))
            }
            zlib_rs::ReturnCode::BufError
                if result.output.len() == limit
                    && (self.config.min_cap_per_mille == 0
                        || result.decoded_len.is_some_and(|n| self.eligible(n, limit))) =>
            {
                Ok(if self.config.retry_rejected {
                    FlateTranscode::RetryCpu
                } else {
                    FlateTranscode::Rejected
                })
            }
            zlib_rs::ReturnCode::Ok
            | zlib_rs::ReturnCode::BufError
            | zlib_rs::ReturnCode::DataError => Ok(FlateTranscode::NotRouted),
            code => Err(compression_code_error(code)),
        }
    }

    fn searcher(
        &self,
        stats: Option<&Mutex<crate::FlateBatchStats>>,
    ) -> Result<&zlib_rs::GpuMatchSearcher> {
        self.searcher
            .get_or_init(|| {
                let start = std::time::Instant::now();
                let result = zlib_rs::GpuMatchSearcher::new(self.config.adapter.as_deref());
                if let Some(stats) = stats {
                    stats
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .gpu_initialization_ms = start.elapsed().as_secs_f64() * 1000.0;
                }
                result
            })
            .as_ref()
            .map_err(|error| Error::Compression(format!("GPU Flate initialization: {error}")))
    }
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

    #[cfg(all(feature = "cuda", not(target_family = "wasm")))]
    pub(crate) fn transcode_enabled(&self) -> bool {
        self.gpu.as_ref().is_some_and(|gpu| {
            gpu.config.transcode_source
                && gpu
                    .config
                    .adapter
                    .as_deref()
                    .is_some_and(|a| a.starts_with("cuda:"))
        })
    }

    /// Run detached stream jobs on the same bounded pool as Flate
    /// compression. A worker owns the decoded input only until its job
    /// completes; no document-wide decoded buffer batch is needed.
    pub(crate) fn for_each<T: Send>(&self, tasks: &mut [T], run: impl Fn(&mut T) + Sync) {
        #[cfg(not(target_family = "wasm"))]
        if let Some(pool) = &self.compressor {
            pool.for_each(tasks, run);
            return;
        }
        tasks.iter_mut().for_each(run);
    }

    #[cfg(all(feature = "cuda", not(target_family = "wasm")))]
    fn record_transcode(&self, result: &zlib_rs::GpuTranscodeResult) {
        let Some(stats) = &self.stats else {
            return;
        };
        if result.decoded_bytes_processed == 0 {
            return;
        }
        let mut stats = stats.lock().unwrap_or_else(PoisonError::into_inner);
        let record = |target: &mut crate::report::FlateBatchWork| {
            target.batches = target.batches.saturating_add(1);
            target.streams = target.streams.saturating_add(1);
            target.decoded_bytes = target
                .decoded_bytes
                .saturating_add(result.decoded_bytes_processed);
        };
        record(&mut stats.gpu_assisted);
        record(&mut stats.gpu_transcoded);
        if result.device_indexed {
            stats.gpu_device_indexed_streams = stats.gpu_device_indexed_streams.saturating_add(1);
        }
        stats.gpu_device_tokens = stats
            .gpu_device_tokens
            .saturating_add(result.device_tokens as usize);
        stats.gpu_transcode_source_bytes = stats
            .gpu_transcode_source_bytes
            .saturating_add(result.source_bytes_uploaded);
        stats.gpu_transcode_descriptor_bytes = stats
            .gpu_transcode_descriptor_bytes
            .saturating_add(result.descriptor_bytes_uploaded);
    }

    /// Transcode one detached job. Independent jobs lease different CUDA
    /// lanes concurrently, while other workers decode/encode fallback jobs.
    #[cfg(all(feature = "cuda", not(target_family = "wasm")))]
    pub(crate) fn transcode_source(
        &self,
        source: &[u8],
        limit: usize,
        level: crate::FlateLevel,
    ) -> Result<FlateTranscode> {
        let Some(gpu) = &self.gpu else {
            return Ok(FlateTranscode::NotRouted);
        };
        if !self.transcode_enabled() {
            return Ok(FlateTranscode::NotRouted);
        }
        let window = gpu.config.match_window_bytes.unwrap_or_else(|| {
            std::num::NonZeroUsize::new(16 * 1024 * 1024).unwrap_or(std::num::NonZeroUsize::MIN)
        });
        // Keep the measured single-window routing policy unchanged.
        let Ok(minimum_decoded_len) = u32::try_from(
            gpu.config
                .min_stream_bytes
                .max(window.get().saturating_add(1)),
        ) else {
            return Ok(FlateTranscode::NotRouted);
        };
        let decode_bound = hayro_syntax::Limits::DEFAULT
            .max_decoded_stream_size
            .bound()
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(u32::MAX);
        let decoded_limit = if gpu.config.min_cap_per_mille == 0 {
            decode_bound
        } else {
            u32::try_from((limit as u128) * 1000 / u128::from(gpu.config.min_cap_per_mille))
                .unwrap_or(u32::MAX)
                .min(decode_bound)
        };
        if minimum_decoded_len > decoded_limit
            || minimum_decoded_len as usize > source.len().saturating_mul(1032)
        {
            return Ok(FlateTranscode::NotRouted);
        }
        let config = flate_config(level);
        let searcher = gpu.searcher(self.stats.as_ref())?;
        if !searcher.supports_zlib_transcode(config) {
            return Ok(FlateTranscode::NotRouted);
        }
        let result = searcher
            .recompress_zlib(
                source,
                zlib_rs::GpuTranscodeOptions {
                    output_limit: limit,
                    decoded_limit,
                    minimum_decoded_len,
                    config,
                    search_budget: gpu.config.search_budget.get(),
                    window,
                },
            )
            .map_err(|error| Error::Compression(format!("GPU Flate transcode: {error}")))?;
        let Some(result) = result else {
            return Ok(FlateTranscode::NotRouted);
        };
        self.record_transcode(&result);
        gpu.classify_transcode(result, limit)
    }

    #[cfg(all(feature = "cuda", not(target_family = "wasm")))]
    pub(crate) fn retry_transcoded(
        &self,
        decoded: &[u8],
        limit: usize,
        level: crate::FlateLevel,
    ) -> Result<Option<Vec<u8>>> {
        let mut output = Vec::new();
        let result = self.compress_cpu(
            &mut [zlib_rs::BatchJob::with_vec(
                decoded,
                &mut output,
                limit.min(zlib_rs::compress_bound(decoded.len())),
            )
            .map_err(|error| {
                Error::Compression(format!("failed to reserve Flate retry: {error}"))
            })?],
            flate_config(level),
        )[0];
        let accepted = flate_length_with_limit(result.written, result.code)?.map(|_| output);
        if let Some(stats) = &self.stats {
            let mut stats = stats.lock().unwrap_or_else(PoisonError::into_inner);
            stats.gpu_rejected_cpu_retries = stats.gpu_rejected_cpu_retries.saturating_add(1);
            if accepted.is_some() {
                stats.gpu_rejected_cpu_accepted = stats.gpu_rejected_cpu_accepted.saturating_add(1);
            }
        }
        Ok(accepted)
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

    #[cfg_attr(
        not(all(feature = "gpu", not(target_family = "wasm"))),
        expect(
            clippy::unnecessary_wraps,
            reason = "GPU dispatch can fail; CPU-only dispatch shares this contract"
        )
    )]
    fn compress_jobs(
        &self,
        mut jobs: Vec<zlib_rs::BatchJob<'_>>,
        config: zlib_rs::DeflateConfig,
        maximum_lengths: &[usize],
    ) -> Result<Vec<zlib_rs::BatchCompressResult>> {
        #[cfg(not(all(feature = "gpu", not(target_family = "wasm"))))]
        let _ = maximum_lengths;
        #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
        if let Some(gpu) = &self.gpu
            && jobs
                .iter()
                .zip(maximum_lengths)
                .any(|(job, &limit)| gpu.eligible(job.input.len(), limit))
        {
            let searcher = gpu.searcher(self.stats.as_ref())?;
            let search_budget = gpu.config.search_budget.get();
            let count = jobs.len();
            let (cpu_jobs, gpu_jobs): (Vec<_>, Vec<_>) = jobs
                .into_iter()
                .enumerate()
                .partition(|(index, job)| !gpu.eligible(job.input.len(), maximum_lengths[*index]));
            let (cpu_indices, mut cpu_jobs): (Vec<_>, Vec<_>) = cpu_jobs.into_iter().unzip();
            let (gpu_indices, mut gpu_jobs): (Vec<_>, Vec<_>) = gpu_jobs.into_iter().unzip();
            self.record_jobs(&gpu_jobs, true);
            let (cpu_results, mut gpu_results) = if let Some(pool) = self.compressor.as_deref() {
                self.record_jobs(&cpu_jobs, false);
                pool.compress_mixed_gpu(
                    &mut cpu_jobs,
                    &mut gpu_jobs,
                    config,
                    searcher,
                    search_budget,
                    gpu.config.match_window_bytes,
                )
                .map_err(|error| Error::Compression(format!("GPU Flate search: {error}")))?
            } else {
                let cpu_results = self.compress_cpu(&mut cpu_jobs, config);
                let gpu_results = match gpu.config.match_window_bytes {
                    Some(window) => zlib_rs::compress_slices_batch_gpu_windowed(
                        &mut gpu_jobs,
                        config,
                        searcher,
                        search_budget,
                        window,
                    ),
                    None => zlib_rs::compress_slices_batch_gpu(
                        &mut gpu_jobs,
                        config,
                        searcher,
                        search_budget,
                    ),
                }
                .map_err(|error| Error::Compression(format!("GPU Flate search: {error}")))?;
                (cpu_results, gpu_results)
            };
            if gpu.config.retry_rejected {
                self.retry_rejected_gpu_jobs(gpu_jobs, &mut gpu_results, config);
            }
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

    #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
    fn retry_rejected_gpu_jobs(
        &self,
        jobs: Vec<zlib_rs::BatchJob<'_>>,
        results: &mut [zlib_rs::BatchCompressResult],
        config: zlib_rs::DeflateConfig,
    ) {
        let (indices, mut retry_jobs): (Vec<_>, Vec<_>) = jobs
            .into_iter()
            .enumerate()
            .filter(|(index, _)| results[*index].code == zlib_rs::ReturnCode::BufError)
            .unzip();
        if retry_jobs.is_empty() {
            return;
        }
        let retries = self.compress_cpu(&mut retry_jobs, config);
        if let Some(stats) = &self.stats {
            let mut stats = stats.lock().unwrap_or_else(PoisonError::into_inner);
            stats.gpu_rejected_cpu_retries =
                stats.gpu_rejected_cpu_retries.saturating_add(retries.len());
            stats.gpu_rejected_cpu_accepted = stats.gpu_rejected_cpu_accepted.saturating_add(
                retries
                    .iter()
                    .filter(|result| result.code == zlib_rs::ReturnCode::Ok)
                    .count(),
            );
        }
        for (index, result) in indices.into_iter().zip(retries) {
            results[index] = result;
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
        let results = self.compress_jobs(jobs, flate_config(level), maximum_lengths)?;
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
        #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
        if self.gpu.is_some() {
            return Ok(self
                .encode_many_with_limits(inputs, maximum_lengths, level)?
                .into_iter()
                .map(|output| output.map(|bytes| bytes.len()))
                .collect());
        }
        if inputs.len() != maximum_lengths.len() {
            return Err(Error::Invalid(
                "Flate input and output-limit counts differ".to_owned(),
            ));
        }
        let mut jobs = inputs
            .iter()
            .zip(maximum_lengths)
            .map(|(&input, &limit)| (input, limit, Ok(None)))
            .collect::<Vec<_>>();
        self.for_each(&mut jobs, |(input, limit, result)| {
            *result = flate_capped_encoded_len(input, *limit, level);
        });
        if let Some(stats) = &self.stats {
            let mut stats = stats.lock().unwrap_or_else(PoisonError::into_inner);
            stats.cpu.batches = stats.cpu.batches.saturating_add(1);
            stats.cpu.streams = stats.cpu.streams.saturating_add(inputs.len());
            stats.cpu.decoded_bytes = inputs.iter().fold(stats.cpu.decoded_bytes, |n, input| {
                n.saturating_add(input.len())
            });
        }
        jobs.into_iter().map(|(_, _, result)| result).collect()
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
    // Only the length is needed. Drive the same ordinary zlib encoder to
    // Finish using a bounded scratch output instead of allocating a bound
    // proportional to input size. Probe one extra byte to distinguish exact
    // cap equality from a still-pending trailer.
    let mut encoder = zlib_rs::Deflate::new_with_config(flate_config(level));
    let mut scratch = [std::mem::MaybeUninit::<u8>::uninit(); 16 * 1024];
    loop {
        let previous_in = encoder.total_in();
        let consumed = usize::try_from(previous_in)
            .map_err(|_| Error::Compression("Flate input counter overflow".into()))?;
        let written = usize::try_from(encoder.total_out())
            .map_err(|_| Error::Compression("Flate output counter overflow".into()))?;
        let capacity = scratch
            .len()
            .min(maximum_length.saturating_sub(written).saturating_add(1));
        let remaining = &decoded[consumed..];
        // The streaming API exposes the ABI's u32 input cursor. As in
        // compress_slice, do not finish until the final input chunk is visible.
        let chunk_length = remaining.len().min(u32::MAX as usize);
        let flush = if chunk_length == remaining.len() {
            zlib_rs::DeflateFlush::Finish
        } else {
            zlib_rs::DeflateFlush::NoFlush
        };
        let status = encoder
            .compress_uninit(&remaining[..chunk_length], &mut scratch[..capacity], flush)
            .map_err(|error| Error::Compression(format!("Flate length measurement: {error:?}")))?;
        let total = usize::try_from(encoder.total_out())
            .map_err(|_| Error::Compression("Flate output counter overflow".into()))?;
        if total > maximum_length {
            return Ok(None);
        }
        if status == zlib_rs::Status::StreamEnd {
            return Ok(Some(total));
        }
        if total == written && encoder.total_in() == previous_in {
            return Err(Error::Compression(
                "Flate length measurement made no progress".into(),
            ));
        }
    }
}

/// The ordinary zlib encoding, only if it fits the exact byte budget.
pub fn encode_flate_capped(
    decoded: &[u8],
    maximum_length: usize,
    level: crate::FlateLevel,
) -> Result<Option<Vec<u8>>> {
    let mut output = Vec::new();
    let limit = maximum_length.min(zlib_rs::compress_bound(decoded.len()));
    let result = zlib_rs::BatchJob::with_vec(decoded, &mut output, limit)
        .map_err(|error| Error::Compression(format!("failed to reserve Flate output: {error}")))?
        .compress(flate_config(level));
    Ok(flate_length_with_limit(result.written, result.code)?.map(|_| output))
}

/// Return the encoded byte length of ordinary zlib/Flate data.
pub fn flate_encoded_len(decoded: &[u8], level: crate::FlateLevel) -> Result<usize> {
    flate_capped_encoded_len(decoded, usize::MAX, level)?
        .ok_or_else(|| Error::Compression("Flate output exceeded usize".into()))
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
    fn scalar_vector_destination_matches_initialized_slice() -> Result<()> {
        let mut state = 0x3278_1234_u32;
        let random = (0..128 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect::<Vec<_>>();
        let repeated = b"Repeated PDF content stream operands and operators. ".repeat(4096);
        for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
            for input in [b"".as_slice(), b"a", random.as_slice(), repeated.as_slice()] {
                let mut reference = vec![0; zlib_rs::compress_bound(input.len())];
                let (expected, code) =
                    zlib_rs::compress_slice(&mut reference, input, flate_config(level));
                assert_eq!(code, zlib_rs::ReturnCode::Ok);
                for cap in [
                    0,
                    expected.len() - 1,
                    expected.len(),
                    expected.len() + 1,
                    usize::MAX,
                ] {
                    let actual = encode_flate_capped(input, cap, level)?;
                    assert_eq!(
                        actual.as_deref(),
                        (expected.len() <= cap).then_some(expected.as_ref())
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn length_sink_matches_one_shot_across_scratch_boundaries() -> Result<()> {
        let mut state = 0x8765_4321_u32;
        let data = (0..512 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state.to_le_bytes()[0]
            })
            .collect::<Vec<_>>();
        for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
            for size in [0, 1, 16_383, 16_384, 16_385, 65_536, data.len()] {
                let input = &data[..size];
                let expected = encode_flate(input, level)?.len();
                for cap in [
                    0,
                    1,
                    16_384,
                    expected - 1,
                    expected,
                    expected + 1,
                    usize::MAX,
                ] {
                    assert_eq!(
                        flate_capped_encoded_len(input, cap, level)?,
                        (expected <= cap).then_some(expected)
                    );
                }
            }
        }
        Ok(())
    }

    #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
    #[test]
    fn tight_cap_routing_does_not_initialize_gpu() -> Result<()> {
        let input = b"highly repetitive PDF data".repeat(1024);
        let expected = encode_flate(&input, crate::FlateLevel::SIZE)?;
        let config = crate::Config {
            flate_gpu: Some(crate::FlateGpuConfig {
                adapter: Some("intentionally nonexistent adapter".to_owned()),
                search_budget: std::num::NonZeroU32::MIN,
                min_stream_bytes: 0,
                min_cap_per_mille: 50,
                match_window_bytes: None,
                retry_rejected: false,
                transcode_source: false,
            }),
            ..crate::Config::default()
        };
        let encoder = FlateBatchEncoder::from_config(&config)?.for_document();
        let limits = [expected.len()];
        assert_eq!(
            encoder.encode_many_capped(&[&input], &limits, crate::FlateLevel::SIZE)?,
            [Some(expected)]
        );
        assert_eq!(encoder.stats().cpu.streams, 1);
        assert_eq!(encoder.stats().gpu_assisted.streams, 0);
        assert!(encoder.stats().gpu_initialization_ms <= 0.0);
        Ok(())
    }

    #[cfg(all(feature = "gpu", not(target_family = "wasm")))]
    #[test]
    fn cpu_retry_replaces_only_gpu_cap_rejections() -> Result<()> {
        let encoder = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?.for_document();
        let input = b"repeatable GPU test input ".repeat(2048);
        let expected = encode_flate(&input, crate::FlateLevel::SIZE)?;
        let mut outputs = [vec![42], vec![0], vec![0], vec![43]];
        let mut results = [
            zlib_rs::BatchCompressResult {
                written: 1,
                code: zlib_rs::ReturnCode::Ok,
            },
            zlib_rs::BatchCompressResult {
                written: 1,
                code: zlib_rs::ReturnCode::BufError,
            },
            zlib_rs::BatchCompressResult {
                written: 1,
                code: zlib_rs::ReturnCode::BufError,
            },
            zlib_rs::BatchCompressResult {
                written: 1,
                code: zlib_rs::ReturnCode::StreamError,
            },
        ];
        let jobs = outputs
            .iter_mut()
            .zip([
                expected.len(),
                expected.len(),
                expected.len() - 1,
                expected.len(),
            ])
            .map(|(output, cap)| {
                zlib_rs::BatchJob::with_vec(&input, output, cap)
                    .map_err(|error| Error::Compression(error.to_string()))
            })
            .collect::<Result<Vec<_>>>()?;
        encoder.retry_rejected_gpu_jobs(jobs, &mut results, flate_config(crate::FlateLevel::SIZE));
        assert_eq!(outputs[0], [42]);
        assert_eq!(outputs[1], expected);
        assert_eq!(outputs[3], [43]);
        assert_eq!(results[0].code, zlib_rs::ReturnCode::Ok);
        assert_eq!(results[1].code, zlib_rs::ReturnCode::Ok);
        assert_eq!(results[2].code, zlib_rs::ReturnCode::BufError);
        assert_eq!(results[3].code, zlib_rs::ReturnCode::StreamError);
        assert_eq!(encoder.stats().cpu.streams, 2);
        assert_eq!(encoder.stats().gpu_rejected_cpu_retries, 2);
        assert_eq!(encoder.stats().gpu_rejected_cpu_accepted, 1);
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
                min_cap_per_mille: 0,
                match_window_bytes: None,
                retry_rejected: false,
                transcode_source: false,
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
        for window in [None, std::num::NonZeroUsize::new(65_537)] {
            config
                .flate_gpu
                .as_mut()
                .ok_or_else(|| Error::Invalid("missing GPU configuration".into()))?
                .match_window_bytes = window;
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
                min_cap_per_mille: 0,
                match_window_bytes: None,
                retry_rejected: false,
                transcode_source: false,
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
