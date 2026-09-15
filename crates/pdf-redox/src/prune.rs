use crate::{EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result};
use flpdf::{DetachedResourceUsage, find_resources_detached};
use std::collections::{BTreeSet, VecDeque};

fn resolved_dictionary(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|value| match value {
            OwnedObject::Dictionary(dictionary) => Some(dictionary),
            _ => None,
        }))
}

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

fn page_content(document: &EditDocument, page: ObjectHandle) -> Result<Vec<u8>> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(Vec::new());
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    decoded_content_value(document, contents, &mut out)?;
    Ok(out)
}

fn form_content(document: &EditDocument, form: ObjectHandle) -> Result<Vec<u8>> {
    document.decoded_stream_data(form, flpdf::DecodeLevel::Specialized)
}

fn form_resources(document: &EditDocument, form: ObjectHandle) -> Result<Option<OwnedDictionary>> {
    let Some(object) = document.current_owned_object(form)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    resolved_dictionary(document, dictionary.get(b"Resources".as_slice()))
}

fn page_resources(document: &EditDocument, page: ObjectHandle) -> Result<Option<OwnedDictionary>> {
    let Some(value) = document.inherited_page_value(page, b"Resources")? else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(&value)? {
        Some(OwnedObject::Dictionary(dictionary)) => Some(dictionary),
        _ => None,
    })
}

fn xobject_handles(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<Vec<(Vec<u8>, ObjectHandle)>> {
    let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, value) in xobjects {
        let OwnedObject::Reference(handle) = value else {
            continue;
        };
        out.push((name, handle));
    }
    Ok(out)
}

fn is_form(document: &EditDocument, handle: ObjectHandle) -> Result<bool> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    if !matches!(object, OwnedObject::Stream { .. }) {
        return Ok(false);
    }
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(false);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(false);
    };
    Ok(
        matches!(document.resolve_owned_value(subtype)?, Some(OwnedObject::Name(name)) if name == b"Form"),
    )
}

fn scan(content: &[u8]) -> Option<DetachedResourceUsage> {
    find_resources_detached(content)
        .ok()
        .filter(|usage| !usage.pending_operands)
}

fn borrowed_form_names(
    document: &EditDocument,
    resources: &OwnedDictionary,
    root_usage: &DetachedResourceUsage,
) -> Result<BTreeSet<Vec<u8>>> {
    let mut used = BTreeSet::new();
    let mut pending = VecDeque::new();
    let mut seen = BTreeSet::new();
    let xobjects = xobject_handles(document, resources)?;
    if let Some(names) = root_usage.names_by_resource_type.get(b"XObject".as_slice()) {
        for (name, handle) in &xobjects {
            if names.contains(name) && is_form(document, *handle)? {
                pending.push_back(*handle);
            }
        }
    }
    while let Some(form) = pending.pop_front() {
        if !seen.insert(form) {
            continue;
        }
        if form_resources(document, form)?.is_some() {
            continue;
        }
        let Ok(content) = form_content(document, form) else {
            continue;
        };
        let Some(usage) = scan(&content) else {
            continue;
        };
        used.extend(usage.names.iter().cloned());
        if let Some(names) = usage.names_by_resource_type.get(b"XObject".as_slice()) {
            for (name, handle) in &xobjects {
                if names.contains(name) && is_form(document, *handle)? {
                    pending.push_back(*handle);
                }
            }
        }
    }
    Ok(used)
}

fn pruned_resources(
    document: &EditDocument,
    mut resources: OwnedDictionary,
    mut used_names: BTreeSet<Vec<u8>>,
) -> Result<OwnedDictionary> {
    for category in [b"Font".as_slice(), b"XObject".as_slice()] {
        let Some(value) = resources.get(category).cloned() else {
            continue;
        };
        let Some(mut dictionary) = resolved_dictionary(document, Some(&value))? else {
            continue;
        };
        dictionary.retain(|name, _| used_names.contains(name));
        resources.insert(category.to_vec(), OwnedObject::Dictionary(dictionary));
    }
    used_names.clear();
    Ok(resources)
}

fn install_page_resources(
    document: &mut EditDocument,
    page: ObjectHandle,
    resources: OwnedDictionary,
) -> Result<()> {
    let object = match page {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

fn install_form_resources(
    document: &mut EditDocument,
    form: ObjectHandle,
    resources: OwnedDictionary,
) -> Result<()> {
    let object = match form {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

pub(crate) fn should_prune_resources_hayro(document: &EditDocument) -> Result<bool> {
    let catalog = ObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = document.current_owned_object(catalog)? else {
        return Ok(false);
    };
    let Some(catalog) = catalog.as_dictionary() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(pages)) = catalog.get(b"Pages".as_slice()) else {
        return Ok(false);
    };

    let mut queue = VecDeque::from([*pages]);
    let mut nodes_seen = BTreeSet::new();
    let mut indirect_resources_seen = BTreeSet::new();

    while let Some(node) = queue.pop_front() {
        if !nodes_seen.insert(node) {
            continue;
        }
        let Some(object) = document.current_owned_object(node)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };

        if let Some(kids) = dictionary.get(b"Kids".as_slice())
            && let Some(OwnedObject::Array(kids)) = document.resolve_owned_value(kids)?
        {
            if dictionary.contains_key(b"Resources".as_slice()) {
                return Ok(true);
            }
            for kid in kids {
                if let OwnedObject::Reference(kid) = kid {
                    queue.push_back(kid);
                }
            }
            continue;
        }

        let Some(resources_value) = dictionary.get(b"Resources".as_slice()) else {
            continue;
        };
        if let OwnedObject::Reference(resources) = resources_value
            && !indirect_resources_seen.insert(*resources)
        {
            return Ok(true);
        }
        let Some(resources) = document.resolve_owned_value(resources_value)? else {
            continue;
        };
        let Some(resources) = resources.as_dictionary() else {
            continue;
        };
        let Some(xobjects_value) = resources.get(b"XObject".as_slice()) else {
            continue;
        };
        if let OwnedObject::Reference(xobjects) = xobjects_value
            && !indirect_resources_seen.insert(*xobjects)
        {
            return Ok(true);
        }
        let Some(xobjects) = document.resolve_owned_value(xobjects_value)? else {
            continue;
        };
        let Some(xobjects) = xobjects.as_dictionary() else {
            continue;
        };
        for value in xobjects.values() {
            let OwnedObject::Reference(handle) = value else {
                continue;
            };
            if is_form(document, *handle)? {
                queue.push_back(*handle);
            }
        }
    }

    Ok(false)
}

pub(crate) fn prune_resources_hayro(document: &mut EditDocument) -> Result<()> {
    // Prune forms with local resource scopes first. Resource-less forms are
    // intentionally accounted against their caller's scope below.
    let mut forms = BTreeSet::new();
    for handle in document.reachable_output_objects()? {
        if is_form(document, handle)? && form_resources(document, handle)?.is_some() {
            forms.insert(handle);
        }
    }
    for form in forms {
        let Some(resources) = form_resources(document, form)? else {
            continue;
        };
        let Ok(content) = form_content(document, form) else {
            continue;
        };
        let Some(usage) = scan(&content) else {
            continue;
        };
        let mut names = usage.names.clone();
        names.extend(borrowed_form_names(document, &resources, &usage)?);
        let resources = pruned_resources(document, resources, names)?;
        install_form_resources(document, form, resources)?;
    }

    for page in document.page_handles()? {
        let Some(resources) = page_resources(document, page)? else {
            continue;
        };
        let Ok(content) = page_content(document, page) else {
            continue;
        };
        let Some(usage) = scan(&content) else {
            continue;
        };
        let mut names = usage.names.clone();
        names.extend(borrowed_form_names(document, &resources, &usage)?);
        let resources = pruned_resources(document, resources, names)?;
        install_page_resources(document, page, resources)?;
    }
    Ok(())
}
