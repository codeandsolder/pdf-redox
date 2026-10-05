use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    StreamData, source::CurrentObject,
};
use hayro_syntax::{
    PdfVersion,
    object::{MaybeRef as HayroMaybeRef, Name as HayroName, Object as HayroObject},
};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    hash::Hash,
};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TargetedDedupStats {
    pub duplicate_streams_detected: usize,
    pub duplicate_raw_bytes: usize,
    pub references_canonicalized: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExactObjectDedupStats {
    pub duplicate_objects_detected: usize,
    pub references_canonicalized: usize,
}

struct FingerprintRedirectPlan<K> {
    canonical_by_fingerprint: HashMap<[u8; 32], CowObjectHandle>,
    redirects: HashMap<K, CowObjectHandle>,
    duplicate_refs: HashSet<CowObjectHandle>,
    duplicate_raw_bytes: usize,
}

impl<K> FingerprintRedirectPlan<K> {
    fn new() -> Self {
        Self {
            canonical_by_fingerprint: HashMap::new(),
            redirects: HashMap::new(),
            duplicate_refs: HashSet::new(),
            duplicate_raw_bytes: 0,
        }
    }

    fn targeted_stats(&self, references_canonicalized: usize) -> TargetedDedupStats {
        TargetedDedupStats {
            duplicate_streams_detected: self.duplicate_refs.len(),
            duplicate_raw_bytes: self.duplicate_raw_bytes,
            references_canonicalized,
        }
    }
}

impl<K: Eq + Hash> FingerprintRedirectPlan<K> {
    fn observe(&mut self, redirect_key: K, target: CowObjectHandle, fingerprint: [u8; 32]) -> bool {
        if let Some(canonical) = self.canonical_by_fingerprint.get(&fingerprint).copied() {
            if canonical != target {
                self.redirects.insert(redirect_key, canonical);
                return true;
            }
        } else {
            self.canonical_by_fingerprint.insert(fingerprint, target);
        }
        false
    }

    fn observe_stream(
        &mut self,
        redirect_key: K,
        target: CowObjectHandle,
        fingerprint: [u8; 32],
        raw_bytes: usize,
    ) {
        if self.observe(redirect_key, target, fingerprint) && self.duplicate_refs.insert(target) {
            self.duplicate_raw_bytes += raw_bytes;
        }
    }

    fn redirect(&self, key: &K) -> Option<CowObjectHandle> {
        self.redirects.get(key).copied()
    }

    fn into_redirects(self) -> HashMap<K, CowObjectHandle> {
        self.redirects
    }
}

const HAYRO_FONT_FILE_KEYS: [&[u8]; 3] = [b"FontFile", b"FontFile2", b"FontFile3"];

fn hayro_font_program_holders(document: &EditDocument) -> Result<Vec<DirectReferenceHolder>> {
    let mut holders = Vec::new();
    for key in HAYRO_FONT_FILE_KEYS {
        holders.extend(hayro_direct_reference_holders(document, key)?);
    }
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

    let is_semantic = |key: &[u8]| key != b"Length" && !ignored_dictionary_keys.contains(&key);
    let semantic_entry_count = dictionary
        .keys()
        .filter(|key| is_semantic(key.as_slice()))
        .count();
    hasher.update((semantic_entry_count as u64).to_le_bytes());
    for (key, value) in dictionary
        .iter()
        .filter(|(key, _)| is_semantic(key.as_slice()))
    {
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

pub fn canonicalize_font_program_streams_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = hayro_font_program_holders(document)?;
    let mut plan = FingerprintRedirectPlan::new();

    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_font_program_fingerprint(document, holder.target, &holder.key)?
        else {
            continue;
        };
        plan.observe_stream(
            (holder.key.clone(), holder.target),
            holder.target,
            fingerprint,
            raw_bytes,
        );
    }

    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = plan.redirect(&(holder.key.clone(), holder.target)) else {
            continue;
        };
        if rewrite_direct_reference_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }

    Ok(plan.targeted_stats(references_canonicalized))
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
        CowObjectHandle::New(id) => document.edit_added_object(id)?,
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
    let mut plan = FingerprintRedirectPlan::new();
    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, holder.target, domain)?
        else {
            continue;
        };
        plan.observe_stream(holder.target, holder.target, fingerprint, raw_bytes);
    }
    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = plan.redirect(&holder.target) else {
            continue;
        };
        if rewrite_direct_reference_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(plan.targeted_stats(references_canonicalized))
}

pub fn canonicalize_metadata_streams_hayro(
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
        CowObjectHandle::New(id) => document.edit_added_object(id)?,
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

pub fn canonicalize_icc_profiles_hayro(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let holders = hayro_icc_array_holders(document)?;
    let mut plan = FingerprintRedirectPlan::new();
    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, holder.target, b"icc-profile")?
        else {
            continue;
        };
        plan.observe_stream(holder.target, holder.target, fingerprint, raw_bytes);
    }
    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = plan.redirect(&holder.target) else {
            continue;
        };
        if rewrite_direct_array_reference_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(plan.targeted_stats(references_canonicalized))
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
/// whole-xref enumeration. On damaged or oddly indexed PDFs Hayro
/// can therefore find additional real `/ToUnicode` holders that the legacy
/// pass skipped; every rewrite still requires an exact stream fingerprint.
pub fn canonicalize_to_unicode_cmaps_hayro(
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
        CowObjectHandle::New(id) => document.edit_added_object(id)?,
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

pub fn canonicalize_type3_charprocs_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = hayro_type3_glyph_holders(document)?;
    let mut plan = FingerprintRedirectPlan::new();

    for holder in &holders {
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, holder.glyph, b"type3-charproc")?
        else {
            continue;
        };
        plan.observe_stream(holder.glyph, holder.glyph, fingerprint, raw_bytes);
    }

    let mut references_canonicalized = 0_usize;
    for holder in &holders {
        let Some(canonical) = plan.redirect(&holder.glyph) else {
            continue;
        };
        if rewrite_type3_glyph_holder(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }

    Ok(plan.targeted_stats(references_canonicalized))
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
    document.reachable_streams_with_subtype(subtype)
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
    let mut plan = FingerprintRedirectPlan::new();
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
        if plan.observe(stream, stream, fingerprint) && duplicate_refs.insert(stream) {
            *duplicate_raw_bytes += raw_bytes;
        }
    }
    Ok(plan.into_redirects())
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
        CowObjectHandle::New(id) => document.edit_added_object(id)?,
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

fn inspect_hayro_dictionary_target_dictionary(
    root: CowObjectHandle,
    dictionary: &hayro_syntax::object::Dict<'_>,
    key: &[u8],
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) {
    if let Some(target) = dictionary.get_ref(key) {
        targets.insert(DirectDictionaryTarget {
            root: CowObjectHandle::Existing(target.into()),
            path: Vec::new(),
        });
    } else if matches!(
        dictionary.get_raw::<HayroObject<'_>>(key),
        Some(HayroMaybeRef::NotRef(HayroObject::Dict(_)))
    ) {
        let mut target_path = path.clone();
        target_path.push(DirectPathStep::DictKey(key.to_vec()));
        targets.insert(DirectDictionaryTarget {
            root,
            path: target_path,
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
        inspect_hayro_dictionary_target(root, &value, key, path, targets);
        path.pop();
    }
}

fn inspect_hayro_dictionary_target(
    root: CowObjectHandle,
    object: &HayroObject<'_>,
    key: &[u8],
    path: &mut Vec<DirectPathStep>,
    targets: &mut BTreeSet<DirectDictionaryTarget>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            inspect_hayro_dictionary_target_dictionary(root, dictionary, key, path, targets);
        }
        HayroObject::Stream(stream) => {
            inspect_hayro_dictionary_target_dictionary(root, stream.dict(), key, path, targets);
        }
        HayroObject::Array(values) => {
            for (index, value) in values.raw_iter().enumerate() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_hayro_dictionary_target(root, &value, key, path, targets);
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
    document.walk_output_objects(|root, object| {
        match object {
            CurrentObject::Source(object) => {
                inspect_hayro_dictionary_target(root, &object, key, &mut Vec::new(), &mut targets);
            }
            CurrentObject::Owned(object) => {
                inspect_owned_dictionary_target(root, object, key, &mut Vec::new(), &mut targets);
            }
        }
        Ok(())
    })?;
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
            CowObjectHandle::New(id) => document.edit_added_object(id)?,
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

pub fn canonicalize_image_xobjects_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let images = reachable_streams_with_subtype(document, b"Image")?;
    if !stream_payloads_may_repeat(document, &images)? {
        return Ok(TargetedDedupStats::default());
    }
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
        let mut plan = FingerprintRedirectPlan::new();
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
            plan.observe(handle, handle, fingerprint);
        }
        let next = plan.into_redirects();
        if next == redirects {
            return Ok(next);
        }
        redirects = next;
    }
    Ok(redirects)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RedirectReferenceHolder {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
    target: CowObjectHandle,
}

fn inspect_hayro_redirect_reference_holders(
    root: CowObjectHandle,
    object: &HayroObject<'_>,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    path: &mut Vec<DirectPathStep>,
    holders: &mut Vec<RedirectReferenceHolder>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            for (name, value) in dictionary.entries() {
                path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
                match value {
                    HayroMaybeRef::Ref(target) => {
                        let target = CowObjectHandle::Existing(target.into());
                        if redirects.contains_key(&target) {
                            holders.push(RedirectReferenceHolder {
                                root,
                                path: path.clone(),
                                target,
                            });
                        }
                    }
                    HayroMaybeRef::NotRef(value) => {
                        inspect_hayro_redirect_reference_holders(
                            root, &value, redirects, path, holders,
                        );
                    }
                }
                path.pop();
            }
        }
        HayroObject::Stream(stream) => {
            for (name, value) in stream.dict().entries() {
                path.push(DirectPathStep::DictKey(name.as_ref().to_vec()));
                match value {
                    HayroMaybeRef::Ref(target) => {
                        let target = CowObjectHandle::Existing(target.into());
                        if redirects.contains_key(&target) {
                            holders.push(RedirectReferenceHolder {
                                root,
                                path: path.clone(),
                                target,
                            });
                        }
                    }
                    HayroMaybeRef::NotRef(value) => {
                        inspect_hayro_redirect_reference_holders(
                            root, &value, redirects, path, holders,
                        );
                    }
                }
                path.pop();
            }
        }
        HayroObject::Array(array) => {
            for (index, value) in array.raw_iter().enumerate() {
                path.push(DirectPathStep::ArrayIndex(index));
                match value {
                    HayroMaybeRef::Ref(target) => {
                        let target = CowObjectHandle::Existing(target.into());
                        if redirects.contains_key(&target) {
                            holders.push(RedirectReferenceHolder {
                                root,
                                path: path.clone(),
                                target,
                            });
                        }
                    }
                    HayroMaybeRef::NotRef(value) => {
                        inspect_hayro_redirect_reference_holders(
                            root, &value, redirects, path, holders,
                        );
                    }
                }
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

fn inspect_owned_redirect_reference_holders(
    root: CowObjectHandle,
    object: &OwnedObject,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    path: &mut Vec<DirectPathStep>,
    holders: &mut Vec<RedirectReferenceHolder>,
) {
    match object {
        OwnedObject::Reference(target) => {
            if redirects.contains_key(target) {
                holders.push(RedirectReferenceHolder {
                    root,
                    path: path.clone(),
                    target: *target,
                });
            }
        }
        OwnedObject::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                path.push(DirectPathStep::ArrayIndex(index));
                inspect_owned_redirect_reference_holders(root, value, redirects, path, holders);
                path.pop();
            }
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            for (name, value) in dictionary {
                path.push(DirectPathStep::DictKey(name.clone()));
                inspect_owned_redirect_reference_holders(root, value, redirects, path, holders);
                path.pop();
            }
        }
        OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
}

fn rewrite_all_references_hayro(
    document: &mut EditDocument,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    if redirects.is_empty() {
        return Ok(0);
    }

    let mut holders = Vec::new();
    document.walk_output_objects(|root, object| {
        match object {
            CurrentObject::Source(object) => inspect_hayro_redirect_reference_holders(
                root,
                &object,
                redirects,
                &mut Vec::new(),
                &mut holders,
            ),
            CurrentObject::Owned(object) => inspect_owned_redirect_reference_holders(
                root,
                object,
                redirects,
                &mut Vec::new(),
                &mut holders,
            ),
        }
        Ok(())
    })?;

    let mut rewritten = 0usize;
    for holder in holders {
        let canonical = canonical_cow_redirect(holder.target, redirects);
        if canonical == holder.target {
            continue;
        }
        let root = match holder.root {
            CowObjectHandle::Existing(id) => document.edit_object(id)?,
            CowObjectHandle::New(id) => document.edit_added_object(id)?,
        };
        let Some(value) = object_at_direct_path_mut(root, &holder.path) else {
            continue;
        };
        let OwnedObject::Reference(current) = value else {
            continue;
        };
        if *current != holder.target {
            continue;
        }
        *current = canonical;
        rewritten = rewritten.saturating_add(1);
    }
    Ok(rewritten)
}

fn exact_dictionary_redirects_hayro(
    document: &EditDocument,
    handles: &BTreeSet<CowObjectHandle>,
    domain: &[u8],
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut canonical_by_fingerprint =
        HashMap::<[u8; 32], Vec<(CowObjectHandle, OwnedObject)>>::new();
    let mut redirects = HashMap::new();

    for &handle in handles {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        if !matches!(object, OwnedObject::Dictionary(_)) {
            continue;
        }
        let mut hasher = Sha256::new();
        hash_len_prefixed(&mut hasher, domain);
        hash_owned_object(&mut hasher, &object)?;
        let fingerprint: [u8; 32] = hasher.finalize().into();

        let candidates = canonical_by_fingerprint.entry(fingerprint).or_default();
        if let Some((canonical, _)) = candidates
            .iter()
            .find(|(_, candidate)| *candidate == object)
        {
            if *canonical != handle {
                redirects.insert(handle, *canonical);
            }
        } else {
            candidates.push((handle, object));
        }
    }

    Ok(redirects)
}

pub fn canonicalize_exact_extgstate_dictionaries_hayro(
    document: &mut EditDocument,
) -> Result<ExactObjectDedupStats> {
    let targets = hayro_dictionary_targets(document, b"ExtGState")?;
    let mut handles = BTreeSet::new();
    for target in &targets {
        let Some(snapshot) = document.current_owned_object(target.root)? else {
            continue;
        };
        let Some(dictionary) =
            object_at_direct_path(&snapshot, &target.path).and_then(OwnedObject::as_dictionary)
        else {
            continue;
        };
        for value in dictionary.values() {
            if let OwnedObject::Reference(handle) = value {
                handles.insert(*handle);
            }
        }
    }

    let redirects =
        exact_dictionary_redirects_hayro(document, &handles, b"exact-extgstate-dictionary")?;
    let references_canonicalized =
        rewrite_dictionary_target_entries(document, &targets, &redirects)?;
    Ok(ExactObjectDedupStats {
        duplicate_objects_detected: redirects.len(),
        references_canonicalized,
    })
}

pub fn canonicalize_exact_structure_attribute_dictionaries_hayro(
    document: &mut EditDocument,
) -> Result<ExactObjectDedupStats> {
    let mut holders = Vec::new();
    let mut handles = BTreeSet::new();
    document.walk_output_objects(|root, object| {
        let target = match object {
            CurrentObject::Source(HayroObject::Dict(dictionary)) => {
                let is_struct_elem = dictionary
                    .get::<HayroName<'_>>(b"Type")
                    .is_some_and(|name| name.as_ref() == b"StructElem");
                is_struct_elem
                    .then(|| dictionary.get_ref(b"A"))
                    .flatten()
                    .map(|target| CowObjectHandle::Existing(target.into()))
            }
            CurrentObject::Source(HayroObject::Stream(stream)) => {
                let dictionary = stream.dict();
                let is_struct_elem = dictionary
                    .get::<HayroName<'_>>(b"Type")
                    .is_some_and(|name| name.as_ref() == b"StructElem");
                is_struct_elem
                    .then(|| dictionary.get_ref(b"A"))
                    .flatten()
                    .map(|target| CowObjectHandle::Existing(target.into()))
            }
            CurrentObject::Owned(object) => {
                let Some(dictionary) = object.as_dictionary() else {
                    return Ok(());
                };
                let Some(object_type) = dictionary.get(b"Type".as_slice()) else {
                    return Ok(());
                };
                if !matches!(
                    document.resolve_owned_value(object_type)?,
                    Some(OwnedObject::Name(name)) if name == b"StructElem"
                ) {
                    return Ok(());
                }
                match dictionary.get(b"A".as_slice()) {
                    Some(OwnedObject::Reference(target)) => Some(*target),
                    _ => None,
                }
            }
            CurrentObject::Source(_) => None,
        };

        let Some(target) = target else {
            return Ok(());
        };
        holders.push(DirectReferenceHolder {
            root,
            path: Vec::new(),
            key: b"A".to_vec(),
            target,
        });
        handles.insert(target);
        Ok(())
    })?;

    let redirects = exact_dictionary_redirects_hayro(
        document,
        &handles,
        b"exact-structure-attribute-dictionary",
    )?;
    let mut references_canonicalized = 0usize;
    for holder in &holders {
        let Some(canonical) = redirects.get(&holder.target).copied() else {
            continue;
        };
        if rewrite_direct_reference_holder(document, holder, canonical)? {
            references_canonicalized = references_canonicalized.saturating_add(1);
        }
    }
    Ok(ExactObjectDedupStats {
        duplicate_objects_detected: redirects.len(),
        references_canonicalized,
    })
}

fn reachable_dictionaries_with_type(
    document: &EditDocument,
    object_type: &[u8],
) -> Result<Vec<CowObjectHandle>> {
    let mut handles = Vec::new();
    document.walk_output_objects(|handle, object| {
        let matches_type = match object {
            CurrentObject::Source(HayroObject::Dict(dictionary)) => dictionary
                .get::<HayroName<'_>>(b"Type")
                .is_some_and(|name| name.as_ref() == object_type),
            CurrentObject::Owned(OwnedObject::Dictionary(dictionary)) => {
                let Some(value) = dictionary.get(b"Type".as_slice()) else {
                    return Ok(());
                };
                matches!(
                    document.resolve_owned_value(value)?,
                    Some(OwnedObject::Name(name)) if name == object_type
                )
            }
            CurrentObject::Source(_) | CurrentObject::Owned(_) => false,
        };
        if matches_type {
            handles.push(handle);
        }
        Ok(())
    })?;
    Ok(handles)
}

pub fn canonicalize_exact_font_dictionaries_hayro(
    document: &mut EditDocument,
) -> Result<ExactObjectDedupStats> {
    let handles = reachable_dictionaries_with_type(document, b"Font")?;
    let mut canonical_by_fingerprint =
        HashMap::<[u8; 32], Vec<(CowObjectHandle, OwnedObject)>>::new();
    let mut redirects = HashMap::<CowObjectHandle, CowObjectHandle>::new();

    for handle in handles {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let OwnedObject::Dictionary(_) = &object else {
            continue;
        };

        let mut hasher = Sha256::new();
        hash_len_prefixed(&mut hasher, b"exact-font-dictionary");
        hash_owned_object(&mut hasher, &object)?;
        let fingerprint: [u8; 32] = hasher.finalize().into();

        let candidates = canonical_by_fingerprint.entry(fingerprint).or_default();
        if let Some((canonical, _)) = candidates
            .iter()
            .find(|(_, candidate)| *candidate == object)
        {
            if *canonical != handle {
                redirects.insert(handle, *canonical);
            }
        } else {
            candidates.push((handle, object));
        }
    }

    let references_canonicalized = rewrite_all_references_hayro(document, &redirects)?;
    Ok(ExactObjectDedupStats {
        duplicate_objects_detected: redirects.len(),
        references_canonicalized,
    })
}

fn exact_form_font_redirects_hayro(
    document: &EditDocument,
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut plan = FingerprintRedirectPlan::new();
    for handle in reachable_dictionaries_with_type(document, b"Font")? {
        let Some(fingerprint) = hash_non_stream_object_with_redirects(
            document,
            handle,
            b"form-font-resource-dictionary",
            exact_redirects,
        )?
        else {
            continue;
        };
        plan.observe(handle, handle, fingerprint);
    }
    Ok(plan.into_redirects())
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
    let mut plan = FingerprintRedirectPlan::new();
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
        plan.observe(image, image, fingerprint);
    }
    Ok(plan.into_redirects())
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
            OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
                Some(dictionary)
            }
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
        let mut plan = FingerprintRedirectPlan::new();
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
            plan.observe(form, form, fingerprint);
        }
        let next = plan.into_redirects();
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
            CowObjectHandle::New(id) => document.edit_added_object(id)?,
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

fn stream_payloads_may_repeat(document: &EditDocument, forms: &[CowObjectHandle]) -> Result<bool> {
    let mut seen = HashSet::<[u8; 32]>::new();
    for &form in forms {
        let Some(OwnedObject::Stream { data, .. }) = document.current_owned_object(form)? else {
            continue;
        };
        let raw = data.bytes(document.source())?;
        let fingerprint: [u8; 32] = Sha256::digest(raw.as_ref()).into();
        if !seen.insert(fingerprint) {
            // Hash collisions only cause a conservative fallback to the exact
            // dedup pass; they can never make this proof skip equal payloads.
            return Ok(true);
        }
    }
    Ok(false)
}

fn form_dependency_redirects_hayro(
    document: &EditDocument,
    ignored_form_dictionary_keys: &[&[u8]],
) -> Result<FormDependencyRedirects> {
    let form_streams = reachable_streams_with_subtype(document, b"Form")?;
    if !stream_payloads_may_repeat(document, &form_streams)? {
        return Ok(FormDependencyRedirects {
            form_streams,
            exact: HashMap::new(),
            fonts: HashMap::new(),
            images: HashMap::new(),
            forms: HashMap::new(),
        });
    }
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

pub fn canonicalize_form_xobjects_hayro(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name"]
    } else {
        &[]
    };
    let dependencies = form_dependency_redirects_hayro(document, ignored)?;
    if dependencies.forms.is_empty() {
        return Ok(TargetedDedupStats::default());
    }
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

pub fn canonicalize_appearance_streams_hayro(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = appearance_dictionary_targets(document)?;
    if holders.is_empty() {
        return Ok(TargetedDedupStats::default());
    }
    let dependencies = form_dependency_redirects_hayro(document, &[])?;
    let mut plan = FingerprintRedirectPlan::new();
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
            plan.observe_stream(*appearance_ref, *appearance_ref, fingerprint, raw_bytes);
        }
    }
    let references_canonicalized =
        rewrite_dictionary_target_entries(document, &holders, &plan.redirects)?;
    Ok(plan.targeted_stats(references_canonicalized))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PageContentHolder {
    Dictionary(DirectReferenceHolder),
    Array(DirectArrayReferenceHolder),
}

impl PageContentHolder {
    const fn target(&self) -> CowObjectHandle {
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
    document.page_handles()
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
                    }));
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

pub fn canonicalize_page_contents_hayro(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let holders = page_content_holders_hayro(document)?;
    let mut plan = FingerprintRedirectPlan::new();
    for holder in &holders {
        let stream = holder.target();
        let Some((fingerprint, raw_bytes)) =
            hayro_stream_fingerprint(document, stream, b"page-content")?
        else {
            continue;
        };
        plan.observe_stream(stream, stream, fingerprint, raw_bytes);
    }
    let mut references_canonicalized = 0;
    for holder in &holders {
        let Some(canonical) = plan.redirect(&holder.target()) else {
            continue;
        };
        if holder.rewrite(document, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(plan.targeted_stats(references_canonicalized))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourcePdf, test_support::ClassicPdfBuilder};

    fn duplicate_image_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q /A Do /B Do Q")?;
        let image = b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8";
        pdf.stream(5, image, &[0x80])?;
        pdf.stream(6, image, &[0x80])?;
        pdf.finish(1)
    }

    fn duplicate_page_content_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 5 0 R >>")?;
        pdf.object(4, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 6 0 R >>")?;
        pdf.stream(5, b"", b"0 0 m 10 10 l S")?;
        pdf.stream(6, b"", b"0 0 m 10 10 l S")?;
        pdf.finish(1)
    }

    #[test]
    fn fingerprint_redirect_plan_keeps_first_target_and_counts_duplicate_once() {
        let canonical = CowObjectHandle::Existing(crate::ObjectId::new(10, 0));
        let duplicate = CowObjectHandle::Existing(crate::ObjectId::new(11, 0));
        let fingerprint = [0x5a; 32];
        let mut plan = FingerprintRedirectPlan::new();

        plan.observe_stream(
            (b"FontFile".to_vec(), canonical),
            canonical,
            fingerprint,
            17,
        );
        plan.observe_stream(
            (b"FontFile".to_vec(), duplicate),
            duplicate,
            fingerprint,
            17,
        );
        plan.observe_stream(
            (b"FontFile2".to_vec(), duplicate),
            duplicate,
            fingerprint,
            17,
        );

        assert_eq!(
            plan.redirect(&(b"FontFile".to_vec(), duplicate)),
            Some(canonical)
        );
        assert_eq!(
            plan.redirect(&(b"FontFile2".to_vec(), duplicate)),
            Some(canonical)
        );
        assert_eq!(
            plan.targeted_stats(2),
            TargetedDedupStats {
                duplicate_streams_detected: 1,
                duplicate_raw_bytes: 17,
                references_canonicalized: 2,
            }
        );
    }

    #[test]
    fn hayro_image_dedup_canonicalizes_exact_resource_duplicates() -> Result<()> {
        let mut document = EditDocument::from_bytes(duplicate_image_fixture()?)?;
        let stats = canonicalize_image_xobjects_hayro(&mut document)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 1);
        Ok(())
    }

    #[test]
    fn hayro_page_content_dedup_canonicalizes_exact_streams() -> Result<()> {
        let mut document = EditDocument::from_bytes(duplicate_page_content_fixture()?)?;
        let stats = canonicalize_page_contents_hayro(&mut document)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 2);
        Ok(())
    }
}
