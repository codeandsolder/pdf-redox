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

#[derive(Debug)]
struct FlateReuseEntry {
    level: crate::FlateLevel,
    decoded_sha256: [u8; 32],
    encoded: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct FlateReuseCache {
    entries: BTreeMap<ObjectHandle, FlateReuseEntry>,
    retained_bytes: usize,
}

impl FlateReuseCache {
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
                encoded,
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
        (entry.level == level && entry.decoded_sha256 == decoded_sha256).then_some(entry.encoded)
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

    fn consider_high_effort(&mut self, high_effort: Vec<u8>) {
        self.high_effort_tested = true;
        if high_effort.len() < self.bytes.len() {
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

struct UnfilteredFlateCandidate {
    handle: ObjectHandle,
    raw: Vec<u8>,
    encoded: Option<AdaptiveFlateEncoding>,
}

fn collect_unfiltered_candidates(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
) -> Result<Vec<UnfilteredFlateCandidate>> {
    let mut candidates = Vec::new();
    for handle in document.reachable_streams()? {
        let Some(crate::OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        // External /F streams do not have their authoritative payload in the
        // inline stream bytes. Never compress their fallback or strip filters.
        if crate::stream_codec::has_external_file_stream_entries(&dictionary) {
            continue;
        }
        let raw = data.bytes(document.source())?.into_owned();
        let has_filter = dictionary.get(b"Filter".as_slice()).is_some_and(|value| {
            !matches!(
                document.resolve_owned_value(value),
                Ok(Some(crate::OwnedObject::Null) | None)
            )
        });
        if raw.is_empty() {
            if has_filter {
                let object = document.edit_handle(handle)?;
                if let crate::OwnedObject::Stream { dictionary, data } = object {
                    dictionary.remove(b"Filter".as_slice());
                    dictionary.remove(b"DecodeParms".as_slice());
                    dictionary.remove(b"Length".as_slice());
                    *data = crate::StreamData::Owned(Vec::new());
                }
            }
            continue;
        }
        if has_filter {
            continue;
        }

        let encoded = if level == crate::FlateLevel::SIZE {
            reuse_cache
                .take_matching(handle, level, &raw)
                .map(AdaptiveFlateEncoding::baseline)
        } else {
            None
        };
        candidates.push(UnfilteredFlateCandidate {
            handle,
            raw,
            encoded,
        });
    }
    Ok(candidates)
}

fn batch_missing_baselines(
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
        let inputs = indices
            .iter()
            .map(|&index| candidates[index].raw.as_slice())
            .collect::<Vec<_>>();
        batcher.encode_many(&inputs, level)?
    };
    for (index, bytes) in indices.into_iter().zip(outputs) {
        candidates[index].encoded = Some(AdaptiveFlateEncoding::baseline(bytes));
    }
    Ok(())
}

fn batch_high_effort_candidates(
    candidates: &mut [UnfilteredFlateCandidate],
    level: crate::FlateLevel,
    batcher: &FlateBatchEncoder,
) -> Result<()> {
    let indices = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| {
            let encoded = candidate.encoded.as_ref()?;
            should_test_high_effort(candidate.raw.len(), level, encoded.bytes.len(), true)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let outputs = {
        let inputs = indices
            .iter()
            .map(|&index| candidates[index].raw.as_slice())
            .collect::<Vec<_>>();
        batcher.encode_many(&inputs, crate::FlateLevel::SIZE)?
    };
    for (index, bytes) in indices.into_iter().zip(outputs) {
        if let Some(encoded) = &mut candidates[index].encoded {
            encoded.consider_high_effort(bytes);
        }
    }
    Ok(())
}

fn is_safe_lone_flate(
    document: &crate::EditDocument,
    dictionary: &crate::OwnedDictionary,
) -> Result<bool> {
    let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
        return Ok(false);
    };
    if !matches!(document.resolve_owned_value(filter)?, Some(crate::OwnedObject::Name(name)) if name == b"FlateDecode")
    {
        return Ok(false);
    }
    if crate::stream_codec::has_external_file_stream_entries(dictionary) {
        return Ok(false);
    }
    if let Some(kind) = dictionary.get(b"Type".as_slice())
        && matches!(document.resolve_owned_value(kind)?, Some(crate::OwnedObject::Name(name)) if matches!(name.as_slice(), b"Metadata" | b"ObjStm" | b"XRef"))
    {
        return Ok(false);
    }
    Ok(true)
}

pub fn optimize_flate_streams_with_cache(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
    batcher: &FlateBatchEncoder,
) -> Result<FlateOptimizationStats> {
    struct Candidate {
        handle: ObjectHandle,
        raw_len: usize,
        decoded: Vec<u8>,
        encoded: Option<AdaptiveFlateEncoding>,
    }

    let mut candidates = Vec::new();
    for handle in document.reachable_streams()? {
        let Some(crate::OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        if !is_safe_lone_flate(document, &dictionary)? {
            continue;
        }
        let raw_len = data.bytes(document.source())?.len();
        let Ok(decoded) = document.decoded_stream_data(handle) else {
            continue;
        };
        let encoded = reuse_cache
            .take_matching(handle, level, &decoded)
            .map(AdaptiveFlateEncoding::baseline);
        candidates.push(Candidate {
            handle,
            raw_len,
            decoded,
            encoded,
        });
    }

    let batch_indices = candidates
        .iter()
        .enumerate()
        .filter_map(|(index, candidate)| candidate.encoded.is_none().then_some(index))
        .collect::<Vec<_>>();
    let batch_outputs = {
        let inputs = batch_indices
            .iter()
            .map(|&index| candidates[index].decoded.as_slice())
            .collect::<Vec<_>>();
        batcher.encode_many(&inputs, level)?
    };
    for (index, bytes) in batch_indices.into_iter().zip(batch_outputs) {
        candidates[index].encoded = Some(AdaptiveFlateEncoding::baseline(bytes));
    }

    let mut stats = FlateOptimizationStats::default();
    for candidate in candidates {
        let Some(encoded) = candidate.encoded else {
            continue;
        };
        if encoded.high_effort_tested {
            stats.high_effort_streams_tested = stats.high_effort_streams_tested.saturating_add(1);
        }
        let saving = candidate.raw_len.saturating_sub(encoded.bytes.len());
        if saving < MIN_FLATE_SAVINGS_BYTES
            || saving.saturating_mul(100)
                < candidate.raw_len.saturating_mul(MIN_FLATE_SAVINGS_PERCENT)
        {
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
            *data = crate::StreamData::Owned(encoded.bytes);
            stats.streams_selected = stats.streams_selected.saturating_add(1);
            stats.estimated_savings_bytes = stats.estimated_savings_bytes.saturating_add(saving);
        }
    }
    Ok(stats)
}

pub fn compress_unfiltered_streams(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
    reuse_cache: &mut FlateReuseCache,
    batcher: &FlateBatchEncoder,
) -> Result<FlateOptimizationStats> {
    // Size-mode transforms may already have produced exact level-9 bytes for their admission gate;
    // collection preserves those while Processing leaves the baseline work for the batch executor.
    let mut candidates = collect_unfiltered_candidates(document, level, reuse_cache)?;
    batch_missing_baselines(&mut candidates, level, batcher)?;
    batch_high_effort_candidates(&mut candidates, level, batcher)?;

    let mut stats = FlateOptimizationStats::default();
    for candidate in candidates {
        let Some(encoded) = candidate.encoded else {
            continue;
        };
        if encoded.high_effort_tested {
            stats.high_effort_streams_tested = stats.high_effort_streams_tested.saturating_add(1);
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
            *data = crate::StreamData::Owned(encoded.bytes);
        }
    }
    Ok(stats)
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
            data: StreamData::Owned(data.to_vec()),
        }));
        let page = document.edit_object(crate::ObjectId::new(3, 0))?;
        let page = page
            .as_dictionary_mut()
            .ok_or_else(|| crate::Error::Invalid("test page is not a dictionary".to_owned()))?;
        page.insert(b"Contents".to_vec(), OwnedObject::Reference(handle));
        Ok((document, handle))
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
            let candidates = collect_unfiltered_candidates(
                &mut document,
                crate::FlateLevel::PROCESSING,
                &mut reuse,
            )?;
            assert!(
                candidates
                    .iter()
                    .all(|candidate| candidate.handle != handle),
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
