use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    StreamData, content::resolved_dictionary, source::CurrentValue,
};
use hayro_syntax::PdfVersion;
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
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

fn fingerprint_redirects(
    handles: impl IntoIterator<Item = CowObjectHandle>,
    mut fingerprint: impl FnMut(CowObjectHandle) -> Result<Option<[u8; 32]>>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut plan = FingerprintRedirectPlan::new();
    for handle in handles {
        let Some(fingerprint) = fingerprint(handle)? else {
            continue;
        };
        plan.observe(handle, handle, fingerprint);
    }
    Ok(plan.into_redirects())
}

fn canonicalize_stream_holders<H, K>(
    document: &mut EditDocument,
    holders: &[H],
    mut target: impl FnMut(&H) -> CowObjectHandle,
    mut redirect_key: impl FnMut(&H) -> K,
    mut fingerprint: impl FnMut(&EditDocument, &H) -> Result<Option<([u8; 32], usize)>>,
    mut rewrite: impl FnMut(&mut EditDocument, &H, CowObjectHandle) -> Result<bool>,
) -> Result<TargetedDedupStats>
where
    K: Eq + Hash,
{
    let mut plan = FingerprintRedirectPlan::new();
    for holder in holders {
        let Some((fingerprint, raw_bytes)) = fingerprint(document, holder)? else {
            continue;
        };
        plan.observe_stream(redirect_key(holder), target(holder), fingerprint, raw_bytes);
    }

    let mut references_canonicalized = 0_usize;
    for holder in holders {
        let Some(canonical) = plan.redirect(&redirect_key(holder)) else {
            continue;
        };
        if rewrite(document, holder, canonical)? {
            references_canonicalized += 1;
        }
    }
    Ok(plan.targeted_stats(references_canonicalized))
}

const HAYRO_FONT_FILE_KEYS: [&[u8]; 3] = [b"FontFile", b"FontFile2", b"FontFile3"];

struct FontProgramHolderVisitor<'a> {
    holders_by_key: &'a mut [Vec<DirectReferenceHolder>; HAYRO_FONT_FILE_KEYS.len()],
}

impl DirectObjectVisitor for FontProgramHolderVisitor<'_> {
    fn visit_dictionary(
        &mut self,
        _document: &EditDocument,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        for (index, key) in HAYRO_FONT_FILE_KEYS.iter().copied().enumerate() {
            // Historical behavior treats a direct value under `/FontFileN` as
            // opaque only to that same key's search. The generic walker still
            // descends so the other font-file keys remain discoverable; a
            // repeated key earlier in the path marks only this key inactive.
            if path
                .iter()
                .any(|step| matches!(step, DirectPathStep::DictKey(name) if name.as_slice() == key))
            {
                continue;
            }
            if let Some(target) = dictionary.reference(key) {
                self.holders_by_key[index].push(DirectReferenceHolder {
                    root,
                    path: path.to_vec(),
                    key: key.to_vec(),
                    target,
                });
            }
        }
        Ok(DirectWalkControl::Descend)
    }
}

fn font_program_holders(document: &EditDocument) -> Result<Vec<DirectReferenceHolder>> {
    // Keep the historical key-major output ordering even though all three keys
    // are now observed during one shared direct-object traversal.
    let mut holders_by_key: [Vec<DirectReferenceHolder>; HAYRO_FONT_FILE_KEYS.len()] =
        std::array::from_fn(|_| Vec::new());
    walk_direct_output_objects(
        document,
        &mut FontProgramHolderVisitor {
            holders_by_key: &mut holders_by_key,
        },
    )?;
    Ok(holders_by_key.into_iter().flatten().collect())
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

/// Reference projection is independent of COS encoding. Recursive resource
/// fingerprints have a distinct reference tag; top-level stream projections
/// only redirect direct dictionary values, not references nested beneath them.
#[derive(Clone, Copy)]
enum FingerprintReferences<'a> {
    Exact,
    Recursive(&'a HashMap<CowObjectHandle, CowObjectHandle>),
    TopLevel(&'a HashMap<CowObjectHandle, CowObjectHandle>),
}

impl FingerprintReferences<'_> {
    fn hash(self, hasher: &mut Sha256, handle: CowObjectHandle, is_root: bool) {
        let handle = match self {
            Self::Recursive(redirects) => {
                hasher.update([0x70]);
                canonical_cow_redirect(handle, redirects)
            }
            Self::TopLevel(redirects) if is_root => canonical_cow_redirect(handle, redirects),
            Self::Exact | Self::TopLevel(_) => handle,
        };
        hash_cow_handle(hasher, handle);
    }
}

fn hash_stream_identity(hasher: &mut Sha256, data: &StreamData) {
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

// A frame holds only unvisited siblings. Stream identities are emitted after
// the dictionary, not before it, preserving the unambiguous COS encoding.
enum FingerprintFrame<'a> {
    Array(std::slice::Iter<'a, OwnedObject>),
    Dictionary {
        entries: std::collections::btree_map::Iter<'a, Vec<u8>, OwnedObject>,
        data: Option<&'a StreamData>,
    },
}

impl<'a> FingerprintFrame<'a> {
    fn next_value(&mut self, hasher: &mut Sha256) -> Option<&'a OwnedObject> {
        match self {
            Self::Array(values) => values.next(),
            Self::Dictionary { entries, data } => {
                if let Some((key, value)) = entries.next() {
                    hash_len_prefixed(hasher, key);
                    Some(value)
                } else {
                    if let Some(data) = data.take() {
                        hash_stream_identity(hasher, data);
                    }
                    None
                }
            }
        }
    }
}

fn hash_cos_value(hasher: &mut Sha256, root: &OwnedObject, references: FingerprintReferences<'_>) {
    let mut frames = SmallVec::<[FingerprintFrame<'_>; 4]>::new();
    let mut next = Some(root);
    loop {
        if let Some(object) = next.take() {
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
                OwnedObject::Reference(handle) => {
                    references.hash(hasher, *handle, frames.is_empty());
                }
                OwnedObject::Array(values) => {
                    hasher.update([0x06]);
                    hasher.update((values.len() as u64).to_le_bytes());
                    frames.push(FingerprintFrame::Array(values.iter()));
                }
                OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
                    let data = match object {
                        OwnedObject::Stream { data, .. } => Some(data),
                        _ => None,
                    };
                    hasher.update([if data.is_some() { 0x08 } else { 0x07 }]);
                    hasher.update((dictionary.len() as u64).to_le_bytes());
                    frames.push(FingerprintFrame::Dictionary {
                        entries: dictionary.iter(),
                        data,
                    });
                }
            }
        }
        while let Some(frame) = frames.last_mut() {
            if let Some(value) = frame.next_value(hasher) {
                next = Some(value);
                break;
            }
            frames.pop();
        }
        if next.is_none() {
            return;
        }
    }
}

/// Hash source and overlay stream data in place using one reference policy,
/// without copying encoded payloads or collecting a temporary entry vector.
fn hash_stream_with_keys(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    references: FingerprintReferences<'_>,
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object_cow(stream)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object.as_ref() else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_len_prefixed(&mut hasher, raw.as_ref());
    let entries = dictionary.iter().filter(|(key, _)| {
        key.as_slice() != b"Length" && !ignored_dictionary_keys.contains(&key.as_slice())
    });
    hasher.update((entries.clone().count() as u64).to_le_bytes());
    for (key, value) in entries {
        hash_len_prefixed(&mut hasher, key);
        hash_cos_value(&mut hasher, value, references);
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn source_stream_fingerprint_ignoring(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
) -> Result<Option<([u8; 32], usize)>> {
    hash_stream_with_keys(
        document,
        stream,
        domain,
        ignored_dictionary_keys,
        FingerprintReferences::Exact,
    )
}

fn source_stream_fingerprint(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
) -> Result<Option<([u8; 32], usize)>> {
    source_stream_fingerprint_ignoring(document, stream, domain, &[])
}

fn source_font_program_fingerprint(
    document: &EditDocument,
    program: CowObjectHandle,
    key: &[u8],
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object_cow(program)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object.as_ref() else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    let domain = [b"/".as_slice(), key].concat();
    hash_len_prefixed(&mut hasher, &domain);
    hash_len_prefixed(&mut hasher, raw.as_ref());

    hasher.update((dictionary.len() as u64).to_le_bytes());
    for (dict_key, value) in dictionary {
        hash_len_prefixed(&mut hasher, dict_key);
        if matches!(dict_key.as_slice(), b"Length1" | b"Length2" | b"Length3")
            && let Some(OwnedObject::Integer(integer)) = document.resolve_owned_value(value)?
        {
            hash_cos_value(
                &mut hasher,
                &OwnedObject::Integer(integer),
                FingerprintReferences::Exact,
            );
        } else {
            hash_cos_value(&mut hasher, value, FingerprintReferences::Exact);
        }
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

pub fn canonicalize_font_program_streams(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let holders = font_program_holders(document)?;
    canonicalize_stream_holders(
        document,
        &holders,
        |holder| holder.target,
        |holder| (holder.key.clone(), holder.target),
        |document, holder| source_font_program_fingerprint(document, holder.target, &holder.key),
        rewrite_direct_reference_holder,
    )
}

use crate::direct_object::{
    DirectArrayView, DirectDictionaryView, DirectObjectVisitor, DirectPathStep, DirectWalkControl,
    object_at_direct_path_mut, value_at_direct_path, walk_direct_output_objects,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectReferenceHolder {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
    key: Vec<u8>,
    target: CowObjectHandle,
}

struct DirectReferenceVisitor<'a> {
    key: &'a [u8],
    holders: &'a mut Vec<DirectReferenceHolder>,
}

impl DirectObjectVisitor for DirectReferenceVisitor<'_> {
    fn visit_reference(
        &mut self,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        target: CowObjectHandle,
    ) -> Result<()> {
        let Some(DirectPathStep::DictKey(key)) = path.last() else {
            return Ok(());
        };
        if key.as_slice() != self.key {
            return Ok(());
        }
        self.holders.push(DirectReferenceHolder {
            root,
            path: path[..path.len() - 1].to_vec(),
            key: self.key.to_vec(),
            target,
        });
        Ok(())
    }

    fn descend_dictionary_value(&self, key: &[u8]) -> bool {
        key != self.key
    }
}

fn direct_reference_holders(
    document: &EditDocument,
    key: &[u8],
) -> Result<Vec<DirectReferenceHolder>> {
    let mut holders = Vec::new();
    walk_direct_output_objects(
        document,
        &mut DirectReferenceVisitor {
            key,
            holders: &mut holders,
        },
    )?;
    Ok(holders)
}

fn rewrite_direct_reference_holder(
    document: &mut EditDocument,
    holder: &DirectReferenceHolder,
    canonical: CowObjectHandle,
) -> Result<bool> {
    let root = document.edit_handle(holder.root)?;
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

fn canonicalize_named_stream_references(
    document: &mut EditDocument,
    key: &[u8],
    domain: &[u8],
) -> Result<TargetedDedupStats> {
    let holders = direct_reference_holders(document, key)?;
    canonicalize_stream_holders(
        document,
        &holders,
        |holder| holder.target,
        |holder| holder.target,
        |document, holder| source_stream_fingerprint(document, holder.target, domain),
        rewrite_direct_reference_holder,
    )
}

pub fn canonicalize_metadata_streams(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    canonicalize_named_stream_references(document, b"Metadata", b"metadata")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectArrayReferenceHolder {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
    index: usize,
    target: CowObjectHandle,
}

struct IccArrayVisitor<'a> {
    holders: &'a mut Vec<DirectArrayReferenceHolder>,
}

impl DirectObjectVisitor for IccArrayVisitor<'_> {
    fn visit_array(
        &mut self,
        document: &EditDocument,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        array: DirectArrayView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        if !array.resolved_name_is(document, 0, b"ICCBased")? {
            return Ok(DirectWalkControl::Descend);
        }
        let Some(profile) = array.reference(1) else {
            return Ok(DirectWalkControl::Descend);
        };
        self.holders.push(DirectArrayReferenceHolder {
            root,
            path: path.to_vec(),
            index: 1,
            target: profile,
        });
        Ok(DirectWalkControl::SkipChildren)
    }
}

fn icc_array_holders(document: &EditDocument) -> Result<Vec<DirectArrayReferenceHolder>> {
    let mut holders = Vec::new();
    walk_direct_output_objects(
        document,
        &mut IccArrayVisitor {
            holders: &mut holders,
        },
    )?;
    Ok(holders)
}

fn rewrite_direct_array_reference_holder(
    document: &mut EditDocument,
    holder: &DirectArrayReferenceHolder,
    canonical: CowObjectHandle,
) -> Result<bool> {
    let root = document.edit_handle(holder.root)?;
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

pub fn canonicalize_icc_profiles(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let holders = icc_array_holders(document)?;
    canonicalize_stream_holders(
        document,
        &holders,
        |holder| holder.target,
        |holder| holder.target,
        |document, holder| source_stream_fingerprint(document, holder.target, b"icc-profile"),
        rewrite_direct_array_reference_holder,
    )
}

/// Hayro/COW `ToUnicode` canonicalization.
///
/// This deliberately walks the reachable Hayro graph rather than reproducing
/// whole-xref enumeration. On damaged or oddly indexed PDFs Hayro
/// can therefore find additional real `/ToUnicode` holders that the legacy
/// pass skipped; every rewrite still requires an exact stream fingerprint.
pub fn canonicalize_to_unicode_cmaps(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    canonicalize_named_stream_references(document, b"ToUnicode", b"to-unicode")
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct DirectDictionaryTarget {
    root: CowObjectHandle,
    path: Vec<DirectPathStep>,
}

impl DirectDictionaryTarget {
    fn from_property(
        root: CowObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
        key: &[u8],
    ) -> Option<Self> {
        let value = dictionary.raw_value(key)?;
        if let Some(target) = value.reference() {
            return Some(Self {
                root: target,
                path: Vec::new(),
            });
        }
        value.is_dictionary().then(|| {
            let mut path = path.to_vec();
            path.push(DirectPathStep::DictKey(key.to_vec()));
            Self { root, path }
        })
    }

    fn current<'a>(&self, document: &'a EditDocument) -> Result<Option<CurrentValue<'a>>> {
        Ok(document
            .current_value(self.root)?
            .and_then(|root| value_at_direct_path(root, &self.path))
            .filter(CurrentValue::has_dictionary))
    }

    fn entries<'a>(
        &self,
        document: &'a EditDocument,
    ) -> Result<Option<crate::source::CurrentDictionaryIter<'a>>> {
        Ok(self
            .current(document)?
            .and_then(|dictionary| dictionary.dictionary_entries()))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HayroType3GlyphHolder {
    target: DirectDictionaryTarget,
    name: Vec<u8>,
    glyph: CowObjectHandle,
}

struct Type3TargetVisitor<'a> {
    targets: &'a mut BTreeSet<DirectDictionaryTarget>,
}

impl DirectObjectVisitor for Type3TargetVisitor<'_> {
    fn visit_dictionary(
        &mut self,
        document: &EditDocument,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        if dictionary.resolved_name_is(document, b"Subtype", b"Type3")?
            && let Some(target) =
                DirectDictionaryTarget::from_property(root, path, dictionary, b"CharProcs")
        {
            self.targets.insert(target);
        }
        Ok(DirectWalkControl::Descend)
    }

    fn descend_dictionary_value(&self, key: &[u8]) -> bool {
        key != b"CharProcs"
    }
}

fn type3_charproc_targets(document: &EditDocument) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = BTreeSet::new();
    walk_direct_output_objects(
        document,
        &mut Type3TargetVisitor {
            targets: &mut targets,
        },
    )?;
    Ok(targets)
}

fn type3_glyph_holders(document: &EditDocument) -> Result<Vec<HayroType3GlyphHolder>> {
    let mut holders = Vec::new();
    for target in type3_charproc_targets(document)? {
        let Some(entries) = target.entries(document)? else {
            continue;
        };
        for (name, glyph) in entries {
            let Some(glyph) = glyph.reference() else {
                continue;
            };
            holders.push(HayroType3GlyphHolder {
                target: target.clone(),
                name: name.as_ref().to_vec(),
                glyph,
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
    let root = document.edit_handle(holder.target.root)?;
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

pub fn canonicalize_type3_charprocs(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let holders = type3_glyph_holders(document)?;
    canonicalize_stream_holders(
        document,
        &holders,
        |holder| holder.glyph,
        |holder| holder.glyph,
        |document, holder| source_stream_fingerprint(document, holder.glyph, b"type3-charproc"),
        rewrite_type3_glyph_holder,
    )
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

fn source_stream_fingerprint_with_redirects(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<([u8; 32], usize)>> {
    hash_stream_with_keys(
        document,
        stream,
        domain,
        ignored_dictionary_keys,
        FingerprintReferences::Recursive(redirects),
    )
}

fn reachable_streams_with_subtype(
    document: &EditDocument,
    subtype: &[u8],
) -> Result<Vec<CowObjectHandle>> {
    document.reachable_streams_with_subtype(subtype)
}

fn reachable_streams_with_type(
    document: &EditDocument,
    object_type: &[u8],
) -> Result<Vec<CowObjectHandle>> {
    let mut handles = Vec::new();
    document.walk_output_objects(|handle, object| {
        let matches_type =
            object.is_stream() && object.resolved_name_is(document, b"Type", object_type)?;
        if matches_type {
            handles.push(handle);
        }
        Ok(())
    })?;
    Ok(handles)
}

fn exact_stream_redirects(
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
        let Some((fingerprint, raw_bytes)) = source_stream_fingerprint_with_redirects(
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
    rewrite_dictionary_target(
        document,
        &DirectDictionaryTarget {
            root: handle,
            path: Vec::new(),
        },
        Some(keys),
        redirects,
    )
}

struct DictionaryTargetVisitor<'a> {
    key: &'a [u8],
    targets: &'a mut BTreeSet<DirectDictionaryTarget>,
}

impl DirectObjectVisitor for DictionaryTargetVisitor<'_> {
    fn visit_dictionary(
        &mut self,
        _document: &EditDocument,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        if let Some(target) =
            DirectDictionaryTarget::from_property(root, path, dictionary, self.key)
        {
            self.targets.insert(target);
        }
        Ok(DirectWalkControl::Descend)
    }

    fn descend_dictionary_value(&self, key: &[u8]) -> bool {
        key != self.key
    }
}

fn dictionary_targets(
    document: &EditDocument,
    key: &[u8],
) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = BTreeSet::new();
    walk_direct_output_objects(
        document,
        &mut DictionaryTargetVisitor {
            key,
            targets: &mut targets,
        },
    )?;
    Ok(targets)
}

fn apply_dictionary_redirect_changes(
    document: &mut EditDocument,
    target: &DirectDictionaryTarget,
    changes: Vec<(Vec<u8>, CowObjectHandle)>,
) -> Result<usize> {
    if changes.is_empty() {
        return Ok(0);
    }
    let root = document.edit_handle(target.root)?;
    let Some(dictionary) =
        object_at_direct_path_mut(root, &target.path).and_then(OwnedObject::as_dictionary_mut)
    else {
        return Ok(0);
    };
    let count = changes.len();
    for (name, canonical) in changes {
        dictionary.insert(name, OwnedObject::Reference(canonical));
    }
    Ok(count)
}

fn rewrite_dictionary_target(
    document: &mut EditDocument,
    target: &DirectDictionaryTarget,
    selected_keys: Option<&[&[u8]]>,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    let Some(dictionary) = target.current(document)? else {
        return Ok(0);
    };
    let mut changes = Vec::new();
    let mut inspect = |key: &[u8], value: CurrentValue<'_>| {
        if let Some(reference) = value.reference() {
            let canonical = canonical_cow_redirect(reference, redirects);
            if canonical != reference {
                changes.push((key.to_vec(), canonical));
            }
        }
    };
    if let Some(keys) = selected_keys {
        for &key in keys {
            if let Some(value) = dictionary.dictionary_entry(key) {
                inspect(key, value);
            }
        }
    } else if let Some(entries) = dictionary.dictionary_entries() {
        for (key, value) in entries {
            inspect(key.as_ref(), value);
        }
    }
    drop(dictionary);
    apply_dictionary_redirect_changes(document, target, changes)
}

fn rewrite_dictionary_target_entries(
    document: &mut EditDocument,
    targets: &BTreeSet<DirectDictionaryTarget>,
    selected_keys: Option<&[&[u8]]>,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    let mut rewritten = 0;
    for target in targets {
        rewritten += rewrite_dictionary_target(document, target, selected_keys, redirects)?;
    }
    Ok(rewritten)
}

const FACTORED_PATTERN_CELL_NAME: &[u8] = b"PdfRedoxCell";
const FACTORED_PATTERN_CONTENT: &[u8] = b"/PdfRedoxCell Do\n";
const MIN_PATTERN_DUPLICATE_RAW_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PatternPayloadFactorStats {
    pub groups_factored: usize,
    pub patterns_rewritten: usize,
    pub forms_created: usize,
    pub duplicate_raw_bytes_factored: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FormMetadataPayloadFactorStats {
    pub groups_factored: usize,
    pub wrappers_rewritten: usize,
    pub duplicate_raw_bytes_factored: usize,
}

fn pattern_integer(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    key: &[u8],
) -> Result<Option<i64>> {
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => Some(value),
        _ => None,
    })
}

fn pattern_already_factored(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<bool> {
    let Some(resources) = dictionary.get(b"Resources".as_slice()) else {
        return Ok(false);
    };
    let Some(resources) = resolved_dictionary(document, Some(resources))? else {
        return Ok(false);
    };
    if resources.len() != 1 {
        return Ok(false);
    }
    let Some(xobjects) = resources.get(b"XObject".as_slice()) else {
        return Ok(false);
    };
    let Some(xobjects) = resolved_dictionary(document, Some(xobjects))? else {
        return Ok(false);
    };
    Ok(xobjects.len() == 1 && xobjects.contains_key(FACTORED_PATTERN_CELL_NAME))
}

fn pattern_payload_fingerprint(
    document: &EditDocument,
    pattern: CowObjectHandle,
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object_cow(pattern)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object.as_ref() else {
        return Ok(None);
    };
    if pattern_integer(document, dictionary, b"PatternType")? != Some(1)
        || pattern_integer(document, dictionary, b"PaintType")? != Some(1)
        || pattern_already_factored(document, dictionary)?
        || dictionary.contains_key(b"F".as_slice())
        || dictionary.contains_key(b"FFilter".as_slice())
        || dictionary.contains_key(b"FDecodeParms".as_slice())
    {
        return Ok(None);
    }
    let (Some(bbox), Some(resources_value)) = (
        dictionary.get(b"BBox".as_slice()),
        dictionary.get(b"Resources".as_slice()),
    ) else {
        return Ok(None);
    };
    let Some(resources) = resolved_dictionary(document, Some(resources_value))? else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, b"pattern-cell-payload");
    hash_len_prefixed(&mut hasher, raw.as_ref());
    for key in [b"Filter".as_slice(), b"DecodeParms".as_slice()] {
        hash_len_prefixed(&mut hasher, key);
        if let Some(value) = dictionary.get(key) {
            hash_cos_value(&mut hasher, value, FingerprintReferences::Exact);
        } else {
            hasher.update([0]);
        }
    }
    hash_cos_value(&mut hasher, bbox, FingerprintReferences::Exact);
    hash_cos_value(
        &mut hasher,
        &OwnedObject::Dictionary(resources),
        FingerprintReferences::Exact,
    );
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn pattern_payloads_equal(
    document: &EditDocument,
    left: CowObjectHandle,
    right: CowObjectHandle,
) -> Result<bool> {
    let Some(left) = document.current_owned_object_cow(left)? else {
        return Ok(false);
    };
    let OwnedObject::Stream {
        dictionary: left_dict,
        data: left_data,
    } = left.as_ref()
    else {
        return Ok(false);
    };
    let Some(right) = document.current_owned_object_cow(right)? else {
        return Ok(false);
    };
    let OwnedObject::Stream {
        dictionary: right_dict,
        data: right_data,
    } = right.as_ref()
    else {
        return Ok(false);
    };
    if left_data.bytes(document.source())? != right_data.bytes(document.source())? {
        return Ok(false);
    }
    for key in [
        b"BBox".as_slice(),
        b"Filter".as_slice(),
        b"DecodeParms".as_slice(),
    ] {
        if left_dict.get(key) != right_dict.get(key) {
            return Ok(false);
        }
    }
    let (Some(left_resources), Some(right_resources)) = (
        left_dict.get(b"Resources".as_slice()),
        right_dict.get(b"Resources".as_slice()),
    ) else {
        return Ok(false);
    };
    Ok(resolved_dictionary(document, Some(left_resources))?
        == resolved_dictionary(document, Some(right_resources))?)
}

fn shared_pattern_form(
    document: &mut EditDocument,
    representative: CowObjectHandle,
) -> Result<Option<CowObjectHandle>> {
    let Some(OwnedObject::Stream { dictionary, data }) =
        document.current_owned_object(representative)?
    else {
        return Ok(None);
    };
    let (Some(bbox), Some(resources)) = (
        dictionary.get(b"BBox".as_slice()).cloned(),
        dictionary.get(b"Resources".as_slice()).cloned(),
    ) else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?.into_owned();
    let mut form = OwnedDictionary::from([
        (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
        (b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec())),
        (b"BBox".to_vec(), bbox),
        (b"Resources".to_vec(), resources),
    ]);
    for key in [b"Filter".as_slice(), b"DecodeParms".as_slice()] {
        if let Some(value) = dictionary.get(key) {
            form.insert(key.to_vec(), value.clone());
        }
    }
    Ok(Some(CowObjectHandle::New(document.add_object(
        OwnedObject::Stream {
            dictionary: form,
            data: StreamData::Owned(raw),
        },
    ))))
}

/// Convert one stream into a Form paint wrapper while retaining all
/// non-stream dictionary metadata on the original object.
fn rewrite_as_form_wrapper(
    document: &mut EditDocument,
    wrapper: CowObjectHandle,
    resource_name: &[u8],
    target: CowObjectHandle,
    commands: &[u8],
) -> Result<bool> {
    let object = document.edit_handle(wrapper)?;
    let OwnedObject::Stream { dictionary, data } = object else {
        return Ok(false);
    };
    dictionary.insert(
        b"Resources".to_vec(),
        OwnedObject::Dictionary(OwnedDictionary::from([(
            b"XObject".to_vec(),
            OwnedObject::Dictionary(OwnedDictionary::from([(
                resource_name.to_vec(),
                OwnedObject::Reference(target),
            )])),
        )])),
    );
    dictionary.remove(b"Filter".as_slice());
    dictionary.remove(b"DecodeParms".as_slice());
    dictionary.remove(b"Length".as_slice());
    *data = StreamData::Owned(commands.to_vec());
    Ok(true)
}

fn rewrite_pattern_as_form_wrapper(
    document: &mut EditDocument,
    pattern: CowObjectHandle,
    form: CowObjectHandle,
) -> Result<bool> {
    rewrite_as_form_wrapper(
        document,
        pattern,
        FACTORED_PATTERN_CELL_NAME,
        form,
        FACTORED_PATTERN_CONTENT,
    )
}

pub fn factor_duplicate_pattern_payloads(
    document: &mut EditDocument,
) -> Result<PatternPayloadFactorStats> {
    #[derive(Debug)]
    struct Group {
        representative: CowObjectHandle,
        members: Vec<CowObjectHandle>,
        raw_bytes: usize,
    }

    let patterns = reachable_streams_with_type(document, b"Pattern")?;
    let mut buckets = BTreeMap::<[u8; 32], Vec<Group>>::new();
    for pattern in patterns {
        let Some((fingerprint, raw_bytes)) = pattern_payload_fingerprint(document, pattern)? else {
            continue;
        };
        let groups = buckets.entry(fingerprint).or_default();
        let mut matched = None;
        for (index, group) in groups.iter().enumerate() {
            if pattern_payloads_equal(document, pattern, group.representative)? {
                matched = Some(index);
                break;
            }
        }
        if let Some(index) = matched {
            groups[index].members.push(pattern);
        } else {
            groups.push(Group {
                representative: pattern,
                members: vec![pattern],
                raw_bytes,
            });
        }
    }

    let mut stats = PatternPayloadFactorStats::default();
    for group in buckets.into_values().flatten() {
        if group.members.len() < 2 {
            continue;
        }
        let duplicate_raw_bytes = group
            .raw_bytes
            .saturating_mul(group.members.len().saturating_sub(1));
        if duplicate_raw_bytes < MIN_PATTERN_DUPLICATE_RAW_BYTES {
            continue;
        }
        let Some(form) = shared_pattern_form(document, group.representative)? else {
            continue;
        };
        let mut rewritten = 0usize;
        for pattern in group.members {
            rewritten = rewritten.saturating_add(usize::from(rewrite_pattern_as_form_wrapper(
                document, pattern, form,
            )?));
        }
        if rewritten < 2 {
            continue;
        }
        stats.groups_factored = stats.groups_factored.saturating_add(1);
        stats.forms_created = stats.forms_created.saturating_add(1);
        stats.patterns_rewritten = stats.patterns_rewritten.saturating_add(rewritten);
        stats.duplicate_raw_bytes_factored = stats
            .duplicate_raw_bytes_factored
            .saturating_add(duplicate_raw_bytes);
    }
    Ok(stats)
}

pub fn canonicalize_image_xobjects(document: &mut EditDocument) -> Result<TargetedDedupStats> {
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
    let mask_redirects = exact_stream_redirects(
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
    let redirects = exact_stream_redirects(
        document,
        &images,
        b"image-xobject",
        ignored,
        &empty_redirects,
        &mut duplicate_refs,
        &mut duplicate_raw_bytes,
    )?;
    let xobject_targets = dictionary_targets(document, b"XObject")?;
    references_canonicalized +=
        rewrite_dictionary_target_entries(document, &xobject_targets, None, &redirects)?;
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
    let Some(object) = document.current_owned_object_cow(handle)? else {
        return Ok(None);
    };
    if matches!(object.as_ref(), OwnedObject::Stream { .. }) {
        return Ok(None);
    }
    if !matches!(
        object.as_ref(),
        OwnedObject::Dictionary(_) | OwnedObject::Array(_)
    ) {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_cos_value(
        &mut hasher,
        &object,
        FingerprintReferences::Recursive(redirects),
    );
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
            let Some(object) = document.current_owned_object_cow(*handle)? else {
                return Ok(());
            };
            if matches!(
                object.as_ref(),
                OwnedObject::Dictionary(_) | OwnedObject::Array(_)
            ) {
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
        let Some(object) = document.current_owned_object_cow(form)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = object.as_ref() else {
            continue;
        };
        let Some(resources) = dictionary.get(b"Resources".as_slice()) else {
            continue;
        };
        collect_form_resource_handles_from_value(document, resources, &mut seen, &mut handles)?;
    }
    Ok(handles.into_iter().collect())
}

fn exact_non_stream_resource_redirects(
    document: &EditDocument,
    handles: &[CowObjectHandle],
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut redirects = HashMap::new();
    for _ in 0..=handles.len() {
        let next = fingerprint_redirects(handles.iter().copied(), |handle| {
            hash_non_stream_object_with_redirects(
                document,
                handle,
                b"form-resource-exact-object",
                &redirects,
            )
        })?;
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

fn record_redirect_reference_holder(
    root: CowObjectHandle,
    path: &[DirectPathStep],
    target: CowObjectHandle,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    holders: &mut Vec<RedirectReferenceHolder>,
) {
    if redirects.contains_key(&target) {
        holders.push(RedirectReferenceHolder {
            root,
            path: path.to_vec(),
            target,
        });
    }
}

struct RedirectReferenceVisitor<'a> {
    redirects: &'a HashMap<CowObjectHandle, CowObjectHandle>,
    holders: &'a mut Vec<RedirectReferenceHolder>,
}

impl DirectObjectVisitor for RedirectReferenceVisitor<'_> {
    fn visit_reference(
        &mut self,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        target: CowObjectHandle,
    ) -> Result<()> {
        record_redirect_reference_holder(root, path, target, self.redirects, self.holders);
        Ok(())
    }
}

fn reachable_sampled_function_streams(document: &EditDocument) -> Result<Vec<CowObjectHandle>> {
    let mut handles = Vec::new();
    document.walk_output_objects(|handle, object| {
        let sampled = object.is_stream()
            && matches!(
                object
                    .resolved_entry(document, b"FunctionType")?
                    .map(CurrentValue::into_cow)
                    .as_deref(),
                Some(OwnedObject::Integer(0))
            );
        if sampled {
            handles.push(handle);
        }
        Ok(())
    })?;
    Ok(handles)
}

fn rewrite_all_references(
    document: &mut EditDocument,
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<usize> {
    if redirects.is_empty() {
        return Ok(0);
    }

    let mut holders = Vec::new();
    walk_direct_output_objects(
        document,
        &mut RedirectReferenceVisitor {
            redirects,
            holders: &mut holders,
        },
    )?;

    let mut rewritten = 0usize;
    for holder in holders {
        let canonical = canonical_cow_redirect(holder.target, redirects);
        if canonical == holder.target {
            continue;
        }
        let root = document.edit_handle(holder.root)?;
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

pub fn canonicalize_sampled_function_streams(
    document: &mut EditDocument,
) -> Result<TargetedDedupStats> {
    let streams = reachable_sampled_function_streams(document)?;
    if streams.len() < 2 {
        return Ok(TargetedDedupStats::default());
    }
    let dependency_redirects = HashMap::new();
    let mut duplicate_refs = HashSet::new();
    let mut duplicate_raw_bytes = 0usize;
    let redirects = exact_stream_redirects(
        document,
        &streams,
        b"exact-sampled-function-stream",
        &[],
        &dependency_redirects,
        &mut duplicate_refs,
        &mut duplicate_raw_bytes,
    )?;
    let references_canonicalized = rewrite_all_references(document, &redirects)?;
    Ok(TargetedDedupStats {
        duplicate_streams_detected: duplicate_refs.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

pub fn exact_dictionary_fingerprint(object: &OwnedObject, domain: &[u8]) -> Option<[u8; 32]> {
    if !matches!(object, OwnedObject::Dictionary(_)) {
        return None;
    }
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_cos_value(&mut hasher, object, FingerprintReferences::Exact);
    Some(hasher.finalize().into())
}

fn exact_dictionary_redirects(
    document: &EditDocument,
    handles: impl IntoIterator<Item = CowObjectHandle>,
    domain: &[u8],
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    // Store borrowed overlay dictionaries without a deep clone; source-only
    // dictionaries are materialized once and retained for collision checks.
    let mut canonical_by_fingerprint =
        HashMap::<[u8; 32], Vec<(CowObjectHandle, std::borrow::Cow<'_, OwnedObject>)>>::new();
    let mut redirects = HashMap::new();

    for handle in handles {
        let Some(object) = document.current_owned_object_cow(handle)? else {
            continue;
        };
        let Some(fingerprint) = exact_dictionary_fingerprint(&object, domain) else {
            continue;
        };

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

pub fn has_exact_duplicate_extgstate_dictionaries(
    document: &EditDocument,
    handles: &[CowObjectHandle],
) -> Result<bool> {
    let mut fingerprints = HashSet::new();
    for handle in handles {
        let Some(object) = document.current_owned_object_cow(*handle)? else {
            continue;
        };
        let Some(fingerprint) =
            exact_dictionary_fingerprint(&object, b"exact-extgstate-dictionary")
        else {
            continue;
        };
        if !fingerprints.insert(fingerprint) {
            // A real duplicate or a hash collision both conservatively require
            // the authoritative full pass.
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn canonicalize_exact_extgstate_dictionaries(
    document: &mut EditDocument,
) -> Result<ExactObjectDedupStats> {
    let targets = dictionary_targets(document, b"ExtGState")?;
    let mut handles = BTreeSet::new();
    for target in &targets {
        let Some(entries) = target.entries(document)? else {
            continue;
        };
        handles.extend(entries.filter_map(|(_, value)| value.reference()));
    }

    let redirects = exact_dictionary_redirects(
        document,
        handles.iter().copied(),
        b"exact-extgstate-dictionary",
    )?;
    let references_canonicalized =
        rewrite_dictionary_target_entries(document, &targets, None, &redirects)?;
    Ok(ExactObjectDedupStats {
        duplicate_objects_detected: redirects.len(),
        references_canonicalized,
    })
}

pub fn canonicalize_exact_structure_attribute_dictionaries(
    document: &mut EditDocument,
) -> Result<ExactObjectDedupStats> {
    let mut holders = Vec::new();
    let mut handles = BTreeSet::new();
    document.walk_output_objects(|root, object| {
        if !object.resolved_name_is(document, b"Type", b"StructElem")? {
            return Ok(());
        }
        let target = object
            .dictionary_entry(b"A")
            .and_then(|value| value.reference());

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

    let redirects = exact_dictionary_redirects(
        document,
        handles.iter().copied(),
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
        let matches_type =
            object.is_dictionary() && object.resolved_name_is(document, b"Type", object_type)?;
        if matches_type {
            handles.push(handle);
        }
        Ok(())
    })?;
    Ok(handles)
}

pub fn canonicalize_exact_font_dictionaries(
    document: &mut EditDocument,
) -> Result<ExactObjectDedupStats> {
    let redirects = exact_dictionary_redirects(
        document,
        reachable_dictionaries_with_type(document, b"Font")?,
        b"exact-font-dictionary",
    )?;
    let references_canonicalized = rewrite_all_references(document, &redirects)?;
    Ok(ExactObjectDedupStats {
        duplicate_objects_detected: redirects.len(),
        references_canonicalized,
    })
}

fn exact_form_font_redirects(
    document: &EditDocument,
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    fingerprint_redirects(
        reachable_dictionaries_with_type(document, b"Font")?,
        |handle| {
            hash_non_stream_object_with_redirects(
                document,
                handle,
                b"form-font-resource-dictionary",
                exact_redirects,
            )
        },
    )
}

fn source_stream_fingerprint_top_level_redirects(
    document: &EditDocument,
    stream: CowObjectHandle,
    domain: &[u8],
    ignored_dictionary_keys: &[&[u8]],
    redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<[u8; 32]>> {
    Ok(hash_stream_with_keys(
        document,
        stream,
        domain,
        ignored_dictionary_keys,
        FingerprintReferences::TopLevel(redirects),
    )?
    .map(|(fingerprint, _)| fingerprint))
}

fn virtual_form_image_redirects(
    document: &EditDocument,
    images: &[CowObjectHandle],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name"]
    } else {
        &[]
    };
    fingerprint_redirects(images.iter().copied(), |image| {
        source_stream_fingerprint_top_level_redirects(
            document,
            image,
            b"form-resource-image",
            ignored,
            exact_redirects,
        )
    })
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

fn normalized_named_form_resource(
    document: &EditDocument,
    value: &OwnedObject,
    resource_kind: &[u8],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    form_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<OwnedObject> {
    let Some(dictionary) = resolved_dictionary(document, Some(value))? else {
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

fn normalized_form_resources(
    document: &EditDocument,
    value: &OwnedObject,
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    form_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<OwnedObject> {
    let Some(dictionary) = resolved_dictionary(document, Some(value))? else {
        return Ok(value.clone());
    };
    let mut normalized = OwnedDictionary::new();
    for (key, value) in dictionary {
        let value = match key.as_slice() {
            b"Font" | b"XObject" | b"ColorSpace" | b"ExtGState" | b"Properties" => {
                normalized_named_form_resource(
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

fn source_form_fingerprint(
    document: &EditDocument,
    form: CowObjectHandle,
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    form_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<Option<([u8; 32], usize)>> {
    let Some(object) = document.current_owned_object_cow(form)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object.as_ref() else {
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
    let entries = dictionary.iter().filter(|(key, _)| {
        key.as_slice() != b"Length" && !ignored_dictionary_keys.contains(&key.as_slice())
    });
    hasher.update((entries.clone().count() as u64).to_le_bytes());
    for (key, value) in entries {
        hash_len_prefixed(&mut hasher, key);
        if key.as_slice() == b"Resources" {
            let normalized = normalized_form_resources(
                document,
                value,
                exact_redirects,
                font_redirects,
                image_redirects,
                form_redirects,
            )?;
            hash_cos_value(&mut hasher, &normalized, FingerprintReferences::Exact);
        } else {
            hash_cos_value(&mut hasher, value, FingerprintReferences::Exact);
        }
    }
    Ok(Some((hasher.finalize().into(), raw.len())))
}

fn fixed_point_form_redirects(
    document: &EditDocument,
    forms: &[CowObjectHandle],
    ignored_dictionary_keys: &[&[u8]],
    exact_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    font_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
    image_redirects: &HashMap<CowObjectHandle, CowObjectHandle>,
) -> Result<HashMap<CowObjectHandle, CowObjectHandle>> {
    let mut redirects = HashMap::new();
    for _ in 0..=forms.len() {
        let next = fingerprint_redirects(forms.iter().copied(), |form| {
            Ok(source_form_fingerprint(
                document,
                form,
                ignored_dictionary_keys,
                exact_redirects,
                font_redirects,
                image_redirects,
                &redirects,
            )?
            .map(|(fingerprint, _)| fingerprint))
        })?;
        if next == redirects {
            return Ok(next);
        }
        redirects = next;
    }
    Ok(redirects)
}

struct WidgetMkVisitor<'a> {
    targets: &'a mut BTreeSet<DirectDictionaryTarget>,
}

impl DirectObjectVisitor for WidgetMkVisitor<'_> {
    fn visit_dictionary(
        &mut self,
        document: &EditDocument,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        // Preserve the permissive classification of malformed/missing subtype.
        if !dictionary
            .resolved_name_is(document, b"Subtype", b"Widget")
            .unwrap_or(false)
        {
            return Ok(DirectWalkControl::Descend);
        }
        if let Some(target) = DirectDictionaryTarget::from_property(root, path, dictionary, b"MK") {
            self.targets.insert(target);
        }
        Ok(DirectWalkControl::DescendExcept(&[b"MK"]))
    }

    fn visit_source_stream_length(&self) -> bool {
        false
    }
}

fn widget_mk_targets(document: &EditDocument) -> Result<BTreeSet<DirectDictionaryTarget>> {
    let mut targets = BTreeSet::new();
    walk_direct_output_objects(
        document,
        &mut WidgetMkVisitor {
            targets: &mut targets,
        },
    )?;
    Ok(targets)
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
        let Some(object) = document.current_owned_object_cow(form)? else {
            continue;
        };
        let OwnedObject::Stream { data, .. } = object.as_ref() else {
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

fn form_dependency_redirects(
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
    let exact = exact_non_stream_resource_redirects(document, &resources)?;
    let fonts = exact_form_font_redirects(document, &exact)?;
    let image_redirects = virtual_form_image_redirects(document, &images, &exact)?;
    let forms = fixed_point_form_redirects(
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

const FACTORED_FORM_BODY_NAME: &[u8] = b"PdfRedoxSharedForm";
const FACTORED_FORM_BODY_CONTENT: &[u8] = b"/PdfRedoxSharedForm Do\n";
const MIN_FORM_METADATA_DUPLICATE_RAW_BYTES: usize = 4096;
const MIN_FORM_METADATA_BODY_RAW_BYTES: usize = 256;

fn metadata_wrapper_safe_form(
    document: &EditDocument,
    form: CowObjectHandle,
) -> Result<Option<usize>> {
    let Some(object) = document.current_owned_object_cow(form)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = object.as_ref() else {
        return Ok(None);
    };
    let allowed = [
        b"Type".as_slice(),
        b"Subtype".as_slice(),
        b"FormType".as_slice(),
        b"BBox".as_slice(),
        b"Resources".as_slice(),
        b"Filter".as_slice(),
        b"DecodeParms".as_slice(),
        b"Length".as_slice(),
        b"LastModified".as_slice(),
        b"Name".as_slice(),
    ];
    if dictionary
        .keys()
        .any(|key| !allowed.contains(&key.as_slice()))
    {
        return Ok(None);
    }
    if !dictionary.contains_key(b"LastModified".as_slice()) {
        return Ok(None);
    }
    // Require an explicit empty resource dictionary. An absent `/Resources`
    // can inherit the caller's resources in PDF interpreters, while a present
    // empty dictionary intentionally does not. Wrapping only the latter keeps
    // resource lookup unchanged even for malformed bodies that name resources.
    let Some(resources) = dictionary.get(b"Resources".as_slice()) else {
        return Ok(None);
    };
    let Some(resources) = resolved_dictionary(document, Some(resources))? else {
        return Ok(None);
    };
    if !resources.is_empty() {
        return Ok(None);
    }
    let raw_bytes = data.bytes(document.source())?.len();
    Ok((raw_bytes >= MIN_FORM_METADATA_BODY_RAW_BYTES).then_some(raw_bytes))
}

fn form_last_modified(
    document: &EditDocument,
    form: CowObjectHandle,
) -> Result<Option<OwnedObject>> {
    let Some(object) = document.current_owned_object_cow(form)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, .. } = object.as_ref() else {
        return Ok(None);
    };
    Ok(dictionary.get(b"LastModified".as_slice()).cloned())
}

fn rewrite_form_as_metadata_wrapper(
    document: &mut EditDocument,
    form: CowObjectHandle,
    canonical: CowObjectHandle,
) -> Result<bool> {
    rewrite_as_form_wrapper(
        document,
        form,
        FACTORED_FORM_BODY_NAME,
        canonical,
        FACTORED_FORM_BODY_CONTENT,
    )
}

pub fn factor_form_payloads_across_metadata(
    document: &mut EditDocument,
) -> Result<FormMetadataPayloadFactorStats> {
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name", b"LastModified"]
    } else {
        &[b"LastModified"]
    };
    let dependencies = form_dependency_redirects(document, ignored)?;
    if dependencies.forms.is_empty() {
        return Ok(FormMetadataPayloadFactorStats::default());
    }

    let mut groups = BTreeMap::<CowObjectHandle, Vec<(CowObjectHandle, usize)>>::new();
    for (&form, &canonical) in &dependencies.forms {
        let Some(raw_bytes) = metadata_wrapper_safe_form(document, form)? else {
            continue;
        };
        if metadata_wrapper_safe_form(document, canonical)?.is_none()
            || form_last_modified(document, form)? == form_last_modified(document, canonical)?
        {
            continue;
        }
        groups.entry(canonical).or_default().push((form, raw_bytes));
    }

    let mut stats = FormMetadataPayloadFactorStats::default();
    for (canonical, members) in groups {
        let duplicate_raw_bytes = members
            .iter()
            .map(|(_, raw_bytes)| *raw_bytes)
            .sum::<usize>();
        if duplicate_raw_bytes < MIN_FORM_METADATA_DUPLICATE_RAW_BYTES {
            continue;
        }
        let mut rewritten = 0usize;
        for (form, _) in members {
            rewritten = rewritten.saturating_add(usize::from(rewrite_form_as_metadata_wrapper(
                document, form, canonical,
            )?));
        }
        if rewritten == 0 {
            continue;
        }
        stats.groups_factored = stats.groups_factored.saturating_add(1);
        stats.wrappers_rewritten = stats.wrappers_rewritten.saturating_add(rewritten);
        stats.duplicate_raw_bytes_factored = stats
            .duplicate_raw_bytes_factored
            .saturating_add(duplicate_raw_bytes);
    }
    Ok(stats)
}

pub fn canonicalize_form_xobjects(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let ignored: &[&[u8]] = if document.source().version() > PdfVersion::Pdf10 {
        &[b"Name"]
    } else {
        &[]
    };
    let dependencies = form_dependency_redirects(document, ignored)?;
    if dependencies.forms.is_empty() {
        return Ok(TargetedDedupStats::default());
    }
    let mut duplicate_raw_bytes = 0_usize;
    for &form in &dependencies.form_streams {
        if dependencies.forms.contains_key(&form)
            && let Some(object) = document.current_owned_object_cow(form)?
            && let OwnedObject::Stream { data, .. } = object.as_ref()
        {
            duplicate_raw_bytes += data.bytes(document.source())?.len();
        }
    }
    let xobject_targets = dictionary_targets(document, b"XObject")?;
    let mut references_canonicalized =
        rewrite_dictionary_target_entries(document, &xobject_targets, None, &dependencies.forms)?;
    let icon_targets = widget_mk_targets(document)?;
    references_canonicalized += rewrite_dictionary_target_entries(
        document,
        &icon_targets,
        Some(&[b"I", b"RI", b"IX"]),
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
    let mut targets = dictionary_targets(document, b"AP")?;
    let initial = targets.iter().cloned().collect::<Vec<_>>();
    for target in initial {
        let Some(dictionary) = target.current(document)? else {
            continue;
        };
        for key in [b"N".as_slice(), b"R".as_slice(), b"D".as_slice()] {
            let Some(value) = dictionary.dictionary_entry(key) else {
                continue;
            };
            if let Some(handle) = value.reference() {
                if document
                    .current_value(handle)?
                    .is_some_and(|object| object.is_dictionary())
                {
                    targets.insert(DirectDictionaryTarget {
                        root: handle,
                        path: Vec::new(),
                    });
                }
            } else if value.is_dictionary() {
                let mut path = target.path.clone();
                path.push(DirectPathStep::DictKey(key.to_vec()));
                targets.insert(DirectDictionaryTarget {
                    root: target.root,
                    path,
                });
            }
        }
    }
    Ok(targets)
}

pub fn canonicalize_appearance_streams(document: &mut EditDocument) -> Result<TargetedDedupStats> {
    let holders = appearance_dictionary_targets(document)?;
    if holders.is_empty() {
        return Ok(TargetedDedupStats::default());
    }
    let dependencies = form_dependency_redirects(document, &[])?;
    let mut plan = FingerprintRedirectPlan::new();
    for holder in &holders {
        let Some(entries) = holder.entries(document)? else {
            continue;
        };
        for (_, appearance) in entries {
            let Some(appearance_ref) = appearance.reference() else {
                continue;
            };
            let Some((fingerprint, raw_bytes)) = source_form_fingerprint(
                document,
                appearance_ref,
                &[],
                &dependencies.exact,
                &dependencies.fonts,
                &dependencies.images,
                &dependencies.forms,
            )?
            else {
                continue;
            };
            plan.observe_stream(appearance_ref, appearance_ref, fingerprint, raw_bytes);
        }
    }
    let references_canonicalized =
        rewrite_dictionary_target_entries(document, &holders, None, &plan.redirects)?;
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

fn page_content_holders(
    document: &EditDocument,
    pages: &[CowObjectHandle],
) -> Result<Vec<PageContentHolder>> {
    let mut holders = Vec::new();
    for &page in pages {
        let Some(contents) = document
            .current_value(page)?
            .and_then(|object| object.dictionary_entry(b"Contents"))
        else {
            continue;
        };
        let (root, path, contents) = if let Some(target) = contents.reference() {
            let Some(resolved) = document.current_value(target)? else {
                continue;
            };
            if resolved.is_stream() {
                holders.push(PageContentHolder::Dictionary(DirectReferenceHolder {
                    root: page,
                    path: Vec::new(),
                    key: b"Contents".to_vec(),
                    target,
                }));
                continue;
            }
            (target, Vec::new(), resolved)
        } else {
            (
                page,
                vec![DirectPathStep::DictKey(b"Contents".to_vec())],
                contents,
            )
        };
        let Some(values) = contents.array_items() else {
            continue;
        };
        for (index, value) in values.enumerate() {
            let Some(target) = value.reference() else {
                continue;
            };
            if document
                .current_value(target)?
                .is_some_and(|object| object.is_stream())
            {
                holders.push(PageContentHolder::Array(DirectArrayReferenceHolder {
                    root,
                    path: path.clone(),
                    index,
                    target,
                }));
            }
        }
    }
    Ok(holders)
}

pub fn canonicalize_page_contents(
    document: &mut EditDocument,
    pages: &[CowObjectHandle],
) -> Result<TargetedDedupStats> {
    let holders = page_content_holders(document, pages)?;
    canonicalize_stream_holders(
        document,
        &holders,
        PageContentHolder::target,
        PageContentHolder::target,
        |document, holder| source_stream_fingerprint(document, holder.target(), b"page-content"),
        |document, holder, canonical| holder.rewrite(document, canonical),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourcePdf, test_support::ClassicPdfBuilder};

    #[test]
    fn cos_fingerprint_preserves_exact_and_projected_encodings() -> Result<()> {
        let mut document = EditDocument::from_bytes(duplicate_image_fixture()?)?;
        let existing = |number, generation| {
            CowObjectHandle::Existing(crate::ObjectId::new(number, generation))
        };
        let original = existing(42, 3);
        let added = CowObjectHandle::New(document.add_object(OwnedObject::Null));
        let canonical = existing(7, 0);
        let redirects = HashMap::from([(original, added), (added, canonical)]);
        let value = OwnedObject::Dictionary(OwnedDictionary::from([
            (
                b"A".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Null,
                    OwnedObject::Boolean(true),
                    OwnedObject::Integer(-123),
                    OwnedObject::Real(1.25),
                    OwnedObject::Name(b"N\0".to_vec()),
                    OwnedObject::String(vec![0, 255]),
                    OwnedObject::Reference(original),
                    OwnedObject::Stream {
                        dictionary: OwnedDictionary::from([(
                            b"N".to_vec(),
                            OwnedObject::Reference(added),
                        )]),
                        data: StreamData::Source(crate::ObjectId::new(99, 2)),
                    },
                ]),
            ),
            (
                b"M".to_vec(),
                OwnedObject::Stream {
                    dictionary: OwnedDictionary::new(),
                    data: StreamData::Owned(b"q Q".to_vec()),
                },
            ),
            (b"Z".to_vec(), OwnedObject::Reference(original)),
        ]));
        let digest = |object: &OwnedObject, mode| {
            let mut hasher = Sha256::new();
            hash_cos_value(&mut hasher, object, mode);
            <[u8; 32]>::from(hasher.finalize())
        };
        // Frozen digest values from the previous exact and recursive byte protocols,
        // including stream dictionary-before-payload order.
        assert_eq!(
            digest(&value, FingerprintReferences::Exact),
            [
                0x26, 0x4d, 0x87, 0x6d, 0x7c, 0xd9, 0xbb, 0xf0, 0x1f, 0x8a, 0xd8, 0x98, 0x61, 0x3a,
                0x6e, 0xa7, 0x79, 0xd1, 0x2c, 0xb4, 0x8b, 0xb9, 0xb6, 0x2d, 0x27, 0xbf, 0x5b, 0x8d,
                0xc1, 0xc7, 0xe1, 0x0e
            ]
        );
        assert_eq!(
            digest(&value, FingerprintReferences::Recursive(&redirects)),
            [
                0xd3, 0x10, 0x66, 0xfb, 0xb6, 0x9f, 0xe9, 0xbd, 0x06, 0xfe, 0xc0, 0x63, 0x04, 0xd2,
                0x88, 0x1b, 0x42, 0x49, 0x34, 0x04, 0x08, 0xb6, 0x36, 0xdb, 0x9c, 0xd5, 0x96, 0xc5,
                0x1b, 0x1f, 0xc5, 0x0a
            ]
        );
        assert_eq!(
            digest(&value, FingerprintReferences::TopLevel(&redirects)),
            [
                0x26, 0x4d, 0x87, 0x6d, 0x7c, 0xd9, 0xbb, 0xf0, 0x1f, 0x8a, 0xd8, 0x98, 0x61, 0x3a,
                0x6e, 0xa7, 0x79, 0xd1, 0x2c, 0xb4, 0x8b, 0xb9, 0xb6, 0x2d, 0x27, 0xbf, 0x5b, 0x8d,
                0xc1, 0xc7, 0xe1, 0x0e
            ]
        );
        assert_eq!(
            digest(
                &OwnedObject::Reference(original),
                FingerprintReferences::TopLevel(&redirects)
            ),
            [
                0x6c, 0x0b, 0xa4, 0xbe, 0xaa, 0x20, 0xbe, 0x97, 0x4e, 0x1f, 0x85, 0xcc, 0xbc, 0x32,
                0xd5, 0x2e, 0x57, 0xb9, 0x84, 0xc7, 0xa6, 0x14, 0x2a, 0x32, 0x88, 0xce, 0x87, 0x0c,
                0x06, 0x84, 0xff, 0x48
            ]
        );
        Ok(())
    }

    #[test]
    fn cos_fingerprint_handles_deep_owned_nesting() -> Result<()> {
        let depth = 20_000;
        let mut value = OwnedObject::Null;
        for _ in 0..depth {
            value = OwnedObject::Array(vec![value]);
        }
        let mut expected = Sha256::new();
        for _ in 0..depth {
            expected.update([0x06]);
            expected.update(1u64.to_le_bytes());
        }
        expected.update([0x00]);
        let mut actual = Sha256::new();
        hash_cos_value(&mut actual, &value, FingerprintReferences::Exact);
        assert_eq!(actual.finalize(), expected.finalize());
        // The separate owned-object destructor is recursive; dismantle
        // the synthetic input after exercising the iterative encoder.
        for _ in 0..depth {
            let OwnedObject::Array(mut children) = value else {
                return Err(crate::Error::Invalid("unexpected fixture shape".to_owned()));
            };
            value = children
                .pop()
                .ok_or_else(|| crate::Error::Invalid("missing fixture child".to_owned()))?;
        }
        assert_eq!(value, OwnedObject::Null);
        Ok(())
    }

    #[test]
    fn lazy_widget_targets_keep_per_dictionary_mk_exclusion() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Extra 4 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R >>")?;
        pdf.object(4, b"<< /Widget [<< /Subtype 8 0 R /MK << /I 5 0 R /Nested << /Subtype /Widget /MK << /I 6 0 R >> >> >> /Sibling << /Subtype /Widget /MK 7 0 R >> >>] /NonWidget << /MK [<< /Subtype /Widget /MK << /I 6 0 R >> >>] >> >>")?;
        pdf.object(5, b"null")?;
        pdf.object(6, b"null")?;
        pdf.object(7, b"<< /I 5 0 R /IX 5 0 R >>")?;
        pdf.object(8, b"/Text")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let existing = |number| CowObjectHandle::Existing(crate::ObjectId::new(number, 0));
        *document.edit_handle(existing(8))? = OwnedObject::Name(b"Widget".to_vec());
        let key = |key: &[u8]| DirectPathStep::DictKey(key.to_vec());
        let widget = DirectDictionaryTarget {
            root: existing(4),
            path: vec![key(b"Widget"), DirectPathStep::ArrayIndex(0), key(b"MK")],
        };
        let nested_nonwidget = DirectDictionaryTarget {
            root: existing(4),
            path: vec![
                key(b"NonWidget"),
                key(b"MK"),
                DirectPathStep::ArrayIndex(0),
                key(b"MK"),
            ],
        };
        let indirect = DirectDictionaryTarget {
            root: existing(7),
            path: Vec::new(),
        };
        assert_eq!(
            widget_mk_targets(&document)?,
            BTreeSet::from([widget.clone(), nested_nonwidget, indirect.clone()])
        );
        let redirects = HashMap::from([(existing(5), existing(6))]);
        assert_eq!(
            rewrite_dictionary_target(&mut document, &widget, Some(&[b"RI"]), &redirects)?,
            0
        );
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(4, 0))
                .is_none()
        );
        assert_eq!(
            rewrite_dictionary_target(&mut document, &widget, Some(&[b"I"]), &redirects)?,
            1
        );
        assert_eq!(
            widget
                .current(&document)?
                .and_then(|value| value.dictionary_entry(b"I"))
                .and_then(|value| value.reference()),
            Some(existing(6))
        );
        assert_eq!(
            rewrite_dictionary_target(&mut document, &indirect, None, &redirects)?,
            2
        );
        assert_eq!(
            rewrite_dictionary_target(&mut document, &indirect, None, &redirects)?,
            0
        );
        Ok(())
    }

    #[test]
    fn lazy_page_contents_holders_preserve_array_positions_and_overlay_identity() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R 9 0 R] /Count 3 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /Contents [5 0 R 7 0 R 6 0 R 5 0 R null] >>",
        )?;
        pdf.object(4, b"<< /Type /Page /Parent 2 0 R /Contents 8 0 R >>")?;
        pdf.stream(5, b"", b"q Q")?;
        pdf.stream(6, b"", b"q Q")?;
        pdf.object(7, b"null")?;
        pdf.object(8, b"[5 0 R 6 0 R]")?;
        pdf.object(9, b"<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let existing = |number| CowObjectHandle::Existing(crate::ObjectId::new(number, 0));
        let pages = [existing(3), existing(4), existing(9)];
        let holders = page_content_holders(&document, &pages)?;
        let array = |root, path, index, target| {
            PageContentHolder::Array(DirectArrayReferenceHolder {
                root,
                path,
                index,
                target,
            })
        };
        let contents_path = vec![DirectPathStep::DictKey(b"Contents".to_vec())];
        assert_eq!(
            holders,
            vec![
                array(existing(3), contents_path.clone(), 0, existing(5)),
                array(existing(3), contents_path.clone(), 2, existing(6)),
                array(existing(3), contents_path, 3, existing(5)),
                array(existing(8), Vec::new(), 0, existing(5)),
                array(existing(8), Vec::new(), 1, existing(6)),
                PageContentHolder::Dictionary(DirectReferenceHolder {
                    root: existing(9),
                    path: Vec::new(),
                    key: b"Contents".to_vec(),
                    target: existing(6)
                }),
            ]
        );
        for number in [3, 4, 8, 9] {
            let _ = document.edit_handle(existing(number))?;
        }
        assert_eq!(page_content_holders(&document, &pages)?, holders);
        Ok(())
    }

    fn duplicate_image_fixture_with_payload(sample_count: usize) -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q /A Do /B Do Q")?;
        let image = format!(
            "/Type /XObject /Subtype /Image /Width {sample_count} /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8"
        );
        let pixels = vec![0x80; sample_count];
        pdf.stream(5, image.as_bytes(), &pixels)?;
        pdf.stream(6, image.as_bytes(), &pixels)?;
        pdf.finish(1)
    }

    fn duplicate_image_fixture() -> Result<Vec<u8>> {
        duplicate_image_fixture_with_payload(1)
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
    fn font_program_holder_single_walk_matches_three_legacy_scans() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Private << /FontFile 5 0 R /Nested << /FontFile2 6 0 R /Deeper [<< /FontFile3 7 0 R >>] >> /Malformed << /FontFile2 << /FontFile2 8 0 R /FontFile3 9 0 R >> >> >> >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"font-one")?;
        pdf.stream(6, b"", b"font-two")?;
        pdf.stream(7, b"", b"font-three")?;
        pdf.stream(8, b"", b"shadowed-font-two")?;
        pdf.stream(9, b"", b"nested-font-three")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;

        let expected = HAYRO_FONT_FILE_KEYS
            .into_iter()
            .map(|key| direct_reference_holders(&document, key))
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect::<Vec<_>>();
        let actual = font_program_holders(&document)?;
        assert_eq!(actual, expected);
        assert!(!actual.iter().any(|holder| {
            holder.target == CowObjectHandle::Existing(crate::ObjectId::new(8, 0))
        }));
        assert!(actual.iter().any(|holder| {
            holder.target == CowObjectHandle::Existing(crate::ObjectId::new(9, 0))
        }));

        let root = CowObjectHandle::Existing(crate::ObjectId::new(1, 0));
        let _ = document.edit_handle(root)?;
        assert_eq!(font_program_holders(&document)?, expected);
        Ok(())
    }

    #[test]
    fn dictionary_target_shared_walk_matches_source_and_owned() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Private << /XObject << /A 5 0 R /Nested << /XObject << /B 7 0 R >> >> >> /Other << /XObject 6 0 R >> >> >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"image-a")?;
        pdf.object(6, b"<< /C 5 0 R >>")?;
        pdf.stream(7, b"", b"shadowed-image")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;

        let root = CowObjectHandle::Existing(crate::ObjectId::new(1, 0));
        let indirect = CowObjectHandle::Existing(crate::ObjectId::new(6, 0));
        let expected = BTreeSet::from([
            DirectDictionaryTarget {
                root,
                path: vec![
                    DirectPathStep::DictKey(b"Private".to_vec()),
                    DirectPathStep::DictKey(b"XObject".to_vec()),
                ],
            },
            DirectDictionaryTarget {
                root: indirect,
                path: Vec::new(),
            },
        ]);
        assert_eq!(dictionary_targets(&document, b"XObject")?, expected);

        // Materializing the catalog switches the same direct subtree to the
        // owned representation; traversal semantics and target identity stay
        // exactly the same.
        let _ = document.edit_handle(root)?;
        assert_eq!(dictionary_targets(&document, b"XObject")?, expected);
        Ok(())
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

    fn overlay_with_owned_streams(input: &[u8], stream_ids: &[u32]) -> Result<EditDocument> {
        let mut overlay = EditDocument::from_bytes(input.to_vec())?;
        for &id in stream_ids {
            let handle = CowObjectHandle::Existing(crate::ObjectId::new(id, 0));
            let bytes = overlay
                .source()
                .stream_data(crate::ObjectId::new(id, 0))?
                .into_owned();
            if let OwnedObject::Stream { data, .. } = overlay.edit_handle(handle)? {
                *data = StreamData::Owned(bytes);
            }
            let Some(std::borrow::Cow::Borrowed(OwnedObject::Stream {
                data: StreamData::Owned(borrowed),
                ..
            })) = overlay.current_owned_object_cow(handle)?
            else {
                return Err(crate::Error::Invalid("stream data was cloned".into()));
            };
            let Some(CurrentValue::Owned(OwnedObject::Stream {
                data: StreamData::Owned(original),
                ..
            })) = overlay.current_value(handle)?
            else {
                return Err(crate::Error::Invalid("missing overlay stream".into()));
            };
            assert_eq!(borrowed.as_ptr(), original.as_ptr());
        }
        Ok(overlay)
    }

    fn dedup_source_overlay_pair<T: std::fmt::Debug + PartialEq>(
        input: Vec<u8>,
        stream_ids: &[u32],
        pass: impl Fn(&mut EditDocument) -> Result<T>,
    ) -> Result<()> {
        let mut overlay = overlay_with_owned_streams(&input, stream_ids)?;
        let mut source = EditDocument::from_bytes(input)?;
        for &id in stream_ids {
            let handle = CowObjectHandle::Existing(crate::ObjectId::new(id, 0));
            assert_eq!(
                source_stream_fingerprint(&source, handle, b"owned-payload")?,
                source_stream_fingerprint(&overlay, handle, b"owned-payload")?
            );
        }
        assert_eq!(source.write_compact()?, overlay.write_compact()?);
        assert_eq!(pass(&mut source)?, pass(&mut overlay)?);
        assert_eq!(source.write_compact()?, overlay.write_compact()?);
        Ok(())
    }

    #[test]
    fn exact_dictionary_redirects_compare_borrowed_overlay_candidates() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        for (id, value) in [
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".as_slice()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".as_slice()),
            (3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ExtGState << /A 5 0 R /B 6 0 R /C 7 0 R >> >> /Contents 4 0 R >>".as_slice()),
            (5, b"<< /Type /ExtGState /CA 0.5 /ca 0.5 >>".as_slice()),
            (6, b"<< /Type /ExtGState /CA 0.5 /ca 0.5 >>".as_slice()),
            (7, b"<< /Type /ExtGState /CA 0.9 /ca 0.5 >>".as_slice()),
        ] {
            pdf.object(id, value)?;
        }
        pdf.stream(4, b"", b"q /A gs Q q /B gs Q q /C gs Q")?;
        let bytes = pdf.finish(1)?;
        let mut source = EditDocument::from_bytes(bytes.clone())?;
        let mut overlay = EditDocument::from_bytes(bytes)?;
        let handles = [5, 6, 7].map(|id| CowObjectHandle::Existing(crate::ObjectId::new(id, 0)));

        for handle in handles {
            let _ = overlay.edit_handle(handle)?;
            let Some(std::borrow::Cow::Borrowed(borrowed)) =
                overlay.current_owned_object_cow(handle)?
            else {
                return Err(crate::Error::Invalid("expected borrowed dictionary".into()));
            };
            let Some(CurrentValue::Owned(original)) = overlay.current_value(handle)? else {
                return Err(crate::Error::Invalid("missing owned dictionary".into()));
            };
            assert!(std::ptr::eq(borrowed, original));
        }
        assert!(has_exact_duplicate_extgstate_dictionaries(
            &source, &handles
        )?);
        assert!(has_exact_duplicate_extgstate_dictionaries(
            &overlay, &handles
        )?);
        let expected = exact_dictionary_redirects(&source, handles, b"exact-extgstate-dictionary")?;
        assert_eq!(
            exact_dictionary_redirects(&overlay, handles, b"exact-extgstate-dictionary")?,
            expected
        );
        assert_eq!(expected.len(), 1);
        assert_eq!(expected.get(&handles[1]), Some(&handles[0]));
        assert_eq!(
            canonicalize_exact_extgstate_dictionaries(&mut source)?,
            canonicalize_exact_extgstate_dictionaries(&mut overlay)?
        );
        assert_eq!(source.write_compact()?, overlay.write_compact()?);
        Ok(())
    }

    #[test]
    fn owned_image_stream_dedup_matches_source_with_megabyte_payloads() -> Result<()> {
        dedup_source_overlay_pair(
            duplicate_image_fixture_with_payload(1 << 20)?,
            &[5, 6],
            canonicalize_image_xobjects,
        )
    }

    #[test]
    fn owned_page_stream_dedup_matches_source() -> Result<()> {
        dedup_source_overlay_pair(duplicate_page_content_fixture()?, &[5, 6], |document| {
            let pages = document.page_handles()?;
            canonicalize_page_contents(document, &pages)
        })
    }

    #[test]
    fn owned_pattern_dedup_preserves_fingerprints_and_output() -> Result<()> {
        let (input, _) = duplicate_pattern_payload_fixture()?;
        let mut source = EditDocument::from_bytes(input.clone())?;
        let mut overlay = overlay_with_owned_streams(&input, &[5, 6])?;
        for id in [5, 6] {
            let handle = CowObjectHandle::Existing(crate::ObjectId::new(id, 0));
            assert_eq!(
                pattern_payload_fingerprint(&source, handle)?,
                pattern_payload_fingerprint(&overlay, handle)?
            );
        }
        let a = CowObjectHandle::Existing(crate::ObjectId::new(5, 0));
        let b = CowObjectHandle::Existing(crate::ObjectId::new(6, 0));
        assert_eq!(
            pattern_payloads_equal(&source, a, b)?,
            pattern_payloads_equal(&overlay, a, b)?
        );
        assert_eq!(
            factor_duplicate_pattern_payloads(&mut source)?,
            factor_duplicate_pattern_payloads(&mut overlay)?
        );
        assert_eq!(source.write_compact()?, overlay.write_compact()?);
        Ok(())
    }

    #[test]
    fn owned_form_metadata_dedup_preserves_hashes_and_output() -> Result<()> {
        let (input, _) = metadata_variant_form_fixture(false)?;
        let mut source = EditDocument::from_bytes(input.clone())?;
        let mut overlay = overlay_with_owned_streams(&input, &[5, 6])?;
        let handles = [5, 6].map(|id| CowObjectHandle::Existing(crate::ObjectId::new(id, 0)));
        for handle in handles {
            assert_eq!(
                metadata_wrapper_safe_form(&source, handle)?,
                metadata_wrapper_safe_form(&overlay, handle)?
            );
            assert_eq!(
                form_last_modified(&source, handle)?,
                form_last_modified(&overlay, handle)?
            );
            let empty = HashMap::new();
            assert_eq!(
                source_form_fingerprint(&source, handle, &[], &empty, &empty, &empty, &empty)?,
                source_form_fingerprint(&overlay, handle, &[], &empty, &empty, &empty, &empty)?
            );
        }
        assert_eq!(
            stream_payloads_may_repeat(&source, &handles)?,
            stream_payloads_may_repeat(&overlay, &handles)?
        );
        assert_eq!(
            factor_form_payloads_across_metadata(&mut source)?,
            factor_form_payloads_across_metadata(&mut overlay)?
        );
        assert_eq!(source.write_compact()?, overlay.write_compact()?);
        Ok(())
    }

    #[test]
    fn hayro_image_dedup_canonicalizes_exact_resource_duplicates() -> Result<()> {
        let mut document = EditDocument::from_bytes(duplicate_image_fixture()?)?;
        let stats = canonicalize_image_xobjects(&mut document)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 1);
        Ok(())
    }

    #[test]
    fn hayro_page_content_dedup_canonicalizes_exact_streams() -> Result<()> {
        let mut document = EditDocument::from_bytes(duplicate_page_content_fixture()?)?;
        let pages = document.page_handles()?;
        let stats = canonicalize_page_contents(&mut document, &pages)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 2);
        Ok(())
    }

    fn duplicate_pattern_payload_fixture() -> Result<(Vec<u8>, usize)> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Pattern << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(
            4,
            b"",
            b"/Pattern cs /A scn 0 0 50 100 re f /B scn 50 0 50 100 re f",
        )?;
        let body = b"0 0 1 rg 0 0 10 10 re f\n".repeat(64);
        let common = b"/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources <<>>";
        let mut first = common.to_vec();
        first.extend_from_slice(b" /Matrix [1 0 0 1 0 0]");
        let mut second = common.to_vec();
        second.extend_from_slice(b" /Matrix [1 0 0 1 3 4]");
        pdf.stream(5, &first, &body)?;
        pdf.stream(6, &second, &body)?;
        Ok((pdf.finish(1)?, body.len()))
    }

    fn duplicate_pattern_payload_two_group_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Pattern << /A 5 0 R /B 6 0 R /C 7 0 R /D 8 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(
            4,
            b"",
            b"/Pattern cs /A scn 0 0 25 100 re f /B scn 25 0 25 100 re f /C scn 50 0 25 100 re f /D scn 75 0 25 100 re f",
        )?;
        let first_body = b"0 0 1 rg 0 0 10 10 re f\n".repeat(64);
        let second_body = b"1 0 0 rg 0 0 10 10 re f\n".repeat(64);
        let common = b"/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources <<>>";
        for (object, matrix, body) in [
            (
                5_u32,
                b" /Matrix [1 0 0 1 0 0]".as_slice(),
                first_body.as_slice(),
            ),
            (
                6,
                b" /Matrix [1 0 0 1 3 4]".as_slice(),
                first_body.as_slice(),
            ),
            (
                7,
                b" /Matrix [1 0 0 1 6 8]".as_slice(),
                second_body.as_slice(),
            ),
            (
                8,
                b" /Matrix [1 0 0 1 9 12]".as_slice(),
                second_body.as_slice(),
            ),
        ] {
            let mut dictionary = common.to_vec();
            dictionary.extend_from_slice(matrix);
            pdf.stream(object, &dictionary, body)?;
        }
        pdf.finish(1)
    }

    #[test]
    fn pattern_payload_factoring_is_byte_deterministic_across_independent_groups() -> Result<()> {
        let input = duplicate_pattern_payload_two_group_fixture()?;
        let mut expected = None;
        for _ in 0..16 {
            let mut document = EditDocument::from_bytes(input.clone())?;
            let stats = factor_duplicate_pattern_payloads(&mut document)?;
            assert_eq!(stats.groups_factored, 2);
            assert_eq!(stats.patterns_rewritten, 4);
            assert_eq!(stats.forms_created, 2);
            let output = document.write_compact()?;
            if let Some(expected) = &expected {
                assert_eq!(&output, expected);
            } else {
                expected = Some(output);
            }
        }
        Ok(())
    }

    #[test]
    fn pattern_payload_factoring_preserves_distinct_matrices_and_is_idempotent() -> Result<()> {
        let (input, body_len) = duplicate_pattern_payload_fixture()?;
        let mut document = EditDocument::from_bytes(input)?;
        let stats = factor_duplicate_pattern_payloads(&mut document)?;
        assert_eq!(stats.groups_factored, 1);
        assert_eq!(stats.patterns_rewritten, 2);
        assert_eq!(stats.forms_created, 1);
        assert_eq!(stats.duplicate_raw_bytes_factored, body_len);

        let first = document
            .current_owned_object(CowObjectHandle::Existing(crate::ObjectId::new(5, 0)))?
            .ok_or_else(|| crate::Error::Invalid("first pattern is missing".to_owned()))?;
        let second = document
            .current_owned_object(CowObjectHandle::Existing(crate::ObjectId::new(6, 0)))?
            .ok_or_else(|| crate::Error::Invalid("second pattern is missing".to_owned()))?;
        let OwnedObject::Stream {
            dictionary: first_dictionary,
            data: first_data,
        } = first
        else {
            return Err(crate::Error::Invalid(
                "first pattern is not a stream".to_owned(),
            ));
        };
        let OwnedObject::Stream {
            dictionary: second_dictionary,
            data: second_data,
        } = second
        else {
            return Err(crate::Error::Invalid(
                "second pattern is not a stream".to_owned(),
            ));
        };
        assert_eq!(
            first_data.bytes(document.source())?.as_ref(),
            FACTORED_PATTERN_CONTENT
        );
        assert_eq!(
            second_data.bytes(document.source())?.as_ref(),
            FACTORED_PATTERN_CONTENT
        );
        assert_ne!(
            first_dictionary.get(b"Matrix".as_slice()),
            second_dictionary.get(b"Matrix".as_slice())
        );
        assert!(pattern_already_factored(&document, &first_dictionary)?);
        assert!(pattern_already_factored(&document, &second_dictionary)?);

        assert_eq!(
            factor_duplicate_pattern_payloads(&mut document)?,
            PatternPayloadFactorStats::default()
        );
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 1);
        Ok(())
    }

    fn metadata_variant_form_fixture(matrix: bool) -> Result<(Vec<u8>, Vec<u8>)> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q /A Do /B Do Q")?;
        let mut body = Vec::new();
        for _ in 0..400 {
            body.extend_from_slice(b"0 0 m 10 10 l S\n");
        }
        let matrix_entry = if matrix {
            b" /Matrix [1 0 0 1 0 0]".as_slice()
        } else {
            b"".as_slice()
        };
        let mut first = b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources <<>> /LastModified (first)".to_vec();
        first.extend_from_slice(matrix_entry);
        let mut second = b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources <<>> /LastModified (second)".to_vec();
        second.extend_from_slice(matrix_entry);
        pdf.stream(5, &first, &body)?;
        pdf.stream(6, &second, &body)?;
        Ok((pdf.finish(1)?, body))
    }

    #[test]
    fn form_metadata_payload_factoring_preserves_last_modified_and_is_idempotent() -> Result<()> {
        let (input, body) = metadata_variant_form_fixture(false)?;
        let mut document = EditDocument::from_bytes(input)?;
        let stats = factor_form_payloads_across_metadata(&mut document)?;
        assert_eq!(stats.groups_factored, 1);
        assert_eq!(stats.wrappers_rewritten, 1);
        assert_eq!(stats.duplicate_raw_bytes_factored, body.len());

        let mut original_bodies = 0usize;
        let mut wrappers = 0usize;
        let mut last_modified = Vec::new();
        for id in [5_u32, 6] {
            let object = document
                .current_owned_object(CowObjectHandle::Existing(crate::ObjectId::new(id, 0)))?
                .ok_or_else(|| crate::Error::Invalid("form is missing".to_owned()))?;
            let OwnedObject::Stream { dictionary, data } = object else {
                return Err(crate::Error::Invalid("form is not a stream".to_owned()));
            };
            last_modified.push(dictionary.get(b"LastModified".as_slice()).cloned());
            let bytes = data.bytes(document.source())?;
            if bytes.as_ref() == body.as_slice() {
                original_bodies += 1;
            } else if bytes.as_ref() == FACTORED_FORM_BODY_CONTENT {
                wrappers += 1;
                assert!(!dictionary.contains_key(b"Filter".as_slice()));
                let resources = dictionary
                    .get(b"Resources".as_slice())
                    .and_then(OwnedObject::as_dictionary)
                    .ok_or_else(|| crate::Error::Invalid("wrapper Resources missing".to_owned()))?;
                let xobjects = resources
                    .get(b"XObject".as_slice())
                    .and_then(OwnedObject::as_dictionary)
                    .ok_or_else(|| {
                        crate::Error::Invalid("wrapper XObject resources missing".to_owned())
                    })?;
                assert!(matches!(
                    xobjects.get(FACTORED_FORM_BODY_NAME),
                    Some(OwnedObject::Reference(_))
                ));
            }
        }
        assert_eq!(original_bodies, 1);
        assert_eq!(wrappers, 1);
        assert_eq!(last_modified.len(), 2);
        assert_ne!(last_modified[0], last_modified[1]);
        assert_eq!(
            factor_form_payloads_across_metadata(&mut document)?,
            FormMetadataPayloadFactorStats::default()
        );
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 1);
        Ok(())
    }

    #[test]
    fn form_metadata_payload_factoring_rejects_matrix_semantics() -> Result<()> {
        let (input, _) = metadata_variant_form_fixture(true)?;
        let mut document = EditDocument::from_bytes(input)?;
        assert_eq!(
            factor_form_payloads_across_metadata(&mut document)?,
            FormMetadataPayloadFactorStats::default()
        );
        Ok(())
    }

    fn sampled_function_fixture(distinct_dictionary: bool) -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Shading << /S1 7 0 R /S2 8 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"/S1 sh /S2 sh")?;
        let first =
            b"/FunctionType 0 /Domain [0 1] /Range [0 1 0 1 0 1] /Size [2] /BitsPerSample 8";
        let second = if distinct_dictionary {
            b"/FunctionType 0 /Domain [0 1] /Range [0 1 0 1 0 1] /Size [2] /BitsPerSample 8 /Decode [0 0.5 0 1 0 1]".as_slice()
        } else {
            first.as_slice()
        };
        let samples = [0_u8, 0, 0, 255, 255, 255];
        pdf.stream(5, first, &samples)?;
        pdf.stream(6, second, &samples)?;
        pdf.object(7, b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 10 0] /Function 5 0 R /Extend [true true] >>")?;
        pdf.object(8, b"<< /ShadingType 2 /ColorSpace /DeviceRGB /Coords [0 0 10 0] /Function 6 0 R /Extend [true true] >>")?;
        pdf.finish(1)
    }

    #[test]
    fn sampled_function_dedup_rewrites_references_globally_and_is_idempotent() -> Result<()> {
        let mut document = EditDocument::from_bytes(sampled_function_fixture(false)?)?;
        let stats = canonicalize_sampled_function_streams(&mut document)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.duplicate_raw_bytes, 6);
        assert_eq!(stats.references_canonicalized, 1);

        let first = document
            .current_owned_object(CowObjectHandle::Existing(crate::ObjectId::new(7, 0)))?
            .ok_or_else(|| crate::Error::Invalid("first shading missing".to_owned()))?;
        let second = document
            .current_owned_object(CowObjectHandle::Existing(crate::ObjectId::new(8, 0)))?
            .ok_or_else(|| crate::Error::Invalid("second shading missing".to_owned()))?;
        let first_function = first
            .as_dictionary()
            .and_then(|dictionary| dictionary.get(b"Function".as_slice()));
        let second_function = second
            .as_dictionary()
            .and_then(|dictionary| dictionary.get(b"Function".as_slice()));
        assert_eq!(first_function, second_function);
        assert_eq!(
            canonicalize_sampled_function_streams(&mut document)?,
            TargetedDedupStats::default()
        );
        let output = document.write_compact()?;
        assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 1);
        Ok(())
    }

    #[test]
    fn sampled_function_dedup_keeps_semantically_distinct_dictionaries() -> Result<()> {
        let mut document = EditDocument::from_bytes(sampled_function_fixture(true)?)?;
        assert_eq!(
            canonicalize_sampled_function_streams(&mut document)?,
            TargetedDedupStats::default()
        );
        Ok(())
    }
}
