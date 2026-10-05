use crate::stream_codec::{encode_flate, set_plain_flate};
use crate::{FlatePolicy, Result};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlateOptimizationStats {
    pub streams_selected: usize,
    pub estimated_savings_bytes: usize,
    pub high_effort_streams_tested: usize,
    pub high_effort_streams_selected: usize,
    pub high_effort_extra_savings_bytes: usize,
}

const ADAPTIVE_FLATE_MIN_DECODED_BYTES: usize = 1024 * 1024;
const ADAPTIVE_FLATE_MAX_BASELINE_RATIO_PERCENT: usize = 10;

struct AdaptiveFlateEncoding {
    bytes: Vec<u8>,
    high_effort_tested: bool,
    high_effort_selected: bool,
    high_effort_extra_savings_bytes: usize,
}

fn adaptive_flate_encode(
    decoded: &[u8],
    level: i32,
    adaptive_high_effort: bool,
) -> crate::Result<AdaptiveFlateEncoding> {
    let baseline = encode_flate(decoded, level)?;
    if !adaptive_high_effort
        || level >= 9
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

    let high_effort = encode_flate(decoded, 9)?;
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

fn is_safe_lone_flate_hayro(
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

pub fn apply_flate_policy_hayro(
    document: &mut crate::EditDocument,
    policy: FlatePolicy,
    level: i32,
    adaptive_high_effort: bool,
) -> Result<FlateOptimizationStats> {
    let (min_savings_bytes, min_savings_percent, force) = match policy {
        FlatePolicy::Preserve => return Ok(FlateOptimizationStats::default()),
        FlatePolicy::Selective {
            min_savings_bytes,
            min_savings_percent,
        } => (min_savings_bytes, min_savings_percent, false),
        FlatePolicy::RecompressAll => (0, 0, true),
    };
    let mut stats = FlateOptimizationStats::default();
    for handle in document.reachable_streams()? {
        let Some(crate::OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        if !is_safe_lone_flate_hayro(document, &dictionary)? {
            continue;
        }
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = document.decoded_stream_data(handle) else {
            continue;
        };
        let Ok(encoded) = adaptive_flate_encode(&decoded, level, adaptive_high_effort) else {
            continue;
        };
        if encoded.high_effort_tested {
            stats.high_effort_streams_tested = stats.high_effort_streams_tested.saturating_add(1);
        }
        let high_effort_selected = encoded.high_effort_selected;
        let high_effort_extra_savings_bytes = encoded.high_effort_extra_savings_bytes;
        let repacked = encoded.bytes;
        let saving = raw.len().saturating_sub(repacked.len());
        if !force
            && (saving < min_savings_bytes
                || saving.saturating_mul(100)
                    < raw.len().saturating_mul(usize::from(min_savings_percent)))
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

pub fn compress_unfiltered_streams_hayro(
    document: &mut crate::EditDocument,
    level: i32,
    adaptive_high_effort: bool,
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
        let encoded = adaptive_flate_encode(&raw, level, adaptive_high_effort)?;
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
