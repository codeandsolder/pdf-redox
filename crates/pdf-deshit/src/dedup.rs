use crate::Result;
use flpdf::{ObjectHandle, ObjectRef, Pdf};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TargetedDedupStats {
    pub duplicate_streams_detected: usize,
    pub duplicate_raw_bytes: usize,
    pub references_canonicalized: usize,
}

fn stream_fingerprint(object: &ObjectHandle, domain: &[u8]) -> Result<Option<[u8; 32]>> {
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
    // stream-dictionary entry exactly as represented.
    let Some(entries) = dict.as_dictionary() else {
        return Ok(None);
    };
    let dictionary = ObjectHandle::dictionary(
        entries
            .into_iter()
            .filter(|(key, _)| key.as_slice() != b"/Length")
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

fn metadata_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_metadata_holders(object, true, &mut holders);
    }
    holders
}

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

fn icc_arrays(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut arrays = Vec::new();
    for object in objects {
        collect_direct_icc_arrays(object, true, &mut arrays);
    }
    arrays
}

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

fn xobject_fingerprint(
    object: &ObjectHandle,
    subtype_name: &[u8],
    domain: &[u8],
) -> Result<Option<[u8; 32]>> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok(None);
    };
    let subtype = dict.try_get_key(b"/Subtype")?;
    if !subtype.try_is_name_and_equals(subtype_name)? {
        return Ok(None);
    }
    stream_fingerprint(object, domain)
}

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

fn xobject_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_xobject_holders(object, true, &mut holders);
    }
    holders
}

fn canonicalize_xobject_subtype<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    subtype_name: &[u8],
    domain: &[u8],
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let holders = xobject_holders(&objects);
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_raw_bytes = 0_usize;

    for object in &objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        let Ok(Some(fingerprint)) = xobject_fingerprint(object, subtype_name, domain) else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            redirects.insert(object_ref, canonical_ref);
            duplicate_raw_bytes += object.get_raw_stream_data()?.len();
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }

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

    Ok(TargetedDedupStats {
        duplicate_streams_detected: redirects.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

pub(crate) fn canonicalize_image_xobjects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    canonicalize_xobject_subtype(pdf, b"Image", b"image-xobject")
}

pub(crate) fn canonicalize_form_xobjects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    canonicalize_xobject_subtype(pdf, b"Form", b"form-xobject")
}

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

pub(crate) fn canonicalize_appearance_streams<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<TargetedDedupStats> {
    let objects = pdf.get_all_objects()?;
    let holders = appearance_holders(pdf, &objects);
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
            let Ok(Some(fingerprint)) =
                xobject_fingerprint(&appearance, b"Form", b"appearance-stream")
            else {
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

fn to_unicode_holders(objects: &[ObjectHandle]) -> Vec<ObjectHandle> {
    let mut holders = Vec::new();
    for object in objects {
        collect_direct_to_unicode_holders(object, true, &mut holders);
    }
    holders
}

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

const FONT_FILE_KEYS: [&[u8]; 3] = [b"/FontFile", b"/FontFile2", b"/FontFile3"];

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
            let Ok(Some(fingerprint)) = stream_fingerprint(&font_program, key) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
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
}
