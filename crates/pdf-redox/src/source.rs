use crate::{Error, Result, SourceLoadError};
use flpdf::{DecodeLevel, ObjectHandle as FlObjectHandle};
use hayro_syntax::{
    Pdf, PdfVersion,
    object::{Dict, MaybeRef, Object, ObjectIdentifier, Stream},
};
use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    rc::Rc,
    sync::Arc,
};

/// Immutable, lazily parsed source PDF backed by Hayro.
///
/// Hayro keeps the original bytes alive and parses objects on demand. Mutations
/// belong in [`ObjectOverlay`] rather than in this source representation.
pub struct SourcePdf {
    pdf: Pdf,
    bytes: Arc<Vec<u8>>,
}

impl SourcePdf {
    /// Parse owned PDF bytes without making another full-document copy.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_shared(Arc::new(bytes))
    }

    /// Parse PDF bytes already held in shared storage.
    pub fn from_shared(bytes: Arc<Vec<u8>>) -> Result<Self> {
        let pdf = Pdf::new(Arc::clone(&bytes)).map_err(SourceLoadError::from)?;
        Ok(Self { pdf, bytes })
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

    pub(crate) fn object_with_references(
        &self,
        id: ObjectId,
    ) -> Result<(Object<'_>, Vec<ObjectId>)> {
        let object = self.object(id)?;
        let mut references = BTreeSet::new();
        collect_hayro_references(&object, &mut references);
        Ok((object, references.into_iter().collect()))
    }

    pub(crate) fn contains_object(&self, id: ObjectId) -> bool {
        self.pdf.xref().get::<Object<'_>>(id.into()).is_some()
    }

    /// Collect indirect references reachable directly from one source object.
    ///
    /// This walks only the borrowed COS structure of that object. Referenced
    /// objects are not resolved or materialized, and stream payload bytes are
    /// never touched.
    pub fn references(&self, id: ObjectId) -> Result<Vec<ObjectId>> {
        Ok(self.object_with_references(id)?.1)
    }

    /// Return the encoded/decrypted bytes of a source stream on demand.
    ///
    /// For ordinary unencrypted files this remains a borrowed view into the
    /// source PDF. Hayro may allocate when decryption is required.
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

const MAX_TRAILER_NESTING: usize = 256;

fn is_writer_owned_trailer_key(name: &[u8], xref_stream: bool) -> bool {
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

pub(crate) fn owned_from_flpdf(handle: &FlObjectHandle, depth: usize) -> Result<OwnedObject> {
    if depth > MAX_TRAILER_NESTING {
        return Err(Error::Invalid(
            "trailer direct-object nesting exceeds the supported limit".to_owned(),
        ));
    }

    if let Some(reference) = handle.object_ref() {
        let number = i32::try_from(reference.number).map_err(|_| {
            Error::Invalid("trailer object number exceeds the supported range".to_owned())
        })?;
        return Ok(OwnedObject::Reference(ObjectHandle::Existing(
            ObjectId::new(number, i32::from(reference.generation)),
        )));
    }

    // Detached flpdf helper results may carry lazily provided direct values.
    // Force only those non-reference handles to materialize before inspecting
    // their concrete type; source indirect references have already returned.
    let _ = handle.type_name()?;
    if handle.is_null() {
        return Ok(OwnedObject::Null);
    }
    if let Some(value) = handle.as_boolean() {
        return Ok(OwnedObject::Boolean(value));
    }
    if let Some(value) = handle.as_integer() {
        return Ok(OwnedObject::Integer(value));
    }
    if let Some(value) = handle.as_real() {
        return Ok(OwnedObject::Real(value));
    }
    if let Some(value) = handle.as_name() {
        return Ok(OwnedObject::Name(value));
    }
    if let Some(value) = handle.as_string() {
        return Ok(OwnedObject::String(value));
    }
    if let Some(values) = handle.as_array() {
        let values = values
            .iter()
            .map(|value| owned_from_flpdf(value, depth + 1))
            .collect::<Result<Vec<_>>>()?;
        return Ok(OwnedObject::Array(values));
    }
    if let Some(stream_dictionary) = handle.as_stream_dict() {
        let mut dictionary = OwnedDictionary::new();
        let entries = stream_dictionary.as_dictionary().ok_or_else(|| {
            Error::Invalid("direct trailer stream dictionary is not a dictionary".to_owned())
        })?;
        for (key, value) in entries {
            let name = key.strip_prefix(b"/").unwrap_or(key.as_slice());
            dictionary.insert(name.to_vec(), owned_from_flpdf(&value, depth + 1)?);
        }
        return Ok(OwnedObject::Stream {
            dictionary,
            data: StreamData::Owned(handle.get_raw_stream_data()?.as_ref().clone()),
        });
    }
    if let Some(entries) = handle.as_dictionary() {
        let mut dictionary = OwnedDictionary::new();
        for (key, value) in entries {
            let name = key.strip_prefix(b"/").unwrap_or(key.as_slice());
            dictionary.insert(name.to_vec(), owned_from_flpdf(&value, depth + 1)?);
        }
        return Ok(OwnedObject::Dictionary(dictionary));
    }

    Err(Error::Invalid(format!(
        "unsupported direct trailer object type {}",
        handle.type_name()?
    )))
}

/// Stable identifier of an existing indirect PDF object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ObjectId {
    number: i32,
    generation: i32,
}

impl ObjectId {
    pub const fn new(number: i32, generation: i32) -> Self {
        Self { number, generation }
    }

    pub const fn number(self) -> i32 {
        self.number
    }

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
    pub const fn index(self) -> usize {
        self.0
    }
}

/// A reference that can point either at a source object or at a newly created
/// overlay object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ObjectHandle {
    Existing(ObjectId),
    New(NewObjectId),
}

pub type OwnedDictionary = BTreeMap<Vec<u8>, OwnedObject>;

/// Stream payload used by an edited COS object.
///
/// Source-backed payloads defer touching the bytes until the writer actually
/// needs them. A transform that changes the encoded payload explicitly switches
/// to [`Self::Owned`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamData {
    Source(ObjectId),
    Owned(Vec<u8>),
}

impl StreamData {
    pub fn bytes<'a>(&'a self, source: &'a SourcePdf) -> Result<Cow<'a, [u8]>> {
        match self {
            Self::Source(id) => source.stream_data(*id),
            Self::Owned(bytes) => Ok(Cow::Borrowed(bytes)),
        }
    }

    pub const fn is_source_backed(&self) -> bool {
        matches!(self, Self::Source(_))
    }
}

/// Owned COS value used by the mutation overlay.
///
/// This is intentionally independent from Hayro's borrowed object types so an
/// optimization pass only pays allocation cost for objects it actually edits.
#[derive(Debug, Clone, PartialEq)]
pub enum OwnedObject {
    Null,
    Boolean(bool),
    Integer(i64),
    Real(f64),
    Name(Vec<u8>),
    String(Vec<u8>),
    Reference(ObjectHandle),
    Array(Vec<Self>),
    Dictionary(OwnedDictionary),
    Stream {
        dictionary: OwnedDictionary,
        data: StreamData,
    },
}

impl OwnedObject {
    pub fn as_dictionary(&self) -> Option<&OwnedDictionary> {
        match self {
            Self::Dictionary(dictionary) | Self::Stream { dictionary, .. } => Some(dictionary),
            _ => None,
        }
    }

    pub fn as_dictionary_mut(&mut self) -> Option<&mut OwnedDictionary> {
        match self {
            Self::Dictionary(dictionary) | Self::Stream { dictionary, .. } => Some(dictionary),
            _ => None,
        }
    }

    /// Collect indirect references contained in this owned COS value.
    pub fn references(&self) -> Vec<ObjectHandle> {
        let mut references = BTreeSet::new();
        collect_owned_references(self, &mut references);
        references.into_iter().collect()
    }
}

/// Mutation applied to an object that already exists in the source PDF.
#[derive(Debug, Clone, PartialEq)]
pub enum ExistingObjectChange {
    Replace(OwnedObject),
    Delete,
}

/// Copy-on-write object graph layered over an immutable [`SourcePdf`].
#[derive(Debug, Clone, Default)]
pub struct ObjectOverlay {
    existing: BTreeMap<ObjectId, ExistingObjectChange>,
    added: Vec<OwnedObject>,
}

impl ObjectOverlay {
    pub fn replace(&mut self, id: ObjectId, object: OwnedObject) {
        self.existing
            .insert(id, ExistingObjectChange::Replace(object));
    }

    pub fn delete(&mut self, id: ObjectId) {
        self.existing.insert(id, ExistingObjectChange::Delete);
    }

    /// Materialize an existing source object only when it is first edited.
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

    pub fn clear_change(&mut self, id: ObjectId) -> Option<ExistingObjectChange> {
        self.existing.remove(&id)
    }

    pub fn change(&self, id: ObjectId) -> Option<&ExistingObjectChange> {
        self.existing.get(&id)
    }

    pub fn changes(&self) -> impl Iterator<Item = (ObjectId, &ExistingObjectChange)> {
        self.existing.iter().map(|(id, change)| (*id, change))
    }

    pub fn add(&mut self, object: OwnedObject) -> NewObjectId {
        let id = NewObjectId(self.added.len());
        self.added.push(object);
        id
    }

    pub fn added(&self, id: NewObjectId) -> Option<&OwnedObject> {
        self.added.get(id.index())
    }

    pub fn added_mut(&mut self, id: NewObjectId) -> Option<&mut OwnedObject> {
        self.added.get_mut(id.index())
    }

    pub fn added_objects(&self) -> &[OwnedObject] {
        &self.added
    }

    pub fn is_empty(&self) -> bool {
        self.existing.is_empty() && self.added.is_empty()
    }
}

/// Borrowed view of one object encountered while walking the current COW graph.
pub(crate) enum CurrentObject<'a> {
    /// Object parsed lazily from the immutable Hayro source.
    Source(Object<'a>),
    /// Object already materialized in the overlay.
    Owned(&'a OwnedObject),
}

fn contains_indirect_reference(value: &OwnedObject) -> bool {
    match value {
        OwnedObject::Reference(_) => true,
        OwnedObject::Array(values) => values.iter().any(contains_indirect_reference),
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            dictionary.values().any(contains_indirect_reference)
        }
        _ => false,
    }
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
    streams: BTreeMap<ObjectId, Vec<u8>>,
}

/// A lazily parsed source document plus the objects changed by optimization
/// passes. This is the target architecture for the Hayro migration.
pub struct EditDocument {
    source: SourcePdf,
    trailer: OwnedDictionary,
    overlay: ObjectOverlay,
    decoded_content_stream_cache: RefCell<DecodedContentCache>,
}

impl EditDocument {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let source = SourcePdf::from_bytes(bytes)?;
        let trailer = source.preserved_trailer();
        Ok(Self {
            source,
            trailer,
            overlay: ObjectOverlay::default(),
            decoded_content_stream_cache: RefCell::new(DecodedContentCache::default()),
        })
    }

    pub fn source(&self) -> &SourcePdf {
        &self.source
    }

    pub fn trailer(&self) -> &OwnedDictionary {
        &self.trailer
    }

    pub fn trailer_mut(&mut self) -> &mut OwnedDictionary {
        &mut self.trailer
    }

    pub fn overlay(&self) -> &ObjectOverlay {
        &self.overlay
    }

    pub fn overlay_mut(&mut self) -> &mut ObjectOverlay {
        &mut self.overlay
    }

    pub fn edit_object(&mut self, id: ObjectId) -> Result<&mut OwnedObject> {
        self.overlay.edit(&self.source, id)
    }

    /// Materialize the current value of an object handle from either the source
    /// graph or the COW overlay without mutating the document.
    pub(crate) fn detached_flpdf_object(&self, value: &OwnedObject) -> Result<FlObjectHandle> {
        self.owned_to_flpdf_detached(value, 0)
    }

    fn owned_to_flpdf_detached(&self, value: &OwnedObject, depth: usize) -> Result<FlObjectHandle> {
        if depth > 256 {
            return Err(Error::Invalid(
                "detached COS conversion nesting exceeds supported depth".to_owned(),
            ));
        }
        match value {
            OwnedObject::Reference(handle) => {
                let Some(value) = self.current_owned_object(*handle)? else {
                    return Ok(FlObjectHandle::null());
                };
                self.owned_to_flpdf_detached(&value, depth + 1)
            }
            OwnedObject::Null => Ok(FlObjectHandle::null()),
            OwnedObject::Boolean(value) => Ok(FlObjectHandle::boolean(*value)),
            OwnedObject::Integer(value) => Ok(FlObjectHandle::integer(*value)),
            OwnedObject::Real(value) => Ok(FlObjectHandle::real(*value)),
            OwnedObject::Name(value) => Ok(FlObjectHandle::name(value.clone())),
            OwnedObject::String(value) => Ok(FlObjectHandle::string(value.clone())),
            OwnedObject::Array(values) => Ok(FlObjectHandle::array(
                values
                    .iter()
                    .map(|value| self.owned_to_flpdf_detached(value, depth + 1))
                    .collect::<Result<Vec<_>>>()?,
            )),
            OwnedObject::Dictionary(dictionary) => Ok(FlObjectHandle::dictionary(
                dictionary
                    .iter()
                    .map(|(key, value)| {
                        Ok((
                            [b"/".as_slice(), key.as_slice()].concat(),
                            self.owned_to_flpdf_detached(value, depth + 1)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?,
            )),
            OwnedObject::Stream { dictionary, data } => {
                let bytes = data.bytes(self.source())?.into_owned();
                let mut entries = dictionary
                    .iter()
                    .filter(|(key, _)| key.as_slice() != b"Length")
                    .map(|(key, value)| {
                        Ok((
                            [b"/".as_slice(), key.as_slice()].concat(),
                            self.owned_to_flpdf_detached(value, depth + 1)?,
                        ))
                    })
                    .collect::<Result<Vec<_>>>()?;
                entries.push((
                    b"/Length".to_vec(),
                    FlObjectHandle::integer(bytes.len() as i64),
                ));
                let dictionary = FlObjectHandle::dictionary(entries);
                Ok(FlObjectHandle::stream(dictionary, Rc::new(bytes)))
            }
        }
    }

    /// Decode a current stream value through flpdf's standalone filter codecs.
    ///
    /// Only filter-related dictionary entries are materialized into the
    /// detached helper object. Decoding does not need resources, metadata, or
    /// other semantic stream keys, and avoiding them prevents unrelated COS
    /// cycles/deep graphs from being copied merely to inflate a stream.
    pub(crate) fn decoded_owned_stream_data(
        &self,
        stream: &OwnedObject,
        level: DecodeLevel,
    ) -> Result<Vec<u8>> {
        let OwnedObject::Stream { dictionary, data } = stream else {
            return Err(Error::Invalid("object is not a stream".to_owned()));
        };
        let bytes = data.bytes(self.source())?.into_owned();
        let mut entries = Vec::new();
        for key in [
            b"Filter".as_slice(),
            b"DecodeParms".as_slice(),
            b"F".as_slice(),
            b"FFilter".as_slice(),
            b"FDecodeParms".as_slice(),
        ] {
            let Some(value) = dictionary.get(key) else {
                continue;
            };
            entries.push((
                [b"/".as_slice(), key].concat(),
                self.owned_to_flpdf_detached(value, 0)?,
            ));
        }
        entries.push((
            b"/Length".to_vec(),
            FlObjectHandle::integer(bytes.len() as i64),
        ));
        let handle = FlObjectHandle::stream(FlObjectHandle::dictionary(entries), Rc::new(bytes));
        Ok(handle.get_stream_data(level)?.as_ref().clone())
    }

    /// Decode a current indirect stream through the standalone filter-codec bridge.
    pub(crate) fn decoded_stream_data(
        &self,
        handle: ObjectHandle,
        level: DecodeLevel,
    ) -> Result<Vec<u8>> {
        let Some(stream) = self.current_owned_object(handle)? else {
            return Err(Error::Invalid(
                "stream reference resolves to null".to_owned(),
            ));
        };
        self.decoded_owned_stream_data(&stream, level)
    }

    /// Decode page/Form content while memoizing untouched source streams.
    ///
    /// The cache is intentionally content-specific rather than attached to the
    /// generic stream decoder so large image/font payloads are never retained
    /// merely because an optimization pass inspected them. Overlay-edited or
    /// newly created streams bypass the cache, so mutations need no invalidation.
    pub(crate) fn decoded_content_stream_data(&self, handle: ObjectHandle) -> Result<Vec<u8>> {
        if let ObjectHandle::Existing(id) = handle
            && self.overlay.change(id).is_none()
        {
            let Some(stream) = self.current_owned_object(handle)? else {
                return Err(Error::Invalid(
                    "stream reference resolves to null".to_owned(),
                ));
            };
            if stream_filter_configuration_is_direct(&stream) {
                if let Some(decoded) = self.decoded_content_stream_cache.borrow().streams.get(&id) {
                    return Ok(decoded.clone());
                }
                let decoded = self.decoded_owned_stream_data(&stream, DecodeLevel::Specialized)?;
                if decoded.len() <= MAX_DECODED_CONTENT_STREAM_BYTES {
                    let mut cache = self.decoded_content_stream_cache.borrow_mut();
                    if cache.bytes.saturating_add(decoded.len()) <= MAX_DECODED_CONTENT_CACHE_BYTES
                    {
                        cache.bytes = cache.bytes.saturating_add(decoded.len());
                        cache.streams.insert(id, decoded.clone());
                    }
                }
                return Ok(decoded);
            }
        }
        self.decoded_stream_data(handle, DecodeLevel::Specialized)
    }

    pub(crate) fn current_owned_object(&self, handle: ObjectHandle) -> Result<Option<OwnedObject>> {
        match handle {
            ObjectHandle::Existing(id) => match self.overlay.change(id) {
                Some(ExistingObjectChange::Replace(object)) => Ok(Some(object.clone())),
                Some(ExistingObjectChange::Delete) => Err(Error::DeletedReferencedObject {
                    number: id.number,
                    generation: id.generation,
                }),
                None => match self.source.materialize(id) {
                    Ok(object) => Ok(Some(object)),
                    Err(Error::MissingSourceObject { .. }) => Ok(None),
                    Err(error) => Err(error),
                },
            },
            ObjectHandle::New(id) => self
                .overlay
                .added(id)
                .cloned()
                .ok_or_else(|| Error::MissingNewObject { index: id.index() })
                .map(Some),
        }
    }

    /// Resolve a chain of indirect references against the current COW graph.
    /// Cycles and missing source objects resolve to `None`, matching PDF null-like
    /// behavior used by the migration passes.
    pub(crate) fn resolve_owned_value(&self, value: &OwnedObject) -> Result<Option<OwnedObject>> {
        let mut value = value.clone();
        let mut seen = BTreeSet::new();
        loop {
            let OwnedObject::Reference(handle) = value else {
                return Ok(Some(value));
            };
            if !seen.insert(handle) {
                return Ok(None);
            }
            let Some(next) = self.current_owned_object(handle)? else {
                return Ok(None);
            };
            value = next;
        }
    }

    /// Write the current COW graph as a compact fresh PDF using classic xref output.
    /// Production optimization normally selects writer options through `Config`.
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
        let Some(catalog) = self.current_owned_object(catalog_handle)? else {
            return Ok(Vec::new());
        };
        let Some(catalog) = catalog.as_dictionary() else {
            return Ok(Vec::new());
        };
        let Some(root) = catalog.get(b"Pages".as_slice()).cloned() else {
            return Ok(Vec::new());
        };

        let mut pages = Vec::new();
        let mut seen = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(value) = pending.pop() {
            let (handle, object) = match value {
                OwnedObject::Reference(handle) => {
                    if !seen.insert(handle) {
                        continue;
                    }
                    let Some(object) = self.current_owned_object(handle)? else {
                        continue;
                    };
                    (Some(handle), object)
                }
                object => (None, object),
            };
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };

            let kind = match dictionary.get(b"Type".as_slice()) {
                Some(value) => self.resolve_owned_value(value)?,
                None => None,
            };
            if matches!(kind, Some(OwnedObject::Name(name)) if name == b"Page") {
                if let Some(handle) = handle {
                    pages.push(handle);
                }
                continue;
            }

            let Some(kids) = dictionary.get(b"Kids".as_slice()) else {
                // Be lenient with damaged leaf page dictionaries that omit
                // `/Type /Page`, matching PDF readers' common recovery path.
                if let Some(handle) = handle
                    && dictionary.contains_key(b"Parent".as_slice())
                {
                    pages.push(handle);
                }
                continue;
            };
            let Some(OwnedObject::Array(kids)) = self.resolve_owned_value(kids)? else {
                continue;
            };
            for kid in kids.into_iter().rev() {
                pending.push(kid);
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

    /// Resolve one inheritable page-tree value from a current page dictionary.
    pub(crate) fn inherited_page_value(
        &self,
        page: ObjectHandle,
        key: &[u8],
    ) -> Result<Option<OwnedObject>> {
        let Some(mut object) = self.current_owned_object(page)? else {
            return Ok(None);
        };
        let mut seen = BTreeSet::new();
        for _ in 0..=100 {
            let Some(dictionary) = object.as_dictionary() else {
                return Ok(None);
            };
            if let Some(value) = dictionary.get(key) {
                return Ok(Some(value.clone()));
            }
            let Some(OwnedObject::Reference(parent)) = dictionary.get(b"Parent".as_slice()) else {
                return Ok(None);
            };
            if !seen.insert(*parent) {
                return Ok(None);
            }
            let Some(parent_object) = self.current_owned_object(*parent)? else {
                return Ok(None);
            };
            object = parent_object;
        }
        Ok(None)
    }

    /// Collect all objects reachable from the document catalog after applying
    /// overlay replacements/deletions.
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

    pub(crate) fn reachable_output_objects(&self) -> Result<Vec<ObjectHandle>> {
        self.reachable_from(self.output_roots())
    }

    /// Visit every object reachable from the current output roots exactly once.
    ///
    /// Untouched source objects stay as borrowed Hayro values, while overlay
    /// replacements and newly added objects are passed by reference. This keeps
    /// inspection passes lazy and gives all of them the same missing/deleted
    /// object semantics instead of open-coding the COW traversal repeatedly.
    pub(crate) fn walk_output_objects<F>(&self, mut visit: F) -> Result<()>
    where
        F: for<'a> FnMut(ObjectHandle, CurrentObject<'a>) -> Result<()>,
    {
        let mut seen = BTreeSet::new();
        let mut pending = self.output_roots();

        while let Some(handle) = pending.pop() {
            if !seen.insert(handle) {
                continue;
            }

            let references = match handle {
                ObjectHandle::Existing(id) => match self.overlay.change(id) {
                    Some(ExistingObjectChange::Replace(object)) => {
                        visit(handle, CurrentObject::Owned(object))?;
                        object.references()
                    }
                    Some(ExistingObjectChange::Delete) => {
                        return Err(Error::DeletedReferencedObject {
                            number: id.number,
                            generation: id.generation,
                        });
                    }
                    None => {
                        let (object, references) = match self.source.object_with_references(id) {
                            Ok(value) => value,
                            Err(Error::MissingSourceObject { .. }) => continue,
                            Err(error) => return Err(error),
                        };
                        visit(handle, CurrentObject::Source(object))?;
                        references.into_iter().map(ObjectHandle::Existing).collect()
                    }
                },
                ObjectHandle::New(id) => {
                    let object = self
                        .overlay
                        .added(id)
                        .ok_or_else(|| Error::MissingNewObject { index: id.index() })?;
                    visit(handle, CurrentObject::Owned(object))?;
                    object.references()
                }
            };

            pending.extend(
                references
                    .into_iter()
                    .filter(|reference| !seen.contains(reference)),
            );
        }
        Ok(())
    }

    /// Collect all objects reachable from an explicit root set.
    pub fn reachable_from(
        &self,
        roots: impl IntoIterator<Item = ObjectHandle>,
    ) -> Result<Vec<ObjectHandle>> {
        let mut seen = BTreeSet::new();
        let mut pending = roots.into_iter().collect::<Vec<_>>();

        while let Some(handle) = pending.pop() {
            if seen.contains(&handle) {
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

            seen.insert(handle);
            for reference in references {
                if !seen.contains(&reference) {
                    pending.push(reference);
                }
            }
        }

        Ok(seen.into_iter().collect())
    }

    fn references_for_handle(&self, handle: ObjectHandle) -> Result<Vec<ObjectHandle>> {
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
    match object {
        Object::Dict(dictionary) => {
            for (_, value) in dictionary.entries() {
                collect_hayro_maybe_ref(value, references);
            }
        }
        Object::Array(array) => {
            for value in array.raw_iter() {
                collect_hayro_maybe_ref(value, references);
            }
        }
        Object::Stream(stream) => {
            for (name, value) in stream.dict().entries() {
                if name.as_ref() == b"Length" {
                    continue;
                }
                collect_hayro_maybe_ref(value, references);
            }
        }
        Object::Null(_)
        | Object::Boolean(_)
        | Object::Number(_)
        | Object::String(_)
        | Object::Name(_) => {}
    }
}

fn collect_hayro_maybe_ref(value: MaybeRef<Object<'_>>, references: &mut BTreeSet<ObjectId>) {
    match value {
        MaybeRef::Ref(reference) => {
            references.insert(reference.into());
        }
        MaybeRef::NotRef(object) => collect_hayro_references(&object, references),
    }
}

fn collect_owned_references(object: &OwnedObject, references: &mut BTreeSet<ObjectHandle>) {
    match object {
        OwnedObject::Reference(reference) => {
            references.insert(*reference);
        }
        OwnedObject::Array(values) => {
            for value in values {
                collect_owned_references(value, references);
            }
        }
        OwnedObject::Dictionary(dictionary) => {
            for value in dictionary.values() {
                collect_owned_references(value, references);
            }
        }
        OwnedObject::Stream { dictionary, .. } => {
            for (name, value) in dictionary {
                if name.as_slice() != b"Length" {
                    collect_owned_references(value, references);
                }
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

fn owned_from_maybe_ref(value: MaybeRef<Object<'_>>) -> OwnedObject {
    match value {
        MaybeRef::Ref(reference) => {
            OwnedObject::Reference(ObjectHandle::Existing(reference.into()))
        }
        MaybeRef::NotRef(object) => owned_from_hayro(object, None),
    }
}

fn owned_dictionary(dictionary: &Dict<'_>) -> OwnedDictionary {
    dictionary
        .entries()
        .map(|(name, value)| (name.as_ref().to_vec(), owned_from_maybe_ref(value)))
        .collect()
}

fn owned_stream_dictionary(dictionary: &Dict<'_>) -> OwnedDictionary {
    dictionary
        .entries()
        .filter(|(name, _)| name.as_ref() != b"Length")
        .map(|(name, value)| (name.as_ref().to_vec(), owned_from_maybe_ref(value)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn invalid_source_is_rejected() {
        assert!(SourcePdf::from_bytes(b"not a pdf".to_vec()).is_err());
    }

    #[test]
    fn materialized_stream_keeps_source_payload_lazy() {
        let source = match SourcePdf::from_bytes(sample_pdf()) {
            Ok(source) => source,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let stream_id = ObjectId::new(4, 0);
        let object = match source.materialize(stream_id) {
            Ok(object) => object,
            Err(error) => panic!("stream should materialize: {error}"),
        };

        let (dictionary, data) = match object {
            OwnedObject::Stream { dictionary, data } => (dictionary, data),
            other => panic!("expected stream, got {other:?}"),
        };
        assert!(!dictionary.contains_key(b"Length".as_slice()));
        assert_eq!(data, StreamData::Source(stream_id));
        let bytes = match data.bytes(&source) {
            Ok(bytes) => bytes,
            Err(error) => panic!("stream bytes should resolve: {error}"),
        };
        assert_eq!(bytes.as_ref(), b"q Q");
    }

    #[test]
    fn decoded_source_stream_cache_is_bypassed_after_edit() {
        let mut document = match EditDocument::from_bytes(sample_pdf()) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let stream_id = ObjectId::new(4, 0);
        let handle = ObjectHandle::Existing(stream_id);
        let first = match document.decoded_content_stream_data(handle) {
            Ok(bytes) => bytes,
            Err(error) => panic!("source stream should decode: {error}"),
        };
        assert_eq!(first, b"q Q");
        let stream = match document.edit_object(stream_id) {
            Ok(stream) => stream,
            Err(error) => panic!("source stream should be editable: {error}"),
        };
        let OwnedObject::Stream { data, .. } = stream else {
            panic!("fixture object should be a stream");
        };
        *data = StreamData::Owned(b"BT ET".to_vec());
        let second = match document.decoded_content_stream_data(handle) {
            Ok(bytes) => bytes,
            Err(error) => panic!("edited stream should decode: {error}"),
        };
        assert_eq!(second, b"BT ET");
    }

    #[test]
    fn first_edit_materializes_only_target_object() {
        let mut document = match EditDocument::from_bytes(sample_pdf()) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        assert!(document.overlay().is_empty());

        let catalog_id = document.source().catalog_id();
        let catalog = match document.edit_object(catalog_id) {
            Ok(object) => object,
            Err(error) => panic!("catalog should materialize: {error}"),
        };
        let dictionary = match catalog.as_dictionary_mut() {
            Some(dictionary) => dictionary,
            None => panic!("catalog should be a dictionary"),
        };
        dictionary.insert(b"Lang".to_vec(), OwnedObject::String(b"en".to_vec()));

        assert_eq!(document.overlay().changes().count(), 1);
    }

    #[test]
    fn reachability_walks_source_without_materializing_overlay() {
        let document = match EditDocument::from_bytes(sample_pdf()) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let reachable = match document.reachable_objects() {
            Ok(reachable) => reachable,
            Err(error) => panic!("source graph should be reachable: {error}"),
        };

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
    }

    #[test]
    fn reachability_follows_new_overlay_references() {
        let mut document = match EditDocument::from_bytes(sample_pdf()) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        let added = document
            .overlay_mut()
            .add(OwnedObject::Dictionary(OwnedDictionary::new()));
        let catalog_id = document.source().catalog_id();
        let catalog = match document.edit_object(catalog_id) {
            Ok(object) => object,
            Err(error) => panic!("catalog should materialize: {error}"),
        };
        let dictionary = match catalog.as_dictionary_mut() {
            Some(dictionary) => dictionary,
            None => panic!("catalog should be a dictionary"),
        };
        dictionary.insert(
            b"PieceInfo".to_vec(),
            OwnedObject::Reference(ObjectHandle::New(added)),
        );

        let reachable = match document.reachable_objects() {
            Ok(reachable) => reachable,
            Err(error) => panic!("overlay graph should be reachable: {error}"),
        };
        assert!(reachable.contains(&ObjectHandle::New(added)));
    }

    #[test]
    fn reachability_rejects_dangling_deleted_reference() {
        let mut document = match EditDocument::from_bytes(sample_pdf()) {
            Ok(document) => document,
            Err(error) => panic!("sample PDF should parse: {error}"),
        };
        document.overlay_mut().delete(ObjectId::new(4, 0));

        match document.reachable_objects() {
            Err(Error::DeletedReferencedObject {
                number: 4,
                generation: 0,
            }) => {}
            other => panic!("expected dangling-reference error, got {other:?}"),
        }
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
