use crate::{EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData};

fn decoded_content_value(
    document: &EditDocument,
    value: &OwnedObject,
    out: &mut Vec<u8>,
) -> Result<()> {
    let value = match value {
        OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
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

fn replace_page_content(
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
