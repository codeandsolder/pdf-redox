use crate::{Error, Result, SourceLoadError};
use hayro_syntax::{
    Pdf, PdfVersion,
    object::{Dict, MaybeRef, Object, ObjectIdentifier, Stream},
};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, btree_map::Entry},
    sync::Arc,
};

/// Immutable, lazily parsed source PDF backed by Hayro.
///
/// Hayro keeps the original bytes alive and parses objects on demand. Mutations
/// belong in [`ObjectOverlay`] rather than in this source representation.
pub struct SourcePdf {
    pdf: Pdf,
}

impl SourcePdf {
    /// Parse owned PDF bytes without making another full-document copy.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_shared(Arc::new(bytes))
    }

    /// Parse PDF bytes already held in shared storage.
    pub fn from_shared(bytes: Arc<Vec<u8>>) -> Result<Self> {
        let pdf = Pdf::new(bytes).map_err(SourceLoadError::from)?;
        Ok(Self { pdf })
    }

    /// Original source bytes, unchanged.
    pub fn bytes(&self) -> &[u8] {
        self.pdf.data().as_ref()
    }

    /// Number of objects indexed by the source cross-reference graph.
    pub fn object_count(&self) -> usize {
        self.pdf.len()
    }

    /// Number of pages in the source page tree.
    pub fn page_count(&self) -> usize {
        self.pdf.pages().len()
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

    pub(crate) fn contains_object(&self, id: ObjectId) -> bool {
        self.pdf.xref().get::<Object<'_>>(id.into()).is_some()
    }

    /// Collect indirect references reachable directly from one source object.
    ///
    /// This walks only the borrowed COS structure of that object. Referenced
    /// objects are not resolved or materialized, and stream payload bytes are
    /// never touched.
    pub fn references(&self, id: ObjectId) -> Result<Vec<ObjectId>> {
        let object =
            self.pdf
                .xref()
                .get::<Object<'_>>(id.into())
                .ok_or(Error::MissingSourceObject {
                    number: id.number,
                    generation: id.generation,
                })?;
        let mut references = BTreeSet::new();
        collect_hayro_references(&object, &mut references);
        Ok(references.into_iter().collect())
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
    Array(Vec<OwnedObject>),
    Dictionary(OwnedDictionary),
    Stream {
        dictionary: OwnedDictionary,
        data: StreamData,
    },
}

impl OwnedObject {
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

    pub fn added_objects(&self) -> &[OwnedObject] {
        &self.added
    }

    pub fn is_empty(&self) -> bool {
        self.existing.is_empty() && self.added.is_empty()
    }
}

/// A lazily parsed source document plus the objects changed by optimization
/// passes. This is the target architecture for the Hayro migration.
pub struct EditDocument {
    source: SourcePdf,
    overlay: ObjectOverlay,
}

impl EditDocument {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Ok(Self {
            source: SourcePdf::from_bytes(bytes)?,
            overlay: ObjectOverlay::default(),
        })
    }

    pub fn source(&self) -> &SourcePdf {
        &self.source
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

    /// Write the current Hayro/COW graph as a compact fresh PDF.
    ///
    /// This is an experimental migration API and is not used by [`crate::optimize_pdf`]
    /// yet. In particular, trailer-only state that Hayro does not currently
    /// expose publicly, including `/Info` and `/ID`, is not preserved.
    pub fn write_compact_experimental(&self) -> Result<Vec<u8>> {
        crate::writer::write_pdf(self)
    }

    /// Collect all objects reachable from the document catalog after applying
    /// overlay replacements/deletions.
    pub fn reachable_objects(&self) -> Result<Vec<ObjectHandle>> {
        self.reachable_from([ObjectHandle::Existing(self.source.catalog_id())])
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
                .ok_or(Error::MissingNewObject { index: id.index() }),
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
