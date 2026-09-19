use crate::{EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData};

pub(crate) fn decoded_content_value(
    document: &EditDocument,
    value: &OwnedObject,
    out: &mut Vec<u8>,
) -> Result<()> {
    let value = match value {
        OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
            if matches!(value, OwnedObject::Stream { .. }) {
                let bytes = document.decoded_content_stream_data(*handle)?;
                if !out.is_empty() && out.last() != Some(&b'\n') {
                    out.push(b'\n');
                }
                out.extend_from_slice(&bytes);
                return Ok(());
            }
            value
        }
        value => value.clone(),
    };
    match value {
        OwnedObject::Stream { .. } => {
            let bytes =
                document.decoded_owned_stream_data(&value, flpdf::DecodeLevel::Specialized)?;
            if !out.is_empty() && out.last() != Some(&b'\n') {
                out.push(b'\n');
            }
            out.extend_from_slice(&bytes);
        }
        OwnedObject::Array(values) => {
            for value in values {
                decoded_content_value(document, &value, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn resolved_dictionary(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|value| value.as_dictionary().cloned()))
}

pub(crate) fn page_content(document: &EditDocument, page: ObjectHandle) -> Result<Vec<u8>> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(Vec::new());
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut decoded = Vec::new();
    decoded_content_value(document, contents, &mut decoded)?;
    Ok(decoded)
}

pub(crate) fn form_content(document: &EditDocument, form: ObjectHandle) -> Result<Vec<u8>> {
    document.decoded_stream_data(form, flpdf::DecodeLevel::Specialized)
}

pub(crate) fn page_resources(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = document.inherited_page_value(page, b"Resources")? else {
        return Ok(None);
    };
    resolved_dictionary(document, Some(&value))
}

pub(crate) fn form_resources(
    document: &EditDocument,
    form: ObjectHandle,
) -> Result<Option<OwnedDictionary>> {
    let Some(object) = document.current_owned_object(form)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    resolved_dictionary(document, dictionary.get(b"Resources".as_slice()))
}

pub(crate) fn replace_page_content(
    document: &mut EditDocument,
    page: ObjectHandle,
    bytes: Vec<u8>,
) -> Result<()> {
    let stream = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
        dictionary: OwnedDictionary::new(),
        data: StreamData::Owned(bytes),
    }));
    let object = match page {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
    }
    Ok(())
}

pub(crate) fn normalize_page_contents_hayro(document: &mut EditDocument) -> Result<()> {
    for page in document.page_handles()? {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(document, contents, &mut decoded)?;
        let normalized = flpdf::normalize_content_stream(&decoded);
        if normalized.as_bytes() != decoded.as_slice() {
            replace_page_content(document, page, normalized.as_bytes().to_vec())?;
        }
    }
    Ok(())
}
