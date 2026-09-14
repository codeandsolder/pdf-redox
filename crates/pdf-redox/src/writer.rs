//! Compact generic writer for the Hayro/COW migration.
//!
//! This is intentionally not wired into the production optimizer yet. Hayro
//! does not currently expose all trailer roots we need to preserve (`/Info`,
//! `/ID`, and arbitrary trailer entries). The writer is exercised independently
//! so the COS serialization and output planning can mature without weakening
//! preservation guarantees in the existing path.

use crate::{EditDocument, Error, ExistingObjectChange, ObjectHandle, OwnedObject, Result};
use hayro_syntax::{
    PdfVersion,
    object::{Dict, MaybeRef, Object, Stream},
};
use std::{collections::BTreeMap, io::Write as _};

const MAX_CLASSIC_XREF_OFFSET: u64 = 9_999_999_999;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct OutputObjectId(u32);

#[derive(Debug)]
struct OutputPlan {
    ids: BTreeMap<ObjectHandle, OutputObjectId>,
    order: Vec<ObjectHandle>,
    catalog: OutputObjectId,
}

impl OutputPlan {
    fn new(document: &EditDocument) -> Result<Self> {
        let reachable = document.reachable_objects()?;
        if reachable.len() > i32::MAX as usize {
            return Err(Error::TooManyOutputObjects {
                count: reachable.len(),
            });
        }

        let catalog_handle = ObjectHandle::Existing(document.source().catalog_id());
        let mut order = Vec::with_capacity(reachable.len());
        order.push(catalog_handle);
        order.extend(
            reachable
                .into_iter()
                .filter(|handle| *handle != catalog_handle),
        );

        let mut ids = BTreeMap::new();
        for (index, handle) in order.iter().copied().enumerate() {
            ids.insert(handle, OutputObjectId(index as u32 + 1));
        }
        let catalog = ids[&catalog_handle];

        Ok(Self {
            ids,
            order,
            catalog,
        })
    }

    fn id(&self, handle: ObjectHandle) -> Result<OutputObjectId> {
        self.ids
            .get(&handle)
            .copied()
            .ok_or_else(|| missing_mapping_error(handle))
    }
}

pub(crate) fn write_pdf(document: &EditDocument) -> Result<Vec<u8>> {
    let plan = OutputPlan::new(document)?;
    let mut output = Vec::with_capacity(document.source().bytes().len());
    output.extend_from_slice(b"%PDF-");
    output.extend_from_slice(version_bytes(document.source().version()));
    output.extend_from_slice(b"\n%\xE2\xE3\xCF\xD3\n");

    let mut offsets = vec![0usize; plan.order.len() + 1];
    for handle in plan.order.iter().copied() {
        let id = plan.id(handle)?;
        let offset = output.len();
        check_xref_offset(offset)?;
        offsets[id.0 as usize] = offset;
        writeln!(&mut output, "{} 0 obj", id.0)?;
        write_handle_object(&mut output, document, &plan, handle)?;
        output.extend_from_slice(b"\nendobj\n");
    }

    let xref_offset = output.len();
    check_xref_offset(xref_offset)?;
    writeln!(&mut output, "xref")?;
    writeln!(&mut output, "0 {}", offsets.len())?;
    output.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.into_iter().skip(1) {
        check_xref_offset(offset)?;
        writeln!(&mut output, "{offset:010} 00000 n ")?;
    }

    output.extend_from_slice(b"trailer\n<< /Size ");
    write!(&mut output, "{}", plan.order.len() + 1)?;
    output.extend_from_slice(b" /Root ");
    write!(&mut output, "{} 0 R", plan.catalog.0)?;
    output.extend_from_slice(b" >>\nstartxref\n");
    writeln!(&mut output, "{xref_offset}")?;
    output.extend_from_slice(b"%%EOF\n");
    Ok(output)
}

fn write_handle_object(
    output: &mut Vec<u8>,
    document: &EditDocument,
    plan: &OutputPlan,
    handle: ObjectHandle,
) -> Result<()> {
    match handle {
        ObjectHandle::Existing(id) => match document.overlay().change(id) {
            Some(ExistingObjectChange::Replace(object)) => {
                write_owned_object(output, object, document, plan)
            }
            Some(ExistingObjectChange::Delete) => Err(Error::DeletedReferencedObject {
                number: id.number(),
                generation: id.generation(),
            }),
            None => {
                let object = document.source().object(id)?;
                write_hayro_object(output, &object, document, plan)
            }
        },
        ObjectHandle::New(id) => {
            let object = document
                .overlay()
                .added(id)
                .ok_or(Error::MissingNewObject { index: id.index() })?;
            write_owned_object(output, object, document, plan)
        }
    }
}

fn write_hayro_object(
    output: &mut Vec<u8>,
    object: &Object<'_>,
    document: &EditDocument,
    plan: &OutputPlan,
) -> Result<()> {
    match object {
        Object::Null(_) => output.extend_from_slice(b"null"),
        Object::Boolean(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Object::Number(value) => write_hayro_number(output, value)?,
        Object::String(value) => write_pdf_string(output, value.as_bytes()),
        Object::Name(value) => write_pdf_name(output, value.as_ref()),
        Object::Dict(dictionary) => {
            write_hayro_dictionary(output, dictionary, document, plan, false)?
        }
        Object::Array(array) => {
            output.push(b'[');
            for (index, value) in array.raw_iter().enumerate() {
                if index != 0 {
                    output.push(b' ');
                }
                write_hayro_maybe_ref(output, value, document, plan)?;
            }
            output.push(b']');
        }
        Object::Stream(stream) => write_hayro_stream(output, stream, document, plan)?,
    }
    Ok(())
}

fn write_hayro_maybe_ref(
    output: &mut Vec<u8>,
    value: MaybeRef<Object<'_>>,
    document: &EditDocument,
    plan: &OutputPlan,
) -> Result<()> {
    match value {
        MaybeRef::Ref(reference) => write_reference(
            output,
            document,
            plan,
            ObjectHandle::Existing(reference.into()),
        ),
        MaybeRef::NotRef(object) => write_hayro_object(output, &object, document, plan),
    }
}

fn write_hayro_dictionary(
    output: &mut Vec<u8>,
    dictionary: &Dict<'_>,
    document: &EditDocument,
    plan: &OutputPlan,
    skip_length: bool,
) -> Result<()> {
    output.extend_from_slice(b"<<");
    for (name, value) in dictionary.entries() {
        if skip_length && name.as_ref() == b"Length" {
            continue;
        }
        output.push(b' ');
        write_pdf_name(output, name.as_ref());
        output.push(b' ');
        write_hayro_maybe_ref(output, value, document, plan)?;
    }
    output.extend_from_slice(b" >>");
    Ok(())
}

fn write_hayro_stream(
    output: &mut Vec<u8>,
    stream: &Stream<'_>,
    document: &EditDocument,
    plan: &OutputPlan,
) -> Result<()> {
    let data = stream.raw_data();
    output.extend_from_slice(b"<< /Length ");
    write!(&mut *output, "{}", data.len())?;
    for (name, value) in stream.dict().entries() {
        if name.as_ref() == b"Length" {
            continue;
        }
        output.push(b' ');
        write_pdf_name(output, name.as_ref());
        output.push(b' ');
        write_hayro_maybe_ref(output, value, document, plan)?;
    }
    output.extend_from_slice(b" >>\nstream\n");
    output.extend_from_slice(data.as_ref());
    output.extend_from_slice(b"\nendstream");
    Ok(())
}

fn write_owned_object(
    output: &mut Vec<u8>,
    object: &OwnedObject,
    document: &EditDocument,
    plan: &OutputPlan,
) -> Result<()> {
    match object {
        OwnedObject::Null => output.extend_from_slice(b"null"),
        OwnedObject::Boolean(value) => {
            output.extend_from_slice(if *value { b"true" } else { b"false" })
        }
        OwnedObject::Integer(value) => write!(&mut *output, "{value}")?,
        OwnedObject::Real(value) => write_pdf_real(output, *value)?,
        OwnedObject::Name(value) => write_pdf_name(output, value),
        OwnedObject::String(value) => write_pdf_string(output, value),
        OwnedObject::Reference(reference) => write_reference(output, document, plan, *reference)?,
        OwnedObject::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b' ');
                }
                write_owned_object(output, value, document, plan)?;
            }
            output.push(b']');
        }
        OwnedObject::Dictionary(dictionary) => {
            output.extend_from_slice(b"<<");
            for (name, value) in dictionary {
                output.push(b' ');
                write_pdf_name(output, name);
                output.push(b' ');
                write_owned_object(output, value, document, plan)?;
            }
            output.extend_from_slice(b" >>");
        }
        OwnedObject::Stream { dictionary, data } => {
            let data = data.bytes(document.source())?;
            output.extend_from_slice(b"<< /Length ");
            write!(&mut *output, "{}", data.len())?;
            for (name, value) in dictionary {
                if name.as_slice() == b"Length" {
                    continue;
                }
                output.push(b' ');
                write_pdf_name(output, name);
                output.push(b' ');
                write_owned_object(output, value, document, plan)?;
            }
            output.extend_from_slice(b" >>\nstream\n");
            output.extend_from_slice(data.as_ref());
            output.extend_from_slice(b"\nendstream");
        }
    }
    Ok(())
}

fn write_reference(
    output: &mut Vec<u8>,
    document: &EditDocument,
    plan: &OutputPlan,
    handle: ObjectHandle,
) -> Result<()> {
    match plan.id(handle) {
        Ok(id) => {
            write!(&mut *output, "{} 0 R", id.0)?;
            Ok(())
        }
        Err(Error::MissingOutputSourceMapping { .. }) => {
            let ObjectHandle::Existing(id) = handle else {
                return Err(missing_mapping_error(handle));
            };
            if document.source().contains_object(id) {
                Err(missing_mapping_error(handle))
            } else {
                output.extend_from_slice(b"null");
                Ok(())
            }
        }
        Err(error) => Err(error),
    }
}

fn write_hayro_number(output: &mut Vec<u8>, value: &hayro_syntax::object::Number) -> Result<()> {
    let representation = value.to_string();
    if representation.contains('e') || representation.contains('E') {
        output.extend_from_slice(expand_scientific(&representation)?.as_bytes());
    } else {
        output.extend_from_slice(representation.as_bytes());
    }
    Ok(())
}

fn write_pdf_real(output: &mut Vec<u8>, value: f64) -> Result<()> {
    if !value.is_finite() {
        return Err(Error::InvalidReal);
    }
    let representation = value.to_string();
    if representation.contains('e') || representation.contains('E') {
        output.extend_from_slice(expand_scientific(&representation)?.as_bytes());
    } else {
        output.extend_from_slice(representation.as_bytes());
    }
    Ok(())
}

fn expand_scientific(value: &str) -> Result<String> {
    let (mantissa, exponent) = value
        .split_once('e')
        .or_else(|| value.split_once('E'))
        .ok_or(Error::InvalidReal)?;
    let exponent = exponent.parse::<i32>().map_err(|_| Error::InvalidReal)?;
    let negative = mantissa.starts_with('-');
    let unsigned = mantissa.strip_prefix('-').unwrap_or(mantissa);
    let decimal_position = unsigned.find('.').unwrap_or(unsigned.len()) as i32;
    let digits = unsigned.replace('.', "");
    let new_position = decimal_position + exponent;

    let mut expanded = String::with_capacity(digits.len() + exponent.unsigned_abs() as usize + 3);
    if negative {
        expanded.push('-');
    }
    if new_position <= 0 {
        expanded.push_str("0.");
        expanded.extend(std::iter::repeat_n('0', (-new_position) as usize));
        expanded.push_str(&digits);
    } else if new_position as usize >= digits.len() {
        expanded.push_str(&digits);
        expanded.extend(std::iter::repeat_n(
            '0',
            new_position as usize - digits.len(),
        ));
    } else {
        let split = new_position as usize;
        expanded.push_str(&digits[..split]);
        expanded.push('.');
        expanded.push_str(&digits[split..]);
    }
    Ok(expanded)
}

fn write_pdf_name(output: &mut Vec<u8>, name: &[u8]) {
    output.push(b'/');
    for &byte in name {
        if is_direct_name_byte(byte) {
            output.push(byte);
        } else {
            output.push(b'#');
            push_hex_byte(output, byte);
        }
    }
}

fn is_direct_name_byte(byte: u8) -> bool {
    matches!(byte, b'!'..=b'~')
        && !matches!(
            byte,
            b'#' | b'%' | b'(' | b')' | b'/' | b'<' | b'>' | b'[' | b']'
        )
}

fn write_pdf_string(output: &mut Vec<u8>, value: &[u8]) {
    if value.iter().all(|byte| matches!(byte, b' '..=b'~')) {
        output.push(b'(');
        for &byte in value {
            if matches!(byte, b'(' | b')' | b'\\') {
                output.push(b'\\');
            }
            output.push(byte);
        }
        output.push(b')');
    } else {
        output.push(b'<');
        for &byte in value {
            push_hex_byte(output, byte);
        }
        output.push(b'>');
    }
}

fn push_hex_byte(output: &mut Vec<u8>, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.push(HEX[(byte >> 4) as usize]);
    output.push(HEX[(byte & 0x0f) as usize]);
}

fn missing_mapping_error(handle: ObjectHandle) -> Error {
    match handle {
        ObjectHandle::Existing(id) => Error::MissingOutputSourceMapping {
            number: id.number(),
            generation: id.generation(),
        },
        ObjectHandle::New(id) => Error::MissingOutputOverlayMapping { index: id.index() },
    }
}

fn check_xref_offset(offset: usize) -> Result<()> {
    let exceeds_limit = match u64::try_from(offset) {
        Ok(offset) => offset > MAX_CLASSIC_XREF_OFFSET,
        Err(_) => true,
    };
    if exceeds_limit {
        Err(Error::OutputOffsetTooLarge { offset })
    } else {
        Ok(())
    }
}

const fn version_bytes(version: PdfVersion) -> &'static [u8] {
    match version {
        PdfVersion::Pdf10 => b"1.0",
        PdfVersion::Pdf11 => b"1.1",
        PdfVersion::Pdf12 => b"1.2",
        PdfVersion::Pdf13 => b"1.3",
        PdfVersion::Pdf14 => b"1.4",
        PdfVersion::Pdf15 => b"1.5",
        PdfVersion::Pdf16 => b"1.6",
        PdfVersion::Pdf17 => b"1.7",
        PdfVersion::Pdf20 => b"2.0",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectId, OwnedDictionary, SourcePdf};

    #[test]
    fn compact_writer_round_trips_and_drops_unreachable_objects() {
        let document = match EditDocument::from_bytes(sample_pdf(true)) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let output = match write_pdf(&document) {
            Ok(output) => output,
            Err(error) => panic!("rewrite should succeed: {error}"),
        };
        let rewritten = match SourcePdf::from_bytes(output) {
            Ok(source) => source,
            Err(error) => panic!("rewritten PDF should parse: {error}"),
        };

        assert_eq!(rewritten.page_count(), 1);
        assert_eq!(rewritten.object_count(), 4);
        let stream = match rewritten.stream_data(ObjectId::new(4, 0)) {
            Ok(stream) => stream,
            Err(error) => panic!("rewritten stream should exist: {error}"),
        };
        assert_eq!(stream.as_ref(), b"q Q");
    }

    #[test]
    fn compact_writer_serializes_overlay_edits() {
        let mut document = match EditDocument::from_bytes(sample_pdf(false)) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let catalog_id = document.source().catalog_id();
        let catalog = match document.edit_object(catalog_id) {
            Ok(catalog) => catalog,
            Err(error) => panic!("catalog should materialize: {error}"),
        };
        let dictionary = match catalog.as_dictionary_mut() {
            Some(dictionary) => dictionary,
            None => panic!("catalog should be a dictionary"),
        };
        dictionary.insert(b"Lang".to_vec(), OwnedObject::String(b"en-GB".to_vec()));

        let added = document
            .overlay_mut()
            .add(OwnedObject::Dictionary(OwnedDictionary::new()));
        let catalog = match document.edit_object(catalog_id) {
            Ok(catalog) => catalog,
            Err(error) => panic!("catalog should remain editable: {error}"),
        };
        let dictionary = match catalog.as_dictionary_mut() {
            Some(dictionary) => dictionary,
            None => panic!("catalog should be a dictionary"),
        };
        dictionary.insert(
            b"PieceInfo".to_vec(),
            OwnedObject::Reference(ObjectHandle::New(added)),
        );

        let output = match write_pdf(&document) {
            Ok(output) => output,
            Err(error) => panic!("rewrite should succeed: {error}"),
        };
        let rewritten = match SourcePdf::from_bytes(output) {
            Ok(source) => source,
            Err(error) => panic!("rewritten PDF should parse: {error}"),
        };
        assert_eq!(rewritten.object_count(), 5);
        let catalog = match rewritten.materialize(rewritten.catalog_id()) {
            Ok(catalog) => catalog,
            Err(error) => panic!("rewritten catalog should materialize: {error}"),
        };
        let dictionary = match catalog {
            OwnedObject::Dictionary(dictionary) => dictionary,
            other => panic!("expected catalog dictionary, got {other:?}"),
        };
        assert_eq!(
            dictionary.get(b"Lang".as_slice()),
            Some(&OwnedObject::String(b"en-GB".to_vec()))
        );
    }

    #[test]
    fn undefined_source_reference_rewrites_as_null() {
        let mut document = match EditDocument::from_bytes(sample_pdf(false)) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let catalog_id = document.source().catalog_id();
        let catalog = match document.edit_object(catalog_id) {
            Ok(catalog) => catalog,
            Err(error) => panic!("catalog should materialize: {error}"),
        };
        let dictionary = match catalog.as_dictionary_mut() {
            Some(dictionary) => dictionary,
            None => panic!("catalog should be a dictionary"),
        };
        dictionary.insert(
            b"Missing".to_vec(),
            OwnedObject::Reference(ObjectHandle::Existing(ObjectId::new(99, 0))),
        );

        let output = match write_pdf(&document) {
            Ok(output) => output,
            Err(error) => panic!("rewrite should succeed: {error}"),
        };
        let rewritten = match SourcePdf::from_bytes(output) {
            Ok(source) => source,
            Err(error) => panic!("rewritten PDF should parse: {error}"),
        };
        let catalog = match rewritten.materialize(rewritten.catalog_id()) {
            Ok(catalog) => catalog,
            Err(error) => panic!("rewritten catalog should materialize: {error}"),
        };
        let OwnedObject::Dictionary(dictionary) = catalog else {
            panic!("expected catalog dictionary");
        };
        assert_eq!(
            dictionary.get(b"Missing".as_slice()),
            Some(&OwnedObject::Null)
        );
    }

    #[test]
    fn scientific_reals_expand_to_pdf_decimal_syntax() {
        let small = match expand_scientific("1.25e-7") {
            Ok(value) => value,
            Err(error) => panic!("scientific real should expand: {error}"),
        };
        let large = match expand_scientific("-2e3") {
            Ok(value) => value,
            Err(error) => panic!("scientific real should expand: {error}"),
        };
        assert_eq!(small, "0.000000125");
        assert_eq!(large, "-2000");
    }

    #[test]
    fn large_source_integer_does_not_round_through_f64() {
        use hayro_syntax::object::FromBytes;

        let number = match hayro_syntax::object::Number::from_bytes(b"9007199254740993") {
            Some(number) => number,
            None => panic!("integer should parse"),
        };
        let mut output = Vec::new();
        if let Err(error) = write_hayro_number(&mut output, &number) {
            panic!("integer should serialize: {error}");
        }
        assert_eq!(output, b"9007199254740993");
    }

    fn sample_pdf(include_orphan: bool) -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << >> /Contents 4 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        if include_orphan {
            append_object(
                &mut pdf,
                &mut offsets,
                b"5 0 obj\n<< /Unused true >>\nendobj\n",
            );
        }

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n",
                if include_orphan { 6 } else { 5 }
            )
            .as_bytes(),
        );
        pdf
    }

    fn append_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, object: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(object);
    }
}
