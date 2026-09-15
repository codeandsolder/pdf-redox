use crate::{
    EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    StreamData, source::CurrentObject,
};
#[cfg(test)]
use flpdf::{ObjectHandle, ObjectRef, Pdf};
use hayro_syntax::{
    PdfVersion,
    object::{MaybeRef as HayroMaybeRef, Name as HayroName, Object as HayroObject},
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet};
#[cfg(test)]
use std::io::{Read, Seek};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TargetedDedupStats {
    pub duplicate_streams_detected: usize,
    pub duplicate_raw_bytes: usize,
    pub references_canonicalized: usize,
}

#[cfg(test)]
fn stream_fingerprint_ignoring(
    object: &ObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
) -> Result<Option<[u8; 32]>> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok(None);
    };

    // Require byte-identical encoded payload and an identical resolved stream
    // dictionary. This deliberately refuses broader decoded-content
    // equivalence: differing filters, decode parameters, or stream attributes
    // stay as separate objects.
    let raw = object.get_raw_stream_data()?;
    // `/Length` is derived bookkeeping, not stream semantics. Producers often
    // store it in a distinct indirect integer object for every otherwise
    // identical stream, which makes object-number-sensitive serialization
    // falsely distinguish exact duplicates. The encoded payload itself is
    // already hashed below, so omit `/Length` while retaining every other
    // stream-dictionary entry exactly unless a caller supplies an additional
    // specification-backed non-semantic key to ignore.
    let Some(entries) = dict.as_dictionary() else {
        return Ok(None);
    };
    let dictionary = ObjectHandle::dictionary(
        entries
            .into_iter()
            .filter(|(key, _)| {
                key.as_slice() != b"/Length" && !ignored_dictionary_keys.contains(&key.as_slice())
            })
            .collect(),
    )
    .unparse_resolved();
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update((raw.len() as u64).to_le_bytes());
    hasher.update(raw.as_ref());
    hasher.update((dictionary.len() as u64).to_le_bytes());
    hasher.update(dictionary);
    Ok(Some(hasher.finalize().into()))
}

#[cfg(test)]
fn stream_fingerprint(object: &ObjectHandle, domain: &[u8]) -> Result<Option<[u8; 32]>> {
    stream_fingerprint_ignoring(object, domain, &[])
}

#[cfg(test)]
pub(crate) fn canonicalize_metadata_streams<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let holders = metadata_holders(&objects);
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    // Treat the /Metadata reference as authoritative. Real producers exist
    // that omit the stream's nominal /Type /Metadata entry entirely.
    for dict in &holders {
        let Ok(metadata) = dict.try_get_key(b"/Metadata") else {
            continue;
        };
        let Some(metadata_ref) = metadata.object_ref() else {
            continue;
        };
        if pdf.resolve(&metadata).is_err() {
            continue;
        }
        let Ok(Some(fingerprint)) = stream_fingerprint(&metadata, b"metadata") else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != metadata_ref {
                redirects.insert(metadata_ref, canonical_ref);
                if duplicate_refs.insert(metadata_ref) {
                    duplicate_raw_bytes += metadata.get_raw_stream_data()?.len();
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, metadata_ref);
        }
    }

    let mut references_canonicalized = 0_usize;
    for dict in holders {
        let Ok(metadata) = dict.try_get_key(b"/Metadata") else {
            continue;
        };
        let Some(metadata_ref) = metadata.object_ref() else {
            continue;
        };
        let Some(canonical_ref) = redirects.get(&metadata_ref).copied() else {
            continue;
        };

        dict.replace_key(b"/Metadata", pdf.get_object_handle(canonical_ref))?;
        pdf.mark_object_handle_dirty(&dict)?;
        references_canonicalized += 1;
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
fn collect_direct_metadata_holders(
    value: &ObjectHandle,
    include_indirect_root: bool,
    holders: &mut Vec<ObjectHandle>,
) {
    // Every indirect object is visited separately from `get_all_objects`.
    // Recurse only through direct descendants here so cycles in the object
    // graph cannot turn metadata discovery into an unbounded traversal.
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    if let Some(dict) = value.as_stream_dict() {
        if matches!(dict.try_get_key(b"/Metadata"), Ok(metadata) if !metadata.is_null()) {
            holders.push(dict.clone());
        }
        if let Some(entries) = dict.as_dictionary() {
            for (key, child) in entries {
                if key.as_slice() != b"/Metadata" {
                    collect_direct_metadata_holders(&child, false, holders);
                }
            }
        }
        return;
    }

    if matches!(value.try_is_dictionary(), Ok(true)) {
        if matches!(value.try_get_key(b"/Metadata"), Ok(metadata) if !metadata.is_null()) {
            holders.push(value.clone());
        }
        if let Some(entries) = value.as_dictionary() {
            for (key, child) in entries {
                if key.as_slice() != b"/Metadata" {
                    collect_direct_metadata_holders(&child, false, holders);
                }
            }
        }
        return;
    }

    if let Some(items) = value.as_array() {
        for item in items {
            collect_direct_metadata_holders(&item, false, holders);
        }
    }
}

#[cfg(test)]
fn metadata_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_metadata_holders(object, true, &mut holders);
    }
    holders
}

#[cfg(test)]
fn collect_direct_icc_arrays(
    value: &ObjectHandle,
    include_indirect_root: bool,
    arrays: &mut Vec<ObjectHandle>,
) {
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    let Ok(is_array) = value.try_is_array() else {
        return;
    };
    if is_array {
        let Some(items) = value.as_array() else {
            return;
        };
        if items.len() >= 2 && matches!(items[0].try_is_name_and_equals(b"ICCBased"), Ok(true)) {
            arrays.push(value.clone());
            return;
        }
        for item in items {
            collect_direct_icc_arrays(&item, false, arrays);
        }
        return;
    }

    if let Some(dict) = value.as_stream_dict() {
        if let Some(entries) = dict.as_dictionary() {
            for (_, child) in entries {
                collect_direct_icc_arrays(&child, false, arrays);
            }
        }
        return;
    }

    if matches!(value.try_is_dictionary(), Ok(true))
        && let Some(entries) = value.as_dictionary()
    {
        for (_, child) in entries {
            collect_direct_icc_arrays(&child, false, arrays);
        }
    }
}

#[cfg(test)]
fn icc_arrays(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut arrays = Vec::new();
    for object in objects {
        collect_direct_icc_arrays(object, true, &mut arrays);
    }
    arrays
}

#[cfg(test)]
pub(crate) fn canonicalize_icc_profiles<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let arrays = icc_arrays(&objects);
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    for array in &arrays {
        let profile = array.try_get_array_item(1)?;
        let Some(profile_ref) = profile.object_ref() else {
            continue;
        };
        if pdf.resolve(&profile).is_err() {
            continue;
        }
        let Ok(Some(fingerprint)) = stream_fingerprint(&profile, b"icc-profile") else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != profile_ref {
                redirects.insert(profile_ref, canonical_ref);
                if duplicate_refs.insert(profile_ref) {
                    duplicate_raw_bytes += profile.get_raw_stream_data()?.len();
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, profile_ref);
        }
    }

    let mut references_canonicalized = 0_usize;
    for array in arrays {
        let profile = array.try_get_array_item(1)?;
        let Some(profile_ref) = profile.object_ref() else {
            continue;
        };
        let Some(canonical_ref) = redirects.get(&profile_ref).copied() else {
            continue;
        };
        array.set_array_item(1, pdf.get_object_handle(canonical_ref))?;
        pdf.mark_object_handle_dirty(&array)?;
        references_canonicalized += 1;
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
fn xobject_fingerprint(
    object: &ObjectHandle,
    subtype_name: &[u8],
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
) -> Result<Option<[u8; 32]>> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok(None);
    };
    let subtype = dict.try_get_key(b"/Subtype")?;
    if !subtype.try_is_name_and_equals(subtype_name)? {
        return Ok(None);
    }
    stream_fingerprint_ignoring(object, domain, ignored_dictionary_keys)
}

#[cfg(test)]
fn collect_direct_xobject_holders(
    value: &ObjectHandle,
    include_indirect_root: bool,
    holders: &mut Vec<ObjectHandle>,
) {
    // Every indirect object is visited separately from `get_all_objects`.
    // Recurse only through direct descendants so page/Form resource
    // dictionaries are discovered without following cycles in the object graph.
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    if let Some(dict) = value.as_stream_dict() {
        if matches!(dict.try_get_key(b"/XObject"), Ok(xobjects) if !xobjects.is_null()) {
            holders.push(dict.clone());
        }
        if let Some(entries) = dict.as_dictionary() {
            for (key, child) in entries {
                if key.as_slice() != b"/XObject" {
                    collect_direct_xobject_holders(&child, false, holders);
                }
            }
        }
        return;
    }

    if matches!(value.try_is_dictionary(), Ok(true)) {
        if matches!(value.try_get_key(b"/XObject"), Ok(xobjects) if !xobjects.is_null()) {
            holders.push(value.clone());
        }
        if let Some(entries) = value.as_dictionary() {
            for (key, child) in entries {
                if key.as_slice() != b"/XObject" {
                    collect_direct_xobject_holders(&child, false, holders);
                }
            }
        }
        return;
    }

    if let Some(items) = value.as_array() {
        for item in items {
            collect_direct_xobject_holders(&item, false, holders);
        }
    }
}

#[cfg(test)]
fn xobject_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_xobject_holders(object, true, &mut holders);
    }
    holders
}

#[cfg(test)]
fn exact_xobject_redirects(
    objects: &[ObjectHandle],
    subtype_name: &[u8],
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    duplicate_refs: &mut HashSet<ObjectRef>,
    duplicate_raw_bytes: &mut usize,
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();

    for object in objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        let Ok(Some(fingerprint)) =
            xobject_fingerprint(object, subtype_name, domain, ignored_dictionary_keys)
        else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            redirects.insert(object_ref, canonical_ref);
            if duplicate_refs.insert(object_ref) {
                *duplicate_raw_bytes += object.get_raw_stream_data()?.len();
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }

    Ok(redirects)
}

#[cfg(test)]
fn rewrite_xobject_resource_references<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    holders: Vec<ObjectHandle>,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<usize> {
    let mut references_canonicalized = 0_usize;
    for dict in holders {
        let Ok(xobjects) = dict.try_get_key(b"/XObject") else {
            continue;
        };
        if pdf.resolve(&xobjects).is_err() {
            continue;
        }
        let Some(entries) = xobjects.as_dictionary() else {
            continue;
        };
        for (name, target) in entries {
            let Some(target_ref) = target.object_ref() else {
                continue;
            };
            let Some(canonical_ref) = redirects.get(&target_ref).copied() else {
                continue;
            };
            xobjects.replace_key(&name, pdf.get_object_handle(canonical_ref))?;
            pdf.mark_object_handle_dirty(&xobjects)?;
            references_canonicalized += 1;
        }
    }
    Ok(references_canonicalized)
}

#[cfg(test)]
fn collect_direct_form_icon_holders(
    value: &ObjectHandle,
    include_indirect_root: bool,
    holders: &mut Vec<ObjectHandle>,
) {
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    let dictionary = if let Some(dict) = value.as_stream_dict() {
        dict
    } else if value.as_dictionary().is_some() {
        value.clone()
    } else {
        if let Some(items) = value.as_array() {
            for item in items {
                collect_direct_form_icon_holders(&item, false, holders);
            }
        }
        return;
    };

    let is_widget = matches!(
        dictionary.try_get_key(b"/Subtype"),
        Ok(subtype) if matches!(subtype.try_is_name_and_equals(b"Widget"), Ok(true))
    );
    let mut collected_mk = false;
    if is_widget
        && let Ok(mk) = dictionary.try_get_key(b"/MK")
        && !mk.is_null()
    {
        holders.push(mk);
        collected_mk = true;
    }

    if let Some(entries) = dictionary.as_dictionary() {
        for (key, child) in entries {
            if !(collected_mk && key.as_slice() == b"/MK") {
                collect_direct_form_icon_holders(&child, false, holders);
            }
        }
    }
}

#[cfg(test)]
fn form_icon_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_form_icon_holders(object, true, &mut holders);
    }
    holders
}

#[cfg(test)]
fn rewrite_form_icon_references<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    holders: Vec<ObjectHandle>,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<usize> {
    let mut references_canonicalized = 0_usize;
    for holder in holders {
        if pdf.resolve(&holder).is_err() || holder.as_dictionary().is_none() {
            continue;
        }
        for key in [b"/I".as_slice(), b"/RI".as_slice(), b"/IX".as_slice()] {
            let Ok(icon) = holder.try_get_key(key) else {
                continue;
            };
            let Some(icon_ref) = icon.object_ref() else {
                continue;
            };
            let Some(canonical_ref) = redirects.get(&icon_ref).copied() else {
                continue;
            };
            holder.replace_key(key, pdf.get_object_handle(canonical_ref))?;
            pdf.mark_object_handle_dirty(&holder)?;
            references_canonicalized += 1;
        }
    }
    Ok(references_canonicalized)
}

#[cfg(test)]
fn xobject_name_is_ignorable(version: &str) -> bool {
    let Some((major, minor)) = version.split_once('.') else {
        return false;
    };
    let (Ok(major), Ok(minor)) = (major.parse::<u32>(), minor.parse::<u32>()) else {
        return false;
    };
    major > 1 || (major == 1 && minor > 0)
}

#[cfg(test)]
pub(crate) fn canonicalize_image_xobjects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    // Image /Name is required only in PDF 1.0 and obsolescent afterwards.
    // Be deliberately conservative: only a parseable header version greater
    // than 1.0 enables omitting it from identity. A catalog /Version upgrade
    // on a 1.0 header may therefore miss a dedup opportunity, never broaden it.
    let ignored_dictionary_keys: &[&[u8]] = if xobject_name_is_ignorable(pdf.version()) {
        &[b"/Name"]
    } else {
        &[]
    };
    let objects = pdf.get_all_objects()?;
    let holders = xobject_holders(&objects);
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;
    let mut references_canonicalized = 0_usize;

    // First share exact Image XObjects when they are used as explicit or
    // soft masks. Parent image identity includes the /Mask or /SMask object
    // reference, so canonicalizing these dependency edges can make otherwise
    // byte- and dictionary-identical parent images exactly equal without
    // relaxing any image semantics.
    let mask_redirects = exact_xobject_redirects(
        &objects,
        b"Image",
        b"image-xobject",
        ignored_dictionary_keys,
        &mut duplicate_refs,
        &mut duplicate_raw_bytes,
    )?;
    for image in &objects {
        let Some(dict) = image.as_stream_dict() else {
            continue;
        };
        let Ok(subtype) = dict.try_get_key(b"/Subtype") else {
            continue;
        };
        if !matches!(subtype.try_is_name_and_equals(b"Image"), Ok(true)) {
            continue;
        }
        for key in [b"/Mask".as_slice(), b"/SMask".as_slice()] {
            let Ok(mask) = dict.try_get_key(key) else {
                continue;
            };
            let Some(mask_ref) = mask.object_ref() else {
                continue;
            };
            let Some(canonical_ref) = mask_redirects.get(&mask_ref).copied() else {
                continue;
            };
            dict.replace_key(key, pdf.get_object_handle(canonical_ref))?;
            pdf.mark_object_handle_dirty(&dict)?;
            references_canonicalized += 1;
        }
    }

    // Re-fingerprint after mask canonicalization so parent images whose only
    // distinction was duplicate mask-object identity can now be shared via
    // their ordinary resource-dictionary bindings.
    let redirects = exact_xobject_redirects(
        &objects,
        b"Image",
        b"image-xobject",
        ignored_dictionary_keys,
        &mut duplicate_refs,
        &mut duplicate_raw_bytes,
    )?;
    references_canonicalized += rewrite_xobject_resource_references(pdf, holders, &redirects)?;

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
fn dictionary_fingerprint(object: &ObjectHandle, domain: &[u8]) -> Result<Option<[u8; 32]>> {
    if !object.try_is_dictionary()? {
        return Ok(None);
    }
    let dictionary = object.unparse_resolved();
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update((dictionary.len() as u64).to_le_bytes());
    hasher.update(dictionary);
    Ok(Some(hasher.finalize().into()))
}

#[cfg(test)]
fn canonical_redirect_ref(
    mut object_ref: ObjectRef,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> ObjectRef {
    let mut seen = HashSet::new();
    while let Some(next) = redirects.get(&object_ref).copied() {
        if next == object_ref || !seen.insert(object_ref) {
            break;
        }
        object_ref = next;
    }
    object_ref
}

#[cfg(test)]
fn redirected_handle<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    value: ObjectHandle,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> ObjectHandle {
    value
        .object_ref()
        .map(|object_ref| pdf.get_object_handle(canonical_redirect_ref(object_ref, redirects)))
        .unwrap_or(value)
}

#[cfg(test)]
fn normalized_direct_resource_value<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    value: ObjectHandle,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> ObjectHandle {
    if value.object_ref().is_some() {
        return redirected_handle(pdf, value, redirects);
    }
    if let Some(entries) = value.as_dictionary() {
        return ObjectHandle::dictionary(
            entries
                .into_iter()
                .map(|(key, child)| (key, normalized_direct_resource_value(pdf, child, redirects)))
                .collect(),
        );
    }
    if let Some(items) = value.as_array() {
        return ObjectHandle::array(
            items
                .into_iter()
                .map(|child| normalized_direct_resource_value(pdf, child, redirects))
                .collect(),
        );
    }
    value
}

#[cfg(test)]
fn normalized_non_stream_resource_object<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    object: &ObjectHandle,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<Option<ObjectHandle>> {
    if object.as_stream_dict().is_some() || pdf.resolve(object).is_err() {
        return Ok(None);
    }
    if let Some(entries) = object.as_dictionary() {
        return Ok(Some(ObjectHandle::dictionary(
            entries
                .into_iter()
                .map(|(key, value)| (key, normalized_direct_resource_value(pdf, value, redirects)))
                .collect(),
        )));
    }
    if let Some(items) = object.as_array() {
        return Ok(Some(ObjectHandle::array(
            items
                .into_iter()
                .map(|value| normalized_direct_resource_value(pdf, value, redirects))
                .collect(),
        )));
    }
    Ok(None)
}

#[cfg(test)]
fn exact_non_stream_resource_redirects_for_dependencies<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
    dependency_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects = HashMap::new();

    for object in objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        let Some(normalized) =
            normalized_non_stream_resource_object(pdf, object, dependency_redirects)?
        else {
            continue;
        };
        let serialized = normalized.unparse_resolved();
        let mut hasher = Sha256::new();
        hasher.update(b"form-resource-exact-object");
        hasher.update((serialized.len() as u64).to_le_bytes());
        hasher.update(serialized);
        let fingerprint: [u8; 32] = hasher.finalize().into();
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != object_ref {
                redirects.insert(object_ref, canonical_ref);
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }

    Ok(redirects)
}

#[cfg(test)]
fn collect_form_resource_objects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    value: &ObjectHandle,
    seen: &mut HashSet<ObjectRef>,
    resource_objects: &mut Vec<ObjectHandle>,
) {
    let value = if let Some(object_ref) = value.object_ref() {
        if !seen.insert(object_ref) {
            return;
        }
        let handle = pdf.get_object_handle(object_ref);
        if pdf.resolve(&handle).is_err() {
            return;
        }
        handle
    } else {
        value.clone()
    };

    if value.as_stream_dict().is_none()
        && value.object_ref().is_some()
        && (value.as_dictionary().is_some() || value.as_array().is_some())
    {
        resource_objects.push(value.clone());
    }

    if let Some(dictionary) = value.as_stream_dict().or_else(|| {
        if value.as_dictionary().is_some() {
            Some(value.clone())
        } else {
            None
        }
    }) {
        if let Some(entries) = dictionary.as_dictionary() {
            for (_, child) in entries {
                collect_form_resource_objects(pdf, &child, seen, resource_objects);
            }
        }
    } else if let Some(items) = value.as_array() {
        for child in items {
            collect_form_resource_objects(pdf, &child, seen, resource_objects);
        }
    }
}

#[cfg(test)]
fn form_resource_objects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
) -> Vec<ObjectHandle> {
    let mut seen = HashSet::new();
    let mut resource_objects = Vec::new();
    for object in objects {
        let Some(dictionary) = object.as_stream_dict() else {
            continue;
        };
        let Ok(subtype) = dictionary.try_get_key(b"/Subtype") else {
            continue;
        };
        if !matches!(subtype.try_is_name_and_equals(b"Form"), Ok(true)) {
            continue;
        }
        let Ok(resources) = dictionary.try_get_key(b"/Resources") else {
            continue;
        };
        collect_form_resource_objects(pdf, &resources, &mut seen, &mut resource_objects);
    }
    resource_objects
}

#[cfg(test)]
fn exact_non_stream_resource_redirects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    resource_objects: &[ObjectHandle],
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    // Exact resource containers can themselves refer to duplicated indirect
    // containers. Iterate until those identity-only differences stop exposing
    // new exact matches. Restrict the fixed point to objects reachable from
    // Form resources; scanning unrelated page/catalog structure can be orders
    // of magnitude more expensive and cannot affect a Form fingerprint.
    let mut redirects = HashMap::new();
    for _ in 0..=resource_objects.len() {
        let next = exact_non_stream_resource_redirects_for_dependencies(
            pdf,
            resource_objects,
            &redirects,
        )?;
        if next == redirects {
            return Ok(next);
        }
        redirects = next;
    }
    Ok(redirects)
}

#[cfg(test)]
fn normalized_dictionary_with_redirects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    object: &ObjectHandle,
    redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<Option<ObjectHandle>> {
    let Some(normalized) = normalized_non_stream_resource_object(pdf, object, redirects)? else {
        return Ok(None);
    };
    if normalized.as_dictionary().is_none() {
        return Ok(None);
    }
    Ok(Some(normalized))
}

#[cfg(test)]
fn exact_form_font_redirects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects = HashMap::new();

    for object in objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        if object.as_stream_dict().is_some() || !matches!(object.try_is_dictionary(), Ok(true)) {
            continue;
        }
        let Ok(object_type) = object.try_get_key(b"/Type") else {
            continue;
        };
        if !matches!(object_type.try_is_name_and_equals(b"Font"), Ok(true)) {
            continue;
        }
        let Some(normalized) = normalized_dictionary_with_redirects(pdf, object, exact_redirects)?
        else {
            continue;
        };
        let Some(fingerprint) =
            dictionary_fingerprint(&normalized, b"form-font-resource-dictionary")?
        else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != object_ref {
                redirects.insert(object_ref, canonical_ref);
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }

    Ok(redirects)
}

#[cfg(test)]
fn virtual_form_image_redirects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    let ignored: &[&[u8]] = if xobject_name_is_ignorable(pdf.version()) {
        &[b"/Name"]
    } else {
        &[]
    };
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects = HashMap::new();

    for object in objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        let Some(dict) = object.as_stream_dict() else {
            continue;
        };
        let Ok(subtype) = dict.try_get_key(b"/Subtype") else {
            continue;
        };
        if !matches!(subtype.try_is_name_and_equals(b"Image"), Ok(true)) {
            continue;
        }
        let raw = object.get_raw_stream_data()?;
        let Some(entries) = dict.as_dictionary() else {
            continue;
        };
        let dictionary = ObjectHandle::dictionary(
            entries
                .into_iter()
                .filter(|(key, _)| {
                    key.as_slice() != b"/Length" && !ignored.contains(&key.as_slice())
                })
                .map(|(key, value)| (key, redirected_handle(pdf, value, exact_redirects)))
                .collect(),
        )
        .unparse_resolved();
        let mut hasher = Sha256::new();
        hasher.update(b"form-resource-image");
        hasher.update((raw.len() as u64).to_le_bytes());
        hasher.update(raw.as_ref());
        hasher.update((dictionary.len() as u64).to_le_bytes());
        hasher.update(dictionary);
        let fingerprint: [u8; 32] = hasher.finalize().into();
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != object_ref {
                redirects.insert(object_ref, canonical_ref);
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }

    Ok(redirects)
}

#[cfg(test)]
fn normalized_named_form_resource<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    value: &ObjectHandle,
    resource_kind: &[u8],
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
    font_redirects: &HashMap<ObjectRef, ObjectRef>,
    image_redirects: &HashMap<ObjectRef, ObjectRef>,
    form_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> ObjectHandle {
    let resource = if let Some(object_ref) = value.object_ref() {
        let handle = pdf.get_object_handle(object_ref);
        if pdf.resolve(&handle).is_err() {
            return value.clone();
        }
        handle
    } else {
        value.clone()
    };
    let Some(entries) = resource.as_dictionary() else {
        return value.clone();
    };

    let mut normalized = Vec::with_capacity(entries.len());
    for (name, target) in entries {
        let target = match resource_kind {
            b"/Font" => redirected_handle(pdf, target, font_redirects),
            b"/XObject" => {
                if let Some(target_ref) = target.object_ref() {
                    if form_redirects.contains_key(&target_ref) {
                        redirected_handle(pdf, target, form_redirects)
                    } else {
                        redirected_handle(pdf, target, image_redirects)
                    }
                } else {
                    target
                }
            }
            b"/ColorSpace" | b"/ExtGState" | b"/Properties" => {
                redirected_handle(pdf, target, exact_redirects)
            }
            _ => target,
        };
        normalized.push((name, target));
    }
    ObjectHandle::dictionary(normalized)
}

#[cfg(test)]
fn normalized_form_resources<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    resources: &ObjectHandle,
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
    font_redirects: &HashMap<ObjectRef, ObjectRef>,
    image_redirects: &HashMap<ObjectRef, ObjectRef>,
    form_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> ObjectHandle {
    let resources = if let Some(object_ref) = resources.object_ref() {
        let handle = pdf.get_object_handle(object_ref);
        if pdf.resolve(&handle).is_err() {
            return resources.clone();
        }
        handle
    } else {
        resources.clone()
    };
    let Some(entries) = resources.as_dictionary() else {
        return resources;
    };

    let mut normalized = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        let value = match key.as_slice() {
            b"/Font" | b"/XObject" | b"/ColorSpace" | b"/ExtGState" | b"/Properties" => {
                normalized_named_form_resource(
                    pdf,
                    &value,
                    &key,
                    exact_redirects,
                    font_redirects,
                    image_redirects,
                    form_redirects,
                )
            }
            b"/ProcSet" => redirected_handle(pdf, value, exact_redirects),
            _ => value,
        };
        normalized.push((key, value));
    }
    ObjectHandle::dictionary(normalized)
}

#[cfg(test)]
fn form_fingerprint<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    object: &ObjectHandle,
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
    font_redirects: &HashMap<ObjectRef, ObjectRef>,
    image_redirects: &HashMap<ObjectRef, ObjectRef>,
    form_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<Option<[u8; 32]>> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok(None);
    };
    let subtype = dict.try_get_key(b"/Subtype")?;
    if !subtype.try_is_name_and_equals(b"Form")? {
        return Ok(None);
    }
    let raw = object.get_raw_stream_data()?;
    let Some(entries) = dict.as_dictionary() else {
        return Ok(None);
    };
    let mut normalized = Vec::with_capacity(entries.len());
    for (key, value) in entries {
        if key.as_slice() == b"/Length" || ignored_dictionary_keys.contains(&key.as_slice()) {
            continue;
        }
        if key.as_slice() == b"/Resources" {
            normalized.push((
                key,
                normalized_form_resources(
                    pdf,
                    &value,
                    exact_redirects,
                    font_redirects,
                    image_redirects,
                    form_redirects,
                ),
            ));
        } else {
            normalized.push((key, value));
        }
    }
    let dictionary = ObjectHandle::dictionary(normalized).unparse_resolved();
    let domain = b"form-xobject";
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update((raw.len() as u64).to_le_bytes());
    hasher.update(raw.as_ref());
    hasher.update((dictionary.len() as u64).to_le_bytes());
    hasher.update(dictionary);
    Ok(Some(hasher.finalize().into()))
}

#[cfg(test)]
fn form_redirects_for_dependencies<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
    font_redirects: &HashMap<ObjectRef, ObjectRef>,
    image_redirects: &HashMap<ObjectRef, ObjectRef>,
    dependency_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects = HashMap::new();
    for object in objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        let Ok(Some(fingerprint)) = form_fingerprint(
            pdf,
            object,
            ignored_dictionary_keys,
            exact_redirects,
            font_redirects,
            image_redirects,
            dependency_redirects,
        ) else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != object_ref {
                redirects.insert(object_ref, canonical_ref);
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }
    Ok(redirects)
}

#[cfg(test)]
fn fixed_point_form_redirects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<ObjectRef, ObjectRef>,
    font_redirects: &HashMap<ObjectRef, ObjectRef>,
    image_redirects: &HashMap<ObjectRef, ObjectRef>,
) -> Result<HashMap<ObjectRef, ObjectRef>> {
    let form_count = objects
        .iter()
        .filter(|object| {
            let Some(dict) = object.as_stream_dict() else {
                return false;
            };
            matches!(dict.try_get_key(b"/Subtype"), Ok(subtype) if matches!(subtype.try_is_name_and_equals(b"Form"), Ok(true)))
        })
        .count();
    let mut redirects = HashMap::new();
    for _ in 0..=form_count {
        let next = form_redirects_for_dependencies(
            pdf,
            objects,
            ignored_dictionary_keys,
            exact_redirects,
            font_redirects,
            image_redirects,
            &redirects,
        )?;
        if next == redirects {
            return Ok(next);
        }
        redirects = next;
    }
    Ok(redirects)
}

#[cfg(test)]
pub(crate) fn canonicalize_form_xobjects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let holders = xobject_holders(&objects);
    let icon_holders = form_icon_holders(&objects);
    let resource_objects = form_resource_objects(pdf, &objects);
    let exact_redirects = exact_non_stream_resource_redirects(pdf, &resource_objects)?;
    let font_redirects = exact_form_font_redirects(pdf, &objects, &exact_redirects)?;
    let image_redirects = virtual_form_image_redirects(pdf, &objects, &exact_redirects)?;

    // Form /Name has the same PDF 1.0-only requirement and obsolescent status
    // as Image /Name. Keep every rendering-relevant Form key exact.
    let ignored: &[&[u8]] = if xobject_name_is_ignorable(pdf.version()) {
        &[b"/Name"]
    } else {
        &[]
    };

    // Dependency redirects are virtual and are never written back on their
    // own. They only allow Forms whose complete rendering resource graphs are
    // exact duplicates to share one parent Form object. Iterate because a Form
    // may depend on another Form that becomes canonical in an earlier round.
    let redirects = fixed_point_form_redirects(
        pdf,
        &objects,
        ignored,
        &exact_redirects,
        &font_redirects,
        &image_redirects,
    )?;

    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;
    for object in &objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        if redirects.contains_key(&object_ref) && duplicate_refs.insert(object_ref) {
            duplicate_raw_bytes += object.get_raw_stream_data()?.len();
        }
    }
    let mut references_canonicalized =
        rewrite_xobject_resource_references(pdf, holders, &redirects)?;
    references_canonicalized += rewrite_form_icon_references(pdf, icon_holders, &redirects)?;

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
fn collect_direct_appearance_dictionaries(
    value: &ObjectHandle,
    include_indirect_root: bool,
    appearances: &mut Vec<ObjectHandle>,
) {
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    let dictionary = if let Some(dict) = value.as_stream_dict() {
        dict
    } else if matches!(value.try_is_dictionary(), Ok(true)) {
        value.clone()
    } else {
        if let Some(items) = value.as_array() {
            for item in items {
                collect_direct_appearance_dictionaries(&item, false, appearances);
            }
        }
        return;
    };

    if matches!(dictionary.try_get_key(b"/AP"), Ok(ap) if !ap.is_null())
        && let Ok(ap) = dictionary.try_get_key(b"/AP")
    {
        appearances.push(ap);
    }

    if let Some(entries) = dictionary.as_dictionary() {
        for (key, child) in entries {
            if key.as_slice() != b"/AP" {
                collect_direct_appearance_dictionaries(&child, false, appearances);
            }
        }
    }
}

#[cfg(test)]
fn appearance_holders<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    objects: &[ObjectHandle],
) -> Vec<ObjectHandle> {
    let mut appearances = Vec::new();
    for object in objects {
        collect_direct_appearance_dictionaries(object, true, &mut appearances);
    }

    let mut holders = Vec::new();
    for appearance in appearances {
        if pdf.resolve(&appearance).is_err() {
            continue;
        }
        let Some(entries) = appearance.as_dictionary() else {
            continue;
        };
        holders.push(appearance.clone());
        for (key, state_or_stream) in entries {
            if !matches!(key.as_slice(), b"/N" | b"/R" | b"/D") {
                continue;
            }
            if pdf.resolve(&state_or_stream).is_err() || state_or_stream.as_stream_dict().is_some()
            {
                continue;
            }
            if state_or_stream.as_dictionary().is_some() {
                holders.push(state_or_stream);
            }
        }
    }
    holders
}

#[cfg(test)]
pub(crate) fn canonicalize_appearance_streams<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let holders = appearance_holders(pdf, &objects);
    if holders.is_empty() {
        return Ok(TargetedDedupStats::default());
    }

    // Appearance streams are Forms too, but they live under /AP rather than
    // /Resources /XObject. Reuse the same exact virtual dependency graph as
    // Form dedup while retaining appearance's stricter dictionary semantics:
    // unlike general Form XObjects, /Name is not ignored here.
    let resource_objects = form_resource_objects(pdf, &objects);
    let exact_redirects = exact_non_stream_resource_redirects(pdf, &resource_objects)?;
    let font_redirects = exact_form_font_redirects(pdf, &objects, &exact_redirects)?;
    let image_redirects = virtual_form_image_redirects(pdf, &objects, &exact_redirects)?;
    let dependency_redirects = fixed_point_form_redirects(
        pdf,
        &objects,
        &[],
        &exact_redirects,
        &font_redirects,
        &image_redirects,
    )?;

    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    for dictionary in &holders {
        let Some(entries) = dictionary.as_dictionary() else {
            continue;
        };
        for (_, appearance) in entries {
            let Some(appearance_ref) = appearance.object_ref() else {
                continue;
            };
            if pdf.resolve(&appearance).is_err() {
                continue;
            }
            let Ok(Some(fingerprint)) = form_fingerprint(
                pdf,
                &appearance,
                &[],
                &exact_redirects,
                &font_redirects,
                &image_redirects,
                &dependency_redirects,
            ) else {
                continue;
            };
            if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
                if canonical_ref != appearance_ref {
                    redirects.insert(appearance_ref, canonical_ref);
                    if duplicate_refs.insert(appearance_ref) {
                        duplicate_raw_bytes += appearance.get_raw_stream_data()?.len();
                    }
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, appearance_ref);
            }
        }
    }

    let mut references_canonicalized = 0_usize;
    for dictionary in holders {
        let Some(entries) = dictionary.as_dictionary() else {
            continue;
        };
        for (name, appearance) in entries {
            let Some(appearance_ref) = appearance.object_ref() else {
                continue;
            };
            let Some(canonical_ref) = redirects.get(&appearance_ref).copied() else {
                continue;
            };
            dictionary.replace_key(&name, pdf.get_object_handle(canonical_ref))?;
            pdf.mark_object_handle_dirty(&dictionary)?;
            references_canonicalized += 1;
        }
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
pub(crate) fn canonicalize_page_contents<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    #[derive(Clone)]
    struct ContentSlot {
        holder: ObjectHandle,
        index: Option<usize>,
        stream_ref: ObjectRef,
    }

    // Keep canonical page-tree discovery as the primary source so damaged
    // live pages that omit `/Type /Page` retain the behavior of the original
    // pass. Then add detached `/Type /Page` dictionaries that remain
    // reachable through outlines, article threads, private structures, etc.
    // Their content streams have the same invocation semantics: resources
    // stay on the invoking page dictionary, not on the shared stream object.
    let page_refs = flpdf::pages::page_refs(pdf)?;
    let mut pages = Vec::new();
    let mut seen_pages = HashSet::new();
    for page_ref in page_refs {
        seen_pages.insert(page_ref);
        pages.push(pdf.get_object_handle(page_ref));
    }
    for object in pdf.get_all_objects()? {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        if seen_pages.contains(&object_ref) {
            continue;
        }
        if !object.try_is_dictionary()? {
            continue;
        }
        let Ok(object_type) = object.try_get_key(b"/Type") else {
            continue;
        };
        if object_type.as_name() != Some(b"Page".to_vec()) {
            continue;
        }
        seen_pages.insert(object_ref);
        pages.push(object);
    }

    let mut slots = Vec::new();
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    for page in pages {
        if pdf.resolve(&page).is_err() {
            continue;
        }
        let Ok(contents) = page.try_get_key(b"/Contents") else {
            continue;
        };
        if contents.is_null() || pdf.resolve(&contents).is_err() {
            continue;
        }

        let mut candidates: Vec<(ObjectHandle, Option<usize>, ObjectHandle)> = Vec::new();
        if contents.as_stream_dict().is_some() {
            candidates.push((page.clone(), None, contents));
        } else if let Some(items) = contents.as_array() {
            for (index, item) in items.into_iter().enumerate() {
                if pdf.resolve(&item).is_ok() && item.as_stream_dict().is_some() {
                    candidates.push((contents.clone(), Some(index), item));
                }
            }
        }

        for (holder, index, stream) in candidates {
            let Some(stream_ref) = stream.object_ref() else {
                continue;
            };
            let Ok(Some(fingerprint)) = stream_fingerprint(&stream, b"page-content") else {
                continue;
            };
            slots.push(ContentSlot {
                holder,
                index,
                stream_ref,
            });
            if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
                if canonical_ref != stream_ref {
                    redirects.insert(stream_ref, canonical_ref);
                    if duplicate_refs.insert(stream_ref) {
                        duplicate_raw_bytes += stream.get_raw_stream_data()?.len();
                    }
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, stream_ref);
            }
        }
    }

    let mut references_canonicalized = 0_usize;
    for slot in slots {
        let Some(canonical_ref) = redirects.get(&slot.stream_ref).copied() else {
            continue;
        };
        let canonical = pdf.get_object_handle(canonical_ref);
        if let Some(index) = slot.index {
            slot.holder.set_array_item(index, canonical)?;
        } else {
            slot.holder.replace_key(b"/Contents", canonical)?;
        }
        pdf.mark_object_handle_dirty(&slot.holder)?;
        references_canonicalized += 1;
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
fn collect_direct_type3_charprocs(
    value: &ObjectHandle,
    include_indirect_root: bool,
    charprocs: &mut Vec<ObjectHandle>,
) {
    // Every indirect object is visited independently by `get_all_objects`.
    // Recurse only through direct descendants so nested direct font
    // dictionaries are found without following cycles in the object graph.
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    let dictionary = if let Some(dict) = value.as_stream_dict() {
        dict
    } else if matches!(value.try_is_dictionary(), Ok(true)) {
        value.clone()
    } else {
        if let Some(items) = value.as_array() {
            for item in items {
                collect_direct_type3_charprocs(&item, false, charprocs);
            }
        }
        return;
    };

    if matches!(dictionary.try_get_key(b"/Subtype"), Ok(subtype) if matches!(subtype.try_is_name_and_equals(b"Type3"), Ok(true)))
        && matches!(dictionary.try_get_key(b"/CharProcs"), Ok(value) if !value.is_null())
        && let Ok(value) = dictionary.try_get_key(b"/CharProcs")
    {
        charprocs.push(value);
    }

    if let Some(entries) = dictionary.as_dictionary() {
        for (key, child) in entries {
            if key.as_slice() != b"/CharProcs" {
                collect_direct_type3_charprocs(&child, false, charprocs);
            }
        }
    }
}

#[cfg(test)]
pub(crate) fn canonicalize_type3_charprocs<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let mut charprocs = Vec::new();
    for object in &objects {
        collect_direct_type3_charprocs(object, true, &mut charprocs);
    }

    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    for dictionary in &charprocs {
        if pdf.resolve(dictionary).is_err() {
            continue;
        }
        let Some(entries) = dictionary.as_dictionary() else {
            continue;
        };
        for (_, glyph) in entries {
            let Some(glyph_ref) = glyph.object_ref() else {
                continue;
            };
            if pdf.resolve(&glyph).is_err() {
                continue;
            }
            let Ok(Some(fingerprint)) = stream_fingerprint(&glyph, b"type3-charproc") else {
                continue;
            };
            if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
                if canonical_ref != glyph_ref {
                    redirects.insert(glyph_ref, canonical_ref);
                    if duplicate_refs.insert(glyph_ref) {
                        duplicate_raw_bytes += glyph.get_raw_stream_data()?.len();
                    }
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, glyph_ref);
            }
        }
    }

    let mut references_canonicalized = 0_usize;
    for dictionary in charprocs {
        if pdf.resolve(&dictionary).is_err() {
            continue;
        }
        let Some(entries) = dictionary.as_dictionary() else {
            continue;
        };
        for (name, glyph) in entries {
            let Some(glyph_ref) = glyph.object_ref() else {
                continue;
            };
            let Some(canonical_ref) = redirects.get(&glyph_ref).copied() else {
                continue;
            };
            dictionary.replace_key(&name, pdf.get_object_handle(canonical_ref))?;
            pdf.mark_object_handle_dirty(&dictionary)?;
            references_canonicalized += 1;
        }
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
fn collect_direct_to_unicode_holders(
    value: &ObjectHandle,
    include_indirect_root: bool,
    holders: &mut Vec<ObjectHandle>,
) {
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }

    if let Some(dict) = value.as_stream_dict() {
        if matches!(dict.try_get_key(b"/ToUnicode"), Ok(cmap) if !cmap.is_null()) {
            holders.push(dict.clone());
        }
        if let Some(entries) = dict.as_dictionary() {
            for (key, child) in entries {
                if key.as_slice() != b"/ToUnicode" {
                    collect_direct_to_unicode_holders(&child, false, holders);
                }
            }
        }
        return;
    }

    if matches!(value.try_is_dictionary(), Ok(true)) {
        if matches!(value.try_get_key(b"/ToUnicode"), Ok(cmap) if !cmap.is_null()) {
            holders.push(value.clone());
        }
        if let Some(entries) = value.as_dictionary() {
            for (key, child) in entries {
                if key.as_slice() != b"/ToUnicode" {
                    collect_direct_to_unicode_holders(&child, false, holders);
                }
            }
        }
        return;
    }

    if let Some(items) = value.as_array() {
        for item in items {
            collect_direct_to_unicode_holders(&item, false, holders);
        }
    }
}

#[cfg(test)]
fn to_unicode_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_to_unicode_holders(object, true, &mut holders);
    }
    holders
}

#[cfg(test)]
pub(crate) fn canonicalize_to_unicode_cmaps<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let holders = to_unicode_holders(&objects);
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    for dict in &holders {
        let Ok(cmap) = dict.try_get_key(b"/ToUnicode") else {
            continue;
        };
        let Some(cmap_ref) = cmap.object_ref() else {
            continue;
        };
        if pdf.resolve(&cmap).is_err() {
            continue;
        }
        let Ok(Some(fingerprint)) = stream_fingerprint(&cmap, b"to-unicode") else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical_ref != cmap_ref {
                redirects.insert(cmap_ref, canonical_ref);
                if duplicate_refs.insert(cmap_ref) {
                    duplicate_raw_bytes += cmap.get_raw_stream_data()?.len();
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, cmap_ref);
        }
    }

    let mut references_canonicalized = 0_usize;
    for dict in holders {
        let Ok(cmap) = dict.try_get_key(b"/ToUnicode") else {
            continue;
        };
        let Some(cmap_ref) = cmap.object_ref() else {
            continue;
        };
        let Some(canonical_ref) = redirects.get(&cmap_ref).copied() else {
            continue;
        };
        dict.replace_key(b"/ToUnicode", pdf.get_object_handle(canonical_ref))?;
        pdf.mark_object_handle_dirty(&dict)?;
        references_canonicalized += 1;
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
const FONT_FILE_KEYS: [&[u8]; 3] = [b"/FontFile", b"/FontFile2", b"/FontFile3"];

#[cfg(test)]
fn font_program_fingerprint(object: &ObjectHandle, domain: &[u8]) -> Result<Option<[u8; 32]>> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok(None);
    };
    let raw = object.get_raw_stream_data()?;
    let Some(entries) = dict.as_dictionary() else {
        return Ok(None);
    };
    let dictionary = ObjectHandle::dictionary(
        entries
            .into_iter()
            .filter(|(key, _)| key.as_slice() != b"/Length")
            .map(|(key, value)| {
                let value = if matches!(key.as_slice(), b"/Length1" | b"/Length2" | b"/Length3") {
                    value
                        .as_integer()
                        .map(ObjectHandle::integer)
                        .unwrap_or(value)
                } else {
                    value
                };
                (key, value)
            })
            .collect(),
    )
    .unparse_resolved();
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_le_bytes());
    hasher.update(domain);
    hasher.update((raw.len() as u64).to_le_bytes());
    hasher.update(raw.as_ref());
    hasher.update((dictionary.len() as u64).to_le_bytes());
    hasher.update(dictionary);
    Ok(Some(hasher.finalize().into()))
}

#[cfg(test)]
pub(crate) fn canonicalize_font_program_streams<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<(Vec<u8>, ObjectRef), ObjectRef> = HashMap::new();
    let mut duplicate_refs = std::collections::HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;

    for object in &objects {
        let dict = if let Some(dict) = object.as_stream_dict() {
            dict
        } else if object.try_is_dictionary()? {
            object.clone()
        } else {
            continue;
        };
        for key in FONT_FILE_KEYS {
            let font_program = dict.try_get_key(key)?;
            let Some(font_ref) = font_program.object_ref() else {
                continue;
            };
            if pdf.resolve(&font_program).is_err() {
                continue;
            }
            let Ok(Some(fingerprint)) = font_program_fingerprint(&font_program, key) else {
                continue;
            };
            if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
                redirects.insert((key.to_vec(), font_ref), canonical_ref);
                if duplicate_refs.insert(font_ref) {
                    duplicate_raw_bytes += font_program.get_raw_stream_data()?.len();
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, font_ref);
            }
        }
    }

    let mut references_canonicalized = 0_usize;
    for object in &objects {
        let dict = if let Some(dict) = object.as_stream_dict() {
            dict
        } else if object.try_is_dictionary()? {
            object.clone()
        } else {
            continue;
        };
        for key in FONT_FILE_KEYS {
            let font_program = dict.try_get_key(key)?;
            let Some(font_ref) = font_program.object_ref() else {
                continue;
            };
            let Some(canonical_ref) = redirects.get(&(key.to_vec(), font_ref)).copied() else {
                continue;
            };
            dict.replace_key(key, pdf.get_object_handle(canonical_ref))?;
            pdf.mark_object_handle_dirty(&dict)?;
            references_canonicalized += 1;
        }
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

const HAYRO_FONT_FILE_KEYS: [&[u8]; 3] = [b"FontFile", b"FontFile2", b"FontFile3"];

#[derive(Debug, Clone, PartialEq, Eq)]
struct HayroFontProgramHolder {
    holder: CowObjectHandle,
    key: Vec<u8>,
    program: CowObjectHandle,
}

fn inspect_hayro_font_program_holders(
    holder: CowObjectHandle,
    dictionary: &hayro_syntax::object::Dict<'_>,
    holders: &mut Vec<HayroFontProgramHolder>,
) {
    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = dictionary.get_ref(key) {
            holders.push(HayroFontProgramHolder {
                holder,
                key: key.to_vec(),
                program: CowObjectHandle::Existing(program.into()),
            });
        }
    }
}

fn inspect_owned_font_program_holders(
    holder: CowObjectHandle,
    dictionary: &OwnedDictionary,
    holders: &mut Vec<HayroFontProgramHolder>,
) {
    for key in HAYRO_FONT_FILE_KEYS {
        let Some(OwnedObject::Reference(program)) = dictionary.get(key) else {
            continue;
        };
        holders.push(HayroFontProgramHolder {
            holder,
            key: key.to_vec(),
            program: *program,
        });
    }
}

fn hayro_font_program_holders(document: &EditDocument) -> Result<Vec<HayroFontProgramHolder>> {
    let mut holders = Vec::new();
    document.walk_output_objects(|handle, object| {
        match object {
            CurrentObject::Source(object) => match &object {
                HayroObject::Dict(dictionary) => {
                    inspect_hayro_font_program_holders(handle, dictionary, &mut holders);
                }
                HayroObject::Stream(stream) => {
                    inspect_hayro_font_program_holders(handle, stream.dict(), &mut holders);
                }
                _ => {}
            },
            CurrentObject::Owned(object) => {
                if let Some(dictionary) = object.as_dictionary() {
                    inspect_owned_font_program_holders(handle, dictionary, &mut holders);
                }
            }
        }
        Ok(())
    })?;
    Ok(holders)
}

fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_cow_handle(hasher: &mut Sha256, handle: CowObjectHandle) {
    match handle {
        CowObjectHandle::Existing(id) => {
            hasher.update([0x70]);
            hasher.update(id.number().to_le_bytes());
            hasher.update(id.generation().to_le_bytes());
        }
        CowObjectHandle::New(id) => {
            hasher.update([0x71]);
            hasher.update((id.index() as u64).to_le_bytes());
        }
    }
}

fn hash_owned_object(hasher: &mut Sha256, object: &OwnedObject) -> Result<()> {
    match object {
        OwnedObject::Null => hasher.update([0x00]),
        OwnedObject::Boolean(value) => hasher.update([0x01, u8::from(*value)]),
        OwnedObject::Integer(value) => {
            hasher.update([0x02]);
            hasher.update(value.to_le_bytes());
        }
        OwnedObject::Real(value) => {
            hasher.update([0x03]);
            hasher.update(value.to_bits().to_le_bytes());
        }
        OwnedObject::Name(value) => {
            hasher.update([0x04]);
            hash_len_prefixed(hasher, value);
        }
        OwnedObject::String(value) => {
            hasher.update([0x05]);
            hash_len_prefixed(hasher, value);
        }
        OwnedObject::Reference(handle) => hash_cow_handle(hasher, *handle),
        OwnedObject::Array(values) => {
            hasher.update([0x06]);
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_owned_object(hasher, value)?;
            }
        }
        OwnedObject::Dictionary(dictionary) => {
            hasher.update([0x07]);
            hasher.update((dictionary.len() as u64).to_le_bytes());
            for (key, value) in dictionary {
                hash_len_prefixed(hasher, key);
                hash_owned_object(hasher, value)?;
            }
        }
        OwnedObject::Stream { dictionary, data } => {
            hasher.update([0x08]);
            hasher.update((dictionary.len() as u64).to_le_bytes());
            for (key, value) in dictionary {
                hash_len_prefixed(hasher, key);
                hash_owned_object(hasher, value)?;
            }
            match data {
                StreamData::Source(id) => {
                    hasher.update([0x80]);
                    hasher.update(id.number().to_le_bytes());
                    hasher.update(id.generation().to_le_bytes());
                }
                StreamData::Owned(bytes) => {
                    hasher.update([0x81]);
                    hash_len_prefixed(hasher, bytes);
                }
            }
        }
    }
    Ok(())
}

fn hayro_stream_fingerprint_ignoring(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object(stream)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_len_prefixed(&mut hasher, raw.as_ref());

    let is_semantic = |key: &&Vec<u8>| {
        key.as_slice() != b"Length" && !ignored_dictionary_keys.contains(&key.as_slice())
    };
    let semantic_entry_count = dictionary.keys().filter(is_semantic).count();
    hasher.update((semantic_entry_count as u64).to_le_bytes());
    for (key, value) in dictionary.iter().filter(|(key, _)| is_semantic(key)) {
        hash_len_prefixed(&mut hasher, key);
        hash_owned_object(&mut hasher, value)?;
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn hayro_stream_fingerprint(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
) -> Result<Option<([u8; 32], usize)>> {
    hayro_stream_fingerprint_ignoring(document, stream, domain, &[])
}

fn hayro_font_program_fingerprint(
    document: &EditDocument,
    program: CowObjectHandle,
    key: &[u8],
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object(program)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    let domain = [b"/".as_slice(), key].concat();
    hash_len_prefixed(&mut hasher, &domain);
    hash_len_prefixed(&mut hasher, raw.as_ref());

    hasher.update((dictionary.len() as u64).to_le_bytes());
    for (dict_key, value) in &dictionary {
        hash_len_prefixed(&mut hasher, dict_key);
        if matches!(dict_key.as_slice(), b"Length1" | b"Length2" | b"Length3")
            && let Some(OwnedObject::Integer(integer)) = document.resolve_owned_value(value)?
        {
            hash_owned_object(&mut hasher, &OwnedObject::Integer(integer))?;
        } else {
            hash_owned_object(&mut hasher, value)?;
        }
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn rewrite_font_program_holder(
    document: &mut EditDocument,
    holder: &HayroFontProgramHolder,
    canonical: CowObjectHandle,
) -> Result<bool> {
    let object = match holder.holder {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    let Some(dictionary) = object.as_dictionary_mut() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(current)) = dictionary.get(holder.key.as_slice()) else {
        return Ok(false);
    };
    if *current != holder.program {
        return Ok(false);
    }
    dictionary.insert(holder.key.clone(), OwnedObject::Reference(canonical));
    Ok(true)
}

pub(crate) fn canonicalize_font_program_streams_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = hayro_font_program_holders(document)?;
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::<(Vec<u8>, CowObjectHandle), CowObjectHandle>::new();
    let mut duplicate_refs = HashSet::<CowObjectHandle>::new();
    let mut duplicate_raw_bytes = 0_usize;

    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_font_program_fingerprint(document, holder.program, &holder.key)?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != holder.program {
                redirects.insert((holder.key.clone(), holder.program), canonical);
                if duplicate_refs.insert(holder.program) {
                    duplicate_raw_bytes += raw_bytes;
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, holder.program);
        }
    }

    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = redirects
            .get(&(holder.key.clone(), holder.program))
            .copied()
        else {
            continue;
        };
        if rewrite_font_program_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum DirectPathStep {
    DictKey(Vec<u8>),
    ArrayIndex(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectReferenceHolder {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
    key: Vec<u8>,
    target: CowObjectHandle,
}

fn inspect_hayro_direct_reference_holders(
    root: CowObjectHandle,
    object: &HayroObject<'_>,
    key: &[u8],
    path: &mut Vec<DirectPathStep>,
    holders: &mut Vec<DirectReferenceHolder>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            if let Some(target) = dictionary.get_ref(key) {
                holders.push(DirectReferenceHolder {
                    root,
                    path: path.clone(),
                    key: key.to_vec(),
                    target: CowObjectHandle::Existing(target.into()),
                });
            }
            for (name, value) in dictionary.entries() {
                if name.as_ref() == key {
                    continue;
                }
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
                inspect_hayro_direct_reference_holders(root, &value, key, path, holders);
                path.pop();
            }
        }
        HayroObject::Stream(stream) => {
            let dictionary = stream.dict();
            if let Some(target) = dictionary.get_ref(key) {
                holders.push(DirectReferenceHolder {
                    root,
                    path: path.clone(),
                    key: key.to_vec(),
                    target: CowObjectHandle::Existing(target.into()),
                });
            }
            for (name, value) in dictionary.entries() {
                if name.as_ref() == key {
                    continue;
                }
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
                inspect_hayro_direct_reference_holders(root, &value, key, path, holders);
                path.pop();
            }
        }
        HayroObject::Array(array) => {
            for (index, value) in array.raw_iter().enumerate() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_hayro_direct_reference_holders(root, &value, key, path, holders);
                path.pop();
            }
        }
        HayroObject::Null(_)
        | HayroObject::Boolean(_)
        | HayroObject::Number(_)
        | HayroObject::String(_)
        | HayroObject::Name(_) => {}
    }
}

fn inspect_owned_direct_reference_holders(
    root: CowObjectHandle,
    object: &OwnedObject,
    key: &[u8],
    path: &mut Vec<DirectPathStep>,
    holders: &mut Vec<DirectReferenceHolder>,
) {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if let Some(OwnedObject::Reference(target)) = dictionary.get(key) {
                holders.push(DirectReferenceHolder {
                    root,
                    path: path.clone(),
                    key: key.to_vec(),
                    target: *target,
                });
            }
            for (name, value) in dictionary {
                if name.as_slice() == key || matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::DictKey(name.clone()));
                inspect_owned_direct_reference_holders(root, value, key, path, holders);
                path.pop();
            }
        }
        OwnedObject::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_owned_direct_reference_holders(root, value, key, path, holders);
                path.pop();
            }
        }
        OwnedObject::Reference(_)
        | OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
}

fn hayro_direct_reference_holders(
    document: &EditDocument,
    key: &[u8],
) -> Result<Vec<DirectReferenceHolder>> {
    let mut holders = Vec::new();
    document.walk_output_objects(|handle, object| {
        match object {
            CurrentObject::Source(object) => {
                inspect_hayro_direct_reference_holders(
                    handle,
                    &object,
                    key,
                    &mut Vec::new(),
                    &mut holders,
                );
            }
            CurrentObject::Owned(object) => {
                inspect_owned_direct_reference_holders(
                    handle,
                    object,
                    key,
                    &mut Vec::new(),
                    &mut holders,
                );
            }
        }
        Ok(())
    })?;
    Ok(holders)
}

fn rewrite_direct_reference_holder(
    document: &mut EditDocument,
    holder: &DirectReferenceHolder,
    canonical: CowObjectHandle,
) -> Result<bool> {
    let root = match holder.root {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    let Some(holder_object) = object_at_direct_path_mut(root, &holder.path) else {
        return Ok(false);
    };
    let Some(dictionary) = holder_object.as_dictionary_mut() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(current)) = dictionary.get(holder.key.as_slice()) else {
        return Ok(false);
    };
    if *current != holder.target {
        return Ok(false);
    }
    dictionary.insert(holder.key.clone(), OwnedObject::Reference(canonical));
    Ok(true)
}

fn canonicalize_named_stream_references_hayro(
    document: &mut EditDocument,
    key: &[u8],
    domain: &[u8],
) -> Result<TargetedDedupStats> {
    let holders = hayro_direct_reference_holders(document, key)?;
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::<CowObjectHandle, CowObjectHandle>::new();
    let mut duplicate_refs = HashSet::<CowObjectHandle>::new();
    let mut duplicate_raw_bytes = 0_usize;
    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, holder.target, domain)?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != holder.target {
                redirects.insert(holder.target, canonical);
                if duplicate_refs.insert(holder.target) {
                    duplicate_raw_bytes += raw_bytes;
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, holder.target);
        }
    }
    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = redirects.get(&holder.target).copied() else {
            continue;
        };
        if rewrite_direct_reference_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

pub(crate) fn canonicalize_metadata_streams_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    canonicalize_named_stream_references_hayro(document, b"Metadata", b"metadata")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectArrayReferenceHolder {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
    index: usize,
    target: CowObjectHandle,
}

fn inspect_hayro_icc_arrays(
    root: CowObjectHandle,
    object: &HayroObject<'_>,
    path: &mut Vec<DirectPathStep>,
    holders: &mut Vec<DirectArrayReferenceHolder>,
) {
    match object {
        HayroObject::Array(array) => {
            let is_icc = matches!(array.iter::<HayroObject<'_>>().next(), Some(HayroObject::Name(name)) if name.as_ref() == b"ICCBased");
            if is_icc && let Some(HayroMaybeRef::Ref(profile)) = array.raw_iter().nth(1) {
                holders.push(DirectArrayReferenceHolder {
                    root,
                    path: path.clone(),
                    index: 1,
                    target: CowObjectHandle::Existing(profile.into()),
                });
                return;
            }
            for (index, value) in array.raw_iter().enumerate() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_hayro_icc_arrays(root, &value, path, holders);
                path.pop();
            }
        }
        HayroObject::Dict(dictionary) => {
            for (name, value) in dictionary.entries() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
                inspect_hayro_icc_arrays(root, &value, path, holders);
                path.pop();
            }
        }
        HayroObject::Stream(stream) => {
            for (name, value) in stream.dict().entries() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
                inspect_hayro_icc_arrays(root, &value, path, holders);
                path.pop();
            }
        }
        HayroObject::Null(_)
        | HayroObject::Boolean(_)
        | HayroObject::Number(_)
        | HayroObject::String(_)
        | HayroObject::Name(_) => {}
    }
}

fn inspect_owned_icc_arrays(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &OwnedObject,
    path: &mut Vec<DirectPathStep>,
    holders: &mut Vec<DirectArrayReferenceHolder>,
) -> Result<()> {
    match object {
        OwnedObject::Array(values) => {
            let is_icc = values.first().is_some_and(|first| {
                matches!(document.resolve_owned_value(first), Ok(Some(OwnedObject::Name(name))) if name == b"ICCBased")
            });
            if is_icc && let Some(OwnedObject::Reference(profile)) = values.get(1) {
                holders.push(DirectArrayReferenceHolder {
                    root,
                    path: path.clone(),
                    index: 1,
                    target: *profile,
                });
                return Ok(());
            }
            for (index, value) in values.iter().enumerate() {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_owned_icc_arrays(document, root, value, path, holders)?;
                path.pop();
            }
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            for (name, value) in dictionary {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::DictKey(name.clone()));
                inspect_owned_icc_arrays(document, root, value, path, holders)?;
                path.pop();
            }
        }
        OwnedObject::Reference(_)
        | OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
    Ok(())
}

fn hayro_icc_array_holders(document: &EditDocument) -> Result<Vec<DirectArrayReferenceHolder>> {
    let mut holders = Vec::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            inspect_hayro_icc_arrays(handle, &object, &mut Vec::new(), &mut holders);
            Ok(())
        }
        CurrentObject::Owned(object) => {
            inspect_owned_icc_arrays(document, handle, object, &mut Vec::new(), &mut holders)
        }
    })?;
    Ok(holders)
}

fn rewrite_direct_array_reference_holder(
    document: &mut EditDocument,
    holder: &DirectArrayReferenceHolder,
    canonical: CowObjectHandle,
) -> Result<bool> {
    let root = match holder.root {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    let Some(array_object) = object_at_direct_path_mut(root, &holder.path) else {
        return Ok(false);
    };
    let OwnedObject::Array(values) = array_object else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(current)) = values.get(holder.index) else {
        return Ok(false);
    };
    if *current != holder.target {
        return Ok(false);
    }
    values[holder.index] = OwnedObject::Reference(canonical);
    Ok(true)
}

pub(crate) fn canonicalize_icc_profiles_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = hayro_icc_array_holders(document)?;
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::<CowObjectHandle, CowObjectHandle>::new();
    let mut duplicate_refs = HashSet::<CowObjectHandle>::new();
    let mut duplicate_raw_bytes = 0_usize;
    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, holder.target, b"icc-profile")?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != holder.target {
                redirects.insert(holder.target, canonical);
                if duplicate_refs.insert(holder.target) {
                    duplicate_raw_bytes += raw_bytes;
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, holder.target);
        }
    }
    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = redirects.get(&holder.target).copied() else {
            continue;
        };
        if rewrite_direct_array_reference_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

fn object_at_direct_path_mut<'a>(
    mut object: &'a mut OwnedObject,
    path: &[DirectPathStep],
) -> Option<&'a mut OwnedObject> {
    for step in path {
        object = match step {
            DirectPathStep::DictKey(key) => object.as_dictionary_mut()?.get_mut(key.as_slice())?,
            DirectPathStep::ArrayIndex(index) => match object {
                OwnedObject::Array(values) => values.get_mut(*index)?,
                _ => return None,
            },
        };
    }
    Some(object)
}

/// Hayro/COW `ToUnicode` canonicalization.
///
/// This deliberately walks the reachable Hayro graph rather than reproducing
/// flpdf `get_all_objects()` coverage. On damaged or oddly indexed PDFs Hayro
/// can therefore find additional real `/ToUnicode` holders that the legacy
/// pass skipped; every rewrite still requires an exact stream fingerprint.
pub(crate) fn canonicalize_to_unicode_cmaps_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    canonicalize_named_stream_references_hayro(document, b"ToUnicode", b"to-unicode")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DirectDictionaryTarget {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HayroType3GlyphHolder {
    target: DirectDictionaryTarget,
    name: Vec<u8>,
    glyph: CowObjectHandle,
}

fn object_at_direct_path<'a>(
    mut object: &'a OwnedObject,
    path: &[DirectPathStep],
) -> Option<&'a OwnedObject> {
    for step in path {
        object = match step {
            DirectPathStep::DictKey(key) => object.as_dictionary()?.get(key.as_slice())?,
            DirectPathStep::ArrayIndex(index) => match object {
                OwnedObject::Array(values) => values.get(*index)?,
                _ => return None,
            },
        };
    }
    Some(object)
}

fn inspect_hayro_type3_dictionary(
    root: CowObjectHandle,
    dictionary: &hayro_syntax::object::Dict<'_>,
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) {
    let is_type3 = dictionary
        .get::<HayroName<'_>>(b"Subtype")
        .is_some_and(|name| name.as_ref() == b"Type3");
    if is_type3 {
        if let Some(charprocs) = dictionary.get_ref(b"CharProcs") {
            targets.insert(DirectDictionaryTarget {
                root: CowObjectHandle::Existing(charprocs.into()),
                path: Vec::new(),
            });
        } else if matches!(
            dictionary.get_raw::<HayroObject<'_>>(b"CharProcs"),
            Some(HayroMaybeRef::NotRef(HayroObject::Dict(_)))
        ) {
            let mut target_path = path.clone();
            target_path.push(DirectPathStep::DictKey(b"CharProcs".to_vec()));
            targets.insert(DirectDictionaryTarget {
                root,
                path: target_path,
            });
        }
    }

    for (name, value) in dictionary.entries() {
        if name.as_ref() == b"CharProcs" {
            continue;
        }
        let HayroMaybeRef::NotRef(value) = value else {
            continue;
        };
        path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
        inspect_hayro_type3_object(root, &value, path, targets);
        path.pop();
    }
}

fn inspect_hayro_type3_object(
    root: CowObjectHandle,
    object: &HayroObject<'_>,
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            inspect_hayro_type3_dictionary(root, dictionary, path, targets);
        }
        HayroObject::Stream(stream) => {
            inspect_hayro_type3_dictionary(root, stream.dict(), path, targets);
        }
        HayroObject::Array(array) => {
            for (index, value) in array.raw_iter().enumerate() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_hayro_type3_object(root, &value, path, targets);
                path.pop();
            }
        }
        HayroObject::Null(_)
        | HayroObject::Boolean(_)
        | HayroObject::Number(_)
        | HayroObject::String(_)
        | HayroObject::Name(_) => {}
    }
}

fn owned_dictionary_is_type3(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<bool> {
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(false);
    };
    Ok(matches!(
        document.resolve_owned_value(subtype)?,
        Some(OwnedObject::Name(name)) if name == b"Type3"
    ))
}

fn inspect_owned_type3_object(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &OwnedObject,
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) -> Result<()> {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if owned_dictionary_is_type3(document, dictionary)? {
                match dictionary.get(b"CharProcs".as_slice()) {
                    Some(OwnedObject::Reference(charprocs)) => {
                        targets.insert(DirectDictionaryTarget {
                            root: *charprocs,
                            path: Vec::new(),
                        });
                    }
                    Some(OwnedObject::Dictionary(_)) => {
                        let mut target_path = path.clone();
                        target_path.push(DirectPathStep::DictKey(b"CharProcs".to_vec()));
                        targets.insert(DirectDictionaryTarget {
                            root,
                            path: target_path,
                        });
                    }
                    _ => {}
                }
            }
            for (name, value) in dictionary {
                if name.as_slice() == b"CharProcs" || matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::DictKey(name.clone()));
                inspect_owned_type3_object(document, root, value, path, targets)?;
                path.pop();
            }
        }
        OwnedObject::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_owned_type3_object(document, root, value, path, targets)?;
                path.pop();
            }
        }
        OwnedObject::Reference(_)
        | OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
    Ok(())
}

fn hayro_type3_charproc_targets(
    document: &EditDocument,
) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = BTreeSet::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            inspect_hayro_type3_object(handle, &object, &mut Vec::new(), &mut targets);
            Ok(())
        }
        CurrentObject::Owned(object) => {
            inspect_owned_type3_object(document, handle, object, &mut Vec::new(), &mut targets)
        }
    })?;
    Ok(targets)
}

fn hayro_type3_glyph_holders(document: &EditDocument) -> Result<Vec<HayroType3GlyphHolder>> {
    let mut holders = Vec::new();
    for target in hayro_type3_charproc_targets(document)? {
        let Some(root) = document.current_owned_object(target.root)? else {
            continue;
        };
        let Some(charprocs) = object_at_direct_path(&root, &target.path) else {
            continue;
        };
        let Some(dictionary) = charprocs.as_dictionary() else {
            continue;
        };
        for (name, glyph) in dictionary {
            let OwnedObject::Reference(glyph) = glyph else {
                continue;
            };
            holders.push(HayroType3GlyphHolder {
                target: target.clone(),
                name: name.clone(),
                glyph: *glyph,
            });
        }
    }
    Ok(holders)
}

fn rewrite_type3_glyph_holder(
    document: &mut EditDocument,
    holder: &HayroType3GlyphHolder,
    canonical: CowObjectHandle,
) -> Result<bool> {
    let root = match holder.target.root {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    let Some(charprocs) = object_at_direct_path_mut(root, &holder.target.path) else {
        return Ok(false);
    };
    let Some(dictionary) = charprocs.as_dictionary_mut() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(current)) = dictionary.get(holder.name.as_slice()) else {
        return Ok(false);
    };
    if *current != holder.glyph {
        return Ok(false);
    }
    dictionary.insert(holder.name.clone(), OwnedObject::Reference(canonical));
    Ok(true)
}

pub(crate) fn canonicalize_type3_charprocs_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = hayro_type3_glyph_holders(document)?;
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::<CowObjectHandle, CowObjectHandle>::new();
    let mut duplicate_refs = HashSet::<CowObjectHandle>::new();
    let mut duplicate_raw_bytes = 0_usize;

    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, holder.glyph, b"type3-charproc")?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != holder.glyph {
                redirects.insert(holder.glyph, canonical);
                if duplicate_refs.insert(holder.glyph) {
                    duplicate_raw_bytes += raw_bytes;
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, holder.glyph);
        }
    }

    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = redirects.get(&holder.glyph).copied() else {
            continue;
        };
        if rewrite_type3_glyph_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }

    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

fn canonical_cow_redirect(
    mut handle: CowObjectHandle,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> CowObjectHandle {
    let mut seen = HashSet::new();
    while let Some(next) = redirects.get(&handle).copied() {
        if next == handle || !seen.insert(handle) {
            break;
        }
        handle = next;
    }
    handle
}

fn hash_owned_object_with_redirects(
    hasher: &mut Sha256,
    object: &OwnedObject,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<()> {
    match object {
        OwnedObject::Reference(handle) => {
            hasher.update([0x70]);
            hash_cow_handle(hasher, canonical_cow_redirect(*handle, redirects));
        }
        OwnedObject::Array(values) => {
            hasher.update([0x06]);
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_owned_object_with_redirects(hasher, value, redirects)?;
            }
        }
        OwnedObject::Dictionary(dictionary) => {
            hasher.update([0x07]);
            hasher.update((dictionary.len() as u64).to_le_bytes());
            for (key, value) in dictionary {
                hash_len_prefixed(hasher, key);
                hash_owned_object_with_redirects(hasher, value, redirects)?;
            }
        }
        OwnedObject::Stream { dictionary, data } => {
            hasher.update([0x08]);
            hasher.update((dictionary.len() as u64).to_le_bytes());
            for (key, value) in dictionary {
                hash_len_prefixed(hasher, key);
                hash_owned_object_with_redirects(hasher, value, redirects)?;
            }
            match data {
                StreamData::Source(id) => {
                    hasher.update([0x80]);
                    hasher.update(id.number().to_le_bytes());
                    hasher.update(id.generation().to_le_bytes());
                }
                StreamData::Owned(bytes) => {
                    hasher.update([0x81]);
                    hash_len_prefixed(hasher, bytes);
                }
            }
        }
        other => hash_owned_object(hasher, other)?,
    }
    Ok(())
}

fn hayro_stream_fingerprint_with_redirects(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object(stream)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_len_prefixed(&mut hasher, raw.as_ref());
    let entries = dictionary
        .iter()
        .filter(|(key, _)| {
            key.as_slice() != b"Length" && !ignored_dictionary_keys.contains(&key.as_slice())
        })
        .collect::<Vec<_>>();
    hasher.update((entries.len() as u64).to_le_bytes());
    for (key, value) in entries {
        hash_len_prefixed(&mut hasher, key);
        hash_owned_object_with_redirects(&mut hasher, value, redirects)?;
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn reachable_streams_with_subtype(
    document: &EditDocument,
    subtype: &[u8],
) -> Result<Vec<CowObjectHandle>> {
    let mut streams = Vec::new();
    for handle in document.reachable_output_objects()? {
        let Some(OwnedObject::Stream { dictionary, .. }) = document.current_owned_object(handle)?
        else {
            continue;
        };
        let Some(value) = dictionary.get(b"Subtype".as_slice()) else {
            continue;
        };
        if matches!(document.resolve_owned_value(value)?, Some(OwnedObject::Name(name)) if name == subtype)
        {
            streams.push(handle);
        }
    }
    Ok(streams)
}

fn exact_stream_redirects_hayro(
    document: &EditDocument,
    streams: &[CowObjectHandle],
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    dependency_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    duplicate_refs: &mut HashSet<CowObjectHandle>,
    duplicate_raw_bytes: &mut usize,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::new();
    for &stream in streams {
        let Some((fingerprint, raw_bytes)) = hayro_stream_fingerprint_with_redirects(
            document,
            stream,
            domain,
            ignored_dictionary_keys,
            dependency_redirects,
        )?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != stream {
                redirects.insert(stream, canonical);
                if duplicate_refs.insert(stream) {
                    *duplicate_raw_bytes += raw_bytes;
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, stream);
        }
    }
    Ok(redirects)
}

fn rewrite_dictionary_reference_keys(
    document: &mut EditDocument,
    handle: CowObjectHandle,
    keys: &[&[u8]],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(0);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(0);
    };
    let mut changes = Vec::new();
    for &key in keys {
        let Some(OwnedObject::Reference(target)) = dictionary.get(key) else {
            continue;
        };
        let canonical = canonical_cow_redirect(*target, redirects);
        if canonical != *target {
            changes.push((key.to_vec(), canonical));
        }
    }
    if changes.is_empty() {
        return Ok(0);
    }
    let object = match handle {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
    };
    let Some(dictionary) = object.as_dictionary_mut() else {
        return Ok(0);
    };
    let count = changes.len();
    for (key, target) in changes {
        dictionary.insert(key, OwnedObject::Reference(target));
    }
    Ok(count)
}

fn inspect_owned_dictionary_target(
    root: CowObjectHandle,
    object: &OwnedObject,
    key: &[u8],
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if let Some(value) = dictionary.get(key) {
                match value {
                    OwnedObject::Reference(target) => {
                        targets.insert(DirectDictionaryTarget {
                            root: *target,
                            path: Vec::new(),
                        });
                    }
                    OwnedObject::Dictionary(_) => {
                        let mut target_path = path.clone();
                        target_path.push(DirectPathStep::DictKey(key.to_vec()));
                        targets.insert(DirectDictionaryTarget {
                            root,
                            path: target_path,
                        });
                    }
                    _ => {}
                }
            }
            for (name, value) in dictionary {
                if name.as_slice() == key || matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::DictKey(name.clone()));
                inspect_owned_dictionary_target(root, value, key, path, targets);
                path.pop();
            }
        }
        OwnedObject::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_owned_dictionary_target(root, value, key, path, targets);
                path.pop();
            }
        }
        _ => {}
    }
}

fn hayro_dictionary_targets(
    document: &EditDocument,
    key: &[u8],
) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = BTreeSet::new();
    for root in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(root)? else {
            continue;
        };
        inspect_owned_dictionary_target(root, &object, key, &mut Vec::new(), &mut targets);
    }
    Ok(targets)
}

fn rewrite_dictionary_target_entries(
    document: &mut EditDocument,
    targets: &BTreeSet<DirectDictionaryTarget>,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    let mut rewritten = 0;
    for target in targets {
        let Some(root_snapshot) = document.current_owned_object(target.root)? else {
            continue;
        };
        let Some(dictionary) = object_at_direct_path(&root_snapshot, &target.path)
            .and_then(OwnedObject::as_dictionary)
        else {
            continue;
        };
        let changes = dictionary
            .iter()
            .filter_map(|(name, value)| {
                let OwnedObject::Reference(reference) = value else {
                    return None;
                };
                let canonical = canonical_cow_redirect(*reference, redirects);
                (canonical != *reference).then(|| (name.clone(), canonical))
            })
            .collect::<Vec<_>>();
        if changes.is_empty() {
            continue;
        }
        let root = match target.root {
            CowObjectHandle::Existing(id) => document.edit_object(id)?,
            CowObjectHandle::New(id) => document
                .overlay_mut()
                .added_mut(id)
                .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
        };
        let Some(dictionary) =
            object_at_direct_path_mut(root, &target.path).and_then(OwnedObject::as_dictionary_mut)
        else {
            continue;
        };
        for (name, canonical) in changes {
            dictionary.insert(name, OwnedObject::Reference(canonical));
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

pub(crate) fn canonicalize_image_xobjects_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let images = reachable_streams_with_subtype(document, b"Image")?;
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name"]
    } else {
        &[]
    };
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;
    let empty_redirects = HashMap::new();
    let mask_redirects = exact_stream_redirects_hayro(
        document,
        &images,
        b"image-xobject",
        ignored,
        &empty_redirects,
        &mut duplicate_refs,
        &mut duplicate_raw_bytes,
    )?;
    let mut references_canonicalized = 0;
    for &image in &images {
        references_canonicalized += rewrite_dictionary_reference_keys(
            document,
            image,
            &[b"Mask", b"SMask"],
            &mask_redirects,
        )?;
    }
    let redirects = exact_stream_redirects_hayro(
        document,
        &images,
        b"image-xobject",
        ignored,
        &empty_redirects,
        &mut duplicate_refs,
        &mut duplicate_raw_bytes,
    )?;
    let xobject_targets = hayro_dictionary_targets(document, b"XObject")?;
    references_canonicalized +=
        rewrite_dictionary_target_entries(document, &xobject_targets, &redirects)?;
    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

fn hash_non_stream_object_with_redirects(
    document: &EditDocument,
    handle: CowObjectHandle,
    domain: &[u8],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<[u8; 32]>> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(None);
    };
    if matches!(object, OwnedObject::Stream { .. }) {
        return Ok(None);
    }
    if !matches!(object, OwnedObject::Dictionary(_) | OwnedObject::Array(_)) {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_owned_object_with_redirects(&mut hasher, &object, redirects)?;
    Ok(Some(hasher.finalize().into()))
}

fn collect_form_resource_handles_from_value(
    document: &EditDocument,
    value: &OwnedObject,
    seen: &mut BTreeSet<CowObjectHandle>,
    handles: &mut BTreeSet<CowObjectHandle>,
) -> Result<()> {
    match value {
        OwnedObject::Reference(handle) => {
            if !seen.insert(*handle) {
                return Ok(());
            }
            let Some(object) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
            if matches!(object, OwnedObject::Dictionary(_) | OwnedObject::Array(_)) {
                handles.insert(*handle);
            }
            collect_form_resource_handles_from_value(document, &object, seen, handles)?;
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            for child in dictionary.values() {
                collect_form_resource_handles_from_value(document, child, seen, handles)?;
            }
        }
        OwnedObject::Array(values) => {
            for child in values {
                collect_form_resource_handles_from_value(document, child, seen, handles)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn form_resource_handles(
    document: &EditDocument,
    forms: &[CowObjectHandle],
) -> Result<Vec<CowObjectHandle>> {
    let mut seen = BTreeSet::new();
    let mut handles = BTreeSet::new();
    for &form in forms {
        let Some(OwnedObject::Stream { dictionary, .. }) = document.current_owned_object(form)?
        else {
            continue;
        };
        let Some(resources) = dictionary.get(b"Resources".as_slice()) else {
            continue;
        };
        collect_form_resource_handles_from_value(document, resources, &mut seen, &mut handles)?;
    }
    Ok(handles.into_iter().collect())
}

fn exact_non_stream_resource_redirects_hayro(
    document: &EditDocument,
    handles: &[CowObjectHandle],
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut redirects = HashMap::new();
    for _ in 0..=handles.len() {
        let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
        let mut next = HashMap::new();
        for &handle in handles {
            let Some(fingerprint) = hash_non_stream_object_with_redirects(
                document,
                handle,
                b"form-resource-exact-object",
                &redirects,
            )?
            else {
                continue;
            };
            if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
                if canonical != handle {
                    next.insert(handle, canonical);
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, handle);
            }
        }
        if next == redirects {
            return Ok(next);
        }
        redirects = next;
    }
    Ok(redirects)
}

fn exact_form_font_redirects_hayro(
    document: &EditDocument,
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::new();
    for handle in document.reachable_output_objects()? {
        let Some(OwnedObject::Dictionary(dictionary)) = document.current_owned_object(handle)?
        else {
            continue;
        };
        let Some(value) = dictionary.get(b"Type".as_slice()) else {
            continue;
        };
        if !matches!(document.resolve_owned_value(value)?, Some(OwnedObject::Name(name)) if name == b"Font")
        {
            continue;
        }
        let Some(fingerprint) = hash_non_stream_object_with_redirects(
            document,
            handle,
            b"form-font-resource-dictionary",
            exact_redirects,
        )?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != handle {
                redirects.insert(handle, canonical);
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, handle);
        }
    }
    Ok(redirects)
}

fn hayro_stream_fingerprint_top_level_redirects(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<[u8; 32]>> {
    let Some(OwnedObject::Stream { dictionary, data }) = document.current_owned_object(stream)?
    else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_len_prefixed(&mut hasher, raw.as_ref());
    let entries = dictionary
        .iter()
        .filter(|(key, _)| {
            key.as_slice() != b"Length" && !ignored_dictionary_keys.contains(&key.as_slice())
        })
        .collect::<Vec<_>>();
    hasher.update((entries.len() as u64).to_le_bytes());
    for (key, value) in entries {
        hash_len_prefixed(&mut hasher, key);
        if let OwnedObject::Reference(handle) = value {
            hash_cow_handle(&mut hasher, canonical_cow_redirect(*handle, redirects));
        } else {
            hash_owned_object(&mut hasher, value)?;
        }
    }
    Ok(Some(hasher.finalize().into()))
}

fn virtual_form_image_redirects_hayro(
    document: &EditDocument,
    images: &[CowObjectHandle],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name"]
    } else {
        &[]
    };
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::new();
    for &image in images {
        let Some(fingerprint) = hayro_stream_fingerprint_top_level_redirects(
            document,
            image,
            b"form-resource-image",
            ignored,
            exact_redirects,
        )?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != image {
                redirects.insert(image, canonical);
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, image);
        }
    }
    Ok(redirects)
}

fn redirect_reference_value(
    value: &OwnedObject,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> OwnedObject {
    match value {
        OwnedObject::Reference(handle) => {
            OwnedObject::Reference(canonical_cow_redirect(*handle, redirects))
        }
        _ => value.clone(),
    }
}

fn resolved_dictionary_clone(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<OwnedDictionary>> {
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|object| match object {
            OwnedObject::Dictionary(dictionary) => Some(dictionary),
            OwnedObject::Stream { dictionary, .. } => Some(dictionary),
            _ => None,
        }))
}

fn normalized_named_form_resource_hayro(
    document: &EditDocument,
    value: &OwnedObject,
    resource_kind: &[u8],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    form_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<OwnedObject> {
    let Some(dictionary) = resolved_dictionary_clone(document, value)? else {
        return Ok(value.clone());
    };
    let normalized = dictionary
        .into_iter()
        .map(|(name, target)| {
            let target = match resource_kind {
                b"Font" => redirect_reference_value(&target, font_redirects),
                b"XObject" => match &target {
                    OwnedObject::Reference(handle) if form_redirects.contains_key(handle) => {
                        redirect_reference_value(&target, form_redirects)
                    }
                    OwnedObject::Reference(_) => redirect_reference_value(&target, image_redirects),
                    _ => target,
                },
                b"ColorSpace" | b"ExtGState" | b"Properties" => {
                    redirect_reference_value(&target, exact_redirects)
                }
                _ => target,
            };
            (name, target)
        })
        .collect();
    Ok(OwnedObject::Dictionary(normalized))
}

fn normalized_form_resources_hayro(
    document: &EditDocument,
    value: &OwnedObject,
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    form_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<OwnedObject> {
    let Some(dictionary) = resolved_dictionary_clone(document, value)? else {
        return Ok(value.clone());
    };
    let mut normalized = OwnedDictionary::new();
    for (key, value) in dictionary {
        let value = match key.as_slice() {
            b"Font" | b"XObject" | b"ColorSpace" | b"ExtGState" | b"Properties" => {
                normalized_named_form_resource_hayro(
                    document,
                    &value,
                    &key,
                    exact_redirects,
                    font_redirects,
                    image_redirects,
                    form_redirects,
                )?
            }
            b"ProcSet" => redirect_reference_value(&value, exact_redirects),
            _ => value,
        };
        normalized.insert(key, value);
    }
    Ok(OwnedObject::Dictionary(normalized))
}

fn hayro_form_fingerprint(
    document: &EditDocument,
    form: CowObjectHandle,
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    form_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<([u8; 32], usize)>> {
    let Some(OwnedObject::Stream { dictionary, data }) = document.current_owned_object(form)?
    else {
        return Ok(None);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(None);
    };
    if !matches!(document.resolve_owned_value(subtype)?, Some(OwnedObject::Name(name)) if name == b"Form")
    {
        return Ok(None);
    }
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, b"form-xobject");
    hash_len_prefixed(&mut hasher, raw.as_ref());
    let entries = dictionary
        .iter()
        .filter(|(key, _)| {
            key.as_slice() != b"Length" && !ignored_dictionary_keys.contains(&key.as_slice())
        })
        .collect::<Vec<_>>();
    hasher.update((entries.len() as u64).to_le_bytes());
    for (key, value) in entries {
        hash_len_prefixed(&mut hasher, key);
        if key.as_slice() == b"Resources" {
            let normalized = normalized_form_resources_hayro(
                document,
                value,
                exact_redirects,
                font_redirects,
                image_redirects,
                form_redirects,
            )?;
            hash_owned_object(&mut hasher, &normalized)?;
        } else {
            hash_owned_object(&mut hasher, value)?;
        }
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn fixed_point_form_redirects_hayro(
    document: &EditDocument,
    forms: &[CowObjectHandle],
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut redirects = HashMap::new();
    for _ in 0..=forms.len() {
        let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
        let mut next = HashMap::new();
        for &form in forms {
            let Some((fingerprint, _)) = hayro_form_fingerprint(
                document,
                form,
                ignored_dictionary_keys,
                exact_redirects,
                font_redirects,
                image_redirects,
                &redirects,
            )?
            else {
                continue;
            };
            if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
                if canonical != form {
                    next.insert(form, canonical);
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, form);
            }
        }
        if next == redirects {
            return Ok(next);
        }
        redirects = next;
    }
    Ok(redirects)
}

fn inspect_widget_mk_targets(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &OwnedObject,
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) -> Result<()> {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            let is_widget = dictionary.get(b"Subtype".as_slice()).is_some_and(|value| {
                matches!(document.resolve_owned_value(value), Ok(Some(OwnedObject::Name(name))) if name == b"Widget")
            });
            if is_widget && let Some(mk) = dictionary.get(b"MK".as_slice()) {
                match mk {
                    OwnedObject::Reference(handle) => {
                        targets.insert(DirectDictionaryTarget {
                            root: *handle,
                            path: Vec::new(),
                        });
                    }
                    OwnedObject::Dictionary(_) => {
                        let mut target_path = path.clone();
                        target_path.push(DirectPathStep::DictKey(b"MK".to_vec()));
                        targets.insert(DirectDictionaryTarget {
                            root,
                            path: target_path,
                        });
                    }
                    _ => {}
                }
            }
            for (name, value) in dictionary {
                if is_widget && name.as_slice() == b"MK" {
                    continue;
                }
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::DictKey(name.clone()));
                inspect_widget_mk_targets(document, root, value, path, targets)?;
                path.pop();
            }
        }
        OwnedObject::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_widget_mk_targets(document, root, value, path, targets)?;
                path.pop();
            }
        }
        _ => {}
    }
    Ok(())
}

fn widget_mk_targets(document: &EditDocument) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = BTreeSet::new();
    for root in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(root)? else {
            continue;
        };
        inspect_widget_mk_targets(document, root, &object, &mut Vec::new(), &mut targets)?;
    }
    Ok(targets)
}

fn rewrite_selected_dictionary_entries(
    document: &mut EditDocument,
    targets: &BTreeSet<DirectDictionaryTarget>,
    keys: &[&[u8]],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    let mut rewritten = 0;
    for target in targets {
        let Some(snapshot) = document.current_owned_object(target.root)? else {
            continue;
        };
        let Some(dictionary) =
            object_at_direct_path(&snapshot, &target.path).and_then(OwnedObject::as_dictionary)
        else {
            continue;
        };
        let mut changes = Vec::new();
        for &key in keys {
            let Some(OwnedObject::Reference(reference)) = dictionary.get(key) else {
                continue;
            };
            let canonical = canonical_cow_redirect(*reference, redirects);
            if canonical != *reference {
                changes.push((key.to_vec(), canonical));
            }
        }
        if changes.is_empty() {
            continue;
        }
        let root = match target.root {
            CowObjectHandle::Existing(id) => document.edit_object(id)?,
            CowObjectHandle::New(id) => document
                .overlay_mut()
                .added_mut(id)
                .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
        };
        let Some(dictionary) =
            object_at_direct_path_mut(root, &target.path).and_then(OwnedObject::as_dictionary_mut)
        else {
            continue;
        };
        for (key, canonical) in changes {
            dictionary.insert(key, OwnedObject::Reference(canonical));
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

struct FormDependencyRedirects {
    form_streams: Vec<CowObjectHandle>,
    exact: HashMap<CowObjectHandle, CowObjectHandle>,
    fonts: HashMap<CowObjectHandle, CowObjectHandle>,
    images: HashMap<CowObjectHandle, CowObjectHandle>,
    forms: HashMap<CowObjectHandle, CowObjectHandle>,
}

fn form_dependency_redirects_hayro(
    document: &EditDocument,
    ignored_form_dictionary_keys: &[&[u8]],
) -> Result<FormDependencyRedirects> {
    let form_streams = reachable_streams_with_subtype(document, b"Form")?;
    let images = reachable_streams_with_subtype(document, b"Image")?;
    let resources = form_resource_handles(document, &form_streams)?;
    let exact = exact_non_stream_resource_redirects_hayro(document, &resources)?;
    let fonts = exact_form_font_redirects_hayro(document, &exact)?;
    let image_redirects = virtual_form_image_redirects_hayro(document, &images, &exact)?;
    let forms = fixed_point_form_redirects_hayro(
        document,
        &form_streams,
        ignored_form_dictionary_keys,
        &exact,
        &fonts,
        &image_redirects,
    )?;
    Ok(FormDependencyRedirects {
        form_streams,
        exact,
        fonts,
        images: image_redirects,
        forms,
    })
}

pub(crate) fn canonicalize_form_xobjects_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name"]
    } else {
        &[]
    };
    let dependencies = form_dependency_redirects_hayro(document, ignored)?;
    let mut duplicate_raw_bytes = 0_usize;
    for &form in &dependencies.form_streams {
        if dependencies.forms.contains_key(&form)
            && let Some(OwnedObject::Stream { data, .. }) = document.current_owned_object(form)?
        {
            duplicate_raw_bytes += data.bytes(document.source())?.len();
        }
    }
    let xobject_targets = hayro_dictionary_targets(document, b"XObject")?;
    let mut references_canonicalized =
        rewrite_dictionary_target_entries(document, &xobject_targets, &dependencies.forms)?;
    let icon_targets = widget_mk_targets(document)?;
    references_canonicalized += rewrite_selected_dictionary_entries(
        document,
        &icon_targets,
        &[b"I", b"RI", b"IX"],
        &dependencies.forms,
    )?;
    Ok(TargetedDedupStats {
        duplicate_streams_detected: dependencies.forms.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

fn appearance_dictionary_targets(
    document: &EditDocument,
) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = hayro_dictionary_targets(document, b"AP")?;
    let initial = targets.iter().cloned().collect::<Vec<_>>();
    for target in initial {
        let Some(snapshot) = document.current_owned_object(target.root)? else {
            continue;
        };
        let Some(dictionary) =
            object_at_direct_path(&snapshot, &target.path).and_then(OwnedObject::as_dictionary)
        else {
            continue;
        };
        for key in [b"N".as_slice(), b"R".as_slice(), b"D".as_slice()] {
            let Some(value) = dictionary.get(key) else {
                continue;
            };
            match value {
                OwnedObject::Reference(handle) => {
                    if matches!(
                        document.current_owned_object(*handle)?,
                        Some(OwnedObject::Dictionary(_))
                    ) {
                        targets.insert(DirectDictionaryTarget {
                            root: *handle,
                            path: Vec::new(),
                        });
                    }
                }
                OwnedObject::Dictionary(_) => {
                    let mut path = target.path.clone();
                    path.push(DirectPathStep::DictKey(key.to_vec()));
                    targets.insert(DirectDictionaryTarget {
                        root: target.root,
                        path,
                    });
                }
                _ => {}
            }
        }
    }
    Ok(targets)
}

pub(crate) fn canonicalize_appearance_streams_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = appearance_dictionary_targets(document)?;
    if holders.is_empty() {
        return Ok(TargetedDedupStats::default());
    }
    let dependencies = form_dependency_redirects_hayro(document, &[])?;
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::<CowObjectHandle, CowObjectHandle>::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;
    for holder in &holders {
        let Some(snapshot) = document.current_owned_object(holder.root)? else {
            continue;
        };
        let Some(dictionary) =
            object_at_direct_path(&snapshot, &holder.path).and_then(OwnedObject::as_dictionary)
        else {
            continue;
        };
        for appearance in dictionary.values() {
            let OwnedObject::Reference(appearance_ref) = appearance else {
                continue;
            };
            let Some((fingerprint, raw_bytes)) = hayro_form_fingerprint(
                document,
                *appearance_ref,
                &[],
                &dependencies.exact,
                &dependencies.fonts,
                &dependencies.images,
                &dependencies.forms,
            )?
            else {
                continue;
            };
            if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
                if canonical != *appearance_ref {
                    redirects.insert(*appearance_ref, canonical);
                    if duplicate_refs.insert(*appearance_ref) {
                        duplicate_raw_bytes += raw_bytes;
                    }
                }
            } else {
                canonical_by_fingerprint.insert(fingerprint, *appearance_ref);
            }
        }
    }
    let references_canonicalized =
        rewrite_dictionary_target_entries(document, &holders, &redirects)?;
    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PageContentHolder {
    Dictionary(DirectReferenceHolder),
    Array(DirectArrayReferenceHolder),
}

impl PageContentHolder {
    fn target(&self) -> CowObjectHandle {
        match self {
            Self::Dictionary(holder) => holder.target,
            Self::Array(holder) => holder.target,
        }
    }

    fn rewrite(&self, document: &mut EditDocument, canonical: CowObjectHandle) -> Result<bool> {
        match self {
            Self::Dictionary(holder) => {
                rewrite_direct_reference_holder(document, holder, canonical)
            }
            Self::Array(holder) => {
                rewrite_direct_array_reference_holder(document, holder, canonical)
            }
        }
    }
}

fn page_handles_hayro(document: &EditDocument) -> Result<Vec<CowObjectHandle>> {
    let mut pages = BTreeSet::new();
    pages.extend(
        document
            .source()
            .page_ids()
            .into_iter()
            .map(CowObjectHandle::Existing),
    );
    for handle in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(value) = dictionary.get(b"Type".as_slice()) else {
            continue;
        };
        if matches!(document.resolve_owned_value(value)?, Some(OwnedObject::Name(name)) if name == b"Page")
        {
            pages.insert(handle);
        }
    }
    Ok(pages.into_iter().collect())
}

fn page_content_holders_hayro(document: &EditDocument) -> Result<Vec<PageContentHolder>> {
    let mut holders = Vec::new();
    for page in page_handles_hayro(document)? {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        match contents {
            OwnedObject::Reference(target) => match document.current_owned_object(*target)? {
                Some(OwnedObject::Stream { .. }) => {
                    holders.push(PageContentHolder::Dictionary(DirectReferenceHolder {
                        root: page,
                        path: Vec::new(),
                        key: b"Contents".to_vec(),
                        target: *target,
                    }))
                }
                Some(OwnedObject::Array(values)) => {
                    for (index, value) in values.iter().enumerate() {
                        let OwnedObject::Reference(stream) = value else {
                            continue;
                        };
                        if matches!(
                            document.current_owned_object(*stream)?,
                            Some(OwnedObject::Stream { .. })
                        ) {
                            holders.push(PageContentHolder::Array(DirectArrayReferenceHolder {
                                root: *target,
                                path: Vec::new(),
                                index,
                                target: *stream,
                            }));
                        }
                    }
                }
                _ => {}
            },
            OwnedObject::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    let OwnedObject::Reference(stream) = value else {
                        continue;
                    };
                    if matches!(
                        document.current_owned_object(*stream)?,
                        Some(OwnedObject::Stream { .. })
                    ) {
                        holders.push(PageContentHolder::Array(DirectArrayReferenceHolder {
                            root: page,
                            path: vec![DirectPathStep::DictKey(b"Contents".to_vec())],
                            index,
                            target: *stream,
                        }));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(holders)
}

pub(crate) fn canonicalize_page_contents_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = page_content_holders_hayro(document)?;
    let mut canonical_by_fingerprint = HashMap::<[u8; 32], CowObjectHandle>::new();
    let mut redirects = HashMap::<CowObjectHandle, CowObjectHandle>::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0_usize;
    for holder in &holders {
        let stream = holder.target();
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, stream, b"page-content")?
        else {
            continue;
        };
        if let Some(canonical) = canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != stream {
                redirects.insert(stream, canonical);
                if duplicate_refs.insert(stream) {
                    duplicate_raw_bytes += raw_bytes;
                }
            }
        } else {
            canonical_by_fingerprint.insert(fingerprint, stream);
        }
    }
    let mut references_canonicalized = 0;
    for holder in &holders {
        let Some(canonical) = redirects.get(&holder.target()).copied() else {
            continue;
        };
        if holder.rewrite(document, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use flpdf::PdfWriter;
    use std::io::Cursor;
    use std::rc::Rc;

    fn metadata_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new metadata stream has no dictionary".to_owned()))?;
        dict.replace_key(b"/Type", ObjectHandle::name(b"Metadata".to_vec()))?;
        dict.replace_key(b"/Subtype", ObjectHandle::name(b"XML".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(stream)
    }

    fn untyped_metadata_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
    ) -> Result<ObjectHandle> {
        pdf.new_stream_with_data(Rc::new(data.to_vec()))
            .map_err(Into::into)
    }

    fn holder(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        metadata: ObjectHandle,
    ) -> Result<ObjectHandle> {
        Ok(
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/Metadata".to_vec(),
                metadata,
            )]))?,
        )
    }

    #[test]
    fn canonicalizes_only_exact_duplicate_metadata_streams() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let first = metadata_stream(&mut pdf, b"<x:xmpmeta>same</x:xmpmeta>")?;
        let second = metadata_stream(&mut pdf, b"<x:xmpmeta>same</x:xmpmeta>")?;
        let different_dict = metadata_stream(&mut pdf, b"<x:xmpmeta>same</x:xmpmeta>")?;
        let different_dict_handle = different_dict
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("metadata stream has no dictionary".to_owned()))?;
        different_dict_handle.replace_key(b"/Custom", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&different_dict_handle)?;

        let first_holder = holder(&mut pdf, first)?;
        let second_holder = holder(&mut pdf, second)?;
        let different_holder = holder(&mut pdf, different_dict)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestMetadataA", first_holder.clone())?;
        root.replace_key(b"/TestMetadataB", second_holder.clone())?;
        root.replace_key(b"/TestMetadataC", different_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_metadata_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);

        let first_ref = first_holder.try_get_key(b"/Metadata")?.object_ref();
        let second_ref = second_holder.try_get_key(b"/Metadata")?.object_ref();
        let different_ref = different_holder.try_get_key(b"/Metadata")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    #[test]
    fn canonicalizes_streams_when_only_length_reference_identity_differs() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"<x:xmpmeta>same payload, separate indirect lengths</x:xmpmeta>";
        let first = metadata_stream(&mut pdf, payload)?;
        let second = metadata_stream(&mut pdf, payload)?;
        let first_length =
            pdf.make_indirect_object_handle(ObjectHandle::integer(payload.len() as i64))?;
        let second_length =
            pdf.make_indirect_object_handle(ObjectHandle::integer(payload.len() as i64))?;
        let first_dict = first
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("first metadata stream has no dictionary".to_owned()))?;
        let second_dict = second
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("second metadata stream has no dictionary".to_owned()))?;
        first_dict.replace_key(b"/Length", first_length)?;
        second_dict.replace_key(b"/Length", second_length)?;
        pdf.mark_object_handle_dirty(&first_dict)?;
        pdf.mark_object_handle_dirty(&second_dict)?;

        let first_holder = holder(&mut pdf, first)?;
        let second_holder = holder(&mut pdf, second)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestIndirectLengthMetadataA", first_holder.clone())?;
        root.replace_key(b"/TestIndirectLengthMetadataB", second_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_metadata_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            first_holder.try_get_key(b"/Metadata")?.object_ref(),
            second_holder.try_get_key(b"/Metadata")?.object_ref()
        );
        Ok(())
    }

    #[test]
    fn canonicalizes_metadata_nested_in_direct_resource_dictionaries() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"<x:xmpmeta>nested page property metadata</x:xmpmeta>";
        let first = metadata_stream(&mut pdf, payload)?;
        let second = metadata_stream(&mut pdf, payload)?;

        let nested_holder = |metadata: ObjectHandle| {
            ObjectHandle::dictionary(vec![(
                b"/Resources".to_vec(),
                ObjectHandle::dictionary(vec![(
                    b"/Properties".to_vec(),
                    ObjectHandle::dictionary(vec![(
                        b"/MC0".to_vec(),
                        ObjectHandle::dictionary(vec![(b"/Metadata".to_vec(), metadata)]),
                    )]),
                )]),
            )])
        };
        let first_holder = pdf.make_indirect_object_handle(nested_holder(first))?;
        let second_holder = pdf.make_indirect_object_handle(nested_holder(second))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestNestedMetadataA", first_holder.clone())?;
        root.replace_key(b"/TestNestedMetadataB", second_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_metadata_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let nested_metadata_ref = |holder: &ObjectHandle| -> Result<Option<ObjectRef>> {
            Ok(holder
                .try_get_key(b"/Resources")?
                .try_get_key(b"/Properties")?
                .try_get_key(b"/MC0")?
                .try_get_key(b"/Metadata")?
                .object_ref())
        };
        assert_eq!(
            nested_metadata_ref(&first_holder)?,
            nested_metadata_ref(&second_holder)?
        );
        Ok(())
    }

    #[test]
    fn canonicalizes_metadata_references_even_when_type_is_missing() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"<x:xmpmeta>producer forgot Type</x:xmpmeta>";
        let first = untyped_metadata_stream(&mut pdf, payload)?;
        let second = untyped_metadata_stream(&mut pdf, payload)?;
        let first_holder = holder(&mut pdf, first)?;
        let second_holder = holder(&mut pdf, second)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestUntypedMetadataA", first_holder.clone())?;
        root.replace_key(b"/TestUntypedMetadataB", second_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_metadata_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            first_holder.try_get_key(b"/Metadata")?.object_ref(),
            second_holder.try_get_key(b"/Metadata")?.object_ref()
        );
        Ok(())
    }

    fn icc_profile(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
        components: i64,
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new ICC stream has no dictionary".to_owned()))?;
        dict.replace_key(b"/N", ObjectHandle::integer(components))?;
        dict.replace_key(
            b"/Alternate",
            ObjectHandle::name(if components == 4 {
                b"DeviceCMYK".to_vec()
            } else {
                b"DeviceRGB".to_vec()
            }),
        )?;
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(stream)
    }

    fn icc_array(profile: ObjectHandle) -> ObjectHandle {
        ObjectHandle::array(vec![ObjectHandle::name(b"ICCBased".to_vec()), profile])
    }

    #[test]
    fn canonicalizes_only_exact_icc_profile_streams() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"identical ICC profile payload";
        let first_profile = icc_profile(&mut pdf, payload, 4)?;
        let second_profile = icc_profile(&mut pdf, payload, 4)?;
        let different_profile = icc_profile(&mut pdf, payload, 3)?;
        let first = icc_array(first_profile);
        let second = pdf.make_indirect_object_handle(icc_array(second_profile))?;
        let different = icc_array(different_profile);
        let color_spaces = ObjectHandle::dictionary(vec![
            (b"/CS1".to_vec(), first.clone()),
            (b"/CS2".to_vec(), second.clone()),
            (b"/CS3".to_vec(), different.clone()),
        ]);
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestColorSpaces", color_spaces)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_icc_profiles(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_ref = first.try_get_array_item(1)?.object_ref();
        let second_ref = second.try_get_array_item(1)?.object_ref();
        let different_ref = different.try_get_array_item(1)?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    fn image_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
        width: i64,
        height: i64,
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new image stream has no dictionary".to_owned()))?;
        for (key, value) in [
            (b"/Type".as_slice(), ObjectHandle::name(b"XObject".to_vec())),
            (
                b"/Subtype".as_slice(),
                ObjectHandle::name(b"Image".to_vec()),
            ),
            (b"/Width".as_slice(), ObjectHandle::integer(width)),
            (b"/Height".as_slice(), ObjectHandle::integer(height)),
            (
                b"/ColorSpace".as_slice(),
                ObjectHandle::name(b"DeviceGray".to_vec()),
            ),
            (b"/BitsPerComponent".as_slice(), ObjectHandle::integer(8)),
        ] {
            dict.replace_key(key, value)?;
        }
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(stream)
    }

    #[test]
    fn canonicalizes_only_exact_image_xobjects_in_resource_dictionaries() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"01234567";
        let first = image_stream(&mut pdf, payload, 4, 2)?;
        let second = image_stream(&mut pdf, payload, 4, 2)?;
        let different_dict = image_stream(&mut pdf, payload, 8, 1)?;
        let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Im1".to_vec(), first),
            (b"/Im2".to_vec(), second),
            (b"/Im3".to_vec(), different_dict),
        ]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/XObject", xobjects.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_image_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_ref = xobjects.try_get_key(b"/Im1")?.object_ref();
        let second_ref = xobjects.try_get_key(b"/Im2")?.object_ref();
        let different_ref = xobjects.try_get_key(b"/Im3")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    #[test]
    fn canonicalizes_exact_image_masks_before_parent_images() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let mask_payload = b"exact-soft-mask";
        let parent_payload = b"exact-parent-image";
        let first_mask = image_stream(&mut pdf, mask_payload, 4, 2)?;
        let second_mask = image_stream(&mut pdf, mask_payload, 4, 2)?;
        let first_parent = image_stream(&mut pdf, parent_payload, 4, 2)?;
        let second_parent = image_stream(&mut pdf, parent_payload, 4, 2)?;
        let different_parent = image_stream(&mut pdf, parent_payload, 4, 2)?;

        for (parent, mask) in [
            (&first_parent, first_mask.clone()),
            (&second_parent, second_mask),
            (&different_parent, first_mask),
        ] {
            let dict = parent
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("parent image has no dictionary".to_owned()))?;
            dict.replace_key(b"/SMask", mask)?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        let different_dict = different_parent
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("parent image has no dictionary".to_owned()))?;
        different_dict.replace_key(
            b"/Intent",
            ObjectHandle::name(b"RelativeColorimetric".to_vec()),
        )?;
        pdf.mark_object_handle_dirty(&different_dict)?;

        let xobjects = ObjectHandle::dictionary(vec![
            (b"/Im1".to_vec(), first_parent),
            (b"/Im2".to_vec(), second_parent),
            (b"/Im3".to_vec(), different_parent),
        ]);
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestImageHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_image_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 2);
        assert_eq!(stats.references_canonicalized, 2);
        assert_eq!(
            stats.duplicate_raw_bytes,
            mask_payload.len() + parent_payload.len()
        );

        let first_ref = xobjects.try_get_key(b"/Im1")?.object_ref();
        let second_ref = xobjects.try_get_key(b"/Im2")?.object_ref();
        let different_ref = xobjects.try_get_key(b"/Im3")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    #[test]
    fn xobject_name_is_ignored_only_after_pdf_1_0() {
        assert!(!xobject_name_is_ignorable("1.0"));
        assert!(xobject_name_is_ignorable("1.1"));
        assert!(xobject_name_is_ignorable("1.7"));
        assert!(xobject_name_is_ignorable("2.0"));
        assert!(!xobject_name_is_ignorable("garbage"));
    }

    #[test]
    fn canonicalizes_post_1_0_images_that_differ_only_by_obsolescent_name() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        assert_eq!(pdf.version(), "1.3");
        let payload = b"same-image-payload";
        let first = image_stream(&mut pdf, payload, 4, 2)?;
        let second = image_stream(&mut pdf, payload, 4, 2)?;
        let different_intent = image_stream(&mut pdf, payload, 4, 2)?;

        for (image, name) in [
            (&first, b"ImA".as_slice()),
            (&second, b"ImB".as_slice()),
            (&different_intent, b"ImC".as_slice()),
        ] {
            let dict = image
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("image has no dictionary".to_owned()))?;
            dict.replace_key(b"/Name", ObjectHandle::name(name.to_vec()))?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        let different_dict = different_intent
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("image has no dictionary".to_owned()))?;
        different_dict.replace_key(
            b"/Intent",
            ObjectHandle::name(b"RelativeColorimetric".to_vec()),
        )?;
        pdf.mark_object_handle_dirty(&different_dict)?;

        let xobjects = ObjectHandle::dictionary(vec![
            (b"/ImA".to_vec(), first),
            (b"/ImB".to_vec(), second),
            (b"/ImC".to_vec(), different_intent),
        ]);
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestImageHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_image_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_ref = xobjects.try_get_key(b"/ImA")?.object_ref();
        let second_ref = xobjects.try_get_key(b"/ImB")?.object_ref();
        let different_ref = xobjects.try_get_key(b"/ImC")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    #[test]
    fn canonicalizes_exact_image_xobjects_nested_in_resources() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"nested-resource-image";
        let first = image_stream(&mut pdf, payload, 4, 2)?;
        let second = image_stream(&mut pdf, payload, 4, 2)?;

        let nested_holder = |name: &[u8], image: ObjectHandle| {
            ObjectHandle::dictionary(vec![(
                b"/Resources".to_vec(),
                ObjectHandle::dictionary(vec![(
                    b"/XObject".to_vec(),
                    ObjectHandle::dictionary(vec![(name.to_vec(), image)]),
                )]),
            )])
        };
        let first_holder = pdf.make_indirect_object_handle(nested_holder(b"/Im1", first))?;
        let second_holder = pdf.make_indirect_object_handle(nested_holder(b"/Im2", second))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestNestedImageA", first_holder.clone())?;
        root.replace_key(b"/TestNestedImageB", second_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_image_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let image_ref = |holder: &ObjectHandle, name: &[u8]| -> Result<Option<ObjectRef>> {
            Ok(holder
                .try_get_key(b"/Resources")?
                .try_get_key(b"/XObject")?
                .try_get_key(name)?
                .object_ref())
        };
        assert_eq!(
            image_ref(&first_holder, b"/Im1")?,
            image_ref(&second_holder, b"/Im2")?
        );
        Ok(())
    }

    fn form_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
        width: i64,
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new form stream has no dictionary".to_owned()))?;
        dict.replace_key(b"/Type", ObjectHandle::name(b"XObject".to_vec()))?;
        dict.replace_key(b"/Subtype", ObjectHandle::name(b"Form".to_vec()))?;
        dict.replace_key(
            b"/BBox",
            ObjectHandle::array(vec![
                ObjectHandle::integer(0),
                ObjectHandle::integer(0),
                ObjectHandle::integer(width),
                ObjectHandle::integer(10),
            ]),
        )?;
        dict.replace_key(b"/Resources", ObjectHandle::dictionary(Vec::new()))?;
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(stream)
    }

    #[test]
    fn canonicalizes_only_exact_form_xobjects_in_resource_dictionaries() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 10 10 re f Q";
        let first = form_stream(&mut pdf, payload, 10)?;
        let second = form_stream(&mut pdf, payload, 10)?;
        let different_dict = form_stream(&mut pdf, payload, 20)?;
        for (form, name) in [
            (&first, b"FmA".as_slice()),
            (&second, b"FmB".as_slice()),
            (&different_dict, b"FmC".as_slice()),
        ] {
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("form stream has no dictionary".to_owned()))?;
            dict.replace_key(b"/Name", ObjectHandle::name(name.to_vec()))?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Fm1".to_vec(), first),
            (b"/Fm2".to_vec(), second),
            (b"/Fm3".to_vec(), different_dict),
        ]))?;
        let resources = ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]);
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            resources,
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFormHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_ref = xobjects.try_get_key(b"/Fm1")?.object_ref();
        let second_ref = xobjects.try_get_key(b"/Fm2")?.object_ref();
        let different_ref = xobjects.try_get_key(b"/Fm3")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    #[test]
    fn canonicalizes_exact_form_font_resources_before_parent_forms() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"BT /F1 10 Tf (same) Tj ET";
        let make_font = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>, base: &[u8]| {
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Font".to_vec())),
                (b"/Subtype".to_vec(), ObjectHandle::name(b"Type1".to_vec())),
                (b"/BaseFont".to_vec(), ObjectHandle::name(base.to_vec())),
            ]))
        };
        let first_font = make_font(&mut pdf, b"Helvetica")?;
        let second_font = make_font(&mut pdf, b"Helvetica")?;
        let different_font = make_font(&mut pdf, b"Courier")?;
        let first = form_stream(&mut pdf, payload, 10)?;
        let second = form_stream(&mut pdf, payload, 10)?;
        let different = form_stream(&mut pdf, payload, 10)?;

        let attach_font = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                           form: &ObjectHandle,
                           font: ObjectHandle|
         -> Result<ObjectHandle> {
            let fonts = ObjectHandle::dictionary(vec![(b"/F1".to_vec(), font)]);
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("form stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![(b"/Font".to_vec(), fonts.clone())]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
            Ok(fonts)
        };
        let first_fonts = attach_font(&mut pdf, &first, first_font)?;
        let second_fonts = attach_font(&mut pdf, &second, second_font)?;
        let different_fonts = attach_font(&mut pdf, &different, different_font)?;

        let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Fm1".to_vec(), first),
            (b"/Fm2".to_vec(), second),
            (b"/Fm3".to_vec(), different),
        ]))?;
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFormFontHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        // Font-resource identity is normalized only for the Form fingerprint;
        // the source resource dictionaries themselves remain untouched.
        assert_ne!(
            first_fonts.try_get_key(b"/F1")?.object_ref(),
            second_fonts.try_get_key(b"/F1")?.object_ref()
        );
        assert_ne!(
            first_fonts.try_get_key(b"/F1")?.object_ref(),
            different_fonts.try_get_key(b"/F1")?.object_ref()
        );
        assert_eq!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm2")?.object_ref()
        );
        assert_ne!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm3")?.object_ref()
        );
        Ok(())
    }

    #[test]
    fn canonicalizes_nested_exact_form_resource_graphs() -> Result<()> {
        let mut pdf = Pdf::empty()?;

        let encoding = || {
            ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Encoding".to_vec())),
                (
                    b"/BaseEncoding".to_vec(),
                    ObjectHandle::name(b"WinAnsiEncoding".to_vec()),
                ),
            ])
        };
        let first_encoding = pdf.make_indirect_object_handle(encoding())?;
        let second_encoding = pdf.make_indirect_object_handle(encoding())?;
        let make_font = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>, encoding: ObjectHandle| {
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Font".to_vec())),
                (b"/Subtype".to_vec(), ObjectHandle::name(b"Type1".to_vec())),
                (
                    b"/BaseFont".to_vec(),
                    ObjectHandle::name(b"Helvetica".to_vec()),
                ),
                (b"/Encoding".to_vec(), encoding),
            ]))
        };
        let first_font = make_font(&mut pdf, first_encoding)?;
        let second_font = make_font(&mut pdf, second_encoding)?;

        let color_space = || {
            ObjectHandle::array(vec![
                ObjectHandle::name(b"Indexed".to_vec()),
                ObjectHandle::name(b"DeviceRGB".to_vec()),
                ObjectHandle::integer(0),
                ObjectHandle::string(b"\0\0\0".to_vec()),
            ])
        };
        let first_color_space = pdf.make_indirect_object_handle(color_space())?;
        let second_color_space = pdf.make_indirect_object_handle(color_space())?;
        let ext_gstate = || {
            ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"ExtGState".to_vec())),
                (b"/OPM".to_vec(), ObjectHandle::integer(1)),
            ])
        };
        let first_ext_gstate = pdf.make_indirect_object_handle(ext_gstate())?;
        let second_ext_gstate = pdf.make_indirect_object_handle(ext_gstate())?;

        let make_image = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                          color_space: ObjectHandle|
         -> Result<ObjectHandle> {
            let image = pdf.new_stream_with_data(Rc::new(vec![0_u8, 1, 2, 3]))?;
            let dict = image
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("image stream has no dictionary".to_owned()))?;
            dict.replace_key(b"/Type", ObjectHandle::name(b"XObject".to_vec()))?;
            dict.replace_key(b"/Subtype", ObjectHandle::name(b"Image".to_vec()))?;
            dict.replace_key(b"/Width", ObjectHandle::integer(1))?;
            dict.replace_key(b"/Height", ObjectHandle::integer(1))?;
            dict.replace_key(b"/BitsPerComponent", ObjectHandle::integer(8))?;
            dict.replace_key(b"/ColorSpace", color_space)?;
            pdf.mark_object_handle_dirty(&dict)?;
            Ok(image)
        };
        let first_image = make_image(&mut pdf, first_color_space.clone())?;
        let second_image = make_image(&mut pdf, second_color_space.clone())?;

        let attach_resources = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                                form: &ObjectHandle,
                                font: ObjectHandle,
                                color_space: ObjectHandle,
                                ext_gstate: ObjectHandle,
                                image: ObjectHandle|
         -> Result<()> {
            let fonts = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/F1".to_vec(),
                font,
            )]))?;
            let colors = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/CS0".to_vec(),
                color_space,
            )]))?;
            let states = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/GS0".to_vec(),
                ext_gstate,
            )]))?;
            let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/Im0".to_vec(),
                image,
            )]))?;
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("form stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![
                    (b"/Font".to_vec(), fonts),
                    (b"/ColorSpace".to_vec(), colors),
                    (b"/ExtGState".to_vec(), states),
                    (b"/XObject".to_vec(), xobjects),
                ]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
            Ok(())
        };

        let inner_payload = b"q /GS0 gs /Im0 Do BT /F1 10 Tf (x) Tj ET Q";
        let first_inner = form_stream(&mut pdf, inner_payload, 10)?;
        let second_inner = form_stream(&mut pdf, inner_payload, 10)?;
        attach_resources(
            &mut pdf,
            &first_inner,
            first_font.clone(),
            first_color_space,
            first_ext_gstate,
            first_image.clone(),
        )?;
        attach_resources(
            &mut pdf,
            &second_inner,
            second_font.clone(),
            second_color_space,
            second_ext_gstate,
            second_image.clone(),
        )?;

        let wrapper_payload = b"q /F Do Q";
        let first_wrapper = form_stream(&mut pdf, wrapper_payload, 10)?;
        let second_wrapper = form_stream(&mut pdf, wrapper_payload, 10)?;
        let different_wrapper = form_stream(&mut pdf, wrapper_payload, 20)?;
        for (wrapper, inner) in [
            (&first_wrapper, first_inner),
            (&second_wrapper, second_inner.clone()),
            (&different_wrapper, second_inner),
        ] {
            let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/F".to_vec(),
                inner,
            )]))?;
            let dict = wrapper
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("wrapper stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects)]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
        }

        let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/W1".to_vec(), first_wrapper),
            (b"/W2".to_vec(), second_wrapper),
            (b"/W3".to_vec(), different_wrapper),
        ]))?;
        let root = pdf.root_handle()?;
        root.replace_key(
            b"/TestNestedFormResources",
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 2);
        assert_eq!(
            stats.duplicate_raw_bytes,
            inner_payload.len() + wrapper_payload.len()
        );
        assert!(stats.references_canonicalized >= 2);
        assert_eq!(
            xobjects.try_get_key(b"/W1")?.object_ref(),
            xobjects.try_get_key(b"/W2")?.object_ref()
        );
        assert_ne!(
            xobjects.try_get_key(b"/W1")?.object_ref(),
            xobjects.try_get_key(b"/W3")?.object_ref()
        );
        // Dependency equivalence stays virtual; leaf objects are not rewritten.
        assert_ne!(first_font.object_ref(), second_font.object_ref());
        assert_ne!(first_image.object_ref(), second_image.object_ref());
        Ok(())
    }

    #[test]
    fn canonicalizes_forms_across_recursive_exact_properties_resources() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let intent = || {
            ObjectHandle::array(vec![
                ObjectHandle::name(b"View".to_vec()),
                ObjectHandle::name(b"Design".to_vec()),
            ])
        };
        let usage = || {
            ObjectHandle::dictionary(vec![(
                b"/CreatorInfo".to_vec(),
                ObjectHandle::dictionary(vec![
                    (
                        b"/Creator".to_vec(),
                        ObjectHandle::string(b"pdf-redox-test".to_vec()),
                    ),
                    (
                        b"/Subtype".to_vec(),
                        ObjectHandle::name(b"Artwork".to_vec()),
                    ),
                ]),
            )])
        };
        let first_intent = pdf.make_indirect_object_handle(intent())?;
        let second_intent = pdf.make_indirect_object_handle(intent())?;
        let first_usage = pdf.make_indirect_object_handle(usage())?;
        let second_usage = pdf.make_indirect_object_handle(usage())?;

        let make_ocg = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                        name: &[u8],
                        intent: ObjectHandle,
                        usage: ObjectHandle|
         -> Result<ObjectHandle> {
            Ok(
                pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                    (b"/Type".to_vec(), ObjectHandle::name(b"OCG".to_vec())),
                    (b"/Name".to_vec(), ObjectHandle::string(name.to_vec())),
                    (b"/Intent".to_vec(), intent),
                    (b"/Usage".to_vec(), usage),
                ]))?,
            )
        };
        let first_ocg = make_ocg(&mut pdf, b"Layer 1", first_intent, first_usage)?;
        let second_ocg = make_ocg(&mut pdf, b"Layer 1", second_intent, second_usage)?;
        let different_intent = pdf.make_indirect_object_handle(intent())?;
        let different_usage = pdf.make_indirect_object_handle(usage())?;
        let different_ocg = make_ocg(&mut pdf, b"Layer 2", different_intent, different_usage)?;

        let payload = b"/OC /MC0 BDC EMC";
        let first = form_stream(&mut pdf, payload, 10)?;
        let second = form_stream(&mut pdf, payload, 10)?;
        let different = form_stream(&mut pdf, payload, 10)?;
        let attach_properties = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                                 form: &ObjectHandle,
                                 ocg: ObjectHandle|
         -> Result<()> {
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("form stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![(
                    b"/Properties".to_vec(),
                    ObjectHandle::dictionary(vec![(b"/MC0".to_vec(), ocg)]),
                )]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
            Ok(())
        };
        attach_properties(&mut pdf, &first, first_ocg.clone())?;
        attach_properties(&mut pdf, &second, second_ocg.clone())?;
        attach_properties(&mut pdf, &different, different_ocg)?;

        let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Fm1".to_vec(), first),
            (b"/Fm2".to_vec(), second),
            (b"/Fm3".to_vec(), different),
        ]))?;
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestPropertiesFormHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm2")?.object_ref()
        );
        assert_ne!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm3")?.object_ref()
        );
        // The exact OCG graph is virtual; the property objects themselves are untouched.
        assert_ne!(first_ocg.object_ref(), second_ocg.object_ref());
        Ok(())
    }

    #[test]
    fn canonicalizes_forms_across_exact_procset_resources() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 10 10 re f Q";
        let first = form_stream(&mut pdf, payload, 10)?;
        let second = form_stream(&mut pdf, payload, 10)?;
        let different = form_stream(&mut pdf, payload, 10)?;

        let make_procset = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>, names: &[&[u8]]| {
            pdf.make_indirect_object_handle(ObjectHandle::array(
                names
                    .iter()
                    .map(|name| ObjectHandle::name(name.to_vec()))
                    .collect(),
            ))
        };
        let first_procset = make_procset(&mut pdf, &[b"PDF"])?;
        let second_procset = make_procset(&mut pdf, &[b"PDF"])?;
        let different_procset = make_procset(&mut pdf, &[b"PDF", b"Text"])?;

        let attach_procset = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                              form: &ObjectHandle,
                              procset: ObjectHandle|
         -> Result<()> {
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("form stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![(b"/ProcSet".to_vec(), procset)]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
            Ok(())
        };
        attach_procset(&mut pdf, &first, first_procset.clone())?;
        attach_procset(&mut pdf, &second, second_procset.clone())?;
        attach_procset(&mut pdf, &different, different_procset)?;

        let xobjects = ObjectHandle::dictionary(vec![
            (b"/Fm1".to_vec(), first),
            (b"/Fm2".to_vec(), second),
            (b"/Fm3".to_vec(), different),
        ]);
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestProcSetFormHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm2")?.object_ref()
        );
        assert_ne!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm3")?.object_ref()
        );
        // ProcSet equivalence is virtual; the duplicate arrays remain distinct.
        assert_ne!(first_procset.object_ref(), second_procset.object_ref());
        Ok(())
    }

    #[test]
    fn canonicalizes_form_icons_against_resource_xobjects() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 10 10 re f Q";
        let resource_form = form_stream(&mut pdf, payload, 10)?;
        let icon_form = form_stream(&mut pdf, payload, 10)?;

        // Keep the first copy in an ordinary /Resources /XObject dictionary so
        // it becomes the canonical Form. The second copy is reachable only
        // through an annotation appearance-characteristics /MK icon entry.
        // All three icon slots accept Form XObjects and must follow the same
        // exact redirect to avoid canonical-order-dependent second-pass wins.
        let xobjects = ObjectHandle::dictionary(vec![(b"/Fm".to_vec(), resource_form)]);
        let resource_holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let mk = ObjectHandle::dictionary(vec![
            (b"/I".to_vec(), icon_form.clone()),
            (b"/RI".to_vec(), icon_form.clone()),
            (b"/IX".to_vec(), icon_form),
        ]);
        let annotation = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Annot".to_vec())),
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Widget".to_vec())),
            (b"/MK".to_vec(), mk.clone()),
        ]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFormResourceHolder", resource_holder)?;
        root.replace_key(b"/TestFormIconHolder", annotation)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(stats.references_canonicalized, 3);

        let canonical_ref = xobjects.try_get_key(b"/Fm")?.object_ref();
        assert_eq!(mk.try_get_key(b"/I")?.object_ref(), canonical_ref);
        assert_eq!(mk.try_get_key(b"/RI")?.object_ref(), canonical_ref);
        assert_eq!(mk.try_get_key(b"/IX")?.object_ref(), canonical_ref);
        Ok(())
    }

    #[test]
    fn does_not_treat_non_widget_mk_as_form_icon_holder() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 10 10 re f Q";
        let resource_form = form_stream(&mut pdf, payload, 10)?;
        let private_form = form_stream(&mut pdf, payload, 10)?;

        let xobjects = ObjectHandle::dictionary(vec![(b"/Fm".to_vec(), resource_form)]);
        let resource_holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects)]),
        )]))?;
        let mk = ObjectHandle::dictionary(vec![(b"/I".to_vec(), private_form.clone())]);
        let private_holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/MK".to_vec(),
            mk.clone(),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFormResourceHolder", resource_holder)?;
        root.replace_key(b"/TestPrivateMkHolder", private_holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(stats.references_canonicalized, 0);
        assert_eq!(
            mk.try_get_key(b"/I")?.object_ref(),
            private_form.object_ref()
        );
        Ok(())
    }

    #[test]
    fn does_not_rewrite_exact_form_fonts_without_parent_form_match() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let make_font = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>| {
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Font".to_vec())),
                (b"/Subtype".to_vec(), ObjectHandle::name(b"Type1".to_vec())),
                (
                    b"/BaseFont".to_vec(),
                    ObjectHandle::name(b"Helvetica".to_vec()),
                ),
            ]))
        };
        let first_font = make_font(&mut pdf)?;
        let second_font = make_font(&mut pdf)?;
        let first = form_stream(&mut pdf, b"BT /F1 10 Tf (one) Tj ET", 10)?;
        let second = form_stream(&mut pdf, b"BT /F1 10 Tf (two) Tj ET", 10)?;

        let attach_font = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                           form: &ObjectHandle,
                           font: ObjectHandle|
         -> Result<ObjectHandle> {
            let fonts = ObjectHandle::dictionary(vec![(b"/F1".to_vec(), font)]);
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("form stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![(b"/Font".to_vec(), fonts.clone())]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
            Ok(fonts)
        };
        let first_fonts = attach_font(&mut pdf, &first, first_font)?;
        let second_fonts = attach_font(&mut pdf, &second, second_font)?;
        let xobjects = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Fm1".to_vec(), first),
            (b"/Fm2".to_vec(), second),
        ]))?;
        let holder = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/Resources".to_vec(),
            ObjectHandle::dictionary(vec![(b"/XObject".to_vec(), xobjects.clone())]),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestUnmatchedFormFontHolder", holder)?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_form_xobjects(&mut pdf)?;
        assert_eq!(stats, TargetedDedupStats::default());
        assert_ne!(
            first_fonts.try_get_key(b"/F1")?.object_ref(),
            second_fonts.try_get_key(b"/F1")?.object_ref()
        );
        assert_ne!(
            xobjects.try_get_key(b"/Fm1")?.object_ref(),
            xobjects.try_get_key(b"/Fm2")?.object_ref()
        );
        Ok(())
    }

    #[test]
    fn canonicalizes_exact_form_appearance_streams_in_state_dictionaries() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 10 10 re f Q";
        let first = form_stream(&mut pdf, payload, 10)?;
        let second = form_stream(&mut pdf, payload, 10)?;
        let different_dict = form_stream(&mut pdf, payload, 20)?;

        let first_states = ObjectHandle::dictionary(vec![
            (b"/Yes".to_vec(), first),
            (b"/Different".to_vec(), different_dict),
        ]);
        let second_states = ObjectHandle::dictionary(vec![(b"/Yes".to_vec(), second)]);
        let first_ap = ObjectHandle::dictionary(vec![(b"/N".to_vec(), first_states.clone())]);
        let second_ap = ObjectHandle::dictionary(vec![(b"/D".to_vec(), second_states.clone())]);
        let first_annot = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Widget".to_vec())),
            (b"/AP".to_vec(), first_ap),
        ]))?;
        let second_annot = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Widget".to_vec())),
            (b"/AP".to_vec(), second_ap),
        ]))?;
        let root = pdf.root_handle()?;
        root.replace_key(
            b"/TestAppearanceAnnotations",
            ObjectHandle::array(vec![first_annot, second_annot]),
        )?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_appearance_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_ref = first_states.try_get_key(b"/Yes")?.object_ref();
        let second_ref = second_states.try_get_key(b"/Yes")?.object_ref();
        let different_ref = first_states.try_get_key(b"/Different")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    #[test]
    fn canonicalizes_graph_equivalent_form_appearance_streams() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let descriptor = |ascent: i64| {
            ObjectHandle::dictionary(vec![
                (
                    b"/Type".to_vec(),
                    ObjectHandle::name(b"FontDescriptor".to_vec()),
                ),
                (b"/FontName".to_vec(), ObjectHandle::name(b"Arial".to_vec())),
                (b"/Ascent".to_vec(), ObjectHandle::integer(ascent)),
            ])
        };
        let first_descriptor = pdf.make_indirect_object_handle(descriptor(905))?;
        let second_descriptor = pdf.make_indirect_object_handle(descriptor(905))?;
        let different_descriptor = pdf.make_indirect_object_handle(descriptor(906))?;
        let make_font = |pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
                         descriptor: ObjectHandle|
         -> Result<ObjectHandle> {
            Ok(
                pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                    (b"/Type".to_vec(), ObjectHandle::name(b"Font".to_vec())),
                    (
                        b"/Subtype".to_vec(),
                        ObjectHandle::name(b"TrueType".to_vec()),
                    ),
                    (b"/BaseFont".to_vec(), ObjectHandle::name(b"Arial".to_vec())),
                    (b"/FontDescriptor".to_vec(), descriptor),
                ]))?,
            )
        };
        let first_font = make_font(&mut pdf, first_descriptor)?;
        let second_font = make_font(&mut pdf, second_descriptor)?;
        let different_font = make_font(&mut pdf, different_descriptor)?;
        let payload = b"BT /F1 12 Tf (same) Tj ET";
        let first = form_stream(&mut pdf, payload, 10)?;
        let second = form_stream(&mut pdf, payload, 10)?;
        let different = form_stream(&mut pdf, payload, 10)?;
        for (form, font) in [
            (&first, first_font),
            (&second, second_font),
            (&different, different_font),
        ] {
            let dict = form
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("appearance stream has no dictionary".to_owned()))?;
            dict.replace_key(
                b"/Resources",
                ObjectHandle::dictionary(vec![(
                    b"/Font".to_vec(),
                    ObjectHandle::dictionary(vec![(b"/F1".to_vec(), font)]),
                )]),
            )?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        let first_ap = ObjectHandle::dictionary(vec![(b"/N".to_vec(), first)]);
        let second_ap = ObjectHandle::dictionary(vec![(b"/N".to_vec(), second)]);
        let different_ap = ObjectHandle::dictionary(vec![(b"/N".to_vec(), different)]);
        let annotations = vec![
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/AP".to_vec(),
                first_ap.clone(),
            )]))?,
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/AP".to_vec(),
                second_ap.clone(),
            )]))?,
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/AP".to_vec(),
                different_ap.clone(),
            )]))?,
        ];
        let root = pdf.root_handle()?;
        root.replace_key(
            b"/TestGraphAppearanceAnnotations",
            ObjectHandle::array(annotations),
        )?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_appearance_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            first_ap.try_get_key(b"/N")?.object_ref(),
            second_ap.try_get_key(b"/N")?.object_ref()
        );
        assert_ne!(
            first_ap.try_get_key(b"/N")?.object_ref(),
            different_ap.try_get_key(b"/N")?.object_ref()
        );
        Ok(())
    }

    #[test]
    fn canonicalizes_exact_direct_form_appearance_streams() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 5 5 re f Q";
        let first = form_stream(&mut pdf, payload, 5)?;
        let second = form_stream(&mut pdf, payload, 5)?;
        let first_ap = ObjectHandle::dictionary(vec![(b"/N".to_vec(), first)]);
        let second_ap = ObjectHandle::dictionary(vec![(b"/N".to_vec(), second)]);
        let first_annot = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/AP".to_vec(),
            first_ap.clone(),
        )]))?;
        let second_annot = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
            b"/AP".to_vec(),
            second_ap.clone(),
        )]))?;
        let root = pdf.root_handle()?;
        root.replace_key(
            b"/TestDirectAppearanceAnnotations",
            ObjectHandle::array(vec![first_annot, second_annot]),
        )?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_appearance_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            first_ap.try_get_key(b"/N")?.object_ref(),
            second_ap.try_get_key(b"/N")?.object_ref()
        );
        Ok(())
    }

    fn add_page_with_contents(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        contents: ObjectHandle,
        resource_marker: i64,
    ) -> Result<ObjectHandle> {
        let catalog = pdf.root_handle()?;
        let pages = catalog.try_get_key(b"/Pages")?;
        pdf.resolve(&pages)?;
        let page = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
            (b"/Parent".to_vec(), pages.clone()),
            (
                b"/MediaBox".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(612),
                    ObjectHandle::integer(792),
                ]),
            ),
            (
                b"/Resources".to_vec(),
                ObjectHandle::dictionary(vec![(
                    b"/Marker".to_vec(),
                    ObjectHandle::integer(resource_marker),
                )]),
            ),
            (b"/Contents".to_vec(), contents),
        ]))?;
        let kids = pages.try_get_key(b"/Kids")?;
        let mut page_handles = if kids.try_is_array()? {
            kids.try_get_array_as_vector()?
        } else {
            Vec::new()
        };
        page_handles.push(page.clone());
        pages.replace_key(b"/Kids", ObjectHandle::array(page_handles.clone()))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(page_handles.len() as i64))?;
        pdf.mark_object_handle_dirty(&pages)?;
        Ok(page)
    }

    #[test]
    fn canonicalizes_exact_direct_page_content_streams_across_resource_contexts() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q /Im0 Do Q";
        let first = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let second = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let different_dict = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let different_dict_handle = different_dict
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("content stream has no dictionary".to_owned()))?;
        different_dict_handle.replace_key(b"/Custom", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&different_dict_handle)?;

        let first_page = add_page_with_contents(&mut pdf, first, 1)?;
        let second_page = add_page_with_contents(&mut pdf, second, 2)?;
        let different_page = add_page_with_contents(&mut pdf, different_dict, 3)?;

        let stats = canonicalize_page_contents(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            first_page.try_get_key(b"/Contents")?.object_ref(),
            second_page.try_get_key(b"/Contents")?.object_ref()
        );
        assert_ne!(
            first_page.try_get_key(b"/Contents")?.object_ref(),
            different_page.try_get_key(b"/Contents")?.object_ref()
        );
        Ok(())
    }

    #[test]
    fn canonicalizes_exact_page_content_streams_inside_arrays_without_reordering() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 1 0 0 1 0 0 cm Q";
        let first = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let second = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let unique = pdf.new_stream_with_data(Rc::new(b"BT ET".to_vec()))?;
        let first_array = ObjectHandle::array(vec![first, unique.clone()]);
        let second_array = ObjectHandle::array(vec![second]);
        let first_page = add_page_with_contents(&mut pdf, first_array, 1)?;
        let second_page = add_page_with_contents(&mut pdf, second_array, 2)?;

        let stats = canonicalize_page_contents(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_contents = first_page.try_get_key(b"/Contents")?;
        let second_contents = second_page.try_get_key(b"/Contents")?;
        let first_items = first_contents.try_get_array_as_vector()?;
        let second_items = second_contents.try_get_array_as_vector()?;
        assert_eq!(first_items.len(), 2);
        assert_eq!(second_items.len(), 1);
        assert_eq!(first_items[0].object_ref(), second_items[0].object_ref());
        assert_eq!(first_items[1].object_ref(), unique.object_ref());
        Ok(())
    }

    #[test]
    fn canonicalizes_page_content_streams_on_detached_page_dictionaries() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"q 0 0 20 20 re f Q";
        let live_stream = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let detached_stream = pdf.new_stream_with_data(Rc::new(payload.to_vec()))?;
        let live_page = add_page_with_contents(&mut pdf, live_stream, 1)?;

        let detached_page = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
            (
                b"/MediaBox".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(612),
                    ObjectHandle::integer(792),
                ]),
            ),
            (
                b"/Resources".to_vec(),
                ObjectHandle::dictionary(vec![(b"/Marker".to_vec(), ObjectHandle::integer(2))]),
            ),
            (b"/Contents".to_vec(), detached_stream),
        ]))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/DetachedTestPage", detached_page.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_page_contents(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            live_page.try_get_key(b"/Contents")?.object_ref(),
            detached_page.try_get_key(b"/Contents")?.object_ref()
        );
        assert_ne!(
            live_page.try_get_key(b"/Resources")?.unparse_resolved(),
            detached_page.try_get_key(b"/Resources")?.unparse_resolved()
        );
        Ok(())
    }

    fn type3_charproc_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
        extra_key: Option<(&[u8], ObjectHandle)>,
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        if let Some((key, value)) = extra_key {
            let dict = stream.as_stream_dict().ok_or_else(|| {
                Error::Invalid("new CharProc stream has no dictionary".to_owned())
            })?;
            dict.replace_key(key, value)?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        Ok(stream)
    }

    #[test]
    fn canonicalizes_exact_type3_charprocs_across_font_resource_contexts() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"500 0 0 0 500 700 d1 0 0 500 700 re f";
        let first = type3_charproc_stream(&mut pdf, payload, None)?;
        let second = type3_charproc_stream(&mut pdf, payload, None)?;
        let different_dict = type3_charproc_stream(
            &mut pdf,
            payload,
            Some((b"/PrivateMarker", ObjectHandle::integer(1))),
        )?;

        let first_font = ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Type3".to_vec())),
            (
                b"/Resources".to_vec(),
                ObjectHandle::dictionary(vec![(
                    b"/Context".to_vec(),
                    ObjectHandle::name(b"First".to_vec()),
                )]),
            ),
            (
                b"/CharProcs".to_vec(),
                ObjectHandle::dictionary(vec![
                    (b"/A".to_vec(), first),
                    (b"/Different".to_vec(), different_dict),
                ]),
            ),
        ]);
        let second_font = ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Type3".to_vec())),
            (
                b"/Resources".to_vec(),
                ObjectHandle::dictionary(vec![(
                    b"/Context".to_vec(),
                    ObjectHandle::name(b"Second".to_vec()),
                )]),
            ),
            (
                b"/CharProcs".to_vec(),
                ObjectHandle::dictionary(vec![(b"/B".to_vec(), second)]),
            ),
        ]);
        let fonts = ObjectHandle::dictionary(vec![
            (b"/F1".to_vec(), first_font),
            (b"/F2".to_vec(), second_font),
        ]);
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestType3Fonts", fonts.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_type3_charprocs(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let first_ref = fonts
            .try_get_key(b"/F1")?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/A")?
            .object_ref();
        let second_ref = fonts
            .try_get_key(b"/F2")?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/B")?
            .object_ref();
        let different_ref = fonts
            .try_get_key(b"/F1")?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/Different")?
            .object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }

    fn serialized_type3_charproc_dedup_fixture() -> Result<Vec<u8>> {
        let mut pdf = Pdf::empty()?;
        let payload = b"500 0 0 0 500 700 d1 0 0 500 700 re f";
        let first = type3_charproc_stream(&mut pdf, payload, None)?;
        let second = type3_charproc_stream(&mut pdf, payload, None)?;
        let third = type3_charproc_stream(&mut pdf, payload, None)?;
        let different = type3_charproc_stream(
            &mut pdf,
            payload,
            Some((b"/PrivateMarker", ObjectHandle::integer(1))),
        )?;

        let first_font = ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Type3".to_vec())),
            (
                b"/CharProcs".to_vec(),
                ObjectHandle::dictionary(vec![
                    (b"/A".to_vec(), first),
                    (b"/Different".to_vec(), different),
                ]),
            ),
        ]);
        let second_font = ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Type3".to_vec())),
            (
                b"/CharProcs".to_vec(),
                ObjectHandle::dictionary(vec![(b"/B".to_vec(), second)]),
            ),
        ]);
        let third_charprocs = pdf
            .make_indirect_object_handle(ObjectHandle::dictionary(vec![(b"/C".to_vec(), third)]))?;
        let third_font = ObjectHandle::dictionary(vec![
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Type3".to_vec())),
            (b"/CharProcs".to_vec(), third_charprocs),
        ]);
        let fonts = ObjectHandle::dictionary(vec![
            (b"/F1".to_vec(), first_font),
            (b"/Nested".to_vec(), ObjectHandle::array(vec![second_font])),
            (b"/F3".to_vec(), third_font),
        ]);
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestType3Fonts", fonts)?;
        pdf.mark_object_handle_dirty(&root)?;

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.write()?;
        Ok(writer.get_buffer()?)
    }

    #[test]
    fn hayro_type3_charproc_dedup_matches_flpdf_for_direct_and_indirect_charprocs() -> Result<()> {
        let input = serialized_type3_charproc_dedup_fixture()?;
        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = canonicalize_type3_charprocs(&mut flpdf)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = canonicalize_type3_charprocs_hayro(&mut document)?;
        assert_eq!(actual, expected);
        assert_eq!(actual.duplicate_streams_detected, 2);
        assert_eq!(actual.references_canonicalized, 2);

        let output = document.write_compact()?;
        let mut reparsed = Pdf::open(Cursor::new(output))?;
        let fonts = reparsed.root_handle()?.try_get_key(b"/TestType3Fonts")?;
        let first = fonts
            .try_get_key(b"/F1")?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/A")?
            .object_ref();
        let second = fonts
            .try_get_key(b"/Nested")?
            .try_get_array_item(0)?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/B")?
            .object_ref();
        let third = fonts
            .try_get_key(b"/F3")?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/C")?
            .object_ref();
        let different = fonts
            .try_get_key(b"/F1")?
            .try_get_key(b"/CharProcs")?
            .try_get_key(b"/Different")?
            .object_ref();
        assert_eq!(first, second);
        assert_eq!(first, third);
        assert_ne!(first, different);
        Ok(())
    }

    fn to_unicode_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
        extra_key: Option<(&[u8], ObjectHandle)>,
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        if let Some((key, value)) = extra_key {
            let dict = stream.as_stream_dict().ok_or_else(|| {
                Error::Invalid("new ToUnicode stream has no dictionary".to_owned())
            })?;
            dict.replace_key(key, value)?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        Ok(stream)
    }

    #[test]
    fn canonicalizes_only_exact_to_unicode_cmaps() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"/CIDInit /ProcSet findresource begin end";
        let first = to_unicode_stream(&mut pdf, payload, None)?;
        let second = to_unicode_stream(&mut pdf, payload, None)?;
        let different_dict = to_unicode_stream(
            &mut pdf,
            payload,
            Some((b"/UseCMap", ObjectHandle::name(b"Identity-H".to_vec()))),
        )?;

        let holder =
            |cmap: ObjectHandle| ObjectHandle::dictionary(vec![(b"/ToUnicode".to_vec(), cmap)]);
        let first_holder = pdf.make_indirect_object_handle(holder(first))?;
        let second_holder = pdf.make_indirect_object_handle(holder(second))?;
        let different_holder = pdf.make_indirect_object_handle(holder(different_dict))?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestToUnicodeA", first_holder.clone())?;
        root.replace_key(b"/TestToUnicodeB", second_holder.clone())?;
        root.replace_key(b"/TestToUnicodeDifferent", different_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_to_unicode_cmaps(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());

        let cmap_ref = |holder: &ObjectHandle| -> Result<Option<ObjectRef>> {
            Ok(holder.try_get_key(b"/ToUnicode")?.object_ref())
        };
        let canonical = cmap_ref(&first_holder)?;
        assert_eq!(canonical, cmap_ref(&second_holder)?);
        assert_ne!(canonical, cmap_ref(&different_holder)?);
        Ok(())
    }

    #[test]
    fn canonicalizes_to_unicode_nested_in_direct_font_dictionary() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"exact direct font cmap";
        let first = to_unicode_stream(&mut pdf, payload, None)?;
        let second = to_unicode_stream(&mut pdf, payload, None)?;
        let fonts = ObjectHandle::dictionary(vec![
            (
                b"/F1".to_vec(),
                ObjectHandle::dictionary(vec![(b"/ToUnicode".to_vec(), first)]),
            ),
            (
                b"/F2".to_vec(),
                ObjectHandle::dictionary(vec![(b"/ToUnicode".to_vec(), second)]),
            ),
        ]);
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFonts", fonts.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_to_unicode_cmaps(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            fonts
                .try_get_key(b"/F1")?
                .try_get_key(b"/ToUnicode")?
                .object_ref(),
            fonts
                .try_get_key(b"/F2")?
                .try_get_key(b"/ToUnicode")?
                .object_ref()
        );
        Ok(())
    }

    fn font_program(pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>, data: &[u8]) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new font stream has no dictionary".to_owned()))?;
        dict.replace_key(b"/Length1", ObjectHandle::integer(data.len() as i64))?;
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(stream)
    }

    fn font_descriptor(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        key: &[u8],
        font_program: ObjectHandle,
    ) -> Result<ObjectHandle> {
        Ok(
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                key.to_vec(),
                font_program,
            )]))?,
        )
    }

    fn serialized_to_unicode_dedup_fixture() -> Result<Vec<u8>> {
        let mut pdf = Pdf::empty()?;
        let payload = b"exact direct font cmap";
        let first = to_unicode_stream(&mut pdf, payload, None)?;
        let second = to_unicode_stream(&mut pdf, payload, None)?;
        let different = to_unicode_stream(
            &mut pdf,
            payload,
            Some((b"/UseCMap", ObjectHandle::name(b"Identity-H".to_vec()))),
        )?;

        let fonts = ObjectHandle::dictionary(vec![
            (
                b"/F1".to_vec(),
                ObjectHandle::dictionary(vec![(b"/ToUnicode".to_vec(), first)]),
            ),
            (
                b"/Nested".to_vec(),
                ObjectHandle::array(vec![ObjectHandle::dictionary(vec![(
                    b"/ToUnicode".to_vec(),
                    second,
                )])]),
            ),
            (
                b"/Different".to_vec(),
                ObjectHandle::dictionary(vec![(b"/ToUnicode".to_vec(), different)]),
            ),
        ]);
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFonts", fonts)?;
        pdf.mark_object_handle_dirty(&root)?;

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.write()?;
        Ok(writer.get_buffer()?)
    }

    #[test]
    fn hayro_to_unicode_dedup_matches_flpdf_for_nested_direct_holders() -> Result<()> {
        let input = serialized_to_unicode_dedup_fixture()?;
        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = canonicalize_to_unicode_cmaps(&mut flpdf)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = canonicalize_to_unicode_cmaps_hayro(&mut document)?;
        assert_eq!(actual, expected);
        assert_eq!(actual.duplicate_streams_detected, 1);
        assert_eq!(actual.references_canonicalized, 1);

        let output = document.write_compact()?;
        let mut reparsed = Pdf::open(Cursor::new(output))?;
        let fonts = reparsed.root_handle()?.try_get_key(b"/TestFonts")?;
        let first = fonts
            .try_get_key(b"/F1")?
            .try_get_key(b"/ToUnicode")?
            .object_ref();
        let nested = fonts
            .try_get_key(b"/Nested")?
            .try_get_array_item(0)?
            .try_get_key(b"/ToUnicode")?
            .object_ref();
        let different = fonts
            .try_get_key(b"/Different")?
            .try_get_key(b"/ToUnicode")?
            .object_ref();
        assert_eq!(first, nested);
        assert_ne!(first, different);
        Ok(())
    }

    fn serialized_font_program_dedup_fixture() -> Result<Vec<u8>> {
        let mut pdf = Pdf::empty()?;
        let payload = b"same-font-program";
        let first = font_program(&mut pdf, payload)?;
        let second = font_program(&mut pdf, payload)?;
        let different_key = font_program(&mut pdf, payload)?;
        let different_dict = font_program(&mut pdf, payload)?;

        for stream in [&first, &second] {
            let length =
                pdf.make_indirect_object_handle(ObjectHandle::integer(payload.len() as i64))?;
            let dict = stream
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("font stream has no dictionary".to_owned()))?;
            dict.replace_key(b"/Length1", length)?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        let different_dict_handle = different_dict
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("font stream has no dictionary".to_owned()))?;
        different_dict_handle.replace_key(b"/Custom", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&different_dict_handle)?;

        let first_descriptor = font_descriptor(&mut pdf, b"/FontFile2", first)?;
        let second_descriptor = font_descriptor(&mut pdf, b"/FontFile2", second)?;
        let different_key_descriptor = font_descriptor(&mut pdf, b"/FontFile3", different_key)?;
        let different_dict_descriptor = font_descriptor(&mut pdf, b"/FontFile2", different_dict)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFontA", first_descriptor)?;
        root.replace_key(b"/TestFontB", second_descriptor)?;
        root.replace_key(b"/TestFontC", different_key_descriptor)?;
        root.replace_key(b"/TestFontD", different_dict_descriptor)?;
        pdf.mark_object_handle_dirty(&root)?;

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.write()?;
        Ok(writer.get_buffer()?)
    }

    #[test]
    fn hayro_font_program_dedup_matches_flpdf() -> Result<()> {
        let input = serialized_font_program_dedup_fixture()?;
        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = canonicalize_font_program_streams(&mut flpdf)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = canonicalize_font_program_streams_hayro(&mut document)?;
        assert_eq!(actual, expected);
        assert_eq!(actual.duplicate_streams_detected, 1);
        assert_eq!(actual.references_canonicalized, 1);
        assert!(actual.duplicate_raw_bytes >= b"same-font-program".len());

        let output = document.write_compact()?;
        let mut reparsed = Pdf::open(Cursor::new(output))?;
        let root = reparsed.root_handle()?;
        let first = root
            .try_get_key(b"/TestFontA")?
            .try_get_key(b"/FontFile2")?
            .object_ref();
        let second = root
            .try_get_key(b"/TestFontB")?
            .try_get_key(b"/FontFile2")?
            .object_ref();
        let different_key = root
            .try_get_key(b"/TestFontC")?
            .try_get_key(b"/FontFile3")?
            .object_ref();
        let different_dict = root
            .try_get_key(b"/TestFontD")?
            .try_get_key(b"/FontFile2")?
            .object_ref();
        assert_eq!(first, second);
        assert_ne!(first, different_key);
        assert_ne!(first, different_dict);
        Ok(())
    }

    #[test]
    fn canonicalizes_font_programs_only_with_same_fontfile_kind_and_dictionary() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let first = font_program(&mut pdf, b"same-font-program")?;
        let second = font_program(&mut pdf, b"same-font-program")?;
        let different_key = font_program(&mut pdf, b"same-font-program")?;
        let different_dict = font_program(&mut pdf, b"same-font-program")?;
        let different_dict_handle = different_dict
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("font stream has no dictionary".to_owned()))?;
        different_dict_handle.replace_key(b"/Custom", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&different_dict_handle)?;

        let first_descriptor = font_descriptor(&mut pdf, b"/FontFile2", first)?;
        let second_descriptor = font_descriptor(&mut pdf, b"/FontFile2", second)?;
        let different_key_descriptor = font_descriptor(&mut pdf, b"/FontFile3", different_key)?;
        let different_dict_descriptor = font_descriptor(&mut pdf, b"/FontFile2", different_dict)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestFontA", first_descriptor.clone())?;
        root.replace_key(b"/TestFontB", second_descriptor.clone())?;
        root.replace_key(b"/TestFontC", different_key_descriptor.clone())?;
        root.replace_key(b"/TestFontD", different_dict_descriptor.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_font_program_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);

        let first_ref = first_descriptor.try_get_key(b"/FontFile2")?.object_ref();
        let second_ref = second_descriptor.try_get_key(b"/FontFile2")?.object_ref();
        let different_key_ref = different_key_descriptor
            .try_get_key(b"/FontFile3")?
            .object_ref();
        let different_dict_ref = different_dict_descriptor
            .try_get_key(b"/FontFile2")?
            .object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_key_ref);
        assert_ne!(first_ref, different_dict_ref);
        Ok(())
    }

    #[test]
    fn canonicalizes_font_programs_with_equivalent_indirect_length_values() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let payload = b"same-font-program";
        let first = font_program(&mut pdf, payload)?;
        let second = font_program(&mut pdf, payload)?;
        let different_length = font_program(&mut pdf, payload)?;

        for stream in [&first, &second] {
            let length =
                pdf.make_indirect_object_handle(ObjectHandle::integer(payload.len() as i64))?;
            let dict = stream
                .as_stream_dict()
                .ok_or_else(|| Error::Invalid("font stream has no dictionary".to_owned()))?;
            dict.replace_key(b"/Length1", length)?;
            pdf.mark_object_handle_dirty(&dict)?;
        }
        let different_length_ref =
            pdf.make_indirect_object_handle(ObjectHandle::integer(payload.len() as i64 + 1))?;
        let different_length_dict = different_length
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("font stream has no dictionary".to_owned()))?;
        different_length_dict.replace_key(b"/Length1", different_length_ref)?;
        pdf.mark_object_handle_dirty(&different_length_dict)?;

        let first_descriptor = font_descriptor(&mut pdf, b"/FontFile2", first)?;
        let second_descriptor = font_descriptor(&mut pdf, b"/FontFile2", second)?;
        let different_descriptor = font_descriptor(&mut pdf, b"/FontFile2", different_length)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestIndirectLengthA", first_descriptor.clone())?;
        root.replace_key(b"/TestIndirectLengthB", second_descriptor.clone())?;
        root.replace_key(
            b"/TestIndirectLengthDifferent",
            different_descriptor.clone(),
        )?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_font_program_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        assert_eq!(stats.duplicate_raw_bytes, payload.len());
        assert_eq!(
            first_descriptor.try_get_key(b"/FontFile2")?.object_ref(),
            second_descriptor.try_get_key(b"/FontFile2")?.object_ref()
        );
        assert_ne!(
            first_descriptor.try_get_key(b"/FontFile2")?.object_ref(),
            different_descriptor
                .try_get_key(b"/FontFile2")?
                .object_ref()
        );
        Ok(())
    }
}
