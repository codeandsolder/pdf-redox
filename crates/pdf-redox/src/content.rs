use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    source::{CurrentValue, into_owned_dictionary},
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct PageRewriteResult<S> {
    pub stats: S,
    pub rewritten_pages: BTreeSet<ObjectHandle>,
}

/// Decode PDF content streams and arrays in order, resolving indirect values.
/// An explicit work stack prevents deep arrays from overflowing the call stack;
/// active references detect cycles without rejecting repeated sibling streams.
pub fn decoded_content_value(
    document: &EditDocument,
    value: &OwnedObject,
    out: &mut Vec<u8>,
) -> Result<()> {
    enum Task<'a> {
        Visit(std::borrow::Cow<'a, OwnedObject>),
        LeaveReference(ObjectHandle),
    }

    let mut pending = vec![Task::Visit(std::borrow::Cow::Borrowed(value))];
    let mut active_references = BTreeSet::new();
    while let Some(task) = pending.pop() {
        let value = match task {
            Task::Visit(value) => value,
            Task::LeaveReference(handle) => {
                active_references.remove(&handle);
                continue;
            }
        };

        match value {
            std::borrow::Cow::Borrowed(OwnedObject::Array(values)) => {
                pending.extend(
                    values
                        .iter()
                        .rev()
                        .map(|value| Task::Visit(std::borrow::Cow::Borrowed(value))),
                );
            }
            std::borrow::Cow::Owned(OwnedObject::Array(values)) => {
                pending.extend(
                    values
                        .into_iter()
                        .rev()
                        .map(|value| Task::Visit(std::borrow::Cow::Owned(value))),
                );
            }
            value => match value.as_ref() {
                OwnedObject::Reference(handle) => {
                    let handle = *handle;
                    if active_references.contains(&handle) {
                        return Err(crate::Error::Invalid(
                            "cyclic indirect reference in page contents".to_owned(),
                        ));
                    }
                    match document.content_read(handle)? {
                        // A stream is a leaf: no active-reference entry is
                        // needed for the common single-stream page.
                        Some(crate::source::ContentRead::Decoded(decoded)) => {
                            append_decoded_content(out, &decoded);
                        }
                        Some(crate::source::ContentRead::Cached(decoded)) => {
                            append_decoded_content(out, &decoded);
                        }
                        Some(crate::source::ContentRead::Other(resolved)) => {
                            active_references.insert(handle);
                            pending.push(Task::LeaveReference(handle));
                            pending.push(Task::Visit(resolved));
                        }
                        None => {}
                    }
                }
                OwnedObject::Stream { .. } => {
                    let decoded = document.decoded_content_stream_value(value.as_ref())?;
                    append_decoded_content(out, &decoded);
                }
                _ => {}
            },
        }
    }
    Ok(())
}

fn append_decoded_content(out: &mut Vec<u8>, decoded: &[u8]) {
    if !out.is_empty() && out.last() != Some(&b'\n') {
        out.push(b'\n');
    }
    out.extend_from_slice(decoded);
}

pub fn resolved_number_value(document: &EditDocument, value: &OwnedObject) -> Result<Option<f64>> {
    Ok(match document.resolve_owned_value_cow(value)?.as_deref() {
        Some(OwnedObject::Integer(value)) => crate::source::exact_i64_to_f64(*value),
        Some(OwnedObject::Real(value)) => Some(*value),
        _ => None,
    })
}

pub fn resolved_number(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    resolved_number_value(document, value)
}

/// Resolve an array containing exactly `N` numeric values.
pub fn resolved_number_array<const N: usize>(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<[f64; N]>> {
    let resolved = document.resolve_owned_value_cow(value)?;
    let Some(OwnedObject::Array(values)) = resolved.as_deref() else {
        return Ok(None);
    };
    if values.len() != N {
        return Ok(None);
    }
    let mut out = [0.0; N];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = resolved_number_value(document, value)? else {
            return Ok(None);
        };
        out[index] = number;
    }
    Ok(Some(out))
}

/// Resolve an exact-length numeric array and reject non-finite members.
pub fn resolved_finite_number_array<const N: usize>(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<[f64; N]>> {
    let Some(values) = resolved_number_array::<N>(document, value)? else {
        return Ok(None);
    };
    Ok(values.into_iter().all(f64::is_finite).then_some(values))
}

pub fn resolved_bool_value(document: &EditDocument, value: &OwnedObject) -> Result<Option<bool>> {
    Ok(match document.resolve_owned_value_cow(value)?.as_deref() {
        Some(OwnedObject::Boolean(value)) => Some(*value),
        _ => None,
    })
}

pub fn resolved_bool(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    resolved_bool_value(document, value)
}

/// Resolve a PDF name value, returning `None` for missing or non-name values.
pub fn resolved_name(
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

/// Return whether a present value resolves to something other than PDF `null`.
pub fn resolved_non_null(document: &EditDocument, value: Option<&OwnedObject>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    Ok(!matches!(
        document.resolve_owned_value_cow(value)?.as_deref(),
        None | Some(OwnedObject::Null)
    ))
}

pub fn resolved_dictionary(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value_cow(value)?
        .and_then(crate::source::into_owned_dictionary))
}

/// Return decoded page contents only if a /Contents entry is present.
///
/// An absent page or non-dictionary page is distinct from an explicitly
/// present, empty content stream. Callers that rewrite pages must preserve
/// that distinction; read-only consumers can use `page_content`.
pub fn page_content_if_present(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<Option<Vec<u8>>> {
    let Some(page) = document.current_value(page)? else {
        return Ok(None);
    };
    let Some(contents) = page.dictionary_entry(b"Contents") else {
        return Ok(None);
    };
    let mut decoded = Vec::new();
    decoded_content_value(document, contents.into_cow().as_ref(), &mut decoded)?;
    Ok(Some(decoded))
}

pub fn page_content(document: &EditDocument, page: ObjectHandle) -> Result<Vec<u8>> {
    Ok(page_content_if_present(document, page)?.unwrap_or_default())
}

pub fn form_content(document: &EditDocument, form: ObjectHandle) -> Result<Vec<u8>> {
    document.decoded_content_stream_data(form)
}

/// Resolve the first inherited `CropBox`/`MediaBox` numeric array accepted by `accept`.
pub fn page_box_numbers_matching(
    document: &EditDocument,
    page: ObjectHandle,
    mut accept: impl FnMut([f64; 4]) -> bool,
) -> Result<Option<[f64; 4]>> {
    for key in [b"CropBox".as_slice(), b"MediaBox".as_slice()] {
        let Some(value) = document.inherited_page_value_cow(page, key)? else {
            continue;
        };
        let Some(values) = resolved_number_array::<4>(document, &value)? else {
            continue;
        };
        if accept(values) {
            return Ok(Some(values));
        }
    }
    Ok(None)
}

/// Resolve the first inherited numeric `CropBox`/`MediaBox`.
pub fn page_box_numbers(document: &EditDocument, page: ObjectHandle) -> Result<Option<[f64; 4]>> {
    page_box_numbers_matching(document, page, |_| true)
}

fn resolved_current_dictionary(
    document: &EditDocument,
    value: Option<CurrentValue<'_>>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(value) = document.resolve_current_value(value)? else {
        return Ok(None);
    };
    if !value.has_dictionary() {
        return Ok(None);
    }
    Ok(into_owned_dictionary(value.into_cow()))
}

pub fn page_resources(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<Option<OwnedDictionary>> {
    resolved_current_dictionary(
        document,
        document.inherited_page_raw_value(page, b"Resources")?,
    )
}

fn xobject_dictionary_handles(xobjects: OwnedDictionary) -> BTreeMap<Vec<u8>, ObjectHandle> {
    xobjects
        .into_iter()
        .filter_map(|(name, value)| match value {
            OwnedObject::Reference(handle) => Some((name, handle)),
            _ => None,
        })
        .collect()
}

/// Resolve the name -> object handle mapping of referenced `XObjects`.
pub fn xobject_handles(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ObjectHandle>> {
    let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    Ok(xobject_dictionary_handles(xobjects))
}

/// Read only the inherited resource dictionary's `XObject` category.
pub fn page_xobject_handles(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<BTreeMap<Vec<u8>, ObjectHandle>> {
    let Some(resources) = document.inherited_page_raw_value(page, b"Resources")? else {
        return Ok(BTreeMap::new());
    };
    let Some(resources) = document.resolve_current_value(resources)? else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) =
        resolved_current_dictionary(document, resources.dictionary_entry(b"XObject"))?
    else {
        return Ok(BTreeMap::new());
    };
    Ok(xobject_dictionary_handles(xobjects))
}

pub fn form_resources(
    document: &EditDocument,
    form: ObjectHandle,
) -> Result<Option<OwnedDictionary>> {
    let Some(object) = document.current_value(form)? else {
        return Ok(None);
    };
    resolved_current_dictionary(document, object.dictionary_entry(b"Resources"))
}

pub fn effective_page_resources(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<OwnedDictionary> {
    Ok(page_resources(document, page)?.unwrap_or_default())
}

pub fn page_user_unit(document: &EditDocument, page: ObjectHandle) -> Result<Option<f64>> {
    let Some(object) = document.current_value(page)? else {
        return Ok(None);
    };
    if !object.has_dictionary() {
        return Ok(None);
    }
    let Some(value) = object.dictionary_entry(b"UserUnit") else {
        return Ok(Some(1.0));
    };
    let Some(value) = resolved_number_value(document, value.into_cow().as_ref())? else {
        return Ok(None);
    };
    Ok((value.is_finite() && value > 0.0).then_some(value))
}

pub fn install_page_resource(
    document: &mut EditDocument,
    page: ObjectHandle,
    resource_type: &[u8],
    name: Vec<u8>,
    target: ObjectHandle,
) -> Result<()> {
    let mut resources = effective_page_resources(document, page)?;
    let mut entries =
        resolved_dictionary(document, resources.get(resource_type))?.unwrap_or_default();
    entries.insert(name, OwnedObject::Reference(target));
    resources.insert(resource_type.to_vec(), OwnedObject::Dictionary(entries));
    let object = document.edit_handle(page)?;
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

pub fn replace_page_content_with_handle(
    document: &mut EditDocument,
    page: ObjectHandle,
    bytes: Vec<u8>,
) -> Result<ObjectHandle> {
    let stream = ObjectHandle::New(document.add_object(OwnedObject::Stream {
        dictionary: OwnedDictionary::new(),
        data: StreamData::Owned(bytes),
    }));
    let object = document.edit_handle(page)?;
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
    }
    Ok(stream)
}

pub fn replace_page_content(
    document: &mut EditDocument,
    page: ObjectHandle,
    bytes: Vec<u8>,
) -> Result<()> {
    replace_page_content_with_handle(document, page, bytes).map(|_| ())
}

/// Replace page contents with a new Flate-encoded stream instead of leaving
/// entire page commands uncompressed after semantic-only transformations.
pub fn replace_page_content_flate(
    document: &mut EditDocument,
    page: ObjectHandle,
    decoded: &[u8],
    level: crate::FlateLevel,
) -> Result<()> {
    let encoded = crate::stream_codec::encode_flate(decoded, level)?;
    let mut dictionary = OwnedDictionary::new();
    dictionary.insert(
        b"Filter".to_vec(),
        OwnedObject::Name(b"FlateDecode".to_vec()),
    );
    let stream = ObjectHandle::New(document.add_object(OwnedObject::Stream {
        dictionary,
        data: StreamData::Owned(encoded),
    }));
    let object = document.edit_handle(page)?;
    if let Some(dict) = object.as_dictionary_mut() {
        dict.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
    }
    Ok(())
}

pub fn normalize_page_contents(document: &mut EditDocument, pages: &[ObjectHandle]) -> Result<()> {
    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        let normalized = crate::content_stream::normalize_content_stream(&decoded);
        if normalized.as_slice() != decoded.as_slice() {
            replace_page_content(document, page, normalized)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectId;
    use crate::test_support::ClassicPdfBuilder;

    fn content_reference(number: u32) -> OwnedObject {
        OwnedObject::Reference(ObjectHandle::Existing(ObjectId::new(number, 0)))
    }

    #[test]
    fn content_traversal_follows_reference_chains_and_preserves_repeated_streams() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.stream(4, b"", b"q Q")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let alias = document.add_object(content_reference(4));
        let second_alias = document.add_object(OwnedObject::Reference(ObjectHandle::New(alias)));
        let array = document.add_object(OwnedObject::Array(vec![
            content_reference(4),
            OwnedObject::Reference(ObjectHandle::New(second_alias)),
        ]));
        let contents = OwnedObject::Array(vec![
            OwnedObject::Reference(ObjectHandle::New(array)),
            OwnedObject::Reference(ObjectHandle::New(second_alias)),
            OwnedObject::Stream {
                dictionary: OwnedDictionary::new(),
                data: StreamData::Owned(b"BT ET".to_vec()),
            },
        ]);
        let mut decoded = Vec::new();
        decoded_content_value(&document, &contents, &mut decoded)?;
        assert_eq!(decoded, b"q Q\nq Q\nq Q\nBT ET");
        Ok(())
    }

    #[test]
    fn content_traversal_rejects_reference_cycles() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(4, b"[5 0 R]")?;
        pdf.object(5, b"[4 0 R]")?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let mut decoded = Vec::new();
        assert!(matches!(
            decoded_content_value(&document, &content_reference(4), &mut decoded),
            Err(crate::Error::Invalid(message)) if message.contains("cyclic indirect reference")
        ));
        Ok(())
    }

    #[test]
    fn content_traversal_handles_deep_direct_arrays_iteratively() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let mut content = OwnedObject::Stream {
            dictionary: OwnedDictionary::new(),
            data: StreamData::Owned(b"q Q".to_vec()),
        };
        for _ in 0..2048 {
            content = OwnedObject::Array(vec![content]);
        }
        let mut decoded = Vec::new();
        decoded_content_value(&document, &content, &mut decoded)?;
        assert_eq!(decoded, b"q Q");
        Ok(())
    }

    #[test]
    fn page_content_presence_and_inherited_xobjects_preserve_resource_semantics() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 /Resources << /XObject 7 0 R >> >>",
        )?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"")?;
        pdf.object(5, b"<< /Subtype /Image >>")?;
        pdf.object(
            6,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] >>",
        )?;
        pdf.object(7, b"<< /Im 5 0 R /Ignored 42 /Null null >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = document.page_handles()?;
        assert_eq!(
            page_content_if_present(&document, pages[0])?,
            Some(Vec::new())
        );
        assert_eq!(page_content_if_present(&document, pages[1])?, None);
        assert_eq!(page_content(&document, pages[1])?, Vec::<u8>::new());

        let first = page_xobject_handles(&document, pages[0])?;
        assert_eq!(first, page_xobject_handles(&document, pages[1])?);
        assert_eq!(first.len(), 1);
        assert!(first.contains_key(b"Im".as_slice()));

        normalize_page_contents(&mut document, &pages)?;
        assert_eq!(
            page_content_if_present(&document, pages[0])?,
            Some(Vec::new())
        );
        assert_eq!(page_content_if_present(&document, pages[1])?, None);
        Ok(())
    }

    #[test]
    #[expect(
        clippy::float_cmp,
        reason = "the test intentionally models exact zero-area page-box rejection"
    )]
    fn page_box_matching_falls_back_after_rejected_crop_box() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /CropBox [0 0 0 0] /MediaBox [0 0 100 200] >>",
        )?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let page = document.page_handles()?[0];

        let values = page_box_numbers_matching(&document, page, |values| {
            values[0] != values[2] && values[1] != values[3]
        })?;
        assert_eq!(values, Some([0.0, 0.0, 100.0, 200.0]));
        Ok(())
    }
}
