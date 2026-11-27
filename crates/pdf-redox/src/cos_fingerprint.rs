//! Canonical transient COS identity, fingerprint projection and collision proof.
//!
//! Values are visited through the editable source/overlay view. Policies may
//! redirect raw references or resolve them without materializing the graph.
//! Hashes are internal candidate keys, independent of writer serialization.

use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, Result,
    source::{
        CosScalar, CurrentArrayIter, CurrentChildKey, CurrentChildren, CurrentDictionaryIter,
        CurrentValue, NumericValue, StreamIdentity,
    },
};
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use std::{
    borrow::Borrow,
    collections::{BTreeMap, HashMap, HashSet},
};

pub fn hash_len_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

// Fingerprints are in-memory equivalence keys, not a persisted format.
// Containers are prefix-free sequences: a value tag or dictionary-entry tag
// starts each element, and a reserved end tag closes the container. Scalar
// lengths are still explicit. This permits a single pass over lazy COS cursors.
const COS_END: [u8; 1] = [0x09];
const COS_DICTIONARY_ENTRY: [u8; 1] = [0x0a];

fn hash_dictionary_key(hasher: &mut Sha256, key: &[u8]) {
    hasher.update(COS_DICTIONARY_ENTRY);
    hash_len_prefixed(hasher, key);
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
pub enum FingerprintReferences<'a> {
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

fn hash_stream_identity(hasher: &mut Sha256, data: StreamIdentity<'_>) {
    match data {
        StreamIdentity::Source(id) => {
            hasher.update([0x80]);
            hasher.update(id.number().to_le_bytes());
            hasher.update(id.generation().to_le_bytes());
        }
        StreamIdentity::Inline(bytes) => {
            hasher.update([0x81]);
            hash_len_prefixed(hasher, bytes.as_ref());
        }
    }
}

// A frame holds only unvisited siblings. Stream identities are emitted after
// the dictionary, not before it, preserving the unambiguous COS encoding.
enum FingerprintFrame<'a> {
    Array(CurrentArrayIter<'a>),
    Dictionary {
        entries: CurrentDictionaryIter<'a>,
        data: Option<StreamIdentity<'a>>,
    },
}

impl<'a> FingerprintFrame<'a> {
    fn next_value(&mut self, hasher: &mut Sha256) -> Option<CurrentValue<'a>> {
        match self {
            Self::Array(values) => {
                let next = values.next();
                if next.is_none() {
                    hasher.update(COS_END);
                }
                next
            }
            Self::Dictionary { entries, data } => {
                if let Some((key, value)) = entries.next() {
                    hash_dictionary_key(hasher, key.as_ref());
                    Some(value)
                } else {
                    hasher.update(COS_END);
                    if let Some(data) = data.take() {
                        hash_stream_identity(hasher, data);
                    }
                    None
                }
            }
        }
    }
}

fn hash_cos_header<'a>(
    hasher: &mut Sha256,
    object: &CurrentValue<'a>,
    references: FingerprintReferences<'_>,
    is_root: bool,
) -> Option<FingerprintFrame<'a>> {
    if let Some(scalar) = object.scalar() {
        match scalar {
            CosScalar::Null => hasher.update([0x00]),
            CosScalar::Boolean(value) => hasher.update([0x01, u8::from(value)]),
            CosScalar::Number(NumericValue::Integer(value)) => {
                hasher.update([0x02]);
                hasher.update(value.to_le_bytes());
            }
            CosScalar::Number(NumericValue::Real(value)) => {
                hasher.update([0x03]);
                hasher.update(value.to_bits().to_le_bytes());
            }
            CosScalar::Name(value) => {
                hasher.update([0x04]);
                hash_len_prefixed(hasher, value);
            }
            CosScalar::String(value) => {
                hasher.update([0x05]);
                hash_len_prefixed(hasher, value);
            }
            CosScalar::Reference(handle) => references.hash(hasher, handle, is_root),
        }
        None
    } else if let Some(values) = object.array_items() {
        hasher.update([0x06]);
        Some(FingerprintFrame::Array(values))
    } else if let Some(entries) = object.dictionary_entries() {
        let data = object.stream_identity();
        hasher.update([if data.is_some() { 0x08 } else { 0x07 }]);
        Some(FingerprintFrame::Dictionary { entries, data })
    } else {
        unreachable!("non-scalar COS value must be a container")
    }
}

enum FingerprintVisit<'a, S> {
    Leaf,
    Descend(FingerprintFrame<'a>, S),
    Reject,
}

/// The cursor walk is common to all policies. The state is monomorphized:
/// ordinary fingerprints need only root-vs-nested, resolving fingerprints
/// track the bounded semantic depth through direct nodes and references.
fn walk_fingerprint<'a, S: Copy, E>(
    hasher: &mut Sha256,
    root: &CurrentValue<'a>,
    state: S,
    mut visit: impl FnMut(
        &mut Sha256,
        &CurrentValue<'a>,
        S,
    ) -> std::result::Result<FingerprintVisit<'a, S>, E>,
) -> std::result::Result<bool, E> {
    let mut frames = SmallVec::<[(FingerprintFrame<'a>, S); 4]>::new();
    let mut next = visit(hasher, root, state)?;
    loop {
        match next {
            FingerprintVisit::Leaf => {}
            FingerprintVisit::Descend(frame, state) => frames.push((frame, state)),
            FingerprintVisit::Reject => return Ok(false),
        }
        loop {
            let Some((frame, state)) = frames.last_mut() else {
                return Ok(true);
            };
            if let Some(value) = frame.next_value(hasher) {
                next = visit(hasher, &value, *state)?;
                break;
            }
            frames.pop();
        }
    }
}

pub fn hash_cos_value(
    hasher: &mut Sha256,
    root: &CurrentValue<'_>,
    references: FingerprintReferences<'_>,
) {
    match walk_fingerprint::<_, std::convert::Infallible>(
        hasher,
        root,
        true,
        |hasher, value, is_root| {
            Ok(
                hash_cos_header(hasher, value, references, is_root).map_or_else(
                    || FingerprintVisit::Leaf,
                    |frame| FingerprintVisit::Descend(frame, false),
                ),
            )
        },
    ) {
        Ok(true) => {}
        Ok(false) => unreachable!("exact COS encoding cannot reject a value"),
        Err(never) => match never {},
    }
}

const MAX_RESOLVED_COS_DEPTH: usize = 128;
const COS_RESOLVED_REFERENCE: [u8; 1] = [0x72];

fn hash_resolved_value<'a>(
    document: &'a EditDocument,
    hasher: &mut Sha256,
    root: &CurrentValue<'a>,
    depth: usize,
) -> Result<bool> {
    walk_fingerprint(hasher, root, depth, |hasher, value, mut depth| {
        if depth > MAX_RESOLVED_COS_DEPTH {
            return Ok(FingerprintVisit::Reject);
        }
        // Preserve the semantic distinction between a direct value and a
        // reference to that value. A complete indirect chain emits one tag.
        let resolved = if let Some(handle) = value.reference() {
            let Some(indirect) = document.current_value(handle)? else {
                return Ok(FingerprintVisit::Reject);
            };
            let Some(resolved) = document.resolve_current_value(indirect)? else {
                return Ok(FingerprintVisit::Reject);
            };
            hasher.update(COS_RESOLVED_REFERENCE);
            depth += 1;
            Some(resolved)
        } else {
            None
        };
        let value = resolved.as_ref().unwrap_or(value);
        if depth > MAX_RESOLVED_COS_DEPTH || value.is_stream() {
            return Ok(FingerprintVisit::Reject);
        }
        Ok(
            hash_cos_header(hasher, value, FingerprintReferences::Exact, false).map_or_else(
                || FingerprintVisit::Leaf,
                |frame| FingerprintVisit::Descend(frame, depth + 1),
            ),
        )
    })
}

/// Hash a detached dictionary by resolving its references against the current
/// graph. Missing references, nested streams and over-depth graphs cannot
/// establish this stronger semantic equivalence. Never clone the dictionary.
pub fn hash_resolved_owned_dictionary(
    document: &EditDocument,
    hasher: &mut Sha256,
    dictionary: &OwnedDictionary,
) -> Result<bool> {
    hasher.update([0x07]);
    for (key, value) in dictionary {
        hash_dictionary_key(hasher, key);
        if !hash_resolved_value(document, hasher, &CurrentValue::Owned(value), 1)? {
            return Ok(false);
        }
    }
    hasher.update(COS_END);
    Ok(true)
}

type EqualityFrame<'a, 'b> = (CurrentChildren<'a>, CurrentChildren<'b>);

fn compare_cos_node<'a, 'b>(
    left: &CurrentValue<'a>,
    right: &CurrentValue<'b>,
    frames: &mut SmallVec<[EqualityFrame<'a, 'b>; 4]>,
) -> bool {
    match (left.scalar(), right.scalar()) {
        (Some(left), Some(right)) => return left == right,
        (None, None) => {}
        _ => return false,
    }
    if left.stream_identity() != right.stream_identity()
        || left.is_dictionary() != right.is_dictionary()
    {
        return false;
    }
    match (left.editable_children(), right.editable_children()) {
        (Some(left), Some(right)) => {
            frames.push((left, right));
            true
        }
        _ => false,
    }
}

/// Compare the exact editable COS structures, regardless of storage.
/// Hash equality is only a candidate filter; equality never follows references.
pub fn exact_cos_equal(left: &CurrentValue<'_>, right: &CurrentValue<'_>) -> bool {
    let mut frames = SmallVec::<[EqualityFrame<'_, '_>; 4]>::new();
    if !compare_cos_node(left, right, &mut frames) {
        return false;
    }
    compare_cos_frames(frames)
}

fn compare_cos_frames(mut frames: SmallVec<[EqualityFrame<'_, '_>; 4]>) -> bool {
    while let Some((left, right)) = frames.last_mut() {
        match (left.next(), right.next()) {
            (None, None) => {
                frames.pop();
            }
            (Some((left_key, left)), Some((right_key, right))) => {
                let keys_match = match (left_key, right_key) {
                    (CurrentChildKey::Array(left), CurrentChildKey::Array(right)) => left == right,
                    (CurrentChildKey::Dictionary(left), CurrentChildKey::Dictionary(right)) => {
                        left.as_ref() == right.as_ref()
                    }
                    _ => false,
                };
                if !keys_match || !compare_cos_node(&left, &right, &mut frames) {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

pub fn hash_dictionary_entries<'a>(
    hasher: &mut Sha256,
    object: &CurrentValue<'a>,
    include: impl Fn(&(crate::source::CurrentKey<'a>, CurrentValue<'a>)) -> bool,
    mut hash_value: impl FnMut(&mut Sha256, &[u8], CurrentValue<'a>) -> Result<()>,
) -> Result<()> {
    let Some(entries) = object.dictionary_entries() else {
        return Err(crate::Error::Invalid(
            "expected dictionary projection".to_owned(),
        ));
    };
    hasher.update([0x07]);
    for (key, value) in entries.filter(include) {
        hash_dictionary_key(hasher, key.as_ref());
        hash_value(hasher, key.as_ref(), value)?;
    }
    hasher.update(COS_END);
    Ok(())
}

pub fn canonical_cow_redirect(
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

pub fn exact_dictionary_fingerprint(object: &CurrentValue<'_>, domain: &[u8]) -> Option<[u8; 32]> {
    if !object.is_dictionary() {
        return None;
    }
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hash_cos_value(&mut hasher, object, FingerprintReferences::Exact);
    Some(hasher.finalize().into())
}

/// Fingerprint a synthetic dictionary of borrowed values, such as an
/// immutable page identity whose effective properties span multiple nodes.
pub fn exact_projected_dictionary_fingerprint<'a, K, V>(
    entries: impl IntoIterator<Item = (K, V)>,
    domain: &[u8],
) -> [u8; 32]
where
    K: AsRef<[u8]>,
    V: Borrow<CurrentValue<'a>>,
{
    let mut hasher = Sha256::new();
    hash_len_prefixed(&mut hasher, domain);
    hasher.update([0x07]);
    for (key, value) in entries {
        hash_dictionary_key(&mut hasher, key.as_ref());
        hash_cos_value(&mut hasher, value.borrow(), FingerprintReferences::Exact);
    }
    hasher.update(COS_END);
    hasher.finalize().into()
}

pub fn exact_projected_dictionary_equal(
    left: &BTreeMap<Vec<u8>, CurrentValue<'_>>,
    right: &BTreeMap<Vec<u8>, CurrentValue<'_>>,
) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .all(|((left_key, left), (right_key, right))| {
                left_key == right_key && exact_cos_equal(left, right)
            })
}

pub fn optional_cos_equal(left: Option<CurrentValue<'_>>, right: Option<CurrentValue<'_>>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => exact_cos_equal(&left, &right),
        (None, None) => true,
        _ => false,
    }
}

pub fn dictionary_projection_equal(left: &CurrentValue<'_>, right: &CurrentValue<'_>) -> bool {
    let (Some(left), Some(right)) = (left.dictionary_entries(), right.dictionary_entries()) else {
        return false;
    };
    let mut frames = SmallVec::new();
    frames.push((
        CurrentChildren::Dictionary(left),
        CurrentChildren::Dictionary(right),
    ));
    compare_cos_frames(frames)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectId, OwnedObject, test_support::ClassicPdfBuilder};

    fn resolved_digest(
        document: &EditDocument,
        dictionary: &OwnedDictionary,
    ) -> Result<Option<[u8; 32]>> {
        let mut hasher = Sha256::new();
        Ok(
            hash_resolved_owned_dictionary(document, &mut hasher, dictionary)?
                .then(|| hasher.finalize().into()),
        )
    }

    #[test]
    fn resolving_fingerprints_preserve_reference_semantics_and_bounds() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [] /Count 0 >>")?;
        pdf.object(3, b"<< /Value [1 /RGB] >>")?;
        pdf.object(4, b"<< /Value [1 /RGB] >>")?;
        pdf.stream(5, b"", b"stream")?;
        pdf.object(6, b"<< /Cycle 6 0 R >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let handle = |id| CowObjectHandle::Existing(ObjectId::new(id, 0));
        let reference = |id| OwnedObject::Reference(handle(id));
        let dict = |value| OwnedDictionary::from([(b"Value".to_vec(), value)]);
        let a = dict(reference(3));
        let b = dict(reference(4));
        let original = resolved_digest(&document, &a)?;
        assert!(original.is_some());
        assert_eq!(original, resolved_digest(&document, &b)?);
        let direct = document
            .current_owned_object(handle(3))?
            .ok_or_else(|| crate::Error::Invalid("missing fixture object".into()))?;
        assert_ne!(original, resolved_digest(&document, &dict(direct))?);
        assert!(
            resolved_digest(
                &document,
                &dict(OwnedObject::array(vec![reference(3), reference(3)]))
            )?
            .is_some()
        );
        for id in [5, 6, 99] {
            assert!(resolved_digest(&document, &dict(reference(id)))?.is_none());
        }
        *document.edit_handle(handle(4))? = OwnedObject::Integer(42);
        assert_ne!(original, resolved_digest(&document, &b)?);
        let nested = |depth| {
            let mut value = OwnedObject::Null;
            for _ in 0..depth {
                value = OwnedObject::array(vec![value]);
            }
            dict(value)
        };
        assert!(resolved_digest(&document, &nested(MAX_RESOLVED_COS_DEPTH - 1))?.is_some());
        assert!(resolved_digest(&document, &nested(MAX_RESOLVED_COS_DEPTH))?.is_none());
        Ok(())
    }
}
