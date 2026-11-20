use crate::stream_codec::decode_stream;
use crate::{Error, Result, SourceLoadError};
use hayro_syntax::{
    Pdf, PdfVersion,
    object::{Dict, MaybeRef, Name, Object, ObjectIdentifier, Stream, dict::DictEntries},
};
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, btree_map::Entry},
    sync::Arc,
};

/// Immutable evidence about a stream subtype, without resolving references.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceStreamSubtype {
    Name(Vec<u8>),
    Reference(ObjectHandle),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SourceStreamKind {
    NotStream,
    Stream(Option<SourceStreamSubtype>),
}

pub const PAGE_INHERITABLE_KEYS: [&[u8]; 4] = [b"Resources", b"MediaBox", b"CropBox", b"Rotate"];

/// Immutable, lazily parsed source PDF backed by Hayro.
///
/// Hayro keeps the original bytes alive and parses objects on demand. Mutations
/// belong in the copy-on-write object overlay rather than in this source representation.
pub struct SourcePdf {
    pdf: Pdf,
    bytes: Arc<Vec<u8>>,
    direct_reference_cache: RefCell<HashMap<ObjectId, Vec<ObjectId>>>,
    stream_kind_cache: RefCell<HashMap<ObjectId, SourceStreamKind>>,
}

impl SourcePdf {
    /// Parse owned PDF bytes without making another full-document copy.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes do not form a supported PDF.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_shared(Arc::new(bytes))
    }

    /// Parse PDF bytes already held in shared storage.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes do not form a supported PDF.
    pub fn from_shared(bytes: Arc<Vec<u8>>) -> Result<Self> {
        let pdf = Pdf::new(Arc::clone(&bytes)).map_err(SourceLoadError::from)?;
        Ok(Self {
            pdf,
            bytes,
            direct_reference_cache: RefCell::new(HashMap::new()),
            stream_kind_cache: RefCell::new(HashMap::new()),
        })
    }

    /// Original source bytes, unchanged.
    pub fn bytes(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    /// Number of objects indexed by the source cross-reference graph.
    pub fn object_count(&self) -> usize {
        self.pdf.len()
    }

    /// Object identifiers indexed by the source cross-reference graph.
    pub(crate) fn object_ids(&self) -> Vec<ObjectId> {
        self.pdf
            .xref()
            .object_ids()
            .into_iter()
            .map(Into::into)
            .collect()
    }

    /// Number of pages in the source page tree.
    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
    }

    /// Object identifiers of pages resolved from the source page tree.
    pub fn page_ids(&self) -> Vec<ObjectId> {
        self.pdf
            .pages()
            .iter()
            .filter_map(|page| page.raw().obj_id().map(Into::into))
            .collect()
    }

    /// Effective PDF version reported by Hayro.
    pub fn version(&self) -> PdfVersion {
        self.pdf.version()
    }

    /// Object identifier of the document catalog.
    pub fn catalog_id(&self) -> ObjectId {
        self.pdf.xref().root_id().into()
    }

    /// Copy one source COS object into the mutation representation.
    ///
    /// Indirect references stay as references and source stream payloads stay
    /// source-backed, so materializing a dictionary does not recursively clone
    /// the object graph or duplicate large encoded streams.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested object is not present in the source graph.
    pub fn materialize(&self, id: ObjectId) -> Result<OwnedObject> {
        Ok(owned_from_hayro(self.object(id)?, Some(id)))
    }

    pub(crate) fn object(&self, id: ObjectId) -> Result<Object<'_>> {
        self.pdf
            .xref()
            .get::<Object<'_>>(id.into())
            .ok_or(Error::MissingSourceObject {
                number: id.number,
                generation: id.generation,
            })
    }

    fn source_stream_kind_from_object(object: &Object<'_>) -> SourceStreamKind {
        match object {
            Object::Stream(stream) => {
                SourceStreamKind::Stream(stream.dict().get_raw::<Object<'_>>(b"Subtype").and_then(
                    |value| match value {
                        MaybeRef::Ref(reference) => Some(SourceStreamSubtype::Reference(
                            ObjectHandle::Existing(reference.into()),
                        )),
                        MaybeRef::NotRef(Object::Name(name)) => {
                            Some(SourceStreamSubtype::Name(name.as_ref().to_vec()))
                        }
                        MaybeRef::NotRef(_) => None,
                    },
                ))
            }
            _ => SourceStreamKind::NotStream,
        }
    }

    fn cache_source_stream_kind(&self, id: ObjectId, object: &Object<'_>) {
        if self.stream_kind_cache.borrow().contains_key(&id) {
            return;
        }
        self.stream_kind_cache
            .borrow_mut()
            .insert(id, Self::source_stream_kind_from_object(object));
    }

    fn source_stream_kind(&self, id: ObjectId) -> Result<SourceStreamKind> {
        if let Some(kind) = self.stream_kind_cache.borrow().get(&id) {
            return Ok(kind.clone());
        }
        let object = self.object(id)?;
        let kind = Self::source_stream_kind_from_object(&object);
        self.stream_kind_cache.borrow_mut().insert(id, kind.clone());
        Ok(kind)
    }

    pub(crate) fn source_object_is_stream(&self, id: ObjectId) -> Result<bool> {
        if let Some(kind) = self.stream_kind_cache.borrow().get(&id) {
            return Ok(matches!(kind, SourceStreamKind::Stream(_)));
        }
        let object = self.object(id)?;
        let kind = Self::source_stream_kind_from_object(&object);
        let is_stream = matches!(kind, SourceStreamKind::Stream(_));
        self.stream_kind_cache.borrow_mut().insert(id, kind);
        Ok(is_stream)
    }

    pub(crate) fn object_with_references(
        &self,
        id: ObjectId,
    ) -> Result<(Object<'_>, Vec<ObjectId>)> {
        let object = self.object(id)?;
        self.cache_source_stream_kind(id, &object);
        let cached_references = self.direct_reference_cache.borrow().get(&id).cloned();
        let references = cached_references.unwrap_or_else(|| {
            let mut references = BTreeSet::new();
            collect_hayro_references(&object, &mut references);
            let references = references.into_iter().collect::<Vec<_>>();
            self.direct_reference_cache
                .borrow_mut()
                .insert(id, references.clone());
            references
        });
        Ok((object, references))
    }

    pub(crate) fn contains_object(&self, id: ObjectId) -> bool {
        self.pdf.xref().get::<Object<'_>>(id.into()).is_some()
    }

    /// Collect indirect references reachable directly from one source object.
    ///
    /// This walks only the borrowed COS structure of that object. Referenced
    /// objects are not resolved or materialized, and stream payload bytes are
    /// never touched.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested object is not present in the source graph.
    pub fn references(&self, id: ObjectId) -> Result<Vec<ObjectId>> {
        if let Some(references) = self.direct_reference_cache.borrow().get(&id) {
            return Ok(references.clone());
        }

        let object = self.object(id)?;
        self.cache_source_stream_kind(id, &object);
        let mut references = BTreeSet::new();
        collect_hayro_references(&object, &mut references);
        let references = references.into_iter().collect::<Vec<_>>();
        self.direct_reference_cache
            .borrow_mut()
            .insert(id, references.clone());
        Ok(references)
    }

    /// Return the encoded/decrypted bytes of a source stream on demand.
    ///
    /// For ordinary unencrypted files this remains a borrowed view into the
    /// source PDF. Hayro may allocate when decryption is required.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested object does not identify a source stream.
    pub fn stream_data(&self, id: ObjectId) -> Result<Cow<'_, [u8]>> {
        let stream =
            self.pdf
                .xref()
                .get::<Stream<'_>>(id.into())
                .ok_or(Error::ExpectedSourceStream {
                    number: id.number,
                    generation: id.generation,
                })?;
        Ok(stream.raw_data())
    }

    /// Return fully decoded bytes of a source stream on demand.
    ///
    /// # Errors
    ///
    /// Returns an error when the object is not a source stream or its filters cannot be decoded.
    pub fn decoded_stream_data(&self, id: ObjectId) -> Result<Cow<'_, [u8]>> {
        let stream =
            self.pdf
                .xref()
                .get::<Stream<'_>>(id.into())
                .ok_or(Error::ExpectedSourceStream {
                    number: id.number,
                    generation: id.generation,
                })?;
        stream
            .decoded()
            .map_err(|error| Error::Invalid(format!("failed to decode source stream: {error:?}")))
    }

    /// Return trailer entries that belong to document semantics rather than
    /// the input xref/encryption machinery.
    ///
    /// The vendored Hayro accessor exposes the final already-parsed trailer
    /// dictionary, so document construction no longer reparses the PDF through
    /// a second parser merely to preserve `/Info`, `/ID`, or custom roots.
    pub(crate) fn preserved_trailer(&self) -> OwnedDictionary {
        let Some(trailer) = self.pdf.xref().trailer() else {
            return OwnedDictionary::new();
        };
        let xref_stream = trailer.entries().any(|(name, value)| {
            name.as_ref() == b"Type"
                && matches!(value, MaybeRef::NotRef(Object::Name(value)) if value.as_ref() == b"XRef")
        });

        let mut preserved = OwnedDictionary::new();
        for (name, value) in trailer.entries() {
            let name = name.as_ref();
            if is_writer_owned_trailer_key(name, xref_stream) {
                continue;
            }
            preserved.insert(name.to_vec(), owned_from_maybe_ref(value));
        }
        preserved
    }
}

const fn is_writer_owned_trailer_key(name: &[u8], xref_stream: bool) -> bool {
    matches!(name, b"Size" | b"Root" | b"Encrypt" | b"Prev" | b"XRefStm")
        || xref_stream
            && matches!(
                name,
                b"Type"
                    | b"W"
                    | b"Index"
                    | b"Length"
                    | b"Filter"
                    | b"DecodeParms"
                    | b"DL"
                    | b"F"
                    | b"FFilter"
                    | b"FDecodeParms"
            )
}

/// Stable identifier of an existing indirect PDF object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    number: u32,
    generation: i32,
}

impl ObjectId {
    #[must_use]
    /// Creates an identifier from a PDF object number and generation number.
    pub const fn new(number: u32, generation: i32) -> Self {
        Self { number, generation }
    }

    #[must_use]
    /// Returns the PDF indirect-object number.
    pub const fn number(self) -> u32 {
        self.number
    }

    #[must_use]
    /// Returns the PDF indirect-object generation number.
    pub const fn generation(self) -> i32 {
        self.generation
    }
}

impl From<hayro_syntax::object::ObjRef> for ObjectId {
    fn from(value: hayro_syntax::object::ObjRef) -> Self {
        Self::new(value.obj_number, value.gen_number)
    }
}

impl From<ObjectIdentifier> for ObjectId {
    fn from(value: ObjectIdentifier) -> Self {
        Self::new(value.obj_number, value.gen_number)
    }
}

impl From<ObjectId> for ObjectIdentifier {
    fn from(value: ObjectId) -> Self {
        Self::new(value.number, value.generation)
    }
}

/// Identifier of an object created in the overlay and not assigned an output
/// object number yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NewObjectId(usize);

impl NewObjectId {
    #[must_use]
    /// Returns the zero-based overlay-object index.
    pub const fn index(self) -> usize {
        self.0
    }
}

/// A reference that can point either at a source object or at a newly created
/// overlay object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectHandle {
    /// Reference an indirect object that already exists in the source PDF.
    Existing(ObjectId),
    /// Reference an object newly created in the copy-on-write overlay.
    New(NewObjectId),
}

/// Owned PDF dictionary keyed by decoded name bytes without the leading slash.
pub type OwnedDictionary = BTreeMap<Vec<u8>, OwnedObject>;

/// Stream payload used by an edited COS object.
///
/// Source-backed payloads defer touching the bytes until the writer actually
/// needs them. A transform that changes the encoded payload explicitly switches
/// to [`Self::Owned`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamData {
    /// Keep the encoded stream payload backed by the immutable source object.
    Source(ObjectId),
    /// Store an encoded stream payload owned by the overlay.
    Owned(Vec<u8>),
}

impl StreamData {
    /// Return the payload bytes, resolving source-backed data lazily.
    ///
    /// # Errors
    ///
    /// Returns an error when a source-backed payload no longer identifies a source stream.
    pub fn bytes<'a>(&'a self, source: &'a SourcePdf) -> Result<Cow<'a, [u8]>> {
        match self {
            Self::Source(id) => source.stream_data(*id),
            Self::Owned(bytes) => Ok(Cow::Borrowed(bytes)),
        }
    }
}

/// Owned COS value used by the mutation overlay.
///
/// This is intentionally independent from Hayro's borrowed object types so an
/// optimization pass only pays allocation cost for objects it actually edits.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedObject {
    /// PDF null object.
    Null,
    /// PDF boolean object.
    Boolean(bool),
    /// PDF integer object.
    Integer(i64),
    /// PDF real-number object.
    Real(f64),
    /// PDF name bytes without the leading slash.
    Name(Vec<u8>),
    /// PDF string bytes.
    String(Vec<u8>),
    /// Indirect reference to a source or newly created object.
    Reference(ObjectHandle),
    /// PDF array containing owned COS values.
    Array(Vec<Self>),
    /// PDF dictionary keyed by decoded name bytes.
    Dictionary(OwnedDictionary),
    /// PDF stream dictionary together with its encoded payload.
    Stream {
        /// Stream dictionary entries.
        dictionary: OwnedDictionary,
        /// Encoded stream payload.
        data: StreamData,
    },
}

impl OwnedObject {
    #[must_use]
    /// Returns the dictionary view of a dictionary or stream object.
    pub const fn as_dictionary(&self) -> Option<&OwnedDictionary> {
        match self {
            Self::Dictionary(dictionary) | Self::Stream { dictionary, .. } => Some(dictionary),
            _ => None,
        }
    }

    /// Returns the mutable dictionary view of a dictionary or stream object.
    pub const fn as_dictionary_mut(&mut self) -> Option<&mut OwnedDictionary> {
        match self {
            Self::Dictionary(dictionary) | Self::Stream { dictionary, .. } => Some(dictionary),
            _ => None,
        }
    }

    /// Collect indirect references contained in this owned COS value.
    #[must_use]
    pub fn references(&self) -> Vec<ObjectHandle> {
        let mut references = BTreeSet::new();
        collect_owned_references(self, &mut references);
        references.into_iter().collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewObjectCheckpoint(usize);

/// Mutation applied to an object that already exists in the source PDF.
#[derive(Debug, Clone, PartialEq)]
pub enum ExistingObjectChange {
    /// Replace an existing source object with an owned object.
    Replace(OwnedObject),
    /// Delete an existing source object from the rewritten graph.
    Delete,
}

/// Copy-on-write object graph layered over an immutable [`SourcePdf`].
#[derive(Debug, Clone, Default)]
pub struct ObjectOverlay {
    existing: BTreeMap<ObjectId, ExistingObjectChange>,
    added: Vec<OwnedObject>,
}

impl ObjectOverlay {
    /// Records a replacement for an existing source object.
    pub fn replace(&mut self, id: ObjectId, object: OwnedObject) {
        self.existing
            .insert(id, ExistingObjectChange::Replace(object));
    }

    /// Marks an existing source object for deletion.
    pub fn delete(&mut self, id: ObjectId) {
        self.existing.insert(id, ExistingObjectChange::Delete);
    }

    /// Materialize an existing source object only when it is first edited.
    ///
    /// # Errors
    ///
    /// Returns an error when the source object is missing or was deleted from the overlay.
    pub fn edit<'a>(&'a mut self, source: &SourcePdf, id: ObjectId) -> Result<&'a mut OwnedObject> {
        let change = match self.existing.entry(id) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let object = source.materialize(id)?;
                entry.insert(ExistingObjectChange::Replace(object))
            }
        };

        match change {
            ExistingObjectChange::Replace(object) => Ok(object),
            ExistingObjectChange::Delete => Err(Error::DeletedSourceObject {
                number: id.number,
                generation: id.generation,
            }),
        }
    }

    #[must_use]
    /// Returns the pending change for an existing source object, if any.
    pub fn change(&self, id: ObjectId) -> Option<&ExistingObjectChange> {
        self.existing.get(&id)
    }

    /// Iterates over pending changes to existing source objects.
    pub fn changes(&self) -> impl Iterator<Item = (ObjectId, &ExistingObjectChange)> {
        self.existing.iter().map(|(id, change)| (*id, change))
    }

    /// Adds an owned object to the overlay and returns its temporary identifier.
    pub fn add(&mut self, object: OwnedObject) -> NewObjectId {
        let id = NewObjectId(self.added.len());
        self.added.push(object);
        id
    }

    #[must_use]
    /// Returns a newly added overlay object by temporary identifier.
    pub fn added(&self, id: NewObjectId) -> Option<&OwnedObject> {
        self.added.get(id.index())
    }

    /// Returns a mutable newly added overlay object by temporary identifier.
    pub fn added_mut(&mut self, id: NewObjectId) -> Option<&mut OwnedObject> {
        self.added.get_mut(id.index())
    }

    #[must_use]
    /// Returns whether the overlay contains no replacements, deletions, or newly added objects.
    pub fn is_empty(&self) -> bool {
        self.existing.is_empty() && self.added.is_empty()
    }
}

/// A direct value or unresolved reference in either current COS representation.
/// Source children remain lazy until explicitly materialized; owned children
/// borrow the overlay. Reference resolution must always use the current graph.
pub enum CurrentValue<'a> {
    Source(MaybeRef<Object<'a>>, Option<ObjectId>),
    Owned(&'a OwnedObject),
}

impl<'a> CurrentValue<'a> {
    pub fn reference(&self) -> Option<ObjectHandle> {
        match self {
            Self::Source(MaybeRef::Ref(reference), _) => {
                Some(ObjectHandle::Existing((*reference).into()))
            }
            Self::Owned(OwnedObject::Reference(reference)) => Some(*reference),
            _ => None,
        }
    }

    pub fn dictionary_entry(&self, key: &[u8]) -> Option<Self> {
        match self {
            Self::Source(MaybeRef::NotRef(Object::Dict(dictionary)), _) => dictionary
                .get_raw(key)
                .map(|value| Self::Source(value, None)),
            Self::Source(MaybeRef::NotRef(Object::Stream(stream)), _) if key != b"Length" => stream
                .dict()
                .get_raw(key)
                .map(|value| Self::Source(value, None)),
            Self::Owned(value) => value.as_dictionary()?.get(key).map(Self::Owned),
            Self::Source(..) => None,
        }
    }

    pub fn dictionary_keys(&self) -> Option<Vec<Vec<u8>>> {
        Some(
            self.dictionary_entries()?
                .map(|(key, _)| key.as_ref().to_vec())
                .collect(),
        )
    }

    /// Iterate the editable dictionary view, omitting writer-owned source Length.
    pub fn dictionary_entries(&self) -> Option<CurrentDictionaryIter<'a>> {
        self.raw_dictionary_entries(matches!(self, Self::Source(..)))
    }

    fn raw_dictionary_entries(
        &self,
        skip_stream_length: bool,
    ) -> Option<CurrentDictionaryIter<'a>> {
        match self {
            Self::Source(MaybeRef::NotRef(object), _) => {
                source_dictionary_entries(object, skip_stream_length)
            }
            Self::Owned(value) => Some(CurrentDictionaryIter {
                entries: DictionaryEntries::Owned(value.as_dictionary()?.iter()),
                skip_length: skip_stream_length && self.is_stream(),
            }),
            Self::Source(MaybeRef::Ref(_), _) => None,
        }
    }

    pub fn children(&self, skip_stream_length: bool) -> Option<CurrentChildren<'a>> {
        if let Some(entries) = self.raw_dictionary_entries(skip_stream_length) {
            return Some(CurrentChildren::Dictionary(entries));
        }
        self.array_items()
            .map(|items| CurrentChildren::Array(items.enumerate()))
    }

    pub const fn is_stream(&self) -> bool {
        matches!(
            self,
            Self::Source(MaybeRef::NotRef(Object::Stream(_)), _)
                | Self::Owned(OwnedObject::Stream { .. })
        )
    }

    pub fn resolved_entry(&self, document: &'a EditDocument, key: &[u8]) -> Result<Option<Self>> {
        let Some(value) = self.dictionary_entry(key) else {
            return Ok(None);
        };
        document.resolve_current_value(value)
    }

    pub fn resolved_name_is(
        &self,
        document: &'a EditDocument,
        key: &[u8],
        expected: &[u8],
    ) -> Result<bool> {
        Ok(self
            .resolved_entry(document, key)?
            .is_some_and(|value| value.is_name(expected)))
    }

    pub const fn has_dictionary(&self) -> bool {
        matches!(
            self,
            Self::Source(MaybeRef::NotRef(Object::Dict(_) | Object::Stream(_)), _)
                | Self::Owned(OwnedObject::Dictionary(_) | OwnedObject::Stream { .. })
        )
    }

    pub const fn is_dictionary(&self) -> bool {
        matches!(
            self,
            Self::Source(MaybeRef::NotRef(Object::Dict(_)), _)
                | Self::Owned(OwnedObject::Dictionary(_))
        )
    }

    pub const fn is_null(&self) -> bool {
        matches!(
            self,
            Self::Source(MaybeRef::NotRef(Object::Null(_)), _) | Self::Owned(OwnedObject::Null)
        )
    }

    pub fn name(&self) -> Option<&[u8]> {
        match self {
            Self::Source(MaybeRef::NotRef(Object::Name(name)), _) => Some(name.as_ref()),
            Self::Owned(OwnedObject::Name(name)) => Some(name),
            _ => None,
        }
    }

    pub fn is_name(&self, expected: &[u8]) -> bool {
        self.name() == Some(expected)
    }

    pub fn array_items(&self) -> Option<CurrentArrayIter<'a>> {
        match self {
            Self::Source(MaybeRef::NotRef(Object::Array(array)), _) => {
                Some(CurrentArrayIter::Source(array.raw_iter()))
            }
            Self::Owned(OwnedObject::Array(array)) => Some(CurrentArrayIter::Owned(array.iter())),
            _ => None,
        }
    }

    pub fn into_cow(self) -> Cow<'a, OwnedObject> {
        match self {
            Self::Source(MaybeRef::NotRef(value), id) => Cow::Owned(owned_from_hayro(value, id)),
            Self::Source(value, _) => Cow::Owned(owned_from_maybe_ref(value)),
            Self::Owned(value) => Cow::Borrowed(value),
        }
    }
}

pub enum CurrentArrayIter<'a> {
    Source(hayro_syntax::object::array::ArrayIter<'a>),
    Owned(std::slice::Iter<'a, OwnedObject>),
}

impl<'a> Iterator for CurrentArrayIter<'a> {
    type Item = CurrentValue<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Source(iter) => iter.next().map(|value| CurrentValue::Source(value, None)),
            Self::Owned(iter) => iter.next().map(CurrentValue::Owned),
        }
    }
}

pub enum CurrentKey<'a> {
    Source(Name<'a>),
    Owned(&'a [u8]),
}

impl AsRef<[u8]> for CurrentKey<'_> {
    fn as_ref(&self) -> &[u8] {
        match self {
            Self::Source(name) => name.as_ref(),
            Self::Owned(name) => name,
        }
    }
}

enum DictionaryEntries<'a> {
    Source(DictEntries<'a>),
    Owned(std::collections::btree_map::Iter<'a, Vec<u8>, OwnedObject>),
}

pub struct CurrentDictionaryIter<'a> {
    entries: DictionaryEntries<'a>,
    skip_length: bool,
}

impl<'a> Iterator for CurrentDictionaryIter<'a> {
    type Item = (CurrentKey<'a>, CurrentValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let (key, value) = match &mut self.entries {
                DictionaryEntries::Source(entries) => {
                    let (key, value) = entries.next()?;
                    (CurrentKey::Source(key), CurrentValue::Source(value, None))
                }
                DictionaryEntries::Owned(entries) => {
                    let (key, value) = entries.next()?;
                    (CurrentKey::Owned(key), CurrentValue::Owned(value))
                }
            };
            if !self.skip_length || key.as_ref() != b"Length" {
                return Some((key, value));
            }
        }
    }
}

fn source_dictionary_entries<'a>(
    object: &Object<'a>,
    skip_stream_length: bool,
) -> Option<CurrentDictionaryIter<'a>> {
    let (dictionary, skip_length) = match object {
        Object::Dict(dictionary) => (dictionary, false),
        Object::Stream(stream) => (stream.dict(), skip_stream_length),
        _ => return None,
    };
    Some(CurrentDictionaryIter {
        entries: DictionaryEntries::Source(dictionary.entries()),
        skip_length,
    })
}

pub enum CurrentChildKey<'a> {
    Dictionary(CurrentKey<'a>),
    Array(usize),
}

/// An ordered, lazy iterator over a single direct COS container.
pub enum CurrentChildren<'a> {
    Dictionary(CurrentDictionaryIter<'a>),
    Array(std::iter::Enumerate<CurrentArrayIter<'a>>),
}

impl<'a> Iterator for CurrentChildren<'a> {
    type Item = (CurrentChildKey<'a>, CurrentValue<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Dictionary(entries) => entries
                .next()
                .map(|(key, value)| (CurrentChildKey::Dictionary(key), value)),
            Self::Array(entries) => entries
                .next()
                .map(|(index, value)| (CurrentChildKey::Array(index), value)),
        }
    }
}

fn source_children<'a>(
    object: &Object<'a>,
    skip_stream_length: bool,
) -> Option<CurrentChildren<'a>> {
    if let Some(entries) = source_dictionary_entries(object, skip_stream_length) {
        return Some(CurrentChildren::Dictionary(entries));
    }
    match object {
        Object::Array(array) => Some(CurrentChildren::Array(
            CurrentArrayIter::Source(array.raw_iter()).enumerate(),
        )),
        _ => None,
    }
}

/// Walk direct descendants with one iterator per nesting level. No paths,
/// payload materialization, reference resolution or broad sibling stack.
fn current_descendants<'a>(
    children: Option<CurrentChildren<'a>>,
    skip_stream_length: bool,
) -> impl Iterator<Item = CurrentValue<'a>> {
    let mut frames = SmallVec::<[CurrentChildren<'a>; 4]>::new();
    if let Some(children) = children {
        frames.push(children);
    }
    std::iter::from_fn(move || {
        loop {
            if let Some((_, value)) = frames.last_mut()?.next() {
                if let Some(children) = value.children(skip_stream_length) {
                    frames.push(children);
                }
                return Some(value);
            }
            frames.pop();
        }
    })
}

fn contains_indirect_reference(value: &OwnedObject) -> bool {
    let root = CurrentValue::Owned(value);
    root.reference().is_some()
        || current_descendants(root.children(false), false).any(|value| value.reference().is_some())
}

fn stream_filter_configuration_is_direct(stream: &OwnedObject) -> bool {
    let OwnedObject::Stream { dictionary, .. } = stream else {
        return false;
    };
    [
        b"Filter".as_slice(),
        b"DecodeParms".as_slice(),
        b"F".as_slice(),
        b"FFilter".as_slice(),
        b"FDecodeParms".as_slice(),
    ]
    .into_iter()
    .filter_map(|key| dictionary.get(key))
    .all(|value| !contains_indirect_reference(value))
}

const MAX_DECODED_CONTENT_CACHE_BYTES: usize = 256 * 1024 * 1024;
const MAX_DECODED_CONTENT_STREAM_BYTES: usize = 64 * 1024 * 1024;

#[derive(Default)]
struct DecodedContentCache {
    bytes: usize,
    streams: BTreeMap<ObjectId, CachedContentStream>,
    // Indexed by last access so capacity pressure evicts in O(log n), rather
    // than scanning all cached streams to find an eviction candidate.
    oldest_first: BTreeSet<(u64, ObjectId)>,
    access_clock: u64,
}

struct CachedContentStream {
    bytes: Arc<[u8]>,
    last_access: u64,
}

impl DecodedContentCache {
    fn next_access(&mut self) -> u64 {
        // Keep the LRU ordering correct even if the counter eventually wraps.
        // This path is normally unreachable, and costs O(n log n) only once
        // per 2^64 cache accesses.
        if self.access_clock == u64::MAX {
            let prior_order = std::mem::take(&mut self.oldest_first);
            self.access_clock = 0;
            for (_, id) in prior_order {
                self.access_clock += 1;
                if let Some(stream) = self.streams.get_mut(&id) {
                    stream.last_access = self.access_clock;
                    self.oldest_first.insert((self.access_clock, id));
                }
            }
        }
        self.access_clock += 1;
        self.access_clock
    }

    fn get(&mut self, id: ObjectId) -> Option<Arc<[u8]>> {
        if !self.streams.contains_key(&id) {
            return None;
        }
        let age = self.next_access();
        let stream = self.streams.get_mut(&id)?;
        self.oldest_first.remove(&(stream.last_access, id));
        stream.last_access = age;
        self.oldest_first.insert((age, id));
        Some(Arc::clone(&stream.bytes))
    }

    fn evict(&mut self, id: ObjectId) {
        if let Some(stream) = self.streams.remove(&id) {
            self.oldest_first.remove(&(stream.last_access, id));
            self.bytes -= stream.bytes.len();
        }
    }

    fn insert(&mut self, id: ObjectId, decoded: &[u8], capacity: usize) {
        // Replacement must invalidate an old entry even when the new data is
        // too large to retain; otherwise the previous bytes could look fresh.
        self.evict(id);
        if decoded.len() > capacity {
            return;
        }
        while capacity.saturating_sub(self.bytes) < decoded.len() {
            let Some(&(_, oldest_id)) = self.oldest_first.first() else {
                return;
            };
            self.evict(oldest_id);
        }
        let age = self.next_access();
        self.streams.insert(
            id,
            CachedContentStream {
                bytes: Arc::from(decoded),
                last_access: age,
            },
        );
        self.oldest_first.insert((age, id));
        self.bytes += decoded.len();
    }
}

/// A content reference is a fresh stream, a shared cached stream, or a COS value.
/// Cached bytes are reference-counted so consumers can append them directly
/// without copying or holding an active borrow of the document's cache.
pub enum ContentRead<'a> {
    Decoded(Vec<u8>),
    Cached(Arc<[u8]>),
    Other(Cow<'a, OwnedObject>),
}

struct ReachableOutputCache {
    generation: u64,
    traversal: Vec<ObjectHandle>,
    sorted: Vec<ObjectHandle>,
    streams: Option<Vec<ObjectHandle>>,
    streams_by_subtype: HashMap<Vec<u8>, Vec<ObjectHandle>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReachabilityCacheStats {
    pub queries: u64,
    pub rebuilds: u64,
    pub cache_hits: u64,
    pub edge_checks: u64,
    pub edge_stable_reuses: u64,
}

/// A lazily parsed source document plus the objects changed by optimization
/// passes. This is the target architecture for the Hayro migration.
pub struct EditDocument {
    source: SourcePdf,
    trailer: OwnedDictionary,
    overlay: ObjectOverlay,
    decoded_content_stream_cache: RefCell<DecodedContentCache>,
    mutation_generation: Cell<u64>,
    reachable_output_cache: RefCell<Option<ReachableOutputCache>>,
    pending_reference_baselines: RefCell<HashMap<ObjectHandle, Vec<ObjectHandle>>>,
    reachability_cache_stats: Cell<ReachabilityCacheStats>,
}

impl EditDocument {
    /// Parse a source PDF and initialize an empty copy-on-write overlay.
    ///
    /// # Errors
    ///
    /// Returns an error when the source bytes do not form a supported PDF.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let source = SourcePdf::from_bytes(bytes)?;
        let trailer = source.preserved_trailer();
        Ok(Self {
            source,
            trailer,
            overlay: ObjectOverlay::default(),
            decoded_content_stream_cache: RefCell::new(DecodedContentCache::default()),
            mutation_generation: Cell::new(0),
            reachable_output_cache: RefCell::new(None),
            pending_reference_baselines: RefCell::new(HashMap::new()),
            reachability_cache_stats: Cell::new(ReachabilityCacheStats::default()),
        })
    }

    /// Returns the immutable parsed source document.
    pub const fn source(&self) -> &SourcePdf {
        &self.source
    }

    /// Returns the preserved semantic trailer entries.
    pub const fn trailer(&self) -> &OwnedDictionary {
        &self.trailer
    }

    fn invalidate_reachable_metadata_cache(&self) {
        if let Some(cache) = self.reachable_output_cache.borrow_mut().as_mut() {
            cache.streams = None;
            cache.streams_by_subtype.clear();
        }
    }

    fn mark_graph_maybe_mutated(&self) {
        self.pending_reference_baselines.borrow_mut().clear();
        self.invalidate_reachable_metadata_cache();
        self.mutation_generation
            .set(self.mutation_generation.get().wrapping_add(1));
    }

    fn mark_object_maybe_mutated(&self, handle: ObjectHandle) -> Result<()> {
        self.invalidate_reachable_metadata_cache();
        if self
            .pending_reference_baselines
            .borrow()
            .contains_key(&handle)
        {
            return Ok(());
        }

        let generation = self.mutation_generation.get();
        let reachable = self
            .reachable_output_cache
            .borrow()
            .as_ref()
            .is_some_and(|cache| {
                cache.generation == generation && cache.sorted.binary_search(&handle).is_ok()
            });
        if !reachable {
            return Ok(());
        }

        let baseline = self.references_for_handle(handle)?;
        self.pending_reference_baselines
            .borrow_mut()
            .insert(handle, baseline);
        Ok(())
    }

    fn validate_pending_reference_changes(&self) -> Result<()> {
        let pending = std::mem::take(&mut *self.pending_reference_baselines.borrow_mut());
        if pending.is_empty() {
            return Ok(());
        }

        let mut stats = self.reachability_cache_stats.get();
        let mut graph_changed = false;
        for (handle, baseline) in pending {
            stats.edge_checks = stats.edge_checks.saturating_add(1);
            match self.references_for_handle(handle) {
                Ok(current) if current == baseline => {}
                Ok(_) | Err(Error::DeletedReferencedObject { .. }) => {
                    graph_changed = true;
                    break;
                }
                Err(error) => return Err(error),
            }
        }

        if graph_changed {
            self.mutation_generation
                .set(self.mutation_generation.get().wrapping_add(1));
        } else {
            stats.edge_stable_reuses = stats.edge_stable_reuses.saturating_add(1);
        }
        self.reachability_cache_stats.set(stats);
        Ok(())
    }

    pub(super) const fn reachability_cache_stats(&self) -> ReachabilityCacheStats {
        self.reachability_cache_stats.get()
    }

    /// Returns the preserved trailer entries for mutation and invalidates graph caches as required.
    pub fn trailer_mut(&mut self) -> &mut OwnedDictionary {
        self.mark_graph_maybe_mutated();
        &mut self.trailer
    }

    /// Returns the copy-on-write object overlay.
    pub const fn overlay(&self) -> &ObjectOverlay {
        &self.overlay
    }

    // Escape hatch for bulk/opaque overlay mutation. Callers that only need
    // to allocate or edit one known object should prefer add_object,
    // edit_object, or edit_added_object so reachability can reuse a cached
    // graph when indirect-reference edges stay unchanged.
    /// Returns the copy-on-write object overlay for mutation and invalidates graph caches as required.
    pub fn overlay_mut(&mut self) -> &mut ObjectOverlay {
        self.mark_graph_maybe_mutated();
        // Opaque callers can edit or delete any source stream. Discard all
        // cached entries so unreachable decoded bytes cannot exhaust the budget.
        *self.decoded_content_stream_cache.get_mut() = DecodedContentCache::default();
        &mut self.overlay
    }

    // Allocation alone cannot change output reachability; the reachable object
    // that eventually points at this handle will trigger edge validation.
    /// Adds a new object to the document overlay and returns its temporary identifier.
    pub fn add_object(&mut self, object: OwnedObject) -> NewObjectId {
        self.overlay.add(object)
    }

    /// Snapshot the tail of the unnumbered-object arena before speculative allocation.
    ///
    /// The checkpoint is only for transactions that have not linked any subsequently
    /// allocated handles into the reachable object graph. Handles allocated after the
    /// checkpoint must be discarded together with a rollback because their indices may
    /// be reused by later allocations.
    pub(crate) const fn new_object_checkpoint(&self) -> NewObjectCheckpoint {
        NewObjectCheckpoint(self.overlay.added.len())
    }

    /// Discard speculative tail allocations made after `new_object_checkpoint`.
    ///
    /// # Errors
    ///
    /// Returns an error when the checkpoint does not belong to the current overlay tail.
    pub(crate) fn rollback_new_objects(&mut self, checkpoint: NewObjectCheckpoint) -> Result<()> {
        if checkpoint.0 > self.overlay.added.len() {
            return Err(Error::Invalid(
                "new-object rollback checkpoint exceeds current overlay length".to_owned(),
            ));
        }
        self.overlay.added.truncate(checkpoint.0);
        Ok(())
    }

    /// Return a mutable overlay object previously allocated with `add_object`.
    ///
    /// # Errors
    ///
    /// Returns an error if reachability bookkeeping fails or the new-object ID is not allocated.
    pub fn edit_added_object(&mut self, id: NewObjectId) -> Result<&mut OwnedObject> {
        self.mark_object_maybe_mutated(ObjectHandle::New(id))?;
        self.overlay
            .added_mut(id)
            .ok_or_else(|| Error::MissingNewObject { index: id.index() })
    }

    /// Materialize and return a mutable source object in the overlay.
    ///
    /// # Errors
    ///
    /// Returns an error if reachability bookkeeping fails, the source object is missing,
    /// or the object has been deleted from the overlay.
    pub fn edit_object(&mut self, id: ObjectId) -> Result<&mut OwnedObject> {
        self.mark_object_maybe_mutated(ObjectHandle::Existing(id))?;
        let object = self.overlay.edit(&self.source, id)?;
        // A replaced source stream cannot use its old cache entry. Release
        // its budget while leaving unrelated immutable source streams cached.
        self.decoded_content_stream_cache.get_mut().evict(id);
        Ok(object)
    }

    /// Materialize and return a mutable object regardless of whether it came
    /// from the source PDF or was allocated in the copy-on-write overlay.
    ///
    /// # Errors
    ///
    /// Returns an error if reachability bookkeeping fails or the referenced
    /// source/new object is missing or deleted.
    pub fn edit_handle(&mut self, handle: ObjectHandle) -> Result<&mut OwnedObject> {
        match handle {
            ObjectHandle::Existing(id) => self.edit_object(id),
            ObjectHandle::New(id) => self.edit_added_object(id),
        }
    }

    /// Materialize the current value of an object handle from either the source
    /// graph or the COW overlay without mutating the document.
    /// Decode a current stream value through Hayro's standalone filter codecs.
    ///
    /// Only filter-related dictionary entries are materialized into the
    /// detached helper object. Decoding does not need resources, metadata, or
    /// other semantic stream keys, and avoiding them prevents unrelated COS
    /// cycles/deep graphs from being copied merely to inflate a stream.
    pub(crate) fn decoded_owned_stream_data(&self, stream: &OwnedObject) -> Result<Vec<u8>> {
        let OwnedObject::Stream { dictionary, data } = stream else {
            return Err(Error::Invalid("object is not a stream".to_owned()));
        };
        let bytes = data.bytes(self.source())?;
        decode_stream(self, dictionary, bytes.as_ref())
    }

    /// Decode page/Form content while accepting codec warnings that preserve
    /// usable output. Runtime codec errors and unsupported filter chains remain fatal.
    pub(crate) fn decoded_content_stream_value(&self, stream: &OwnedObject) -> Result<Vec<u8>> {
        let OwnedObject::Stream { dictionary, data } = stream else {
            return Err(Error::Invalid("object is not a stream".to_owned()));
        };
        let bytes = data.bytes(self.source())?;
        decode_stream(self, dictionary, bytes.as_ref())
    }

    /// Decode a current indirect stream through the standalone filter-codec bridge.
    pub(crate) fn decoded_stream_data(&self, handle: ObjectHandle) -> Result<Vec<u8>> {
        let Some(stream) = self.current_owned_object_cow(handle)? else {
            return Err(Error::Invalid(
                "stream reference resolves to null".to_owned(),
            ));
        };
        self.decoded_owned_stream_data(&stream)
    }

    /// Resolve a page/Form content reference, decoding streams in one pass.
    ///
    /// Cache lookup precedes source object materialization. A cache entry was
    /// inserted only for an immutable source stream with direct filter values;
    /// references into the mutable overlay are deliberately never cached.
    pub(crate) fn content_read(&self, handle: ObjectHandle) -> Result<Option<ContentRead<'_>>> {
        let unchanged_source_id = match handle {
            ObjectHandle::Existing(id) if self.overlay.change(id).is_none() => Some(id),
            _ => None,
        };
        if let Some(id) = unchanged_source_id
            && let Some(decoded) = self.decoded_content_stream_cache.borrow_mut().get(id)
        {
            return Ok(Some(ContentRead::Cached(decoded)));
        }

        let Some(object) = self.current_owned_object_cow(handle)? else {
            return Ok(None);
        };
        if !matches!(object.as_ref(), OwnedObject::Stream { .. }) {
            return Ok(Some(ContentRead::Other(object)));
        }

        let cache_id =
            unchanged_source_id.filter(|_| stream_filter_configuration_is_direct(&object));
        let decoded = self.decoded_content_stream_value(&object)?;
        if let Some(id) = cache_id
            && decoded.len() <= MAX_DECODED_CONTENT_STREAM_BYTES
        {
            self.decoded_content_stream_cache.borrow_mut().insert(
                id,
                &decoded,
                MAX_DECODED_CONTENT_CACHE_BYTES,
            );
        }
        Ok(Some(ContentRead::Decoded(decoded)))
    }

    /// Decode page/Form content while memoizing untouched source streams.
    ///
    /// The cache is content-specific: image and font decoding never retains
    /// large payloads. Source edits bypass entries without invalidation.
    pub(crate) fn decoded_content_stream_data(&self, handle: ObjectHandle) -> Result<Vec<u8>> {
        match self.content_read(handle)? {
            Some(ContentRead::Decoded(bytes)) => Ok(bytes),
            Some(ContentRead::Cached(bytes)) => Ok(bytes.as_ref().to_vec()),
            Some(ContentRead::Other(_)) => Err(Error::Invalid("object is not a stream".to_owned())),
            None => Err(Error::Invalid(
                "stream reference resolves to null".to_owned(),
            )),
        }
    }

    /// Inspect a current object without materializing untouched source data.
    pub(crate) fn current_value(&self, handle: ObjectHandle) -> Result<Option<CurrentValue<'_>>> {
        match handle {
            ObjectHandle::Existing(id) => match self.overlay.change(id) {
                Some(ExistingObjectChange::Replace(object)) => {
                    Ok(Some(CurrentValue::Owned(object)))
                }
                Some(ExistingObjectChange::Delete) => Err(Error::DeletedReferencedObject {
                    number: id.number,
                    generation: id.generation,
                }),
                None => match self.source.object(id) {
                    Ok(object) => Ok(Some(CurrentValue::Source(
                        MaybeRef::NotRef(object),
                        Some(id),
                    ))),
                    Err(Error::MissingSourceObject { .. }) => Ok(None),
                    Err(error) => Err(error),
                },
            },
            ObjectHandle::New(id) => self
                .overlay
                .added(id)
                .map(CurrentValue::Owned)
                .map(Some)
                .ok_or_else(|| Error::MissingNewObject { index: id.index() }),
        }
    }

    /// Resolve only the reference chain, not the selected object's children.
    pub(crate) fn resolve_current_value<'a>(
        &'a self,
        mut value: CurrentValue<'a>,
    ) -> Result<Option<CurrentValue<'a>>> {
        let mut seen = BTreeSet::new();
        while let Some(handle) = value.reference() {
            if !seen.insert(handle) {
                return Ok(None);
            }
            let Some(next) = self.current_value(handle)? else {
                return Ok(None);
            };
            value = next;
        }
        Ok(Some(value))
    }

    /// Borrow an already-owned overlay object, materializing only untouched
    /// source objects. Useful for reading large overlay streams without cloning
    /// their payload merely to inspect the stream dictionary.
    pub(crate) fn current_owned_object_cow(
        &self,
        handle: ObjectHandle,
    ) -> Result<Option<Cow<'_, OwnedObject>>> {
        Ok(self.current_value(handle)?.map(CurrentValue::into_cow))
    }

    pub(crate) fn current_owned_object(&self, handle: ObjectHandle) -> Result<Option<OwnedObject>> {
        Ok(self.current_owned_object_cow(handle)?.map(Cow::into_owned))
    }

    /// Resolve an indirect chain, borrowing a direct value or overlay object
    /// and materializing only an untouched source object. Cycles and missing
    /// source objects resolve to None, as in the owned resolver.
    pub(crate) fn resolve_owned_value_cow<'a>(
        &'a self,
        value: &'a OwnedObject,
    ) -> Result<Option<Cow<'a, OwnedObject>>> {
        Ok(self
            .resolve_current_value(CurrentValue::Owned(value))?
            .map(CurrentValue::into_cow))
    }

    pub(crate) fn resolve_owned_value(&self, value: &OwnedObject) -> Result<Option<OwnedObject>> {
        Ok(self.resolve_owned_value_cow(value)?.map(Cow::into_owned))
    }

    /// Write the current COW graph as a compact fresh PDF using classic xref output.
    /// Production optimization normally selects writer options through `Config`.
    ///
    /// # Errors
    ///
    /// Returns an error when the current object graph cannot be serialized as a valid PDF.
    pub fn write_compact(&self) -> Result<Vec<u8>> {
        crate::writer::write_pdf(self)
    }

    /// Current indirect page handles, including overlay-added/replaced page dictionaries.
    pub(crate) fn page_handles(&self) -> Result<Vec<ObjectHandle>> {
        // Walk the raw current page tree ourselves instead of relying on
        // Hayro's cached `Pages`. Hayro deliberately falls back to a
        // brute-force object scan when a damaged page tree defeats its typed
        // traversal, and that fallback cannot preserve page order. Stable page
        // numbers are part of hidden-text finding IDs and per-page policy.
        let catalog_handle = ObjectHandle::Existing(self.source.catalog_id());
        let Some(catalog) = self.current_value(catalog_handle)? else {
            return Ok(Vec::new());
        };
        let Some(root) = catalog.dictionary_entry(b"Pages") else {
            return Ok(Vec::new());
        };

        let mut pages = Vec::new();
        let mut seen = BTreeSet::new();
        // Keep one ordered iterator per active /Kids array, avoiding copies
        // of child dictionaries and large, broad arrays.
        let mut arrays = Vec::<CurrentArrayIter<'_>>::new();
        let mut next = Some(root);
        loop {
            let Some(value) = next.take().or_else(|| {
                loop {
                    match arrays.last_mut()?.next() {
                        Some(value) => break Some(value),
                        None => {
                            arrays.pop();
                        }
                    }
                }
            }) else {
                break;
            };
            let handle = value.reference();
            let object = if let Some(handle) = handle {
                if !seen.insert(handle) {
                    continue;
                }
                let Some(object) = self.current_value(handle)? else {
                    continue;
                };
                object
            } else {
                value
            };
            if !object.has_dictionary() {
                continue;
            }

            let kind = match object.dictionary_entry(b"Type") {
                Some(value) => self.resolve_current_value(value)?,
                None => None,
            };
            if kind.is_some_and(|value| value.is_name(b"Page")) {
                if let Some(handle) = handle {
                    pages.push(handle);
                }
                continue;
            }

            let Some(kids) = object.dictionary_entry(b"Kids") else {
                // Retain the existing damaged-leaf recovery policy.
                if let Some(handle) = handle
                    && object.dictionary_entry(b"Parent").is_some()
                {
                    pages.push(handle);
                }
                continue;
            };
            if let Some(kids) = self.resolve_current_value(kids)?
                && let Some(iter) = kids.array_items()
            {
                arrays.push(iter);
            }
        }

        if !pages.is_empty() {
            return Ok(pages);
        }

        // Last-resort recovery for files whose raw page tree is too damaged to
        // traverse. This may not preserve order, but retaining readable pages
        // is preferable to returning an empty document.
        Ok(self
            .source
            .page_ids()
            .into_iter()
            .map(ObjectHandle::Existing)
            .collect())
    }

    /// Determine whether a dictionary entry actually overrides an inherited
    /// page property. Match the reader's typed fallback through null/invalid
    /// values while retaining the raw value when inheritance is materialized.
    pub(crate) fn effective_page_value(&self, key: &[u8], value: CurrentValue<'_>) -> Result<bool> {
        let Some(value) = self.resolve_current_value(value)? else {
            return Ok(false);
        };
        Ok(match key {
            b"Resources" => value.is_dictionary(),
            b"MediaBox" | b"CropBox" => {
                let Some(mut values) = value.array_items() else {
                    return Ok(false);
                };
                for _ in 0..4 {
                    let Some(component) = values.next() else {
                        return Ok(false);
                    };
                    if !matches!(
                        self.resolve_current_value(component)?,
                        Some(
                            CurrentValue::Source(MaybeRef::NotRef(Object::Number(_)), _)
                                | CurrentValue::Owned(
                                    OwnedObject::Integer(_) | OwnedObject::Real(_)
                                )
                        )
                    ) {
                        return Ok(false);
                    }
                }
                true
            }
            b"Rotate" => match value {
                CurrentValue::Source(MaybeRef::NotRef(Object::Number(_)), _)
                | CurrentValue::Owned(OwnedObject::Integer(_) | OwnedObject::Real(_)) => {
                    match value.into_cow().as_ref() {
                        OwnedObject::Integer(value) => i32::try_from(*value).is_ok(),
                        OwnedObject::Real(value) => {
                            let integer = value.trunc();
                            integer >= f64::from(i32::MIN) && integer <= f64::from(i32::MAX)
                        }
                        _ => false,
                    }
                }
                _ => false,
            },
            _ => !matches!(
                value,
                CurrentValue::Source(MaybeRef::NotRef(Object::Null(_)), _)
                    | CurrentValue::Owned(OwnedObject::Null)
            ),
        })
    }

    pub(crate) fn effective_page_inheritance_mask(
        &self,
        dictionary: &CurrentValue<'_>,
    ) -> Result<[bool; PAGE_INHERITABLE_KEYS.len()]> {
        let mut mask = [false; PAGE_INHERITABLE_KEYS.len()];
        for (key, effective) in PAGE_INHERITABLE_KEYS.into_iter().zip(&mut mask) {
            if let Some(value) = dictionary.dictionary_entry(key) {
                *effective = self.effective_page_value(key, value)?;
            }
        }
        Ok(mask)
    }

    /// Resolve one inheritable page-tree value from a current page dictionary.
    pub(crate) fn inherited_page_value(
        &self,
        page: ObjectHandle,
        key: &[u8],
    ) -> Result<Option<OwnedObject>> {
        Ok(self
            .inherited_page_value_cow(page, key)?
            .map(Cow::into_owned))
    }

    pub(crate) fn inherited_page_value_cow(
        &self,
        page: ObjectHandle,
        key: &[u8],
    ) -> Result<Option<Cow<'_, OwnedObject>>> {
        let Some(value) = self.inherited_page_raw_value(page, key)? else {
            return Ok(None);
        };
        Ok(Some(value.into_cow()))
    }

    pub(crate) fn inherited_page_raw_value(
        &self,
        page: ObjectHandle,
        key: &[u8],
    ) -> Result<Option<CurrentValue<'_>>> {
        let Some(object) = self.current_value(page)? else {
            return Ok(None);
        };
        self.inherited_page_value_from_view(object, key)
    }

    pub(crate) fn inherited_page_value_from_view<'a>(
        &'a self,
        mut object: CurrentValue<'a>,
        key: &[u8],
    ) -> Result<Option<CurrentValue<'a>>> {
        let mut seen = BTreeSet::new();
        loop {
            if let Some(value) = object.dictionary_entry(key)
                && self.effective_page_value(key, value)?
            {
                return Ok(object.dictionary_entry(key));
            }
            let Some(parent) = object
                .dictionary_entry(b"Parent")
                .and_then(|value| value.reference())
            else {
                return Ok(None);
            };
            if !seen.insert(parent) {
                return Ok(None);
            }
            let Some(parent_object) = self.current_value(parent)? else {
                return Ok(None);
            };
            object = parent_object;
        }
    }

    /// Collect all objects reachable from the document catalog after applying
    /// overlay replacements/deletions.
    ///
    /// # Errors
    ///
    /// Returns an error when the current graph contains an invalid or deleted required reference.
    pub fn reachable_objects(&self) -> Result<Vec<ObjectHandle>> {
        self.reachable_from([ObjectHandle::Existing(self.source.catalog_id())])
    }

    pub(crate) fn output_roots(&self) -> Vec<ObjectHandle> {
        let mut roots = vec![ObjectHandle::Existing(self.source.catalog_id())];
        for value in self.trailer.values() {
            roots.extend(value.references());
        }
        roots
    }

    fn ensure_reachable_output_cache(&self) -> Result<bool> {
        self.validate_pending_reference_changes()?;
        let generation = self.mutation_generation.get();
        if self
            .reachable_output_cache
            .borrow()
            .as_ref()
            .is_some_and(|cache| cache.generation == generation)
        {
            return Ok(false);
        }

        let mut stats = self.reachability_cache_stats.get();
        stats.rebuilds = stats.rebuilds.saturating_add(1);
        self.reachability_cache_stats.set(stats);

        let mut seen = HashSet::new();
        let mut traversal = Vec::new();
        let mut pending = self.output_roots();
        while let Some(handle) = pending.pop() {
            if !seen.insert(handle) {
                continue;
            }
            let references = match self.references_for_handle(handle) {
                Ok(references) => references,
                Err(Error::MissingSourceObject { .. })
                    if matches!(handle, ObjectHandle::Existing(_)) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };
            traversal.push(handle);
            pending.extend(
                references
                    .into_iter()
                    .filter(|reference| !seen.contains(reference)),
            );
        }

        let mut sorted = traversal.clone();
        sorted.sort_unstable();
        *self.reachable_output_cache.borrow_mut() = Some(ReachableOutputCache {
            generation,
            traversal,
            sorted,
            streams: None,
            streams_by_subtype: HashMap::new(),
        });
        Ok(true)
    }

    fn record_reachability_query(&self, rebuilt: bool) {
        let mut stats = self.reachability_cache_stats.get();
        stats.queries = stats.queries.saturating_add(1);
        if !rebuilt {
            stats.cache_hits = stats.cache_hits.saturating_add(1);
        }
        self.reachability_cache_stats.set(stats);
    }

    fn reachable_output_traversal(&self) -> Result<Vec<ObjectHandle>> {
        let rebuilt = self.ensure_reachable_output_cache()?;
        self.record_reachability_query(rebuilt);
        self.reachable_output_cache
            .borrow()
            .as_ref()
            .map(|cache| cache.traversal.clone())
            .ok_or_else(|| Error::Invalid("reachable-output cache was not populated".to_owned()))
    }

    pub(crate) fn reachable_output_objects(&self) -> Result<Vec<ObjectHandle>> {
        let rebuilt = self.ensure_reachable_output_cache()?;
        self.record_reachability_query(rebuilt);
        self.reachable_output_cache
            .borrow()
            .as_ref()
            .map(|cache| cache.sorted.clone())
            .ok_or_else(|| Error::Invalid("reachable-output cache was not populated".to_owned()))
    }

    pub(crate) fn stream_subtype_is(&self, handle: ObjectHandle, subtype: &[u8]) -> Result<bool> {
        // Immutable source evidence stores only raw names/references. Any
        // indirect name is resolved through the current overlay each time.
        if let ObjectHandle::Existing(id) = handle
            && self.overlay.change(id).is_none()
        {
            return match self.source.source_stream_kind(id) {
                Ok(SourceStreamKind::Stream(Some(SourceStreamSubtype::Name(name)))) => {
                    Ok(name == subtype)
                }
                Ok(SourceStreamKind::Stream(Some(SourceStreamSubtype::Reference(reference)))) => {
                    Ok(self
                        .resolve_current_value(CurrentValue::Owned(&OwnedObject::Reference(
                            reference,
                        )))?
                        .is_some_and(|value| value.is_name(subtype)))
                }
                Ok(SourceStreamKind::NotStream | SourceStreamKind::Stream(None))
                | Err(Error::MissingSourceObject { .. }) => Ok(false),
                Err(error) => Err(error),
            };
        }
        let Some(object) = self.current_value(handle)? else {
            return Ok(false);
        };
        Ok(object.is_stream() && object.resolved_name_is(self, b"Subtype", subtype)?)
    }

    pub(crate) fn reachable_streams(&self) -> Result<Vec<ObjectHandle>> {
        let generation = self.mutation_generation.get();
        if let Some(cache) = self.reachable_output_cache.borrow().as_ref()
            && cache.generation == generation
            && let Some(streams) = cache.streams.as_ref()
        {
            return Ok(streams.clone());
        }

        let mut streams = Vec::new();
        for handle in self.reachable_output_objects()? {
            let is_stream = match handle {
                ObjectHandle::Existing(id) => match self.overlay.change(id) {
                    Some(ExistingObjectChange::Replace(OwnedObject::Stream { .. })) => true,
                    Some(ExistingObjectChange::Replace(_) | ExistingObjectChange::Delete) => false,
                    None => matches!(
                        self.source.source_stream_kind(id)?,
                        SourceStreamKind::Stream(_)
                    ),
                },
                ObjectHandle::New(id) => {
                    matches!(self.overlay.added(id), Some(OwnedObject::Stream { .. }))
                }
            };
            if is_stream {
                streams.push(handle);
            }
        }

        if let Some(cache) = self.reachable_output_cache.borrow_mut().as_mut()
            && cache.generation == generation
        {
            cache.streams = Some(streams.clone());
        }
        Ok(streams)
    }

    pub(crate) fn reachable_streams_with_subtype(
        &self,
        subtype: &[u8],
    ) -> Result<Vec<ObjectHandle>> {
        let generation = self.mutation_generation.get();
        if let Some(cache) = self.reachable_output_cache.borrow().as_ref()
            && cache.generation == generation
            && let Some(streams) = cache.streams_by_subtype.get(subtype)
        {
            return Ok(streams.clone());
        }

        let mut streams = Vec::new();
        for handle in self.reachable_output_objects()? {
            if self.stream_subtype_is(handle, subtype)? {
                streams.push(handle);
            }
        }

        if let Some(cache) = self.reachable_output_cache.borrow_mut().as_mut()
            && cache.generation == generation
        {
            cache
                .streams_by_subtype
                .insert(subtype.to_vec(), streams.clone());
        }
        Ok(streams)
    }

    /// Visit every object reachable from the current output roots exactly once.
    ///
    /// Untouched source objects stay as borrowed Hayro values, while overlay
    /// replacements and newly added objects are passed by reference. This keeps
    /// inspection passes lazy and gives all of them the same missing/deleted
    /// object semantics instead of open-coding the COW traversal repeatedly.
    pub(crate) fn walk_output_objects<F>(&self, mut visit: F) -> Result<()>
    where
        F: for<'a> FnMut(ObjectHandle, CurrentValue<'a>) -> Result<()>,
    {
        for handle in self.reachable_output_traversal()? {
            if let Some(object) = self.current_value(handle)? {
                visit(handle, object)?;
            }
        }
        Ok(())
    }

    /// Collect all objects reachable from an explicit root set.
    ///
    /// # Errors
    ///
    /// Returns an error when graph traversal encounters an invalid or deleted required reference.
    pub fn reachable_from(
        &self,
        roots: impl IntoIterator<Item = ObjectHandle>,
    ) -> Result<Vec<ObjectHandle>> {
        let mut seen = HashSet::new();
        let mut reachable = Vec::new();
        let mut pending = roots.into_iter().collect::<Vec<_>>();

        while let Some(handle) = pending.pop() {
            if !seen.insert(handle) {
                continue;
            }

            let references = match self.references_for_handle(handle) {
                Ok(references) => references,
                Err(Error::MissingSourceObject { .. })
                    if matches!(handle, ObjectHandle::Existing(_)) =>
                {
                    continue;
                }
                Err(error) => return Err(error),
            };

            reachable.push(handle);
            for reference in references {
                if !seen.contains(&reference) {
                    pending.push(reference);
                }
            }
        }

        reachable.sort_unstable();
        Ok(reachable)
    }

    pub(crate) fn references_for_handle(&self, handle: ObjectHandle) -> Result<Vec<ObjectHandle>> {
        match handle {
            ObjectHandle::Existing(id) => match self.overlay.change(id) {
                Some(ExistingObjectChange::Replace(object)) => Ok(object.references()),
                Some(ExistingObjectChange::Delete) => Err(Error::DeletedReferencedObject {
                    number: id.number,
                    generation: id.generation,
                }),
                None => Ok(self
                    .source
                    .references(id)?
                    .into_iter()
                    .map(ObjectHandle::Existing)
                    .collect()),
            },
            ObjectHandle::New(id) => self
                .overlay
                .added(id)
                .map(OwnedObject::references)
                .ok_or_else(|| Error::MissingNewObject { index: id.index() }),
        }
    }
}

fn collect_hayro_references(object: &Object<'_>, references: &mut BTreeSet<ObjectId>) {
    // Borrow the root: a source string may contain a large decoded buffer.
    for value in current_descendants(source_children(object, true), true) {
        if let Some(ObjectHandle::Existing(reference)) = value.reference() {
            references.insert(reference);
        }
    }
}

fn collect_owned_references(object: &OwnedObject, references: &mut BTreeSet<ObjectHandle>) {
    let root = CurrentValue::Owned(object);
    references.extend(root.reference());
    references.extend(
        current_descendants(root.children(true), true).filter_map(|value| value.reference()),
    );
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the bit test below admits only i64 values exactly representable in binary64"
)]
pub fn exact_i64_to_f64(value: i64) -> Option<f64> {
    let magnitude = value.unsigned_abs();
    let significant_bits = magnitude.bit_width();
    let discarded_bits = significant_bits.saturating_sub(f64::MANTISSA_DIGITS);

    (magnitude.trailing_zeros() >= discarded_bits).then_some(value as f64)
}

#[expect(
    clippy::cast_precision_loss,
    reason = "this conversion is intentionally compared with Hayro's f64 view to canonicalize numerically integral PDF numbers"
)]
#[expect(
    clippy::float_cmp,
    reason = "exact equality is intentional here: only numerically integral real values should canonicalize to Integer"
)]
fn owned_from_hayro(object: Object<'_>, stream_id: Option<ObjectId>) -> OwnedObject {
    match object {
        Object::Null(_) => OwnedObject::Null,
        Object::Boolean(value) => OwnedObject::Boolean(value),
        Object::Number(value) => {
            let real = value.as_f64();
            let integer = value.as_i64();
            if real == integer as f64 {
                OwnedObject::Integer(integer)
            } else {
                OwnedObject::Real(real)
            }
        }
        Object::String(value) => OwnedObject::String(value.as_bytes().to_vec()),
        Object::Name(value) => OwnedObject::Name(value.as_ref().to_vec()),
        Object::Dict(value) => OwnedObject::Dictionary(owned_dictionary(&value)),
        Object::Array(value) => OwnedObject::Array(
            value
                .raw_iter()
                .map(owned_from_maybe_ref)
                .collect::<Vec<_>>(),
        ),
        Object::Stream(value) => OwnedObject::Stream {
            dictionary: owned_stream_dictionary(value.dict()),
            data: stream_id.map_or_else(
                || StreamData::Owned(value.raw_data().into_owned()),
                StreamData::Source,
            ),
        },
    }
}

pub fn owned_from_maybe_ref(value: MaybeRef<Object<'_>>) -> OwnedObject {
    match value {
        MaybeRef::Ref(reference) => {
            OwnedObject::Reference(ObjectHandle::Existing(reference.into()))
        }
        MaybeRef::NotRef(object) => owned_from_hayro(object, None),
    }
}

pub fn into_owned_dictionary(value: Cow<'_, OwnedObject>) -> Option<OwnedDictionary> {
    match value {
        Cow::Owned(
            OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. },
        ) => Some(dictionary),
        Cow::Borrowed(value) => value.as_dictionary().cloned(),
        Cow::Owned(_) => None,
    }
}

fn owned_dictionary(dictionary: &Dict<'_>) -> OwnedDictionary {
    dictionary
        .entries()
        .map(|(name, value)| (name.as_ref().to_vec(), owned_from_maybe_ref(value)))
        .collect()
}

pub fn owned_stream_dictionary(dictionary: &Dict<'_>) -> OwnedDictionary {
    dictionary
        .entries()
        .filter(|(name, _)| name.as_ref() != b"Length")
        .map(|(name, value)| (name.as_ref().to_vec(), owned_from_maybe_ref(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ClassicPdfBuilder;

    #[test]
    fn resolved_and_inherited_overlay_values_remain_borrowed() -> Result<()> {
        let mut pdf = crate::test_support::ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let value = OwnedObject::Dictionary(OwnedDictionary::from([(
            b"Private".to_vec(),
            OwnedObject::String(vec![42; 1024 * 1024]),
        )]));
        let handle = ObjectHandle::New(document.add_object(value));
        let reference = OwnedObject::Reference(handle);
        let chain = ObjectHandle::New(document.add_object(reference.clone()));
        let parent = ObjectHandle::New(document.add_object(OwnedObject::Dictionary(
            OwnedDictionary::from([
                (b"Resources".to_vec(), reference.clone()),
                (
                    b"Unrelated".to_vec(),
                    OwnedObject::String(vec![23; 1024 * 1024]),
                ),
            ]),
        )));
        let page = ObjectHandle::New(document.add_object(OwnedObject::Dictionary(
            OwnedDictionary::from([(b"Parent".to_vec(), OwnedObject::Reference(parent))]),
        )));
        let Some(Cow::Borrowed(original)) = document.current_owned_object_cow(handle)? else {
            return Err(Error::Invalid("missing owned value".to_owned()));
        };
        assert!(
            matches!(document.resolve_owned_value_cow(&OwnedObject::Reference(chain))?, Some(Cow::Borrowed(resolved)) if std::ptr::eq(resolved, original))
        );
        let Some(Cow::Borrowed(inherited)) =
            document.inherited_page_value_cow(page, b"Resources")?
        else {
            return Err(Error::Invalid("inherited value was copied".to_owned()));
        };
        assert_eq!(inherited, &reference);
        assert!(
            matches!(document.resolve_owned_value_cow(inherited)?, Some(Cow::Borrowed(resolved)) if std::ptr::eq(resolved, original))
        );
        Ok(())
    }

    fn selective_page_fixture() -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids 8 0 R /Count 2 /Resources << /XObject << /Root 4 0 R >> >> /Rotate 90 >>")?;
        pdf.object(
            3,
            b"<< /Type 7 0 R /Parent 2 0 R /Resources 9 0 R /Contents 4 0 R /Rotate 10 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /Type /Page /Parent 2 0 R /Resources 9 0 R >>")?;
        pdf.object(7, b"/Page")?;
        pdf.object(8, b"[5 0 R 3 0 R]")?;
        pdf.object(
            9,
            b"<< /XObject << /Local 4 0 R >> /Font << /Private (irrelevant) >> >>",
        )?;
        pdf.object(10, b"180")?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn selective_current_values_resolve_overlay_replacements() -> Result<()> {
        let mut document = selective_page_fixture()?;
        let existing = |n| ObjectHandle::Existing(ObjectId::new(n, 0));
        assert_eq!(document.page_handles()?, vec![existing(5), existing(3)]);
        assert_eq!(
            document.inherited_page_value(existing(3), b"Rotate")?,
            Some(OwnedObject::Reference(existing(10)))
        );
        let Some(raw) = document.inherited_page_raw_value(existing(3), b"Resources")? else {
            return Err(Error::Invalid("missing raw resources".to_owned()));
        };
        assert_eq!(raw.reference(), Some(existing(9)));
        let Some(resources) = document.resolve_current_value(raw)? else {
            return Err(Error::Invalid("missing resource dictionary".to_owned()));
        };
        assert!(matches!(
            resources,
            CurrentValue::Source(MaybeRef::NotRef(Object::Dict(_)), _)
        ));
        assert!(
            resources
                .dictionary_entry(b"XObject")
                .is_some_and(|value| value.is_dictionary())
        );
        *document.edit_handle(existing(8))? = OwnedObject::Array(vec![
            OwnedObject::Reference(existing(3)),
            OwnedObject::Reference(existing(5)),
            OwnedObject::Reference(existing(3)),
        ]);
        *document.edit_handle(existing(9))? = OwnedObject::Null;
        *document.edit_handle(existing(10))? = OwnedObject::Null;
        assert_eq!(document.page_handles()?, vec![existing(3), existing(5)]);
        assert_eq!(
            document.inherited_page_value(existing(3), b"Rotate")?,
            Some(OwnedObject::Integer(90))
        );
        assert!(matches!(
            document.inherited_page_raw_value(existing(3), b"Resources")?,
            Some(CurrentValue::Source(MaybeRef::NotRef(Object::Dict(_)), _))
        ));
        *document.edit_handle(existing(7))? = OwnedObject::Name(b"Pages".to_vec());
        // Recovery still recognizes the damaged leaf with Parent and no Kids.
        assert_eq!(document.page_handles()?, vec![existing(3), existing(5)]);
        assert_eq!(
            crate::content::page_content(&document, existing(3))?,
            b"q Q"
        );
        assert_eq!(
            crate::content::page_xobject_handles(&document, existing(3))?,
            BTreeMap::from([(b"Root".to_vec(), existing(4))])
        );
        Ok(())
    }

    #[test]
    fn lazy_reference_resolution_keeps_source_stream_identity() -> Result<()> {
        let document = selective_page_fixture()?;
        let id = ObjectId::new(4, 0);
        let reference = OwnedObject::Reference(ObjectHandle::Existing(id));
        let Some(value) = document.resolve_owned_value_cow(&reference)? else {
            return Err(Error::Invalid("missing content stream".to_owned()));
        };
        assert!(
            matches!(value.as_ref(), OwnedObject::Stream { data: StreamData::Source(source), .. } if *source == id)
        );
        let Some(view) = document.current_value(ObjectHandle::Existing(id))? else {
            return Err(Error::Invalid("missing stream".to_owned()));
        };
        assert!(view.has_dictionary());
        assert!(view.dictionary_entry(b"Length").is_none());
        Ok(())
    }

    #[test]
    fn exact_i64_to_f64_rejects_only_lossy_conversions() {
        assert_eq!(exact_i64_to_f64(0), Some(0.0));
        assert_eq!(exact_i64_to_f64(1_i64 << 53), Some(9_007_199_254_740_992.0));
        assert_eq!(
            exact_i64_to_f64((1_i64 << 53) + 2),
            Some(9_007_199_254_740_994.0)
        );
        assert_eq!(exact_i64_to_f64((1_i64 << 53) + 1), None);
        assert_eq!(exact_i64_to_f64(-((1_i64 << 53) + 1)), None);
        assert_eq!(
            exact_i64_to_f64(i64::MIN),
            Some(-9_223_372_036_854_775_808.0)
        );
        assert_eq!(exact_i64_to_f64(i64::MAX), None);
    }

    #[test]
    fn overlay_is_copy_on_write() {
        let id = ObjectId::new(12, 0);
        let mut overlay = ObjectOverlay::default();
        assert!(overlay.is_empty());

        overlay.replace(id, OwnedObject::Integer(42));
        assert_eq!(
            overlay.change(id),
            Some(&ExistingObjectChange::Replace(OwnedObject::Integer(42)))
        );

        let added = overlay.add(OwnedObject::Reference(ObjectHandle::Existing(id)));
        assert_eq!(added.index(), 0);
        assert_eq!(
            overlay.added(added),
            Some(&OwnedObject::Reference(ObjectHandle::Existing(id)))
        );

        overlay.delete(id);
        assert_eq!(overlay.change(id), Some(&ExistingObjectChange::Delete));
        assert!(!overlay.is_empty());
    }

    #[test]
    fn source_object_stream_classification_covers_cached_stream_and_non_stream() -> Result<()> {
        let source = SourcePdf::from_bytes(sample_pdf())?;
        let stream = ObjectId::new(4, 0);
        let dictionary = ObjectId::new(1, 0);

        let _ = source.object_with_references(stream)?;
        let _ = source.object_with_references(dictionary)?;
        assert!(source.source_object_is_stream(stream)?);
        assert!(!source.source_object_is_stream(dictionary)?);
        Ok(())
    }

    #[test]
    fn missing_source_stream_subtype_is_null_like() -> Result<()> {
        let document = EditDocument::from_bytes(sample_pdf())?;
        assert!(
            !document
                .stream_subtype_is(ObjectHandle::Existing(ObjectId::new(999_999, 0)), b"Image",)?
        );
        Ok(())
    }

    #[test]
    fn source_stream_subtype_resolves_indirect_name() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /XObject << /F 4 0 R >> >> /Contents 6 0 R >>",
        )?;
        pdf.stream(
            4,
            b"/Type /XObject /Subtype 5 0 R /BBox [0 0 1 1] /Resources <<>>",
            b"q Q",
        )?;
        pdf.object(5, b"/Form")?;
        pdf.stream(6, b"", b"q Q")?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let form = ObjectHandle::Existing(ObjectId::new(4, 0));
        assert!(document.stream_subtype_is(form, b"Form")?);
        assert!(
            document
                .reachable_streams_with_subtype(b"Form")?
                .contains(&form)
        );
        *document.edit_handle(ObjectHandle::Existing(ObjectId::new(5, 0)))? =
            OwnedObject::Name(b"Image".to_vec());
        assert!(!document.stream_subtype_is(form, b"Form")?);
        assert!(document.stream_subtype_is(form, b"Image")?);
        assert!(
            !document
                .reachable_streams_with_subtype(b"Form")?
                .contains(&form)
        );
        assert!(
            document
                .reachable_streams_with_subtype(b"Image")?
                .contains(&form)
        );
        Ok(())
    }

    #[test]
    fn content_decode_keeps_output_from_recoverable_flate_warning() -> Result<()> {
        let document = EditDocument::from_bytes(sample_pdf())?;
        let stream = OwnedObject::Stream {
            dictionary: OwnedDictionary::from([(
                b"Filter".to_vec(),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            )]),
            data: StreamData::Owned(vec![0x78, 0x9c, 0x4b, 0x04]),
        };
        assert_eq!(document.decoded_content_stream_value(&stream)?, b"a");
        Ok(())
    }

    #[test]
    fn invalid_source_is_rejected() {
        assert!(SourcePdf::from_bytes(b"not a pdf".to_vec()).is_err());
    }

    #[test]
    fn materialized_stream_keeps_source_payload_lazy() -> Result<()> {
        let source = SourcePdf::from_bytes(sample_pdf())?;
        let stream_id = ObjectId::new(4, 0);
        let object = source.materialize(stream_id)?;

        let OwnedObject::Stream { dictionary, data } = object else {
            return Err(Error::Invalid("expected stream fixture".to_owned()));
        };
        assert!(!dictionary.contains_key(b"Length".as_slice()));
        assert_eq!(data, StreamData::Source(stream_id));
        let bytes = data.bytes(&source)?;
        assert_eq!(bytes.as_ref(), b"q Q");
        Ok(())
    }

    #[test]
    fn decoded_source_stream_cache_is_bypassed_after_edit() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let stream_id = ObjectId::new(4, 0);
        let handle = ObjectHandle::Existing(stream_id);
        let first = document.decoded_content_stream_data(handle)?;
        assert_eq!(first, b"q Q");
        let stream = document.edit_object(stream_id)?;
        let OwnedObject::Stream { data, .. } = stream else {
            return Err(Error::Invalid(
                "fixture object should be a stream".to_owned(),
            ));
        };
        *data = StreamData::Owned(b"BT ET".to_vec());
        let second = document.decoded_content_stream_data(handle)?;
        assert_eq!(second, b"BT ET");
        Ok(())
    }

    #[test]
    fn borrowed_source_reference_walk_preserves_nested_and_stream_length_rules() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        // Stream /Length is an output-writer detail and must not be counted
        // as an outgoing reference; a nested ordinary dictionary /Length is.
        pdf.object(
            4,
            b"<< /Length 5 0 R /Custom [6 0 R << /Length 7 0 R >>] >>\nstream\nq Q\nendstream",
        )?;
        pdf.object(5, b"3")?;
        pdf.object(6, b"null")?;
        pdf.object(7, b"null")?;

        // This direct array would create dozens of recursive calls under the
        // previous collector; duplicate indirect references must be deduped.
        let mut nested = vec![b'['; 48];
        nested.extend_from_slice(b"6 0 R [7 0 R 6 0 R]");
        nested.extend(std::iter::repeat_n(b']', 48));
        pdf.object(8, &nested)?;

        let source = SourcePdf::from_bytes(pdf.finish(1)?)?;
        let stream_id = ObjectId::new(4, 0);
        let array_id = ObjectId::new(8, 0);
        let expected = vec![ObjectId::new(6, 0), ObjectId::new(7, 0)];
        assert!(matches!(source.object(stream_id)?, Object::Stream(_)));
        assert_eq!(source.references(stream_id)?, expected);
        assert_eq!(source.references(array_id)?, expected);
        let (_, first) = source.object_with_references(array_id)?;
        assert_eq!(first, expected);
        assert_eq!(source.references(array_id)?, expected);
        // Source graph walking must not resolve or traverse either reference:
        // direct adjacency is separate from transitive reachability.
        assert_eq!(source.references(ObjectId::new(6, 0))?, Vec::new());
        Ok(())
    }

    #[test]
    fn owned_reference_walk_handles_deep_cos_without_recursion() {
        let target = ObjectHandle::Existing(ObjectId::new(42, 0));
        let mut nested = OwnedObject::Reference(target);
        // Deep enough to overflow recursive walks on normal test-thread stacks.
        for _ in 0..20_000 {
            nested = OwnedObject::Array(vec![nested]);
        }
        assert!(contains_indirect_reference(&nested));
        assert_eq!(nested.references(), vec![target]);

        // Iteratively dismantle the test fixture too: recursively dropping a
        // 20,000-level owned array would otherwise obscure the traversal fix.
        while let OwnedObject::Array(mut values) = nested {
            let Some(child) = values.pop() else {
                break;
            };
            nested = child;
        }

        let mut no_reference = OwnedObject::Integer(7);
        for _ in 0..20_000 {
            no_reference = OwnedObject::Array(vec![no_reference]);
        }
        assert!(!contains_indirect_reference(&no_reference));
        assert_eq!(no_reference.references(), Vec::<ObjectHandle>::new());
        while let OwnedObject::Array(mut values) = no_reference {
            let Some(child) = values.pop() else {
                break;
            };
            no_reference = child;
        }
    }

    #[test]
    fn owned_reference_walk_only_skips_stream_length_in_graph_collection() {
        let length = ObjectHandle::Existing(ObjectId::new(41, 0));
        let nested = ObjectHandle::Existing(ObjectId::new(42, 0));

        let mut filter_dict = OwnedDictionary::new();
        filter_dict.insert(b"Length".to_vec(), OwnedObject::Reference(nested));
        let mut stream_dict = OwnedDictionary::new();
        stream_dict.insert(b"Length".to_vec(), OwnedObject::Reference(length));
        stream_dict.insert(
            b"DecodeParms".to_vec(),
            OwnedObject::Array(vec![OwnedObject::Dictionary(filter_dict)]),
        );
        let stream = OwnedObject::Stream {
            dictionary: stream_dict,
            data: StreamData::Owned(Vec::new()),
        };

        // Filter eligibility inspects all descendant entries, including
        // /Length, whereas graph traversal excludes only stream /Length.
        assert!(contains_indirect_reference(&stream));
        assert_eq!(stream.references(), vec![nested]);
        assert!(!stream_filter_configuration_is_direct(&stream));

        let mut direct_dict = OwnedDictionary::new();
        direct_dict.insert(
            b"DecodeParms".to_vec(),
            OwnedObject::Array(vec![OwnedObject::Dictionary(OwnedDictionary::new())]),
        );
        let direct = OwnedObject::Stream {
            dictionary: direct_dict,
            data: StreamData::Owned(Vec::new()),
        };
        assert!(stream_filter_configuration_is_direct(&direct));
        assert!(!contains_indirect_reference(&direct));
        assert_eq!(direct.references(), Vec::<ObjectHandle>::new());
    }

    #[test]
    fn decoded_content_cache_replaces_old_entries_with_recently_used_streams() -> Result<()> {
        let one = ObjectId::new(1, 0);
        let two = ObjectId::new(2, 0);
        let three = ObjectId::new(3, 0);
        let four = ObjectId::new(4, 0);
        let mut cache = DecodedContentCache::default();
        let capacity = 10;

        cache.insert(one, b"one", capacity);
        cache.insert(two, b"two", capacity);
        cache.insert(three, b"four", capacity);
        assert_eq!(cache.bytes, capacity);
        assert_eq!(cache.oldest_first.len(), 3);

        let Some(held) = cache.get(one) else {
            return Err(Error::Invalid("initial cache entry missing".to_owned()));
        };
        let Some(again) = cache.get(one) else {
            return Err(Error::Invalid("cache hit should be shared".to_owned()));
        };
        assert!(Arc::ptr_eq(&held, &again));
        // The oldest entry is now two, not one.
        cache.insert(four, b"xx", capacity);
        assert!(cache.get(two).is_none());
        assert!(cache.get(one).is_some());
        assert!(cache.get(three).is_some());
        assert!(cache.get(four).is_some());
        assert_eq!(cache.bytes, 9);
        assert_eq!(cache.oldest_first.len(), 3);

        // Reinserting two evicts one (the least recently used after the reads).
        cache.insert(two, b"long", capacity);
        assert!(cache.get(one).is_none());
        assert_eq!(held.as_ref(), b"one"); // Held snapshots outlive eviction.
        assert_eq!(cache.bytes, 10);
        assert_eq!(cache.oldest_first.len(), cache.streams.len());

        // A payload larger than the total capacity is not retained or allowed
        // to evict anything already useful.
        cache.insert(one, b"larger than the capacity", capacity);
        assert!(cache.get(one).is_none());
        assert_eq!(cache.bytes, 10);

        // A clock rollover must not reorder the live cache entries.
        cache.access_clock = u64::MAX;
        let Some(two_snapshot) = cache.get(two) else {
            return Err(Error::Invalid(
                "expected cached stream after clock rebase".to_owned(),
            ));
        };
        assert_eq!(two_snapshot.as_ref(), b"long");
        assert_eq!(cache.oldest_first.len(), cache.streams.len());
        cache.evict(two);
        assert_eq!(cache.bytes, 6);
        assert!(!cache.oldest_first.iter().any(|(_, id)| *id == two));

        // Replacing an already cached stream with an oversized payload must
        // invalidate its previous data, not preserve a stale cache hit.
        cache.insert(four, b"payload larger than ten", capacity);
        assert!(cache.get(four).is_none());
        assert_eq!(cache.bytes, 4);
        assert_eq!(cache.oldest_first.len(), cache.streams.len());
        Ok(())
    }

    #[test]
    fn source_content_cache_reclaims_bytes_for_targeted_and_opaque_edits() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents [4 0 R 5 0 R] >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"BT ET")?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let first = ObjectHandle::Existing(ObjectId::new(4, 0));
        let second = ObjectHandle::Existing(ObjectId::new(5, 0));
        assert_eq!(document.decoded_content_stream_data(first)?, b"q Q");
        assert_eq!(document.decoded_content_stream_data(second)?, b"BT ET");
        let Some(ContentRead::Cached(first_snapshot)) = document.content_read(first)? else {
            return Err(Error::Invalid("expected first cached stream".to_owned()));
        };
        let Some(ContentRead::Cached(second_snapshot)) = document.content_read(second)? else {
            return Err(Error::Invalid("expected second cached stream".to_owned()));
        };
        {
            let cache = document.decoded_content_stream_cache.borrow();
            assert_eq!(cache.streams.len(), 2);
            assert_eq!(cache.bytes, b"q Q".len() + b"BT ET".len());
        }

        // Editing a non-cached catalog object must preserve both entries.
        let _ = document.edit_object(ObjectId::new(1, 0))?;
        assert_eq!(
            document.decoded_content_stream_cache.borrow().streams.len(),
            2
        );

        let OwnedObject::Stream { data, .. } = document.edit_object(ObjectId::new(4, 0))? else {
            return Err(Error::Invalid("expected first source stream".to_owned()));
        };
        *data = StreamData::Owned(b"q q Q".to_vec());
        {
            let cache = document.decoded_content_stream_cache.borrow();
            assert_eq!(cache.streams.len(), 1);
            assert!(!cache.streams.contains_key(&ObjectId::new(4, 0)));
            assert_eq!(cache.bytes, b"BT ET".len());
        }
        assert_eq!(first_snapshot.as_ref(), b"q Q");
        assert_eq!(document.decoded_content_stream_data(first)?, b"q q Q");
        let Some(ContentRead::Cached(second_again)) = document.content_read(second)? else {
            return Err(Error::Invalid("unrelated cache entry was lost".to_owned()));
        };
        assert!(Arc::ptr_eq(&second_snapshot, &second_again));

        // The opaque overlay API can replace arbitrary source objects, so all
        // remaining cache allocations and their byte accounting are dropped.
        document.overlay_mut().replace(
            ObjectId::new(5, 0),
            OwnedObject::Stream {
                dictionary: OwnedDictionary::new(),
                data: StreamData::Owned(b"ET".to_vec()),
            },
        );
        let cache = document.decoded_content_stream_cache.borrow();
        assert!(cache.streams.is_empty());
        assert_eq!(cache.bytes, 0);
        drop(cache);
        assert_eq!(second_snapshot.as_ref(), b"BT ET");
        assert_eq!(document.decoded_content_stream_data(second)?, b"ET");
        Ok(())
    }

    #[test]
    fn cached_content_reads_share_payload_and_survive_source_edits() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let id = ObjectId::new(4, 0);
        let handle = ObjectHandle::Existing(id);

        // The first read fills the cache without changing the owned stream API.
        assert_eq!(document.decoded_content_stream_data(handle)?, b"q Q");
        let Some(ContentRead::Cached(first)) = document.content_read(handle)? else {
            return Err(Error::Invalid("expected a cached stream".to_owned()));
        };
        let Some(ContentRead::Cached(second)) = document.content_read(handle)? else {
            return Err(Error::Invalid("expected the same cached stream".to_owned()));
        };
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.as_ref(), b"q Q");

        // Holding the shared payload does not lock the cache or prevent COW
        // edits; it remains an immutable snapshot after an overlay change.
        let stream = document.edit_object(id)?;
        let OwnedObject::Stream { data, .. } = stream else {
            return Err(Error::Invalid("expected a stream".to_owned()));
        };
        *data = StreamData::Owned(b"BT ET".to_vec());
        assert_eq!(first.as_ref(), b"q Q");
        assert_eq!(document.decoded_content_stream_data(handle)?, b"BT ET");
        assert!(matches!(
            document.content_read(handle)?,
            Some(ContentRead::Decoded(bytes)) if bytes == b"BT ET"
        ));
        Ok(())
    }

    #[test]
    fn content_cache_reuses_unchanged_source_only_and_bypasses_indirect_filters() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents [4 0 R 5 0 R] >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        let encoded = crate::stream_codec::encode_flate(b"BT ET", crate::FlateLevel::PROCESSING)?;
        pdf.stream(5, b"/Filter 6 0 R", &encoded)?;
        pdf.object(6, b"/FlateDecode")?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let plain = ObjectHandle::Existing(ObjectId::new(4, 0));
        let indirect = ObjectHandle::Existing(ObjectId::new(5, 0));
        assert_eq!(document.decoded_content_stream_data(plain)?, b"q Q");
        assert_eq!(document.decoded_content_stream_data(plain)?, b"q Q");
        assert_eq!(document.decoded_content_stream_data(indirect)?, b"BT ET");
        assert_eq!(document.decoded_content_stream_data(indirect)?, b"BT ET");
        let page = document.page_handles()?[0];
        assert_eq!(
            crate::content::page_content(&document, page)?,
            b"q Q\nBT ET"
        );
        assert_eq!(
            crate::content::page_content(&document, page)?,
            b"q Q\nBT ET"
        );
        {
            let cache = document.decoded_content_stream_cache.borrow();
            assert_eq!(cache.streams.len(), 1);
            assert_eq!(
                cache
                    .streams
                    .get(&ObjectId::new(4, 0))
                    .map(|entry| entry.bytes.as_ref()),
                Some(b"q Q".as_slice())
            );
            assert!(!cache.streams.contains_key(&ObjectId::new(5, 0)));
        }

        *document.edit_object(ObjectId::new(6, 0))? = OwnedObject::Name(b"ASCIIHexDecode".to_vec());
        // Indirect filter aliases depend on the COW graph and must be decoded
        // again after changing the referenced object.
        assert!(document.decoded_content_stream_data(indirect).is_err());
        assert!(crate::content::page_content(&document, page).is_err());
        // Editing an unrelated filter does not invalidate a direct source's
        // cached bytes.
        assert_eq!(document.decoded_content_stream_data(plain)?, b"q Q");
        Ok(())
    }

    #[test]
    fn content_decode_borrows_new_overlay_streams_without_source_cache_entries() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let handle = ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary: OwnedDictionary::new(),
            data: StreamData::Owned(b"BT ET".to_vec()),
        }));
        assert_eq!(document.decoded_content_stream_data(handle)?, b"BT ET");
        assert_eq!(document.decoded_content_stream_data(handle)?, b"BT ET");
        assert!(
            document
                .decoded_content_stream_cache
                .borrow()
                .streams
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn first_edit_materializes_only_target_object() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        assert!(document.overlay().is_empty());

        let catalog_id = document.source().catalog_id();
        let catalog = document.edit_object(catalog_id)?;
        let Some(dictionary) = catalog.as_dictionary_mut() else {
            return Err(Error::Invalid("catalog should be a dictionary".to_owned()));
        };
        dictionary.insert(b"Lang".to_vec(), OwnedObject::String(b"en".to_vec()));

        assert_eq!(document.overlay().changes().count(), 1);
        Ok(())
    }

    #[test]
    fn reachability_walks_source_without_materializing_overlay() -> Result<()> {
        let document = EditDocument::from_bytes(sample_pdf())?;
        let reachable = document.reachable_objects()?;

        assert_eq!(
            reachable,
            vec![
                ObjectHandle::Existing(ObjectId::new(1, 0)),
                ObjectHandle::Existing(ObjectId::new(2, 0)),
                ObjectHandle::Existing(ObjectId::new(3, 0)),
                ObjectHandle::Existing(ObjectId::new(4, 0)),
            ]
        );
        assert!(document.overlay().is_empty());
        Ok(())
    }

    #[test]
    fn speculative_new_object_rollback_discards_only_tail_allocations() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let prefix = document.add_object(OwnedObject::Null);
        let checkpoint = document.new_object_checkpoint();
        let speculative = document.add_object(OwnedObject::Boolean(true));
        assert!(document.overlay().added(speculative).is_some());

        document.rollback_new_objects(checkpoint)?;
        assert!(document.overlay().added(prefix).is_some());
        assert!(document.overlay().added(speculative).is_none());

        let replacement = document.add_object(OwnedObject::Integer(7));
        assert_eq!(replacement.index(), speculative.index());
        Ok(())
    }

    #[test]
    fn unreachable_allocation_does_not_invalidate_output_reachability() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let before = document.reachable_output_objects()?;
        assert_eq!(document.reachability_cache_stats().rebuilds, 1);

        let added = document.add_object(OwnedObject::Dictionary(OwnedDictionary::new()));
        let after_allocation = document.reachable_output_objects()?;
        assert_eq!(after_allocation, before);
        assert!(!after_allocation.contains(&ObjectHandle::New(added)));
        let stats = document.reachability_cache_stats();
        assert_eq!(stats.rebuilds, 1);
        assert!(stats.cache_hits >= 1);

        let catalog_id = document.source().catalog_id();
        let catalog = document.edit_object(catalog_id)?;
        let Some(dictionary) = catalog.as_dictionary_mut() else {
            return Err(Error::Invalid("catalog should be a dictionary".to_owned()));
        };
        dictionary.insert(
            b"PieceInfo".to_vec(),
            OwnedObject::Reference(ObjectHandle::New(added)),
        );

        let linked = document.reachable_output_objects()?;
        assert!(linked.contains(&ObjectHandle::New(added)));
        assert_eq!(document.reachability_cache_stats().rebuilds, 2);
        Ok(())
    }

    #[test]
    fn reachability_follows_new_overlay_references() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let added = document.add_object(OwnedObject::Dictionary(OwnedDictionary::new()));
        let catalog_id = document.source().catalog_id();
        let catalog = document.edit_object(catalog_id)?;
        let Some(dictionary) = catalog.as_dictionary_mut() else {
            return Err(Error::Invalid("catalog should be a dictionary".to_owned()));
        };
        dictionary.insert(
            b"PieceInfo".to_vec(),
            OwnedObject::Reference(ObjectHandle::New(added)),
        );

        let reachable = document.reachable_objects()?;
        assert!(reachable.contains(&ObjectHandle::New(added)));
        Ok(())
    }

    #[test]
    fn reachable_output_cache_reuses_after_edge_stable_edit() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let before = document.reachable_output_objects()?;
        let initial_stats = document.reachability_cache_stats();
        assert_eq!(initial_stats.rebuilds, 1);

        let stream = document.edit_object(ObjectId::new(4, 0))?;
        let OwnedObject::Stream { data, .. } = stream else {
            return Err(Error::Invalid(
                "fixture object should be a stream".to_owned(),
            ));
        };
        *data = StreamData::Owned(b"BT ET".to_vec());

        let after = document.reachable_output_objects()?;
        assert_eq!(after, before);

        let stats = document.reachability_cache_stats();
        assert_eq!(stats.rebuilds, 1);
        assert_eq!(stats.edge_checks, 1);
        assert_eq!(stats.edge_stable_reuses, 1);
        assert!(stats.cache_hits >= 1);
        Ok(())
    }

    #[test]
    fn reachable_output_cache_invalidates_after_object_edit() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let before = document.reachable_output_objects()?;
        assert!(before.contains(&ObjectHandle::Existing(ObjectId::new(4, 0))));
        // Hit the cache before changing the graph.
        let cached = document.reachable_output_objects()?;
        assert_eq!(cached, before);

        let page = document.edit_object(ObjectId::new(3, 0))?;
        let Some(dictionary) = page.as_dictionary_mut() else {
            return Err(Error::Invalid("page should be a dictionary".to_owned()));
        };
        dictionary.remove(b"Contents".as_slice());

        let after = document.reachable_output_objects()?;
        assert!(!after.contains(&ObjectHandle::Existing(ObjectId::new(4, 0))));
        Ok(())
    }

    #[test]
    fn reachable_output_cache_invalidates_after_trailer_edit() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        let added = document.add_object(OwnedObject::Dictionary(OwnedDictionary::new()));
        let before = document.reachable_output_objects()?;
        assert!(!before.contains(&ObjectHandle::New(added)));

        document.trailer_mut().insert(
            b"Info".to_vec(),
            OwnedObject::Reference(ObjectHandle::New(added)),
        );
        let after = document.reachable_output_objects()?;
        assert!(after.contains(&ObjectHandle::New(added)));
        Ok(())
    }

    #[test]
    fn reachability_rejects_dangling_deleted_reference() -> Result<()> {
        let mut document = EditDocument::from_bytes(sample_pdf())?;
        document.overlay_mut().delete(ObjectId::new(4, 0));

        assert!(matches!(
            document.reachable_objects(),
            Err(Error::DeletedReferencedObject {
                number: 4,
                generation: 0,
            })
        ));
        Ok(())
    }

    fn sample_pdf() -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << >> /Contents 4 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        pdf
    }

    fn append_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, object: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(object);
    }
}
