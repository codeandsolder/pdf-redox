use crate::stream_codec::{DecodeLevel, encode_flate, set_plain_flate};
use crate::{FlatePolicy, Result};
#[cfg(test)]
use flpdf::Pdf;
#[cfg(test)]
use flpdf::{ObjectHandle, filters::encode_stream_data_with_flate_level};
#[cfg(test)]
use std::{
    io::{Read, Seek},
    rc::Rc,
};

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

#[cfg(test)]
fn is_safe_lone_flate(dict: &ObjectHandle) -> Result<bool> {
    let filter = dict.try_get_key(b"/Filter")?;
    if !filter.try_is_name_and_equals(b"FlateDecode")? {
        return Ok(false);
    }
    if dict.try_has_key(b"/F")? {
        return Ok(false);
    }
    let type_object = dict.try_get_key(b"/Type")?;
    Ok(!(type_object.try_is_name_and_equals(b"Metadata")?
        || type_object.try_is_name_and_equals(b"ObjStm")?
        || type_object.try_is_name_and_equals(b"XRef")?))
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
        let Ok(decoded) = document.decoded_stream_data(handle, DecodeLevel::Generalized) else {
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
        let object = match handle {
            crate::ObjectHandle::Existing(id) => document.edit_object(id)?,
            crate::ObjectHandle::New(id) => document.edit_added_object(id)?,
        };
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
                let object = match handle {
                    crate::ObjectHandle::Existing(id) => document.edit_object(id)?,
                    crate::ObjectHandle::New(id) => document.edit_added_object(id)?,
                };
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
        let object = match handle {
            crate::ObjectHandle::Existing(id) => document.edit_object(id)?,
            crate::ObjectHandle::New(id) => document.edit_added_object(id)?,
        };
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
pub fn apply_flate_policy<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    policy: FlatePolicy,
    level: i32,
) -> Result<FlateOptimizationStats> {
    let FlatePolicy::Selective {
        min_savings_bytes,
        min_savings_percent,
    } = policy
    else {
        return Ok(FlateOptimizationStats::default());
    };
    let mut stats = FlateOptimizationStats::default();

    for object in pdf.get_all_objects()? {
        let Some(dict) = object.as_stream_dict() else {
            continue;
        };
        if !is_safe_lone_flate(&dict)? {
            continue;
        }
        let raw = object.get_raw_stream_data()?;
        let Ok(decoded) = object.get_stream_data(DecodeLevel::Generalized) else {
            continue;
        };
        let Ok(repacked) = encode_stream_data_with_flate_level(&dict, decoded.as_ref(), level)
        else {
            continue;
        };
        let saving = raw.len().saturating_sub(repacked.len());
        if saving < min_savings_bytes
            || saving.saturating_mul(100)
                < raw.len().saturating_mul(usize::from(min_savings_percent))
        {
            continue;
        }

        // Install exactly the smaller encoded payload we measured while
        // retaining the same lone `/FlateDecode` + `/DecodeParms` dictionary.
        // `encode_stream_data` reapplies PNG/TIFF predictors when present, so
        // these bytes are a true inverse of the decoded data rather than plain
        // zlib bytes mislabeled with the source predictor. Disabling writer
        // filtering makes flpdf emit this verified payload verbatim.
        object.replace_stream_data(Rc::new(repacked), None, None);
        object.set_filter_on_write(false)?;
        pdf.mark_object_handle_dirty(&object)?;
        stats.streams_selected += 1;
        stats.estimated_savings_bytes += saving;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use flate2::{Compression, write::ZlibEncoder};
    use flpdf::{ObjectHandle, ObjectStreamMode, PdfWriter, StreamDataMode};
    use std::{
        io::{Cursor, Write},
        rc::Rc,
    };

    fn compressed(data: &[u8], level: Compression) -> Result<Vec<u8>> {
        let mut encoder = ZlibEncoder::new(Vec::new(), level);
        encoder.write_all(data)?;
        Ok(encoder.finish()?)
    }

    fn linked_flate_stream(
        pdf: &mut Pdf<Cursor<Vec<u8>>>,
        raw: Vec<u8>,
        decode_parms: Option<ObjectHandle>,
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(raw))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new stream has no dictionary".to_owned()))?;
        dict.replace_key(b"/Filter", ObjectHandle::name(b"FlateDecode".to_vec()))?;
        if let Some(value) = decode_parms {
            dict.replace_key(b"/DecodeParms", value)?;
        }
        pdf.mark_object_handle_dirty(&dict)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFlate", stream.clone())?;
        pdf.mark_object_handle_dirty(&root)?;
        Ok(stream)
    }

    #[test]
    fn selective_policy_recompresses_only_a_measured_win() -> Result<()> {
        let source = vec![b'A'; 32 * 1024];
        let original = compressed(&source, Compression::fast())?;
        let mut pdf = Pdf::empty()?;
        let _stream = linked_flate_stream(&mut pdf, original.clone(), None)?;

        let stats = apply_flate_policy(
            &mut pdf,
            FlatePolicy::Selective {
                min_savings_bytes: 1,
                min_savings_percent: 1,
            },
            9,
        )?;
        assert_eq!(stats.streams_selected, 1);
        assert!(stats.estimated_savings_bytes > 0);

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.set_stream_data_mode(StreamDataMode::Compress);
        writer.set_recompress_flate(false);
        writer.set_compression_level(9);
        writer.set_content_normalization(false);
        writer.set_object_stream_mode(ObjectStreamMode::Preserve);
        writer.write()?;
        let output = writer.get_buffer()?;

        let mut reopened = Pdf::open(Cursor::new(output))?;
        let root = reopened.root_handle()?;
        let selected = root.try_get_key(b"/TestFlate")?;
        reopened.resolve(&selected)?;
        let rewritten_raw = selected.get_raw_stream_data()?;
        let decoded = selected.get_stream_data(DecodeLevel::Generalized)?;
        assert!(rewritten_raw.len() < original.len());
        assert_eq!(decoded.as_ref(), &source);
        Ok(())
    }

    #[test]
    fn selective_policy_recompresses_predictor_streams_without_changing_decoded_bytes() -> Result<()>
    {
        let columns = 256_u16;
        let mut source = Vec::with_capacity(usize::from(columns) * 128);
        for row in 0..128_u8 {
            for column in 0..columns {
                source.push(row.wrapping_add((column % 17) as u8));
            }
        }
        let parms = ObjectHandle::dictionary(vec![
            (b"/Predictor".to_vec(), ObjectHandle::integer(12)),
            (
                b"/Columns".to_vec(),
                ObjectHandle::integer(i64::from(columns)),
            ),
            (b"/Colors".to_vec(), ObjectHandle::integer(1)),
            (b"/BitsPerComponent".to_vec(), ObjectHandle::integer(8)),
        ]);
        let encoding_dict = ObjectHandle::dictionary(vec![
            (
                b"/Filter".to_vec(),
                ObjectHandle::name(b"FlateDecode".to_vec()),
            ),
            (b"/DecodeParms".to_vec(), parms.clone()),
        ]);
        let original = encode_stream_data_with_flate_level(&encoding_dict, &source, 1)?;

        let mut pdf = Pdf::empty()?;
        let _stream = linked_flate_stream(&mut pdf, original.clone(), Some(parms))?;
        let stats = apply_flate_policy(
            &mut pdf,
            FlatePolicy::Selective {
                min_savings_bytes: 1,
                min_savings_percent: 1,
            },
            9,
        )?;
        assert_eq!(stats.streams_selected, 1);

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.set_stream_data_mode(StreamDataMode::Compress);
        writer.set_recompress_flate(false);
        writer.set_compression_level(9);
        writer.set_content_normalization(false);
        writer.set_object_stream_mode(ObjectStreamMode::Preserve);
        writer.write()?;
        let output = writer.get_buffer()?;

        let mut reopened = Pdf::open(Cursor::new(output))?;
        let root = reopened.root_handle()?;
        let selected = root.try_get_key(b"/TestFlate")?;
        reopened.resolve(&selected)?;
        let rewritten_raw = selected.get_raw_stream_data()?;
        let decoded = selected.get_stream_data(DecodeLevel::Generalized)?;
        assert!(rewritten_raw.len() < original.len());
        assert_eq!(decoded.as_ref(), source.as_slice());
        Ok(())
    }
    #[test]
    fn adaptive_flate_skips_small_stream() -> Result<()> {
        let dictionary = ObjectHandle::dictionary(vec![(
            b"/Filter".to_vec(),
            ObjectHandle::name(b"FlateDecode".to_vec()),
        )]);
        let source = vec![b'A'; ADAPTIVE_FLATE_MIN_DECODED_BYTES / 2];
        let encoded = adaptive_flate_encode(&dictionary, &source, 6, true)?;
        assert!(!encoded.high_effort_tested);
        assert!(!encoded.high_effort_selected);
        Ok(())
    }

    #[test]
    fn adaptive_flate_keeps_best_large_highly_compressible_encoding() -> Result<()> {
        let dictionary = ObjectHandle::dictionary(vec![(
            b"/Filter".to_vec(),
            ObjectHandle::name(b"FlateDecode".to_vec()),
        )]);
        let source = vec![b'A'; ADAPTIVE_FLATE_MIN_DECODED_BYTES * 2];
        let baseline = encode_stream_data_with_flate_level(&dictionary, &source, 6)?;
        assert!(
            baseline.len().saturating_mul(100)
                <= source
                    .len()
                    .saturating_mul(ADAPTIVE_FLATE_MAX_BASELINE_RATIO_PERCENT)
        );
        let high = encode_stream_data_with_flate_level(&dictionary, &source, 9)?;
        let encoded = adaptive_flate_encode(&dictionary, &source, 6, true)?;
        assert!(encoded.high_effort_tested);
        assert_eq!(encoded.bytes.len(), baseline.len().min(high.len()));
        assert_eq!(encoded.high_effort_selected, high.len() < baseline.len());
        assert_eq!(
            encoded.high_effort_extra_savings_bytes,
            baseline.len().saturating_sub(high.len())
        );
        Ok(())
    }
}
