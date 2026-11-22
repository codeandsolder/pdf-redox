use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    source::{CurrentValue, PageBoxCoordinates, into_owned_dictionary},
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
    value: CurrentValue<'_>,
    out: &mut Vec<u8>,
) -> Result<()> {
    enum Task<'a> {
        Visit(CurrentValue<'a>),
        Array(crate::source::CurrentArrayIter<'a>),
        LeaveReference(ObjectHandle),
    }

    // The stack stores one cursor per nested array, not every sibling.
    let mut pending = smallvec::SmallVec::<[Task<'_>; 4]>::new();
    pending.push(Task::Visit(value));
    let mut active_references = BTreeSet::new();
    while let Some(task) = pending.pop() {
        let value = match task {
            Task::Visit(value) => value,
            Task::Array(mut values) => {
                if let Some(value) = values.next() {
                    pending.push(Task::Array(values));
                    pending.push(Task::Visit(value));
                }
                continue;
            }
            Task::LeaveReference(handle) => {
                active_references.remove(&handle);
                continue;
            }
        };
        if let Some(handle) = value.reference() {
            if active_references.contains(&handle) {
                return Err(crate::Error::Invalid(
                    "cyclic indirect reference in page contents".to_owned(),
                ));
            }
            match document.content_read(handle)? {
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
        } else if let Some(items) = value.array_items() {
            pending.push(Task::Array(items));
        } else if value.is_stream() {
            let decoded = document.decoded_stream_value(&value)?;
            append_decoded_content(out, &decoded);
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

pub fn resolved_number_value(
    document: &EditDocument,
    value: CurrentValue<'_>,
) -> Result<Option<f64>> {
    Ok(document
        .resolve_current_value(value)?
        .and_then(|value| value.number())
        .and_then(crate::source::NumericValue::exact_f64))
}

pub fn resolved_number(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    resolved_number_value(document, CurrentValue::Owned(value))
}

/// Resolve an array containing exactly `N` numeric values.
pub fn resolved_number_array<const N: usize>(
    document: &EditDocument,
    value: CurrentValue<'_>,
) -> Result<Option<[f64; N]>> {
    let Some(value) = document.resolve_current_value(value)? else {
        return Ok(None);
    };
    let Some(mut values) = value.array_items() else {
        return Ok(None);
    };
    // Check the raw argument count before following references, including
    // malformed arrays with a deleted reference and extra operands.
    let components: [Option<CurrentValue<'_>>; N] = std::array::from_fn(|_| values.next());
    if components.iter().any(Option::is_none) || values.next().is_some() {
        return Ok(None);
    }
    let mut out = [0.0; N];
    for (number, component) in out.iter_mut().zip(components) {
        let Some(component) = component else {
            return Ok(None);
        };
        let Some(value) = resolved_number_value(document, component)? else {
            return Ok(None);
        };
        *number = value;
    }
    Ok(Some(out))
}

/// Resolve an exact-length numeric array and reject non-finite members.
pub fn resolved_finite_number_array<const N: usize>(
    document: &EditDocument,
    value: CurrentValue<'_>,
) -> Result<Option<[f64; N]>> {
    let Some(values) = resolved_number_array::<N>(document, value)? else {
        return Ok(None);
    };
    Ok(values.into_iter().all(f64::is_finite).then_some(values))
}

pub fn resolved_bool_value(document: &EditDocument, value: &OwnedObject) -> Result<Option<bool>> {
    Ok(document
        .resolve_current_value(CurrentValue::Owned(value))?
        .and_then(|value| value.boolean()))
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
    Ok(document
        .resolve_current_value(CurrentValue::Owned(value))?
        .and_then(|value| value.name().map(<[u8]>::to_vec)))
}

/// Return whether a present value resolves to something other than PDF `null`.
pub fn resolved_non_null(document: &EditDocument, value: Option<&OwnedObject>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    Ok(document
        .resolve_current_value(CurrentValue::Owned(value))?
        .is_some_and(|value| !value.is_null()))
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
    decoded_content_value(document, contents, &mut decoded)?;
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
        let Some(PageBoxCoordinates::Exact(values)) = document.inherited_page_box(page, key)?
        else {
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
    Ok(document
        .inherited_page_resources(page)?
        .and_then(|resources| into_owned_dictionary(resources.into_cow())))
}

#[derive(Debug, PartialEq, Eq)]
pub struct XObjectBindings {
    pub handles: BTreeMap<Vec<u8>, ObjectHandle>,
    pub complete: bool,
}

fn xobject_dictionary_bindings(
    document: &EditDocument,
    value: Option<CurrentValue<'_>>,
    mut complete: bool,
) -> Result<XObjectBindings> {
    let mut handles = BTreeMap::new();
    if let Some(value) = value
        && let Some(value) = document.resolve_current_value(value)?
        && !value.is_null()
    {
        complete &= value.is_dictionary();
        if let Some(entries) = value.dictionary_entries() {
            for (name, value) in entries {
                if let Some(handle) = value.reference() {
                    handles.insert(name.as_ref().to_vec(), handle);
                } else {
                    complete = false;
                }
            }
        }
    }
    Ok(XObjectBindings { handles, complete })
}

/// Inspect only a resource scope's `XObject` category; completeness is separate
/// from the mapping so callers that require a proof can reject invalid values.
/// Missing resources have no local bindings; explicit malformed resources
/// are incomplete, not equivalent to an inherited Form scope.
pub fn resource_xobject_bindings(
    document: &EditDocument,
    resources: Option<CurrentValue<'_>>,
) -> Result<XObjectBindings> {
    let Some(resources) = resources else {
        return Ok(XObjectBindings {
            handles: BTreeMap::new(),
            complete: true,
        });
    };
    let Some(resources) = document.resolve_current_value(resources)? else {
        return Ok(XObjectBindings {
            handles: BTreeMap::new(),
            complete: false,
        });
    };
    xobject_dictionary_bindings(
        document,
        resources.dictionary_entry(b"XObject"),
        resources.is_dictionary(),
    )
}

/// Resolve the name -> object handle mapping of referenced `XObjects`.
pub fn xobject_handles(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ObjectHandle>> {
    Ok(xobject_dictionary_bindings(
        document,
        resources
            .get(b"XObject".as_slice())
            .map(CurrentValue::Owned),
        true,
    )?
    .handles)
}

/// Read only the effective inherited resource dictionary's `XObject` category.
pub fn page_xobject_bindings(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<XObjectBindings> {
    resource_xobject_bindings(document, document.inherited_page_resources(page)?)
}

pub fn object_subtype(document: &EditDocument, handle: ObjectHandle) -> Result<Option<Vec<u8>>> {
    let Some(object) = document.current_value(handle)? else {
        return Ok(None);
    };
    Ok(object
        .resolved_entry(document, b"Subtype")?
        .and_then(|value| value.name().map(<[u8]>::to_vec)))
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
    let Some(value) = resolved_number_value(document, value)? else {
        return Ok(None);
    };
    Ok((value.is_finite() && value > 0.0).then_some(value))
}

/// Install a category of resource bindings in one transaction per page.
/// Repeated names on a page use the last binding in input order. Existing
/// inherited resources and unrelated categories are preserved.
pub fn install_page_resource_bindings<'a>(
    document: &mut EditDocument,
    resource_type: &[u8],
    bindings: impl IntoIterator<Item = (ObjectHandle, &'a [u8], ObjectHandle)>,
) -> Result<()> {
    let mut grouped = BTreeMap::<ObjectHandle, BTreeMap<&[u8], ObjectHandle>>::new();
    for (page, name, target) in bindings {
        grouped.entry(page).or_default().insert(name, target);
    }
    for (page, bindings) in grouped {
        let mut resources = effective_page_resources(document, page)?;
        let existing = resources.remove(resource_type);
        let mut entries = match existing {
            Some(OwnedObject::Dictionary(entries)) => entries,
            other => resolved_dictionary(document, other.as_ref())?.unwrap_or_default(),
        };
        entries.extend(
            bindings
                .into_iter()
                .map(|(name, target)| (name.to_vec(), OwnedObject::Reference(target))),
        );
        resources.insert(resource_type.to_vec(), OwnedObject::Dictionary(entries));
        let object = document.edit_handle(page)?;
        if let Some(dictionary) = object.as_dictionary_mut() {
            dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
        }
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
    fn scalar_and_array_projection_preserves_source_overlay_equivalence() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /UserUnit 4 0 R >>",
        )?;
        pdf.object(4, b"9223372036854775808.0")?;
        pdf.object(5, b"[4 0 R -0.0 9007199254740994 1.25]")?;
        pdf.object(6, b"[4 0 R 9007199254740993]")?;
        pdf.object(7, b"[4 0 R 1 2]")?;
        pdf.object(8, b"true")?;
        pdf.object(9, b"/Example")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let existing = |number| ObjectHandle::Existing(ObjectId::new(number, 0));
        let exact = content_reference(5);
        let inexact = content_reference(6);
        let too_long = content_reference(7);
        let number = content_reference(4);
        let boolean = content_reference(8);
        let name = content_reference(9);
        let expected = Some([
            9_223_372_036_854_775_808.0,
            0.0,
            9_007_199_254_740_994.0,
            1.25,
        ]);
        for _ in 0..2 {
            assert_eq!(
                resolved_number_value(&document, CurrentValue::Owned(&number))?,
                expected.map(|numbers| numbers[0])
            );
            assert_eq!(
                page_user_unit(&document, existing(3))?,
                expected.map(|numbers| numbers[0])
            );
            assert_eq!(
                resolved_finite_number_array::<4>(&document, CurrentValue::Owned(&exact))?,
                expected
            );
            assert_eq!(
                resolved_number_array::<2>(&document, CurrentValue::Owned(&inexact))?,
                None
            );
            assert_eq!(
                resolved_number_array::<2>(&document, CurrentValue::Owned(&too_long))?,
                None
            );
            assert_eq!(resolved_bool_value(&document, &boolean)?, Some(true));
            assert_eq!(
                resolved_name(&document, Some(&name))?,
                Some(b"Example".to_vec())
            );
            assert!(resolved_non_null(&document, Some(&name))?);
            for number in 3..=9 {
                let _ = document.edit_handle(existing(number))?;
            }
        }
        let cycle = document.add_object(OwnedObject::Null);
        *document.edit_handle(ObjectHandle::New(cycle))? =
            OwnedObject::Reference(ObjectHandle::New(cycle));
        let cycle = OwnedObject::Reference(ObjectHandle::New(cycle));
        assert_eq!(
            resolved_number_value(&document, CurrentValue::Owned(&cycle))?,
            None
        );
        assert!(!resolved_non_null(&document, Some(&cycle))?);
        *document.edit_handle(existing(4))? = OwnedObject::Real(f64::INFINITY);
        assert_eq!(
            resolved_finite_number_array::<4>(&document, CurrentValue::Owned(&exact))?,
            None
        );
        Ok(())
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
        decoded_content_value(&document, CurrentValue::Owned(&contents), &mut decoded)?;
        assert_eq!(decoded, b"q Q\nq Q\nq Q\nBT ET");
        Ok(())
    }

    #[test]
    fn content_traversal_borrows_source_arrays_and_resolves_current_parameters() -> Result<()> {
        use crate::source::ContentRead;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents [4 0 R [5 0 R 4 0 R] 6 0 R] >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"[7 0 R 4 0 R]")?;
        pdf.stream(6, b"", b"BT ET")?;
        let encoded = crate::stream_codec::encode_flate(b"1 0 m", crate::FlateLevel::PROCESSING)?;
        pdf.stream(
            7,
            b"/Filter /FlateDecode /DecodeParms 8 0 R /Unrelated << /A [1 2 3] >>",
            &encoded,
        )?;
        pdf.object(8, b"<< /Predictor 1 /Extra [(test) true 4.5 /Name null] >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let existing = |number| ObjectHandle::Existing(ObjectId::new(number, 0));
        let page = existing(3);
        let expected = b"q Q\n1 0 m\nq Q\nq Q\nBT ET";
        assert!(matches!(
            document.content_read(existing(5))?,
            Some(ContentRead::Other(CurrentValue::Source(..)))
        ));
        assert_eq!(page_content(&document, page)?, expected);
        assert!(matches!(
            document.content_read(existing(4))?,
            Some(ContentRead::Cached(_))
        ));
        assert!(matches!(
            document.content_read(existing(7))?,
            Some(ContentRead::Decoded(_))
        ));
        assert!(document.overlay().is_empty());

        for number in [3, 5, 7, 8] {
            let _ = document.edit_handle(existing(number))?;
        }
        assert!(matches!(
            document.content_read(existing(5))?,
            Some(ContentRead::Other(CurrentValue::Owned(_)))
        ));
        assert_eq!(page_content(&document, page)?, expected);
        *document.edit_handle(existing(8))? = OwnedObject::Stream {
            dictionary: OwnedDictionary::new(),
            data: StreamData::Owned(Vec::new()),
        };
        assert!(matches!(
            page_content(&document, page),
            Err(crate::Error::Invalid(message)) if message.contains("stream cannot be embedded")
        ));
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
            decoded_content_value(&document, CurrentValue::Owned(&content_reference(4)), &mut decoded),
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
        decoded_content_value(&document, CurrentValue::Owned(&content), &mut decoded)?;
        assert_eq!(decoded, b"q Q");
        Ok(())
    }

    #[test]
    fn resource_binding_batch_preserves_inheritance_and_last_binding() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources << /Font << /F 5 0 R >> /XObject 6 0 R >> >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(4, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(5, b"<< /Type /Font >>")?;
        pdf.object(6, b"<< /Existing 5 0 R /NonReference 42 >>")?;
        let mut doc = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = doc.page_handles()?;
        let inherited = effective_page_resources(&doc, pages[1])?;
        let first = ObjectHandle::New(doc.add_object(OwnedObject::Null));
        let last = ObjectHandle::New(doc.add_object(OwnedObject::Null));
        install_page_resource_bindings(
            &mut doc,
            b"XObject",
            [
                (pages[0], b"Added".as_slice(), first),
                (pages[0], b"Other".as_slice(), first),
                (pages[0], b"Added".as_slice(), last),
            ],
        )?;
        let resources = effective_page_resources(&doc, pages[0])?;
        assert_eq!(
            resources.get(b"Font".as_slice()),
            inherited.get(b"Font".as_slice())
        );
        let entries = resolved_dictionary(&doc, resources.get(b"XObject".as_slice()))?
            .ok_or_else(|| crate::Error::Invalid("missing resource category".into()))?;
        assert_eq!(
            entries.get(b"Existing".as_slice()),
            Some(&content_reference(5))
        );
        assert_eq!(
            entries.get(b"NonReference".as_slice()),
            Some(&OwnedObject::Integer(42))
        );
        assert_eq!(
            entries.get(b"Added".as_slice()),
            Some(&OwnedObject::Reference(last))
        );
        assert_eq!(
            entries.get(b"Other".as_slice()),
            Some(&OwnedObject::Reference(first))
        );
        assert_eq!(effective_page_resources(&doc, pages[1])?, inherited);
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

        let first = page_xobject_bindings(&document, pages[0])?;
        assert_eq!(first, page_xobject_bindings(&document, pages[1])?);
        assert_eq!(
            first.handles,
            BTreeMap::from([(
                b"Im".to_vec(),
                ObjectHandle::Existing(crate::ObjectId::new(5, 0))
            )])
        );
        assert!(!first.complete);
        // Direct and indirect resource views share the same binding evidence.
        let root = ObjectHandle::Existing(crate::ObjectId::new(2, 0));
        let map = ObjectHandle::Existing(crate::ObjectId::new(7, 0));
        let _ = document.edit_handle(root)?;
        let _ = document.edit_handle(map)?;
        assert_eq!(page_xobject_bindings(&document, pages[0])?, first);
        *document.edit_handle(map)? = OwnedObject::Dictionary(OwnedDictionary::from([(
            b"Im".to_vec(),
            content_reference(5),
        )]));
        let complete = page_xobject_bindings(&document, pages[1])?;
        assert_eq!(complete.handles, first.handles);
        assert!(complete.complete);
        *document.edit_handle(map)? = OwnedObject::Name(b"Invalid".to_vec());
        assert_eq!(
            page_xobject_bindings(&document, pages[0])?,
            XObjectBindings {
                handles: BTreeMap::new(),
                complete: false,
            }
        );
        *document.edit_handle(map)? = OwnedObject::Null;
        assert_eq!(
            page_xobject_bindings(&document, pages[0])?,
            XObjectBindings {
                handles: BTreeMap::new(),
                complete: true,
            }
        );
        // Explicit malformed form Resources must not become an inherited scope.
        assert!(
            !resource_xobject_bindings(&document, Some(CurrentValue::Owned(&OwnedObject::Null)))?
                .complete
        );
        assert!(resource_xobject_bindings(&document, None)?.complete);

        normalize_page_contents(&mut document, &pages)?;
        assert_eq!(
            page_content_if_present(&document, pages[0])?,
            Some(Vec::new())
        );
        assert_eq!(page_content_if_present(&document, pages[1])?, None);
        Ok(())
    }

    #[test]
    fn page_boxes_project_the_same_numeric_prefix_used_for_inheritance() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /CropBox [1 2 101 202] /MediaBox [0 0 300 400] >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /CropBox 4 0 R >>")?;
        pdf.object(4, b"[10 20 5 0 R 220 /Extra]")?;
        pdf.object(5, b"110")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let existing = |number| ObjectHandle::Existing(crate::ObjectId::new(number, 0));
        let page = existing(3);
        let crop = document.source().syntax().pages()[0].crop_box();
        assert_eq!(
            [crop.x0, crop.y0, crop.x1, crop.y1],
            [10.0, 20.0, 110.0, 220.0]
        );
        assert_eq!(
            page_box_numbers(&document, page)?,
            Some([10.0, 20.0, 110.0, 220.0])
        );
        // Invalidating a coordinate through the overlay makes the parent CropBox effective.
        *document.edit_handle(existing(5))? = OwnedObject::Null;
        assert_eq!(
            page_box_numbers(&document, page)?,
            Some([1.0, 2.0, 101.0, 202.0])
        );
        // An inexact but numeric local box still shadows the ancestor; proof
        // clients retain the established MediaBox fallback instead.
        *document.edit_handle(existing(5))? = OwnedObject::Integer(9_007_199_254_740_993);
        assert_eq!(
            document.inherited_page_value(page, b"CropBox")?,
            Some(OwnedObject::Reference(existing(4)))
        );
        assert!(matches!(
            document.inherited_page_box(page, b"CropBox")?,
            Some(PageBoxCoordinates::Inexact)
        ));
        assert_eq!(
            page_box_numbers(&document, page)?,
            Some([0.0, 0.0, 300.0, 400.0])
        );
        // A wholly owned array uses identical prefix and reference semantics.
        *document.edit_handle(existing(4))? = OwnedObject::Array(vec![
            OwnedObject::Integer(-4),
            OwnedObject::Integer(6),
            OwnedObject::Integer(70),
            OwnedObject::Integer(80),
            OwnedObject::Name(b"Trailing".to_vec()),
        ]);
        assert_eq!(
            page_box_numbers(&document, page)?,
            Some([-4.0, 6.0, 70.0, 80.0])
        );
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
