use crate::stream_codec::{FlateBatchEncoder, set_plain_flate};
use crate::{ObjectHandle, Result};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlateOptimizationStats {
    pub streams_selected: usize,
    pub estimated_savings_bytes: usize,
    pub high_effort_streams_tested: usize,
    pub high_effort_streams_selected: usize,
    pub high_effort_extra_savings_bytes: usize,
}

const MIN_FLATE_SAVINGS_BYTES: usize = 1024;
const MIN_FLATE_SAVINGS_PERCENT: usize = 5;
const ADAPTIVE_FLATE_MIN_DECODED_BYTES: usize = 1024 * 1024;
const ADAPTIVE_FLATE_MAX_BASELINE_RATIO_PERCENT: usize = 10;
const MAX_REUSED_FLATE_BYTES: usize = 64 * 1024 * 1024;
// The fresh PDF writer emits ` /Filter /FlateDecode` for an unfiltered
// stream. An encoded payload must save more than those 21 dictionary bytes
// before adding the filter can decrease the output size.
const NEW_FLATE_FILTER_BYTES: usize = b" /Filter /FlateDecode".len();
// RFC 1950 requires a two-byte header and a four-byte Adler-32 trailer,
// even before accounting for the (nonempty) DEFLATE payload.
const MIN_ZLIB_WRAPPER_BYTES: usize = 6;

// Keep independent stream compression parallel without retaining every decoded
// payload and its worst-case compressed buffer for the entire PDF at once.
// A single stream larger than the limit is processed alone.
#[derive(Clone, Copy)]
struct FlateBatchLimits {
    max_input_bytes: usize,
    max_streams: usize,
}

impl FlateBatchLimits {
    const DEFAULT: Self = Self {
        max_input_bytes: 32 * 1024 * 1024,
        max_streams: 64,
    };

    const fn full(self, count: usize, input_bytes: usize) -> bool {
        count >= self.max_streams || input_bytes >= self.max_input_bytes
    }
}

#[derive(Debug)]
struct FlateReuseEntry {
    level: crate::FlateLevel,
    decoded_sha256: [u8; 32],
    // Match the actual retained allocation to the cache byte budget,
    // rather than retaining an often much larger compression bound.
    encoded: Box<[u8]>,
}

#[derive(Debug, Default)]
pub struct FlateReuseCache {
    entries: BTreeMap<ObjectHandle, FlateReuseEntry>,
    retained_bytes: usize,
}

impl FlateReuseCache {
    #[cfg(test)]
    pub(crate) fn insert(
        &mut self,
        handle: ObjectHandle,
        level: crate::FlateLevel,
        decoded: &[u8],
        encoded: Vec<u8>,
    ) {
        self.insert_with_digest(handle, level, Sha256::digest(decoded).into(), encoded);
    }

    pub(crate) fn insert_with_digest(
        &mut self,
        handle: ObjectHandle,
        level: crate::FlateLevel,
        decoded_sha256: [u8; 32],
        encoded: Vec<u8>,
    ) {
        if encoded.len() > MAX_REUSED_FLATE_BYTES.saturating_sub(self.retained_bytes)
            || self.entries.contains_key(&handle)
        {
            return;
        }
        self.retained_bytes = self.retained_bytes.saturating_add(encoded.len());
        self.entries.insert(
            handle,
            FlateReuseEntry {
                level,
                decoded_sha256,
                encoded: encoded.into_boxed_slice(),
            },
        );
    }

    fn take_matching(
        &mut self,
        handle: ObjectHandle,
        level: crate::FlateLevel,
        decoded: &[u8],
    ) -> Option<Vec<u8>> {
        let entry = self.entries.remove(&handle)?;
        self.retained_bytes = self.retained_bytes.saturating_sub(entry.encoded.len());
        let decoded_sha256: [u8; 32] = Sha256::digest(decoded).into();
        (entry.level == level && entry.decoded_sha256 == decoded_sha256)
            .then(|| entry.encoded.into_vec())
    }
}

struct AdaptiveFlateEncoding {
    bytes: Vec<u8>,
    high_effort_tested: bool,
    high_effort_selected: bool,
    high_effort_extra_savings_bytes: usize,
}

impl AdaptiveFlateEncoding {
    const fn baseline(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            high_effort_tested: false,
            high_effort_selected: false,
            high_effort_extra_savings_bytes: 0,
        }
    }

    fn consider_high_effort(&mut self, high_effort: Option<Vec<u8>>) {
        self.high_effort_tested = true;
        if let Some(high_effort) = high_effort
            && high_effort.len() < self.bytes.len()
        {
            self.high_effort_extra_savings_bytes =
                self.bytes.len().saturating_sub(high_effort.len());
            self.bytes = high_effort;
            self.high_effort_selected = true;
        }
    }
}

fn should_test_high_effort(
    decoded_len: usize,
    level: crate::FlateLevel,
    baseline_len: usize,
    allow_high_effort: bool,
) -> bool {
    allow_high_effort
        && level != crate::FlateLevel::SIZE
        && decoded_len >= ADAPTIVE_FLATE_MIN_DECODED_BYTES
        && baseline_len.saturating_mul(100)
            <= decoded_len.saturating_mul(ADAPTIVE_FLATE_MAX_BASELINE_RATIO_PERCENT)
}

/// Keep ordinary bytes in their original source or overlay allocation. Only
/// retain an owned buffer when decoding the source returned allocated bytes.
#[derive(Debug)]
enum UnfilteredFlateInput {
    Source(crate::ObjectId),
    Overlay(ObjectHandle),
    Decrypted(Vec<u8>),
}

struct UnfilteredFlateCandidate {
    handle: ObjectHandle,
    raw: UnfilteredFlateInput,
    raw_len: usize,
    encoded: Option<AdaptiveFlateEncoding>,
}

impl UnfilteredFlateCandidate {
    fn raw_bytes<'a>(
        &'a self,
        document: &'a crate::EditDocument,
    ) -> Result<std::borrow::Cow<'a, [u8]>> {
        match &self.raw {
            UnfilteredFlateInput::Source(id) => document.source().stream_data(*id),
            UnfilteredFlateInput::Decrypted(bytes) => Ok(std::borrow::Cow::Borrowed(bytes)),
            UnfilteredFlateInput::Overlay(handle) => match document.current_value(*handle)? {
                Some(crate::source::CurrentValue::Owned(crate::OwnedObject::Stream {
                    data,
                    ..
                })) => data.bytes(document.source()),
                _ => Err(crate::Error::Invalid(
                    "Flate candidate no longer references an owned stream".into(),
                )),
            },
        }
    }
}

fn collect_unfiltered_candidate(
    document: &mut crate::EditDocument,
    handle: ObjectHandle,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
) -> Result<Option<UnfilteredFlateCandidate>> {
    let Some(object) = document.current_owned_object_cow(handle)? else {
        return Ok(None);
    };
    let crate::OwnedObject::Stream { dictionary, data } = object.as_ref() else {
        return Ok(None);
    };
    // External /F streams do not have their authoritative payload in the
    // inline stream bytes. Never compress their fallback or strip filters.
    if crate::stream_codec::has_external_file_stream_entries(dictionary)
        || has_special_stream_type(document, dictionary)?
    {
        return Ok(None);
    }
    let raw = data.bytes(document.source())?;
    let has_filter = dictionary.get(b"Filter".as_slice()).is_some_and(|value| {
        !matches!(
            document.resolve_owned_value(value),
            Ok(Some(crate::OwnedObject::Null) | None)
        )
    });
    if raw.is_empty() {
        drop(raw);
        drop(object);
        if has_filter {
            let object = document.edit_handle(handle)?;
            if let crate::OwnedObject::Stream { dictionary, data } = object {
                dictionary.remove(b"Filter".as_slice());
                dictionary.remove(b"DecodeParms".as_slice());
                dictionary.remove(b"Length".as_slice());
                *data = crate::StreamData::Owned(Vec::new().into());
            }
        }
        return Ok(None);
    }
    if has_filter || raw.len() <= NEW_FLATE_FILTER_BYTES + MIN_ZLIB_WRAPPER_BYTES {
        // Even the theoretical zero-byte DEFLATE payload cannot recover the
        // added /Filter dictionary entry for a stream this small.
        return Ok(None);
    }

    let raw_len = raw.len();
    // Both optimization goals may have measured an exact encoded candidate
    // during page transformation. Reuse it only when the decoded bytes and
    // requested compression level still match; changed pages fall through.
    let encoded = reuse_cache
        .take_matching(handle, level, &raw)
        .map(AdaptiveFlateEncoding::baseline);
    // If Hayro returned an owned buffer (e.g. decrypted bytes), preserve
    // that buffer for both compression passes instead of decrypting again.
    let raw = match raw {
        std::borrow::Cow::Borrowed(_) => match data {
            crate::StreamData::Source(id) => UnfilteredFlateInput::Source(*id),
            crate::StreamData::Owned(_) => UnfilteredFlateInput::Overlay(handle),
        },
        std::borrow::Cow::Owned(bytes) => UnfilteredFlateInput::Decrypted(bytes),
    };
    Ok(Some(UnfilteredFlateCandidate {
        handle,
        raw,
        raw_len,
        encoded,
    }))
}

fn batch_missing_baselines(
    document: &crate::EditDocument,
    candidates: &mut [UnfilteredFlateCandidate],
    level: crate::FlateLevel,
    batcher: &FlateBatchEncoder,
) -> Result<()> {
    let indices = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| candidate.encoded.is_none().then_some(index))
        .collect::<Vec<_>>();
    let outputs = {
        // These Cows borrow ordinary streams from SourcePdf. Keep any
        // decryption-owned buffers alive for the full compression batch.
        let raw_buffers = indices
            .iter()
            .map(|&index| candidates[index].raw_bytes(document))
            .collect::<Result<Vec<_>>>()?;
        let inputs = raw_buffers.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        // A new /Filter costs 21 bytes, and an equal-size rewrite is useless.
        // No encoding longer than raw_len - 22 can ever be selected.
        let limits = indices
            .iter()
            .map(|&index| {
                candidates[index]
                    .raw_len
                    .saturating_sub(NEW_FLATE_FILTER_BYTES + 1)
            })
            .collect::<Vec<_>>();
        batcher.encode_many_capped(&inputs, &limits, level)?
    };
    for (index, bytes) in indices.into_iter().zip(outputs) {
        candidates[index].encoded = bytes.map(AdaptiveFlateEncoding::baseline);
    }
    Ok(())
}

fn batch_high_effort_candidates(
    document: &crate::EditDocument,
    candidates: &mut [UnfilteredFlateCandidate],
    level: crate::FlateLevel,
    batcher: &FlateBatchEncoder,
) -> Result<()> {
    let indices = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let encoded = candidate.encoded.as_ref()?;
            should_test_high_effort(candidate.raw_len, level, encoded.bytes.len(), true)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let outputs = {
        let raw_buffers = indices
            .iter()
            .map(|&index| candidates[index].raw_bytes(document))
            .collect::<Result<Vec<_>>>()?;
        let inputs = raw_buffers.iter().map(AsRef::as_ref).collect::<Vec<_>>();
        // High effort is selected only if its result is strictly smaller
        // than baseline. Cap the buffer at baseline_len - 1.
        let limits = indices
            .iter()
            .map(|&index| {
                candidates[index]
                    .encoded
                    .as_ref()
                    .map_or(0, |encoded| encoded.bytes.len().saturating_sub(1))
            })
            .collect::<Vec<_>>();
        batcher.encode_many_capped(&inputs, &limits, crate::FlateLevel::SIZE)?
    };
    for (index, bytes) in indices.into_iter().zip(outputs) {
        if let Some(encoded) = &mut candidates[index].encoded {
            encoded.consider_high_effort(bytes);
        }
    }
    Ok(())
}

// ceil(raw_len * percent / 100), split to avoid overflowing usize
// for large inputs. The same bound is used for admission and allocation.
fn minimum_flate_savings(raw_len: usize) -> usize {
    let proportional = (raw_len / 100) * MIN_FLATE_SAVINGS_PERCENT
        + ((raw_len % 100) * MIN_FLATE_SAVINGS_PERCENT).div_ceil(100);
    MIN_FLATE_SAVINGS_BYTES.max(proportional)
}

fn has_special_stream_type(
    document: &crate::EditDocument,
    dictionary: &crate::OwnedDictionary,
) -> Result<bool> {
    let Some(kind) = dictionary.get(b"Type".as_slice()) else {
        return Ok(false);
    };
    Ok(
        matches!(document.resolve_owned_value(kind)?, Some(crate::OwnedObject::Name(name)) if matches!(name.as_slice(), b"Metadata" | b"ObjStm" | b"XRef")),
    )
}

fn is_safe_lone_flate(
    document: &crate::EditDocument,
    dictionary: &crate::OwnedDictionary,
) -> Result<bool> {
    // Reuse the codec's definition of one Flate filter, including PDF's
    // /Fl abbreviation, one-element filter arrays, and indirect names.
    // An absent /Filter is eligible for initial compression, not recompression.
    Ok(dictionary.contains_key(b"Filter".as_slice())
        && crate::stream_codec::is_unfiltered_or_lone_flate(document, dictionary)?
        && !has_special_stream_type(document, dictionary)?)
}

struct FilteredFlateCandidate {
    handle: ObjectHandle,
    raw_len: usize,
    decoded: Vec<u8>,
    encoded: Option<AdaptiveFlateEncoding>,
}

fn flush_filtered_batch(
    document: &mut crate::EditDocument,
    candidates: &mut Vec<FilteredFlateCandidate>,
    level: crate::FlateLevel,
    batcher: &FlateBatchEncoder,
    stats: &mut FlateOptimizationStats,
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    let indices = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| candidate.encoded.is_none().then_some(index))
        .collect::<Vec<_>>();
    let outputs = {
        let inputs = indices
            .iter()
            .map(|&index| candidates[index].decoded.as_slice())
            .collect::<Vec<_>>();
        // Keep only outputs satisfying both exact admission thresholds.
        // This is especially important when a highly compressed source
        // expands to a very large decoded buffer.
        let limits = indices
            .iter()
            .map(|&index| {
                let raw_len = candidates[index].raw_len;
                raw_len.saturating_sub(minimum_flate_savings(raw_len))
            })
            .collect::<Vec<_>>();
        batcher.encode_many_capped(&inputs, &limits, level)?
    };
    for (index, bytes) in indices.into_iter().zip(outputs) {
        candidates[index].encoded = bytes.map(AdaptiveFlateEncoding::baseline);
    }

    for candidate in candidates.drain(..) {
        let Some(encoded) = candidate.encoded else {
            continue;
        };
        if encoded.high_effort_tested {
            stats.high_effort_streams_tested = stats.high_effort_streams_tested.saturating_add(1);
        }
        let saving = candidate.raw_len.saturating_sub(encoded.bytes.len());
        if saving < minimum_flate_savings(candidate.raw_len) {
            continue;
        }
        if encoded.high_effort_selected {
            stats.high_effort_streams_selected =
                stats.high_effort_streams_selected.saturating_add(1);
            stats.high_effort_extra_savings_bytes = stats
                .high_effort_extra_savings_bytes
                .saturating_add(encoded.high_effort_extra_savings_bytes);
        }
        let object = document.edit_handle(candidate.handle)?;
        if let crate::OwnedObject::Stream { dictionary, data } = object {
            set_plain_flate(dictionary);
            *data = crate::StreamData::Owned(encoded.bytes.into());
            stats.streams_selected = stats.streams_selected.saturating_add(1);
            stats.estimated_savings_bytes = stats.estimated_savings_bytes.saturating_add(saving);
        }
    }
    Ok(())
}

fn optimize_flate_streams_bounded(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
    batcher: &FlateBatchEncoder,
    limits: FlateBatchLimits,
) -> Result<FlateOptimizationStats> {
    let mut candidates = Vec::new();
    let mut staged_bytes = 0_usize;
    let mut stats = FlateOptimizationStats::default();
    for handle in document.reachable_streams()? {
        let Some(object) = document.current_owned_object_cow(handle)? else {
            continue;
        };
        let crate::OwnedObject::Stream { dictionary, data } = object.as_ref() else {
            continue;
        };
        if !is_safe_lone_flate(document, dictionary)? {
            continue;
        }
        let raw_len = data.bytes(document.source())?.len();
        drop(object);
        if raw_len < MIN_FLATE_SAVINGS_BYTES + MIN_ZLIB_WRAPPER_BYTES {
            // An RFC 1950 stream always needs at least six framing bytes.
            // There is no possible encoding that clears the savings floor.
            continue;
        }
        let Ok(decoded) = document.decoded_stream_data(handle) else {
            continue;
        };
        let encoded = reuse_cache
            .take_matching(handle, level, &decoded)
            .map(AdaptiveFlateEncoding::baseline);
        staged_bytes = staged_bytes.saturating_add(decoded.len());
        candidates.push(FilteredFlateCandidate {
            handle,
            raw_len,
            decoded,
            encoded,
        });
        if limits.full(candidates.len(), staged_bytes) {
            flush_filtered_batch(document, &mut candidates, level, batcher, &mut stats)?;
            staged_bytes = 0;
        }
    }
    flush_filtered_batch(document, &mut candidates, level, batcher, &mut stats)?;
    Ok(stats)
}

pub fn optimize_flate_streams_with_cache(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
    batcher: &FlateBatchEncoder,
) -> Result<FlateOptimizationStats> {
    optimize_flate_streams_bounded(
        document,
        level,
        reuse_cache,
        batcher,
        FlateBatchLimits::DEFAULT,
    )
}

fn flush_unfiltered_batch(
    document: &mut crate::EditDocument,
    candidates: &mut Vec<UnfilteredFlateCandidate>,
    level: crate::FlateLevel,
    batcher: &FlateBatchEncoder,
    stats: &mut FlateOptimizationStats,
) -> Result<()> {
    if candidates.is_empty() {
        return Ok(());
    }
    batch_missing_baselines(document, candidates, level, batcher)?;
    batch_high_effort_candidates(document, candidates, level, batcher)?;

    for candidate in candidates.drain(..) {
        let Some(encoded) = candidate.encoded else {
            continue;
        };
        if encoded.high_effort_tested {
            stats.high_effort_streams_tested = stats.high_effort_streams_tested.saturating_add(1);
        }
        let saving = candidate.raw_len.saturating_sub(encoded.bytes.len());
        if saving <= NEW_FLATE_FILTER_BYTES {
            continue;
        }
        if encoded.high_effort_selected {
            stats.high_effort_streams_selected =
                stats.high_effort_streams_selected.saturating_add(1);
            stats.high_effort_extra_savings_bytes = stats
                .high_effort_extra_savings_bytes
                .saturating_add(encoded.high_effort_extra_savings_bytes);
        }
        let object = document.edit_handle(candidate.handle)?;
        if let crate::OwnedObject::Stream { dictionary, data } = object {
            dictionary.insert(
                b"Filter".to_vec(),
                crate::OwnedObject::Name(b"FlateDecode".to_vec()),
            );
            dictionary.remove(b"DecodeParms".as_slice());
            dictionary.remove(b"Length".as_slice());
            *data = crate::StreamData::Owned(encoded.bytes.into());
            stats.streams_selected = stats.streams_selected.saturating_add(1);
            stats.estimated_savings_bytes = stats
                .estimated_savings_bytes
                .saturating_add(saving - NEW_FLATE_FILTER_BYTES);
        }
    }
    Ok(())
}

fn compress_unfiltered_streams_bounded(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
    batcher: &FlateBatchEncoder,
    limits: FlateBatchLimits,
) -> Result<FlateOptimizationStats> {
    // Preserve cached exact size-mode encodings and process the remaining
    // streams in bounded groups, rather than holding all source and output
    // buffers in memory until the final rewrite.
    let mut candidates = Vec::new();
    let mut staged_bytes = 0_usize;
    let mut stats = FlateOptimizationStats::default();
    for handle in document.reachable_streams()? {
        let Some(candidate) = collect_unfiltered_candidate(document, handle, level, reuse_cache)?
        else {
            continue;
        };
        staged_bytes = staged_bytes.saturating_add(candidate.raw_len);
        candidates.push(candidate);
        if limits.full(candidates.len(), staged_bytes) {
            flush_unfiltered_batch(document, &mut candidates, level, batcher, &mut stats)?;
            staged_bytes = 0;
        }
    }
    flush_unfiltered_batch(document, &mut candidates, level, batcher, &mut stats)?;
    Ok(stats)
}

pub fn compress_unfiltered_streams(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
    batcher: &FlateBatchEncoder,
) -> Result<FlateOptimizationStats> {
    compress_unfiltered_streams_bounded(
        document,
        level,
        reuse_cache,
        batcher,
        FlateBatchLimits::DEFAULT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external_stream_fixture(
        external_keys: &[&[u8]],
        filtered: bool,
        data: &[u8],
    ) -> crate::Result<(crate::EditDocument, crate::ObjectHandle)> {
        use crate::test_support::ClassicPdfBuilder;
        use crate::{OwnedDictionary, OwnedObject, StreamData};

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        )?;
        let mut document = crate::EditDocument::from_bytes(pdf.finish(1)?)?;
        let mut dictionary = OwnedDictionary::new();
        for &key in external_keys {
            let value = match key {
                b"F" => OwnedObject::String(b"remote.bin".to_vec()),
                b"FFilter" => OwnedObject::Name(b"FlateDecode".to_vec()),
                b"FDecodeParms" => OwnedObject::Dictionary(OwnedDictionary::from([(
                    b"Predictor".to_vec(),
                    OwnedObject::Integer(1),
                )])),
                _ => {
                    return Err(crate::Error::Invalid(
                        "unknown external test key".to_owned(),
                    ));
                }
            };
            dictionary.insert(key.to_vec(), value);
        }
        if filtered {
            dictionary.insert(
                b"Filter".to_vec(),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            );
        }
        let handle = crate::ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary,
            data: StreamData::Owned(data.to_vec().into()),
        }));
        let page = document.edit_object(crate::ObjectId::new(3, 0))?;
        let page = page
            .as_dictionary_mut()
            .ok_or_else(|| crate::Error::Invalid("test page is not a dictionary".to_owned()))?;
        page.insert(b"Contents".to_vec(), OwnedObject::Reference(handle));
        Ok((document, handle))
    }

    #[test]
    fn minimum_flate_savings_handles_overflow_and_rounding() {
        for raw_len in [
            0,
            1,
            1024,
            1030,
            20_479,
            20_480,
            20_481,
            1_048_576,
            usize::MAX,
        ] {
            assert_eq!(
                minimum_flate_savings(raw_len),
                MIN_FLATE_SAVINGS_BYTES.max(raw_len / 20 + usize::from(raw_len % 20 != 0)),
                "saving threshold for raw length {raw_len}",
            );
        }
    }

    fn assert_source_candidate_borrows(
        document: &mut crate::EditDocument,
        handle: ObjectHandle,
        level: crate::FlateLevel,
        expected_len: usize,
    ) -> crate::Result<()> {
        let candidate =
            collect_unfiltered_candidate(document, handle, level, &mut FlateReuseCache::default())?
                .ok_or_else(|| crate::Error::Invalid("missing source candidate".into()))?;
        assert_eq!(candidate.raw_len, expected_len);
        let ObjectHandle::Existing(id) = handle else {
            return Err(crate::Error::Invalid("expected source handle".into()));
        };
        assert!(matches!(&candidate.raw, UnfilteredFlateInput::Source(raw_id) if *raw_id == id));
        let borrowed = candidate.raw_bytes(document)?;
        let original = document.source().stream_data(id)?;
        assert!(matches!(&borrowed, std::borrow::Cow::Borrowed(_)));
        assert_eq!(borrowed.as_ptr(), original.as_ptr());
        Ok(())
    }

    fn assert_overlay_candidate_borrows(
        document: &mut crate::EditDocument,
        handle: ObjectHandle,
        level: crate::FlateLevel,
    ) -> crate::Result<()> {
        use crate::{OwnedObject, StreamData};

        let candidate =
            collect_unfiltered_candidate(document, handle, level, &mut FlateReuseCache::default())?
                .ok_or_else(|| crate::Error::Invalid("missing overlay candidate".into()))?;
        assert!(
            matches!(&candidate.raw, UnfilteredFlateInput::Overlay(raw_handle) if *raw_handle == handle)
        );
        let Some(crate::source::CurrentValue::Owned(OwnedObject::Stream {
            data: StreamData::Owned(bytes),
            ..
        })) = document.current_value(handle)?
        else {
            return Err(crate::Error::Invalid("missing owned overlay bytes".into()));
        };
        let raw = candidate.raw_bytes(document)?;
        assert!(matches!(&raw, std::borrow::Cow::Borrowed(_)));
        assert_eq!(raw.as_ptr(), bytes.as_ptr());
        Ok(())
    }

    #[test]
    fn source_backed_unfiltered_streams_encode_identically_to_overlay_owned() -> crate::Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        use crate::{ObjectId, OwnedObject, StreamData};

        let compressible = b"BT (source backed contents) Tj ET\n".repeat(2_000);
        let mut state = 0xB3F7_AE12_DAC1_0439_u64;
        let incompressible = (0..128 * 1024)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state.to_le_bytes()[3]
            })
            .collect::<Vec<_>>();
        let mut builder = ClassicPdfBuilder::new();
        builder.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        builder.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        builder.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents [4 0 R 5 0 R] >>",
        )?;
        builder.stream(4, b"", &compressible)?;
        builder.stream(5, b"", &incompressible)?;
        let source_pdf = builder.finish(1)?;

        for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
            for workers in [1, 4] {
                let mut source = crate::EditDocument::from_bytes(source_pdf.clone())?;
                let mut overlay = crate::EditDocument::from_bytes(source_pdf.clone())?;
                let handles = [
                    ObjectHandle::Existing(ObjectId::new(4, 0)),
                    ObjectHandle::Existing(ObjectId::new(5, 0)),
                ];
                assert_eq!(source.reachable_streams()?, handles);

                // Source handles must not materialize a second payload copy.
                for (handle, expected_len) in handles
                    .into_iter()
                    .zip([compressible.len(), incompressible.len()])
                {
                    assert_source_candidate_borrows(&mut source, handle, level, expected_len)?;

                    // Store the unchanged source bytes as an owned COW payload
                    // to check both representations take the same encoder path.
                    let raw = match overlay.current_owned_object(handle)? {
                        Some(OwnedObject::Stream { data, .. }) => {
                            data.bytes(overlay.source())?.into_owned()
                        }
                        _ => return Err(crate::Error::Invalid("missing source data".into())),
                    };
                    if let OwnedObject::Stream { data, .. } = overlay.edit_handle(handle)? {
                        *data = StreamData::Owned(raw.into());
                    }
                }

                for handle in handles {
                    assert_overlay_candidate_borrows(&mut overlay, handle, level)?;
                }

                let batcher = FlateBatchEncoder::new(
                    std::num::NonZeroUsize::new(workers)
                        .ok_or_else(|| crate::Error::Invalid("zero workers".into()))?,
                )?;
                let source_stats = compress_unfiltered_streams(
                    &mut source,
                    level,
                    &mut FlateReuseCache::default(),
                    &batcher,
                )?;
                let overlay_stats = compress_unfiltered_streams(
                    &mut overlay,
                    level,
                    &mut FlateReuseCache::default(),
                    &batcher,
                )?;
                assert_eq!(source_stats, overlay_stats);
                assert_eq!(source_stats.streams_selected, 1);
                assert_eq!(
                    crate::writer::write_pdf(&source)?,
                    crate::writer::write_pdf(&overlay)?,
                    "mode={}, workers={workers}",
                    level.value()
                );
                for (handle, expected) in handles
                    .into_iter()
                    .zip([compressible.as_slice(), incompressible.as_slice()])
                {
                    assert_eq!(source.decoded_stream_data(handle)?, expected);
                }
            }
        }
        Ok(())
    }

    #[test]
    fn newly_added_overlay_stream_borrows_payload_until_compression() -> crate::Result<()> {
        let source = vec![b'Q'; 2 * 1024 * 1024];
        let (mut document, handle) = external_stream_fixture(&[], false, &source)?;
        assert_overlay_candidate_borrows(&mut document, handle, crate::FlateLevel::PROCESSING)?;
        let stats = compress_unfiltered_streams(
            &mut document,
            crate::FlateLevel::PROCESSING,
            &mut FlateReuseCache::default(),
            &FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?,
        )?;
        assert_eq!(stats.streams_selected, 1);
        assert_eq!(document.decoded_stream_data(handle)?, source);
        let rewritten = crate::EditDocument::from_bytes(crate::writer::write_pdf(&document)?)?;
        assert_eq!(
            rewritten.decoded_stream_data(rewritten.reachable_streams()?[0])?,
            source
        );
        Ok(())
    }

    #[test]
    fn flate_size_floor_skips_unprofitable_streams_without_changing_output() -> crate::Result<()> {
        // A zlib stream always carries at least six wrapper bytes, regardless
        // of the compression algorithm's best achievable DEFLATE payload.
        assert_eq!(MIN_ZLIB_WRAPPER_BYTES, 6);
        let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;

        for raw_len in [0_usize, 1, 6, 21, 22, 27, 28, 29, 64, 256] {
            let source = vec![b'A'; raw_len];
            let (mut document, handle) = external_stream_fixture(&[], false, &source)?;
            let before = crate::writer::write_pdf(&document)?;
            let candidate = collect_unfiltered_candidate(
                &mut document,
                handle,
                crate::FlateLevel::PROCESSING,
                &mut FlateReuseCache::default(),
            )?;
            assert_eq!(
                candidate.is_none(),
                raw_len <= NEW_FLATE_FILTER_BYTES + MIN_ZLIB_WRAPPER_BYTES,
                "unfiltered candidate gate at {raw_len} bytes"
            );
            let encoded =
                crate::stream_codec::encode_flate(&source, crate::FlateLevel::PROCESSING)?;
            let should_compress =
                source.len().saturating_sub(encoded.len()) > NEW_FLATE_FILTER_BYTES;
            let stats = compress_unfiltered_streams(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut FlateReuseCache::default(),
                &batcher,
            )?;
            assert_eq!(stats.streams_selected, usize::from(should_compress));
            let after = crate::writer::write_pdf(&document)?;
            if should_compress {
                assert!(after.len() < before.len());
                let reloaded = crate::EditDocument::from_bytes(after)?;
                let streams = reloaded.reachable_streams()?;
                assert_eq!(streams.len(), 1);
                assert_eq!(reloaded.decoded_stream_data(streams[0])?, source);
            } else {
                assert_eq!(after, before, "unfiltered stream {raw_len} changed");
            }
        }
        Ok(())
    }

    #[test]
    fn capped_recompression_handles_large_expansion_without_changing_streams() -> crate::Result<()>
    {
        // A tiny compressed PDF stream can inflate to many MiB. Even though
        // the input must be decoded, its output allocation should be limited
        // by the original encoded size, not its uncompressed length.
        let decoded = vec![0_u8; 8 * 1024 * 1024];
        let original = crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::SIZE)?;
        let allowed = original
            .len()
            .saturating_sub(minimum_flate_savings(original.len()));
        assert!(original.len() < decoded.len() / 100);
        assert!(allowed < decoded.len() / 100);

        let (mut document, handle) = flate_filter_case_document(0, &original)?;
        let before = crate::writer::write_pdf(&document)?;
        let baseline = crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::PROCESSING)?;
        let expected_rewrite = baseline.len() <= allowed;
        let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
        let stats = optimize_flate_streams_with_cache(
            &mut document,
            crate::FlateLevel::PROCESSING,
            &mut FlateReuseCache::default(),
            &batcher,
        )?;
        assert_eq!(stats.streams_selected, usize::from(expected_rewrite));
        let after = crate::writer::write_pdf(&document)?;
        if expected_rewrite {
            assert!(after.len() < before.len());
        } else {
            assert_eq!(after, before);
        }
        assert_eq!(document.decoded_stream_data(handle)?, decoded);
        let reloaded = crate::EditDocument::from_bytes(after)?;
        let streams = reloaded.reachable_streams()?;
        assert_eq!(streams.len(), 1);
        assert_eq!(reloaded.decoded_stream_data(streams[0])?, decoded);
        Ok(())
    }

    #[test]
    fn filtered_flate_size_floor_skips_decoding_until_savings_are_possible() -> crate::Result<()> {
        // Level 0 emits a stored block, so source payload size and encoded
        // length can be selected precisely across the 1,030-byte boundary.
        for encoded_len in [1028_usize, 1029, 1030, 1031, 2048] {
            let decoded = vec![b'A'; encoded_len - 11];
            let mut encoded = vec![0_u8; zlib_rs::compress_bound(decoded.len())];
            let (result, code) =
                zlib_rs::compress_slice(&mut encoded, &decoded, zlib_rs::DeflateConfig::new(0));
            assert_eq!(code, zlib_rs::ReturnCode::Ok);
            let result_len = result.len();
            encoded.truncate(result_len);
            assert_eq!(encoded.len(), encoded_len);

            let (mut document, handle) = flate_filter_case_document(0, &encoded)?;
            assert!(document.reachable_streams()?.contains(&handle));
            let before = crate::writer::write_pdf(&document)?;
            let baseline =
                crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::PROCESSING)?;
            let saving = encoded.len().saturating_sub(baseline.len());
            let should_recompress = saving >= MIN_FLATE_SAVINGS_BYTES
                && saving.saturating_mul(100)
                    >= encoded.len().saturating_mul(MIN_FLATE_SAVINGS_PERCENT);
            let stats = optimize_flate_streams_with_cache(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut FlateReuseCache::default(),
                &FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?,
            )?;
            assert_eq!(
                stats.streams_selected,
                usize::from(should_recompress),
                "encoded length {encoded_len}"
            );
            let after = crate::writer::write_pdf(&document)?;
            if should_recompress {
                let reloaded = crate::EditDocument::from_bytes(after)?;
                let streams = reloaded.reachable_streams()?;
                assert_eq!(streams.len(), 1);
                assert_eq!(reloaded.decoded_stream_data(streams[0])?, decoded);
            } else {
                assert_eq!(after, before, "ineligible Flate stream changed");
            }
        }
        Ok(())
    }

    #[test]
    fn unfiltered_external_file_streams_are_never_recompressed() -> crate::Result<()> {
        // Rewriting the inline fallback while retaining /F changes how the
        // source PDF resolves an external-file stream. Treat every external
        // filter declaration as an ownership boundary, even with no /Filter.
        let source = vec![b'X'; 64 * 1024];
        for entries in [
            &[b"F".as_slice()][..],
            &[b"FFilter".as_slice()][..],
            &[b"FDecodeParms".as_slice()][..],
            &[b"F".as_slice(), b"FFilter".as_slice()][..],
        ] {
            let (mut document, handle) = external_stream_fixture(entries, false, &source)?;
            assert!(document.reachable_streams()?.contains(&handle));
            let before = document.current_owned_object(handle)?;
            let mut reuse = FlateReuseCache::default();
            assert!(
                collect_unfiltered_candidate(
                    &mut document,
                    handle,
                    crate::FlateLevel::PROCESSING,
                    &mut reuse,
                )?
                .is_none(),
                "external-file stream was admitted for recompression: {entries:?}"
            );
            let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
            let _ = compress_unfiltered_streams(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut reuse,
                &batcher,
            )?;
            assert_eq!(
                document.current_owned_object(handle)?,
                before,
                "external stream was mutated: {entries:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn prefiltered_external_file_streams_are_never_recompressed() -> crate::Result<()> {
        let decoded = vec![b'Q'; 64 * 1024];
        let encoded = crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::PROCESSING)?;
        for entries in [
            &[b"F".as_slice()][..],
            &[b"FFilter".as_slice()][..],
            &[b"FDecodeParms".as_slice()][..],
        ] {
            let (mut document, handle) = external_stream_fixture(entries, true, &encoded)?;
            let before = document.current_owned_object(handle)?;
            let mut reuse = FlateReuseCache::default();
            let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
            let _ = optimize_flate_streams_with_cache(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut reuse,
                &batcher,
            )?;
            assert_eq!(
                document.current_owned_object(handle)?,
                before,
                "external filtered stream was rewritten: {entries:?}",
            );
        }
        Ok(())
    }

    #[test]
    fn empty_external_streams_preserve_their_filter_dictionary() -> crate::Result<()> {
        // Empty ordinary Flate streams can drop their filters, but external
        // streams cannot: their inline bytes are not the external payload.
        let (mut document, handle) = external_stream_fixture(&[b"F"], true, b"")?;
        let before = document.current_owned_object(handle)?;
        let mut reuse = FlateReuseCache::default();
        let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
        let _ = compress_unfiltered_streams(
            &mut document,
            crate::FlateLevel::PROCESSING,
            &mut reuse,
            &batcher,
        )?;
        assert_eq!(document.current_owned_object(handle)?, before);
        Ok(())
    }

    #[test]
    fn unfiltered_compression_never_grows_written_pdf() -> crate::Result<()> {
        let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
        let incompressible = (0..131_072)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                seed.to_le_bytes()[4]
            })
            .collect::<Vec<_>>();
        let samples = [
            (b"q Q".to_vec(), false),
            (incompressible, false),
            (vec![b'A'; 8192], true),
            (b"BT /F1 12 Tf (Hello world) Tj ET\n".repeat(256), true),
        ];
        for (source, should_compress) in samples {
            let (mut document, handle) = external_stream_fixture(&[], false, &source)?;
            let before_object = document.current_owned_object(handle)?;
            let before_pdf = crate::writer::write_pdf(&document)?;
            let encoded =
                crate::stream_codec::encode_flate(&source, crate::FlateLevel::PROCESSING)?;
            assert_eq!(
                source.len().saturating_sub(encoded.len()) > NEW_FLATE_FILTER_BYTES,
                should_compress
            );

            let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
            let stats = compress_unfiltered_streams(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut FlateReuseCache::default(),
                &batcher,
            )?;
            assert_eq!(stats.streams_selected, usize::from(should_compress));
            let after_pdf = crate::writer::write_pdf(&document)?;
            if should_compress {
                assert!(after_pdf.len() < before_pdf.len());
                assert_eq!(
                    stats.estimated_savings_bytes,
                    source.len() - encoded.len() - NEW_FLATE_FILTER_BYTES
                );
                assert_eq!(document.decoded_stream_data(handle)?, source);
                let reloaded = crate::EditDocument::from_bytes(after_pdf)?;
                let streams = reloaded.reachable_streams()?;
                assert_eq!(streams.len(), 1);
                assert_eq!(reloaded.decoded_stream_data(streams[0])?, source);
            } else {
                assert_eq!(document.current_owned_object(handle)?, before_object);
                assert_eq!(after_pdf, before_pdf);
                assert_eq!(stats.estimated_savings_bytes, 0);
            }
        }
        Ok(())
    }

    #[test]
    fn unfiltered_special_stream_types_keep_their_encoding() -> crate::Result<()> {
        use crate::OwnedObject;

        for kind in [b"Metadata".as_slice(), b"ObjStm", b"XRef"] {
            let (mut document, handle) = external_stream_fixture(&[], false, &[b'X'; 8192])?;
            if let OwnedObject::Stream { dictionary, .. } = document.edit_handle(handle)? {
                dictionary.insert(b"Type".to_vec(), OwnedObject::Name(kind.to_vec()));
            }
            let before = document.current_owned_object(handle)?;
            let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
            let stats = compress_unfiltered_streams(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut FlateReuseCache::default(),
                &batcher,
            )?;
            assert_eq!(stats.streams_selected, 0, "{kind:?}");
            assert_eq!(document.current_owned_object(handle)?, before, "{kind:?}");
        }
        Ok(())
    }

    #[expect(
        clippy::cast_possible_truncation,
        reason = "modulo 251 bounds the generated sample to the u8 range"
    )]
    fn many_streams_fixture(
        filtered: bool,
    ) -> crate::Result<(crate::EditDocument, Vec<ObjectHandle>, Vec<Vec<u8>>)> {
        use crate::test_support::ClassicPdfBuilder;
        use crate::{OwnedDictionary, OwnedObject, StreamData};

        let mut builder = ClassicPdfBuilder::new();
        builder.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        builder.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        builder.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        )?;
        let mut document = crate::EditDocument::from_bytes(builder.finish(1)?)?;
        let mut handles = Vec::new();
        let mut decoded_payloads = Vec::new();
        for index in 0..76_usize {
            let length = if index % 17 == 0 {
                1_200_000
            } else if index % 11 == 0 {
                0
            } else if index % 7 == 0 {
                48
            } else {
                2048 + (index * 871) % 58_000
            };
            let decoded = (0..length)
                .map(|offset| ((offset / 11 + offset % 37 + index * 3) % 251) as u8)
                .collect::<Vec<_>>();
            let mut dictionary = OwnedDictionary::new();
            if filtered {
                dictionary.insert(
                    b"Filter".to_vec(),
                    OwnedObject::Name(b"FlateDecode".to_vec()),
                );
            }
            if index % 19 == 0 {
                dictionary.insert(b"F".to_vec(), OwnedObject::String(b"external.bin".to_vec()));
            }
            if index % 23 == 0 {
                dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"Metadata".to_vec()));
            }
            let data = if filtered {
                let mut destination = vec![0_u8; zlib_rs::compress_bound(decoded.len())];
                let (encoded, code) = zlib_rs::compress_slice(
                    &mut destination,
                    &decoded,
                    zlib_rs::DeflateConfig::new(0),
                );
                assert_eq!(code, zlib_rs::ReturnCode::Ok);
                let length = encoded.len();
                destination.truncate(length);
                destination
            } else {
                decoded.clone()
            };
            let handle = ObjectHandle::New(document.add_object(OwnedObject::Stream {
                dictionary,
                data: StreamData::Owned(data.into()),
            }));
            handles.push(handle);
            decoded_payloads.push(decoded);
        }
        let page = document.edit_object(crate::ObjectId::new(3, 0))?;
        let page = page.as_dictionary_mut().ok_or_else(|| {
            crate::Error::Invalid("batch-test page is not a dictionary".to_owned())
        })?;
        page.insert(
            b"Contents".to_vec(),
            OwnedObject::Array(
                handles
                    .iter()
                    .copied()
                    .map(OwnedObject::Reference)
                    .collect(),
            ),
        );
        Ok((document, handles, decoded_payloads))
    }

    #[test]
    fn bounded_batches_preserve_exact_streams_and_stats() -> crate::Result<()> {
        let limits = [
            FlateBatchLimits {
                max_input_bytes: 12 * 1024,
                max_streams: 3,
            },
            FlateBatchLimits::DEFAULT,
            FlateBatchLimits {
                max_input_bytes: usize::MAX,
                max_streams: usize::MAX,
            },
        ];
        // Exercise both compression phases, actual useful recompression, empty
        // streams, source-cache reuse, external streams, large single inputs
        // and worker-pool scheduling across multiple flush boundaries.
        for filtered in [false, true] {
            for level in [crate::FlateLevel::PROCESSING, crate::FlateLevel::SIZE] {
                let mut results = Vec::new();
                for (run_index, limit) in limits.into_iter().enumerate() {
                    let (mut document, handles, payloads) = many_streams_fixture(filtered)?;
                    assert_eq!(document.reachable_streams()?.len(), handles.len());
                    let mut reuse_cache = FlateReuseCache::default();
                    for index in (1..handles.len()).step_by(13) {
                        reuse_cache.insert(
                            handles[index],
                            level,
                            &payloads[index],
                            crate::stream_codec::encode_flate(&payloads[index], level)?,
                        );
                    }
                    let batcher = FlateBatchEncoder::new(
                        std::num::NonZeroUsize::new(4).unwrap_or(std::num::NonZeroUsize::MIN),
                    )?;
                    let stats = match (filtered, run_index) {
                        (true, 1) => optimize_flate_streams_with_cache(
                            &mut document,
                            level,
                            &mut reuse_cache,
                            &batcher,
                        )?,
                        (false, 1) => compress_unfiltered_streams(
                            &mut document,
                            level,
                            &mut reuse_cache,
                            &batcher,
                        )?,
                        (true, _) => optimize_flate_streams_bounded(
                            &mut document,
                            level,
                            &mut reuse_cache,
                            &batcher,
                            limit,
                        )?,
                        (false, _) => compress_unfiltered_streams_bounded(
                            &mut document,
                            level,
                            &mut reuse_cache,
                            &batcher,
                            limit,
                        )?,
                    };
                    let objects = handles
                        .into_iter()
                        .map(|handle| document.current_owned_object(handle))
                        .collect::<crate::Result<Vec<_>>>()?;
                    results.push((stats, objects));
                }
                assert_eq!(
                    results[0],
                    results[1],
                    "batch boundary changed encoded bytes or accounting: filtered={filtered}, level={}",
                    level.value(),
                );
                assert_eq!(
                    results[1],
                    results[2],
                    "production default differs from unlimited batch: filtered={filtered}, level={}",
                    level.value(),
                );
                if filtered {
                    assert!(results[0].0.streams_selected > 0);
                }
            }
        }
        Ok(())
    }

    fn flate_filter_case_document(
        case: usize,
        encoded: &[u8],
    ) -> crate::Result<(crate::EditDocument, ObjectHandle)> {
        use crate::test_support::ClassicPdfBuilder;
        use crate::{OwnedDictionary, OwnedObject, StreamData};

        let mut builder = ClassicPdfBuilder::new();
        builder.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        builder.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        builder.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        )?;
        let mut document = crate::EditDocument::from_bytes(builder.finish(1)?)?;
        let filter = match case {
            0 | 10 | 11 | 12 => OwnedObject::Name(b"FlateDecode".to_vec()),
            1 => OwnedObject::Name(b"Fl".to_vec()),
            2 | 13 => OwnedObject::Array(vec![OwnedObject::Name(b"FlateDecode".to_vec())]),
            3 => OwnedObject::Array(vec![OwnedObject::Name(b"Fl".to_vec())]),
            4 | 6 => {
                let name = ObjectHandle::New(
                    document.add_object(OwnedObject::Name(b"FlateDecode".to_vec())),
                );
                if case == 4 {
                    OwnedObject::Reference(name)
                } else {
                    OwnedObject::Array(vec![OwnedObject::Reference(name)])
                }
            }
            5 => OwnedObject::Reference(ObjectHandle::New(document.add_object(
                OwnedObject::Array(vec![OwnedObject::Name(b"FlateDecode".to_vec())]),
            ))),
            7 => OwnedObject::Null,
            8 => OwnedObject::Array(vec![
                OwnedObject::Name(b"FlateDecode".to_vec()),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            ]),
            9 => OwnedObject::Name(b"DCTDecode".to_vec()),
            _ => return Err(crate::Error::Invalid("invalid test case".to_owned())),
        };
        let mut dictionary = OwnedDictionary::from([(b"Filter".to_vec(), filter)]);
        if case == 10 {
            dictionary.insert(b"F".to_vec(), OwnedObject::String(b"external.dat".to_vec()));
        }
        if case == 11 {
            dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"Metadata".to_vec()));
        }
        if case == 12 || case == 13 {
            let params = OwnedObject::Dictionary(OwnedDictionary::from([(
                b"Predictor".to_vec(),
                OwnedObject::Integer(1),
            )]));
            dictionary.insert(
                b"DecodeParms".to_vec(),
                if case == 13 {
                    OwnedObject::Array(vec![params])
                } else {
                    params
                },
            );
        }
        let stream = OwnedObject::Stream {
            dictionary,
            data: StreamData::Owned(encoded.to_vec().into()),
        };
        let handle = ObjectHandle::New(document.add_object(stream));
        let page = document.edit_object(crate::ObjectId::new(3, 0))?;
        let page = page
            .as_dictionary_mut()
            .ok_or_else(|| crate::Error::Invalid("test page missing dictionary".to_owned()))?;
        page.insert(b"Contents".to_vec(), OwnedObject::Reference(handle));
        Ok((document, handle))
    }

    #[test]
    fn flate_filter_alias_arrays_and_references_recompress_losslessly() -> crate::Result<()> {
        use crate::OwnedObject;

        // Level-0 zlib is a valid PDF Flate stream that the size/processing
        // policies can shrink enough to pass the ordinary recompression gate.
        let decoded = b"BT /F1 12 Tf (sample text) Tj ET\n".repeat(8192);
        let mut source_encoded = vec![0_u8; zlib_rs::compress_bound(decoded.len())];
        let (encoded, compression_code) = zlib_rs::compress_slice(
            &mut source_encoded,
            &decoded,
            zlib_rs::DeflateConfig::new(0),
        );
        assert_eq!(compression_code, zlib_rs::ReturnCode::Ok);
        let encoded_len = encoded.len();
        source_encoded.truncate(encoded_len);

        for variant in 0..28 {
            let case = variant % 14;
            let level = if variant < 14 {
                crate::FlateLevel::PROCESSING
            } else {
                crate::FlateLevel::SIZE
            };
            let (mut document, handle) = flate_filter_case_document(case, &source_encoded)?;

            let expected_rewrite = !matches!(case, 7..=11);
            let before = document
                .current_owned_object(handle)?
                .ok_or_else(|| crate::Error::Invalid("test stream missing".to_owned()))?;
            let dictionary = before.as_dictionary().ok_or_else(|| {
                crate::Error::Invalid("test stream missing dictionary".to_owned())
            })?;
            assert_eq!(
                is_safe_lone_flate(&document, dictionary)?,
                expected_rewrite,
                "eligibility case {case}"
            );
            if expected_rewrite {
                assert_eq!(
                    document.decoded_stream_data(handle)?,
                    decoded,
                    "before case {case}"
                );
            }

            let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
            let stats = optimize_flate_streams_with_cache(
                &mut document,
                level,
                &mut FlateReuseCache::default(),
                &batcher,
            )?;
            assert_eq!(
                stats.streams_selected,
                usize::from(expected_rewrite),
                "selected case {case}, level {}",
                level.value()
            );
            let after = document
                .current_owned_object(handle)?
                .ok_or_else(|| crate::Error::Invalid("optimized stream missing".to_owned()))?;
            if expected_rewrite {
                let OwnedObject::Stream { dictionary, data } = after else {
                    return Err(crate::Error::Invalid(
                        "optimized object not stream".to_owned(),
                    ));
                };
                assert_eq!(
                    dictionary.get(b"Filter".as_slice()),
                    Some(&OwnedObject::Name(b"FlateDecode".to_vec())),
                    "normalized filter case {case}"
                );
                assert!(!dictionary.contains_key(b"DecodeParms".as_slice()));
                assert!(data.bytes(document.source())?.len() < encoded_len);
                assert_eq!(
                    document.decoded_stream_data(handle)?,
                    decoded,
                    "after case {case}"
                );

                // Also verify that canonicalizing aliases/indirect filters
                // survives the real output writer and another PDF parse.
                let output = crate::writer::write_pdf(&document)?;
                let reloaded = crate::EditDocument::from_bytes(output)?;
                let streams = reloaded.reachable_streams()?;
                assert_eq!(streams.len(), 1, "written stream graph case {case}");
                assert_eq!(
                    reloaded.decoded_stream_data(streams[0])?,
                    decoded,
                    "written PDF stream case {case}"
                );
            } else {
                assert_eq!(after, before, "excluded stream mutated in case {case}");
            }
        }
        Ok(())
    }

    #[test]
    fn flate_reuse_cache_requires_matching_level_and_decoded_bytes() {
        let handle = crate::ObjectHandle::Existing(crate::ObjectId::new(1, 0));
        let decoded = b"BT /F1 12 Tf (hello) Tj ET";
        let encoded = vec![1, 2, 3, 4];

        let mut cache = FlateReuseCache::default();
        cache.insert(
            handle,
            crate::FlateLevel::PROCESSING,
            decoded,
            encoded.clone(),
        );
        assert_eq!(
            cache.take_matching(handle, crate::FlateLevel::PROCESSING, decoded),
            Some(encoded)
        );

        cache.insert(handle, crate::FlateLevel::PROCESSING, decoded, vec![5, 6]);
        assert_eq!(
            cache.take_matching(handle, crate::FlateLevel::SIZE, decoded),
            None
        );

        cache.insert(handle, crate::FlateLevel::PROCESSING, decoded, vec![7, 8]);
        assert_eq!(
            cache.take_matching(
                handle,
                crate::FlateLevel::PROCESSING,
                b"BT /F1 12 Tf (changed) Tj ET"
            ),
            None
        );

        let digest = Sha256::digest(decoded).into();
        cache.insert_with_digest(handle, crate::FlateLevel::PROCESSING, digest, vec![9, 10]);
        assert_eq!(
            cache.take_matching(handle, crate::FlateLevel::PROCESSING, decoded),
            Some(vec![9, 10])
        );
    }

    #[test]
    fn processing_cached_stream_matches_uncached_compression() -> crate::Result<()> {
        // The final unfiltered-stream phase must honor a previously encoded
        // Processing stream, just as it already does for Size. The digest and
        // level guard are independently covered by the cache tests above.
        let raw = b"0 0 30 30 re f\n".repeat(4_096);
        let (mut cached, handle) = external_stream_fixture(&[], false, &raw)?;
        let (mut uncached, _) = external_stream_fixture(&[], false, &raw)?;
        let encoded = crate::stream_codec::encode_flate(&raw, crate::FlateLevel::PROCESSING)?;
        let mut reuse = FlateReuseCache::default();
        reuse.insert(handle, crate::FlateLevel::PROCESSING, &raw, encoded.clone());

        let candidate = collect_unfiltered_candidate(
            &mut cached,
            handle,
            crate::FlateLevel::PROCESSING,
            &mut reuse,
        )?
        .ok_or_else(|| crate::Error::Invalid("missing cached stream candidate".into()))?;
        assert_eq!(
            candidate
                .encoded
                .as_ref()
                .map(|entry| entry.bytes.as_slice()),
            Some(encoded.as_slice())
        );
        assert_eq!(reuse.retained_bytes, 0);
        reuse.insert(handle, crate::FlateLevel::PROCESSING, &raw, encoded);

        let batcher = FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;
        let from_cache = compress_unfiltered_streams(
            &mut cached,
            crate::FlateLevel::PROCESSING,
            &mut reuse,
            &batcher,
        )?;
        let freshly_encoded = compress_unfiltered_streams(
            &mut uncached,
            crate::FlateLevel::PROCESSING,
            &mut FlateReuseCache::default(),
            &batcher,
        )?;
        assert_eq!(from_cache, freshly_encoded);
        assert_eq!(from_cache.streams_selected, 1);
        assert_eq!(
            crate::writer::write_pdf(&cached)?,
            crate::writer::write_pdf(&uncached)?
        );
        Ok(())
    }

    #[test]
    fn adaptive_size_probe_can_be_disabled_for_processing() -> crate::Result<()> {
        let decoded = vec![0_u8; ADAPTIVE_FLATE_MIN_DECODED_BYTES];
        let baseline = crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::PROCESSING)?;
        assert!(!should_test_high_effort(
            decoded.len(),
            crate::FlateLevel::PROCESSING,
            baseline.len(),
            false
        ));
        assert!(should_test_high_effort(
            decoded.len(),
            crate::FlateLevel::PROCESSING,
            baseline.len(),
            true
        ));
        Ok(())
    }
}
