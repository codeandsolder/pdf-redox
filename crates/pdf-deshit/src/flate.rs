use crate::{FlatePolicy, Result};
use flpdf::{DecodeLevel, ObjectHandle, Pdf, filters::encode_stream_data_with_flate_level};
use std::{
    io::{Read, Seek},
    rc::Rc,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FlateOptimizationStats {
    pub streams_selected: usize,
    pub estimated_savings_bytes: usize,
}

fn is_safe_lone_flate(dict: &ObjectHandle) -> Result<bool> {
    let filter = dict.try_get_key(b"/Filter")?;
    if !filter.try_is_name_and_equals(b"FlateDecode")? {
        return Ok(false);
    }
    if dict.try_has_key(b"/F")? {
        return Ok(false);
    }
    let type_object = dict.try_get_key(b"/Type")?;
    if type_object.try_is_name_and_equals(b"Metadata")?
        || type_object.try_is_name_and_equals(b"ObjStm")?
        || type_object.try_is_name_and_equals(b"XRef")?
    {
        return Ok(false);
    }
    Ok(true)
}

pub(crate) fn apply_flate_policy<R: Read + Seek + 'static>(
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
        let columns = 256_i64;
        let mut source = Vec::with_capacity(columns as usize * 128);
        for row in 0..128_u8 {
            for column in 0..columns as u16 {
                source.push(row.wrapping_add((column % 17) as u8));
            }
        }
        let parms = ObjectHandle::dictionary(vec![
            (b"/Predictor".to_vec(), ObjectHandle::integer(12)),
            (b"/Columns".to_vec(), ObjectHandle::integer(columns)),
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
}
