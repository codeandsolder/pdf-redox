use crate::{
    EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    StreamData, source::CurrentObject,
};
#[cfg(test)]
use flpdf::{DecodeLevel, ObjectRef, Pdf};
use flpdf::{
    ObjectHandle as FlObjectHandle,
    filters::{decode_stream_data, encode_stream_data_with_flate_level},
};
use hayro_syntax::object::{Dict as HayroDict, Name as HayroName, Object as HayroObject};
use std::collections::HashMap;
#[cfg(test)]
use std::{
    collections::HashSet,
    io::{Read, Seek},
    rc::Rc,
};

const RENDERING_UNUSED_TABLES: [[u8; 4]; 13] = [
    *b"BASE", *b"GDEF", *b"GPOS", *b"GSUB", *b"JSTF", *b"MATH", *b"kern", *b"vhea", *b"vmtx",
    *b"DSIG", *b"name", *b"OS/2", *b"PCLT",
];

const CIDFONT_TYPE2_UNUSED_TABLES: [[u8; 4]; 2] = [*b"cmap", *b"post"];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FontProgramUsage {
    simple_truetype: bool,
    cidfont_type2: bool,
}

impl FontProgramUsage {
    fn cidfont_type2_only(self) -> bool {
        self.cidfont_type2 && !self.simple_truetype
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FontOptimizationStats {
    pub programs_optimized: usize,
    pub original_encoded_bytes: usize,
    pub optimized_encoded_bytes: usize,
    pub decoded_table_bytes_removed: usize,
}

fn be16(bytes: &[u8], offset: usize) -> Option<u16> {
    Some(u16::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
    ]))
}

fn be32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_be_bytes([
        *bytes.get(offset)?,
        *bytes.get(offset + 1)?,
        *bytes.get(offset + 2)?,
        *bytes.get(offset + 3)?,
    ]))
}

fn checksum32(data: &[u8]) -> u32 {
    data.chunks(4).fold(0_u32, |sum, chunk| {
        let mut word = [0_u8; 4];
        word[..chunk.len()].copy_from_slice(chunk);
        sum.wrapping_add(u32::from_be_bytes(word))
    })
}

fn is_sfnt_magic(magic: &[u8]) -> bool {
    matches!(magic, [0, 1, 0, 0] | b"OTTO" | b"true" | b"typ1")
}

/// Rebuild an sfnt while removing tables that PDF consumers do not use to
/// render already-positioned text. Returns `(rebuilt, removed_decoded_bytes)`.
///
/// PDF's embedded-TrueType rules require the outline/metric/hinting core and,
/// for simple fonts, `cmap`. Advanced line-layout tables are not required for
/// display. PDF also defines vertical metrics through `CIDFont` `/DW2`/`/W2`,
/// making sfnt `vhea`/`vmtx` irrelevant to PDF rendering.
fn sfnt_for_pdf_rendering(bytes: &[u8], usage: FontProgramUsage) -> Option<(Vec<u8>, usize)> {
    if bytes.len() < 12 || !is_sfnt_magic(&bytes[..4]) {
        return None;
    }
    let table_count = usize::from(be16(bytes, 4)?);
    let directory_bytes = table_count.checked_mul(16)?.checked_add(12)?;
    if directory_bytes > bytes.len() || table_count == 0 {
        return None;
    }

    let mut tables: Vec<([u8; 4], Vec<u8>)> = Vec::with_capacity(table_count);
    let mut removed_bytes = 0_usize;
    for index in 0..table_count {
        let record = 12 + index * 16;
        let tag: [u8; 4] = bytes[record..record + 4].try_into().ok()?;
        let offset = usize::try_from(be32(bytes, record + 8)?).ok()?;
        let length = usize::try_from(be32(bytes, record + 12)?).ok()?;
        let end = offset.checked_add(length)?;
        if end > bytes.len() {
            return None;
        }
        if RENDERING_UNUSED_TABLES.contains(&tag)
            || (usage.cidfont_type2_only() && CIDFONT_TYPE2_UNUSED_TABLES.contains(&tag))
        {
            removed_bytes = removed_bytes.saturating_add(length);
            continue;
        }
        let mut data = bytes[offset..end].to_vec();
        if tag == *b"head" {
            if data.len() < 12 {
                return None;
            }
            data[8..12].fill(0);
        }
        tables.push((tag, data));
    }
    if removed_bytes == 0 || tables.is_empty() {
        return None;
    }

    tables.sort_unstable_by_key(|(tag, _)| *tag);
    let new_count = tables.len();
    let new_count_u16 = u16::try_from(new_count).ok()?;
    let max_power = 1_usize << (usize::BITS - 1 - new_count.leading_zeros());
    let search_range = u16::try_from(max_power.checked_mul(16)?).ok()?;
    let entry_selector = u16::try_from(max_power.trailing_zeros()).ok()?;
    let range_shift = u16::try_from(new_count.checked_mul(16)?)
        .ok()?
        .checked_sub(search_range)?;

    let mut output = Vec::with_capacity(bytes.len().saturating_sub(removed_bytes));
    output.extend_from_slice(&bytes[..4]);
    output.extend_from_slice(&new_count_u16.to_be_bytes());
    output.extend_from_slice(&search_range.to_be_bytes());
    output.extend_from_slice(&entry_selector.to_be_bytes());
    output.extend_from_slice(&range_shift.to_be_bytes());
    let directory_offset = output.len();
    output.resize(directory_offset + new_count * 16, 0);

    let mut head_offset = None;
    for (index, (tag, data)) in tables.iter().enumerate() {
        while output.len() % 4 != 0 {
            output.push(0);
        }
        let offset = output.len();
        output.extend_from_slice(data);
        while output.len() % 4 != 0 {
            output.push(0);
        }

        let record = directory_offset + index * 16;
        output[record..record + 4].copy_from_slice(tag);
        output[record + 4..record + 8].copy_from_slice(&checksum32(data).to_be_bytes());
        output[record + 8..record + 12].copy_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
        output[record + 12..record + 16]
            .copy_from_slice(&u32::try_from(data.len()).ok()?.to_be_bytes());
        if tag == b"head" {
            head_offset = Some(offset);
        }
    }

    if let Some(offset) = head_offset {
        let adjustment = 0xB1B0_AFBA_u32.wrapping_sub(checksum32(&output));
        output[offset + 8..offset + 12].copy_from_slice(&adjustment.to_be_bytes());
    }
    Some((output, removed_bytes))
}

#[cfg(test)]
fn is_lone_flate(stream_dict: &flpdf::ObjectHandle) -> Result<bool> {
    let filter = stream_dict.try_get_key(b"/Filter")?;
    Ok(filter.try_is_name_and_equals(b"FlateDecode")? && !stream_dict.try_has_key(b"/F")?)
}

#[cfg(test)]
pub(crate) fn strip_font_editing_tables<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let objects = pdf.get_all_objects()?;

    // Record how each FontDescriptor is used. `cmap` and `post` are needed by
    // simple TrueType fonts, but CIDFontType2 selects glyphs through PDF's
    // CID-to-GID machinery and does not need those sfnt tables for rendering.
    let mut descriptor_usage = HashMap::<ObjectRef, FontProgramUsage>::new();
    for object in &objects {
        if !object.try_is_dictionary()? {
            continue;
        }
        let subtype = object
            .try_get_key(b"/Subtype")?
            .as_name()
            .unwrap_or_default();
        let descriptor = object.try_get_key(b"/FontDescriptor")?;
        let Some(descriptor_ref) = descriptor.object_ref() else {
            continue;
        };
        let usage = descriptor_usage.entry(descriptor_ref).or_default();
        if subtype == b"TrueType" {
            usage.simple_truetype = true;
        } else if subtype == b"CIDFontType2" {
            usage.cidfont_type2 = true;
        }
    }

    // A program can be shared by multiple FontDescriptors. Merge usage before
    // deciding which tables can be discarded so a simple-font reference keeps
    // `cmap`/`post` even if another descriptor uses the same program as CIDFontType2.
    let mut program_usage = HashMap::<ObjectRef, FontProgramUsage>::new();
    for object in &objects {
        let Some(descriptor_ref) = object.object_ref() else {
            continue;
        };
        let Some(usage) = descriptor_usage.get(&descriptor_ref).copied() else {
            continue;
        };
        if !object.try_is_dictionary()? {
            continue;
        }
        let keys = object.try_get_keys()?;
        for key in [b"/FontFile2".as_slice(), b"/FontFile3".as_slice()] {
            if !keys.contains(key) {
                continue;
            }
            let program = object.try_get_key(key)?;
            let Some(program_ref) = program.object_ref() else {
                continue;
            };
            let merged = program_usage.entry(program_ref).or_default();
            merged.simple_truetype |= usage.simple_truetype;
            merged.cidfont_type2 |= usage.cidfont_type2;
        }
    }

    let mut stats = FontOptimizationStats::default();
    let mut seen = HashSet::<ObjectRef>::new();
    for object in objects {
        if !object.try_is_dictionary()? {
            continue;
        }
        let keys = object.try_get_keys()?;
        for key in [b"/FontFile2".as_slice(), b"/FontFile3".as_slice()] {
            if !keys.contains(key) {
                continue;
            }
            let program = object.try_get_key(key)?;
            let Some(program_ref) = program.object_ref() else {
                continue;
            };
            if !seen.insert(program_ref) {
                continue;
            }
            let Some(stream_dict) = program.as_stream_dict() else {
                continue;
            };
            if !is_lone_flate(&stream_dict)? {
                continue;
            }
            let decoded = match program.get_stream_data(DecodeLevel::Generalized) {
                Ok(data) => data,
                Err(_) => continue,
            };
            let usage = program_usage.get(&program_ref).copied().unwrap_or_default();
            let Some((trimmed, removed_decoded_bytes)) =
                sfnt_for_pdf_rendering(decoded.as_ref(), usage)
            else {
                continue;
            };
            let encoded =
                match encode_stream_data_with_flate_level(&stream_dict, &trimmed, flate_level) {
                    Ok(data) => data,
                    Err(_) => continue,
                };
            let original = program.get_raw_stream_data()?;
            if encoded.len() >= original.len() {
                continue;
            }

            stats.programs_optimized += 1;
            stats.original_encoded_bytes += original.len();
            stats.optimized_encoded_bytes += encoded.len();
            stats.decoded_table_bytes_removed += removed_decoded_bytes;
            program.replace_stream_data(Rc::new(encoded), None, None);
            program.set_filter_on_write(false)?;
            pdf.mark_object_handle_dirty(&program)?;
        }
    }
    Ok(stats)
}

const HAYRO_FONT_FILE_KEYS: [&[u8]; 2] = [b"FontFile2", b"FontFile3"];

fn owned_name_value(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<Vec<u8>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn direct_owned_reference(value: Option<&OwnedObject>) -> Option<CowObjectHandle> {
    match value {
        Some(OwnedObject::Reference(handle)) => Some(*handle),
        _ => None,
    }
}

fn inspect_hayro_font_dictionary(
    holder: CowObjectHandle,
    dictionary: &HayroDict<'_>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) {
    if let Some(descriptor) = dictionary.get_ref(b"FontDescriptor") {
        let subtype = dictionary.get::<HayroName<'_>>(b"Subtype");
        let usage = match subtype.as_ref().map(AsRef::<[u8]>::as_ref) {
            Some(b"TrueType") => FontProgramUsage {
                simple_truetype: true,
                cidfont_type2: false,
            },
            Some(b"CIDFontType2") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: true,
            },
            _ => FontProgramUsage::default(),
        };
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((CowObjectHandle::Existing(descriptor.into()), usage));
        }
    }

    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = dictionary.get_ref(key) {
            descriptor_program_edges.push((holder, CowObjectHandle::Existing(program.into())));
        }
    }
}

fn inspect_owned_font_dictionary(
    document: &EditDocument,
    holder: CowObjectHandle,
    dictionary: &OwnedDictionary,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) -> Result<()> {
    if let Some(descriptor) = direct_owned_reference(dictionary.get(b"FontDescriptor".as_slice())) {
        let usage =
            match owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref() {
                Some(b"TrueType") => FontProgramUsage {
                    simple_truetype: true,
                    cidfont_type2: false,
                },
                Some(b"CIDFontType2") => FontProgramUsage {
                    simple_truetype: false,
                    cidfont_type2: true,
                },
                _ => FontProgramUsage::default(),
            };
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((descriptor, usage));
        }
    }

    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = direct_owned_reference(dictionary.get(key)) {
            descriptor_program_edges.push((holder, program));
        }
    }
    Ok(())
}

fn owned_to_flpdf_resolved(
    document: &EditDocument,
    value: &OwnedObject,
    depth: usize,
) -> Result<FlObjectHandle> {
    if depth > 64 {
        return Err(Error::Invalid(
            "font stream filter object nesting exceeds supported depth".to_owned(),
        ));
    }
    match value {
        OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(FlObjectHandle::null());
            };
            owned_to_flpdf_resolved(document, &value, depth + 1)
        }
        OwnedObject::Null => Ok(FlObjectHandle::null()),
        OwnedObject::Boolean(value) => Ok(FlObjectHandle::boolean(*value)),
        OwnedObject::Integer(value) => Ok(FlObjectHandle::integer(*value)),
        OwnedObject::Real(value) => Ok(FlObjectHandle::real(*value)),
        OwnedObject::Name(value) => Ok(FlObjectHandle::name(value.clone())),
        OwnedObject::String(value) => Ok(FlObjectHandle::string(value.clone())),
        OwnedObject::Array(values) => Ok(FlObjectHandle::array(
            values
                .iter()
                .map(|value| owned_to_flpdf_resolved(document, value, depth + 1))
                .collect::<Result<Vec<_>>>()?,
        )),
        OwnedObject::Dictionary(dictionary) => Ok(FlObjectHandle::dictionary(
            dictionary
                .iter()
                .map(|(key, value)| {
                    Ok((
                        [b"/".as_slice(), key.as_slice()].concat(),
                        owned_to_flpdf_resolved(document, value, depth + 1)?,
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        )),
        OwnedObject::Stream { .. } => Err(Error::Invalid(
            "stream object cannot be used as font filter parameter".to_owned(),
        )),
    }
}

fn flpdf_filter_dictionary(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<FlObjectHandle>> {
    let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
        return Ok(None);
    };
    let Some(OwnedObject::Name(filter_name)) = document.resolve_owned_value(filter)? else {
        return Ok(None);
    };
    if filter_name != b"FlateDecode" || dictionary.contains_key(b"F".as_slice()) {
        return Ok(None);
    }

    let mut entries = vec![(
        b"/Filter".to_vec(),
        FlObjectHandle::name(b"FlateDecode".to_vec()),
    )];
    if let Some(params) = dictionary.get(b"DecodeParms".as_slice()) {
        entries.push((
            b"/DecodeParms".to_vec(),
            owned_to_flpdf_resolved(document, params, 0)?,
        ));
    }
    Ok(Some(FlObjectHandle::dictionary(entries)))
}

fn replace_current_stream_data(
    document: &mut EditDocument,
    handle: CowObjectHandle,
    encoded: Vec<u8>,
) -> Result<()> {
    let object = match handle {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    match object {
        OwnedObject::Stream { data, .. } => {
            *data = StreamData::Owned(encoded);
            Ok(())
        }
        _ => Err(Error::Invalid(
            "font program reference does not resolve to a stream".to_owned(),
        )),
    }
}

/// Hayro/COW port of [`strip_font_editing_tables`].
///
/// The graph walk and mutation are Hayro-native. flpdf is used only for the
/// already-tested stream filter codec semantics so `/DecodeParms` behavior
/// remains identical during the migration.
pub(crate) fn strip_font_editing_tables_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let mut descriptor_usage_edges = Vec::new();
    let mut descriptor_program_edges = Vec::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_hayro_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_hayro_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                _ => {}
            }
            Ok(())
        }
        CurrentObject::Owned(object) => {
            if let Some(dictionary) = object.as_dictionary() {
                inspect_owned_font_dictionary(
                    document,
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                )?;
            }
            Ok(())
        }
    })?;

    let mut descriptor_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, usage) in descriptor_usage_edges {
        let merged = descriptor_usage.entry(descriptor).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
    }

    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, program) in descriptor_program_edges {
        let Some(usage) = descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        let merged = program_usage.entry(program).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
    }

    let mut stats = FontOptimizationStats::default();
    for (program, usage) in program_usage {
        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        let Some(filter_dictionary) = flpdf_filter_dictionary(document, &dictionary)? else {
            continue;
        };
        let raw = data.bytes(document.source())?;
        let decoded = match decode_stream_data(&filter_dictionary, raw.as_ref()) {
            Ok(decoded) => decoded,
            Err(_) => continue,
        };
        let Some((trimmed, removed_decoded_bytes)) = sfnt_for_pdf_rendering(&decoded, usage) else {
            continue;
        };
        let encoded =
            match encode_stream_data_with_flate_level(&filter_dictionary, &trimmed, flate_level) {
                Ok(encoded) => encoded,
                Err(_) => continue,
            };
        if encoded.len() >= raw.len() {
            continue;
        }

        stats.programs_optimized += 1;
        stats.original_encoded_bytes += raw.len();
        stats.optimized_encoded_bytes += encoded.len();
        stats.decoded_table_bytes_removed += removed_decoded_bytes;
        replace_current_stream_data(document, program, encoded)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SourcePdf;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::{Cursor, Write};

    fn append_pdf_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(body);
    }

    fn hayro_font_fixture() -> Result<Vec<u8>> {
        let head = [0_u8; 54];
        let gsub = vec![0x55_u8; 8192];
        let source_font = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyph-data"),
            (*b"hmtx", b"metric-data"),
            (*b"cmap", b"mapping-data"),
            (*b"post", b"post-data"),
            (*b"GSUB", gsub.as_slice()),
        ]);
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(&source_font)?;
        let encoded = encoder.finish()?;

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Type /Font /Subtype /TrueType /BaseFont /TestFont /FontDescriptor 6 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /FontDescriptor /FontName /TestFont /FontFile2 7 0 R >>\nendobj\n",
        );
        let header = format!(
            "7 0 obj\n<< /Length {} /Filter /FlateDecode /DecodeParms << /Predictor 1 >> >>\nstream\n",
            encoded.len()
        );
        let mut stream_object = header.into_bytes();
        stream_object.extend_from_slice(&encoded);
        stream_object.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &stream_object);

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 8 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        Ok(pdf)
    }

    #[test]
    fn hayro_font_table_strip_matches_flpdf_accounting() -> Result<()> {
        let input = hayro_font_fixture()?;
        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = strip_font_editing_tables(&mut flpdf, 9)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = strip_font_editing_tables_hayro(&mut document, 9)?;
        assert_eq!(actual, expected);
        assert_eq!(actual.programs_optimized, 1);
        assert!(actual.optimized_encoded_bytes < actual.original_encoded_bytes);
        assert_eq!(actual.decoded_table_bytes_removed, 8192);

        let output = document.write_compact()?;
        let reparsed = SourcePdf::from_bytes(output)?;
        assert_eq!(reparsed.page_count(), 1);
        Ok(())
    }

    fn sfnt(tables: &[([u8; 4], &[u8])]) -> Vec<u8> {
        let mut owned: Vec<([u8; 4], Vec<u8>)> = tables
            .iter()
            .map(|(tag, data)| (*tag, data.to_vec()))
            .collect();
        owned.sort_unstable_by_key(|(tag, _)| *tag);
        let count = owned.len();
        let max_power = 1_usize << (usize::BITS - 1 - count.leading_zeros());
        let search_range = (max_power * 16) as u16;
        let mut out = Vec::new();
        out.extend_from_slice(&[0, 1, 0, 0]);
        out.extend_from_slice(&(count as u16).to_be_bytes());
        out.extend_from_slice(&search_range.to_be_bytes());
        out.extend_from_slice(&(max_power.trailing_zeros() as u16).to_be_bytes());
        out.extend_from_slice(&((count * 16) as u16 - search_range).to_be_bytes());
        let directory = out.len();
        out.resize(directory + count * 16, 0);
        for (index, (tag, data)) in owned.iter().enumerate() {
            while out.len() % 4 != 0 {
                out.push(0);
            }
            let offset = out.len();
            out.extend_from_slice(data);
            while out.len() % 4 != 0 {
                out.push(0);
            }
            let record = directory + index * 16;
            out[record..record + 4].copy_from_slice(tag);
            out[record + 4..record + 8].copy_from_slice(&checksum32(data).to_be_bytes());
            out[record + 8..record + 12].copy_from_slice(&(offset as u32).to_be_bytes());
            out[record + 12..record + 16].copy_from_slice(&(data.len() as u32).to_be_bytes());
        }
        out
    }

    fn tags(bytes: &[u8]) -> Vec<[u8; 4]> {
        let Some(count) = be16(bytes, 4).map(usize::from) else {
            return Vec::new();
        };
        (0..count)
            .filter_map(|index| bytes.get(12 + index * 16..16 + index * 16)?.try_into().ok())
            .collect()
    }

    #[test]
    fn rendering_sfnt_drops_layout_and_vertical_metric_tables() {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"hmtx", b"metrics"),
            (*b"GSUB", b"substitution"),
            (*b"GPOS", b"positioning"),
            (*b"vhea", b"vertical header"),
            (*b"vmtx", b"vertical metrics"),
        ]);
        let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&source, FontProgramUsage::default())
        else {
            panic!("expected removable rendering-unused tables");
        };
        let remaining = tags(&trimmed);
        assert!(remaining.contains(b"head"));
        assert!(remaining.contains(b"glyf"));
        assert!(remaining.contains(b"hmtx"));
        assert!(!remaining.contains(b"GSUB"));
        assert!(!remaining.contains(b"GPOS"));
        assert!(!remaining.contains(b"vhea"));
        assert!(!remaining.contains(b"vmtx"));
        assert_eq!(removed, 12 + 11 + 15 + 16);
    }

    #[test]
    fn cidfont_type2_drops_cmap_and_post() {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"cmap", b"mapping"),
            (*b"post", b"names"),
            (*b"name", b"editing metadata"),
        ]);
        let cid_usage = FontProgramUsage {
            simple_truetype: false,
            cidfont_type2: true,
        };
        let Some((trimmed, _)) = sfnt_for_pdf_rendering(&source, cid_usage) else {
            panic!("expected CID-only table removal");
        };
        let remaining = tags(&trimmed);
        assert!(!remaining.contains(b"cmap"));
        assert!(!remaining.contains(b"post"));

        let simple_usage = FontProgramUsage {
            simple_truetype: true,
            cidfont_type2: false,
        };
        let Some((simple, _)) = sfnt_for_pdf_rendering(&source, simple_usage) else {
            panic!("expected metadata table removal");
        };
        let simple_remaining = tags(&simple);
        assert!(simple_remaining.contains(b"cmap"));
        assert!(simple_remaining.contains(b"post"));
    }

    #[test]
    fn rendering_sfnt_is_noop_without_removable_tables() {
        let head = [0_u8; 54];
        let source = sfnt(&[(*b"head", &head), (*b"glyf", b"glyphs")]);
        assert!(sfnt_for_pdf_rendering(&source, FontProgramUsage::default()).is_none());
    }
}
