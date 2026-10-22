use crate::stream_codec::{encode_flate, set_plain_flate};
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
                decoded_sha256: Sha256::digest(decoded).into(),
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

fn adaptive_flate_encode(
    decoded: &[u8],
    level: crate::FlateLevel,
    allow_high_effort: bool,
) -> crate::Result<AdaptiveFlateEncoding> {
    let baseline = encode_flate(decoded, level)?;
    if !allow_high_effort
        || level == crate::FlateLevel::SIZE
        || decoded.len() < ADAPTIVE_FLATE_MIN_DECODED_BYTES
        || baseline.len().saturating_mul(100)
            > decoded
                .len()
                .saturating_mul(ADAPTIVE_FLATE_MAX_BASELINE_RATIO_PERCENT)
    {
        return Ok(AdaptiveFlateEncoding {
            bytes: baseline,
            high_effort_tested: false,
            high_effort_selected: false,
            high_effort_extra_savings_bytes: 0,
        });
    }

    let high_effort = encode_flate(decoded, crate::FlateLevel::SIZE)?;
    if high_effort.len() < baseline.len() {
        let extra = baseline.len().saturating_sub(high_effort.len());
        Ok(AdaptiveFlateEncoding {
            bytes: high_effort,
            high_effort_tested: true,
            high_effort_selected: true,
            high_effort_extra_savings_bytes: extra,
        })
    } else {
        Ok(AdaptiveFlateEncoding {
            bytes: baseline,
            high_effort_tested: true,
            high_effort_selected: false,
            high_effort_extra_savings_bytes: 0,
        })
    }
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
    if dictionary.contains_key(b"F".as_slice()) {
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
) -> Result<FlateOptimizationStats> {
    let mut stats = FlateOptimizationStats::default();
    for handle in document.reachable_streams()? {
        let Some(crate::OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        if !is_safe_lone_flate(document, &dictionary)? {
            continue;
        }
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = document.decoded_stream_data(handle) else {
            continue;
        };
        // Existing filtered streams are already compressed and are numerous in
        // real documents. Processing mode deliberately honors its requested
        // level 6 here instead of paying for an opportunistic second level-9
        // encode; Size mode already requests level 9 as its baseline.
        let encoded = if let Some(encoded) = reuse_cache.take_matching(handle, level, &decoded) {
            AdaptiveFlateEncoding {
                bytes: encoded,
                high_effort_tested: false,
                high_effort_selected: false,
                high_effort_extra_savings_bytes: 0,
            }
        } else {
            let Ok(encoded) = adaptive_flate_encode(&decoded, level, false) else {
                continue;
            };
            encoded
        };
        if encoded.high_effort_tested {
            stats.high_effort_streams_tested = stats.high_effort_streams_tested.saturating_add(1);
        }
        let high_effort_selected = encoded.high_effort_selected;
        let high_effort_extra_savings_bytes = encoded.high_effort_extra_savings_bytes;
        let repacked = encoded.bytes;
        let saving = raw.len().saturating_sub(repacked.len());
        if saving < MIN_FLATE_SAVINGS_BYTES
            || saving.saturating_mul(100) < raw.len().saturating_mul(MIN_FLATE_SAVINGS_PERCENT)
        {
            continue;
        }
        if high_effort_selected {
            stats.high_effort_streams_selected =
                stats.high_effort_streams_selected.saturating_add(1);
            stats.high_effort_extra_savings_bytes = stats
                .high_effort_extra_savings_bytes
                .saturating_add(high_effort_extra_savings_bytes);
        }
        let object = document.edit_handle(handle)?;
        if let crate::OwnedObject::Stream { dictionary, data } = object {
            set_plain_flate(dictionary);
            *data = crate::StreamData::Owned(repacked);
            stats.streams_selected += 1;
            stats.estimated_savings_bytes += saving;
        }
    }
    Ok(stats)
}

pub fn compress_unfiltered_streams(
    document: &mut crate::EditDocument,
    level: crate::FlateLevel,
) -> Result<FlateOptimizationStats> {
    let handles = document.reachable_streams()?;
    let mut stats = FlateOptimizationStats::default();
    for handle in handles {
        let Some(crate::OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
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
        // The writer's historical StreamDataMode::Compress behavior always
        // applies Flate to non-empty unfiltered streams, even when a tiny stream grows.
        let encoded = adaptive_flate_encode(&raw, level, true)?;
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
        let object = document.edit_handle(handle)?;
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
    }

    #[test]
    fn adaptive_size_probe_can_be_disabled_for_processing() -> crate::Result<()> {
        let decoded = vec![0_u8; ADAPTIVE_FLATE_MIN_DECODED_BYTES];
        let baseline = encode_flate(&decoded, crate::FlateLevel::PROCESSING)?;
        let processing = adaptive_flate_encode(&decoded, crate::FlateLevel::PROCESSING, false)?;
        assert_eq!(processing.bytes, baseline);
        assert!(!processing.high_effort_tested);

        let adaptive = adaptive_flate_encode(&decoded, crate::FlateLevel::PROCESSING, true)?;
        assert!(adaptive.high_effort_tested);
        Ok(())
    }
}
