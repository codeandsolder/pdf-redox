use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    source::{CurrentChildKey, CurrentChildren, CurrentValue},
};
use hayro_syntax::object::{
    Array as HayroArray, Dict as HayroDict, MaybeRef as HayroMaybeRef, Object as HayroObject,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DirectPathStep {
    DictKey(Vec<u8>),
    ArrayIndex(usize),
}

pub fn object_at_direct_path_mut<'a>(
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

pub fn object_at_direct_path<'a>(
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

/// Project a current value onto a direct child without resolving indirect
/// references or materializing the enclosing object.
pub fn value_at_direct_path<'a>(
    mut value: CurrentValue<'a>,
    path: &[DirectPathStep],
) -> Option<CurrentValue<'a>> {
    for step in path {
        value = match step {
            DirectPathStep::DictKey(key) => value.dictionary_entry(key)?,
            DirectPathStep::ArrayIndex(index) => value.array_items()?.nth(*index)?,
        };
    }
    Some(value)
}

#[derive(Clone, Copy)]
pub enum DirectDictionaryView<'a, 'pdf> {
    Source(&'a HayroDict<'pdf>),
    Owned(&'a OwnedDictionary),
}

impl<'a, 'pdf: 'a> DirectDictionaryView<'a, 'pdf> {
    pub fn raw_value(self, key: &[u8]) -> Option<CurrentValue<'a>> {
        match self {
            Self::Source(dictionary) => dictionary
                .get_raw(key)
                .map(|value| CurrentValue::Source(value, None)),
            Self::Owned(dictionary) => dictionary.get(key).map(CurrentValue::Owned),
        }
    }

    pub fn reference(self, key: &[u8]) -> Option<CowObjectHandle> {
        self.raw_value(key).and_then(|value| value.reference())
    }

    pub fn resolved_name(self, document: &EditDocument, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let Some(value) = self.raw_value(key) else {
            return Ok(None);
        };
        Ok(document
            .resolve_current_value(value)?
            .and_then(|value| value.name().map(<[u8]>::to_vec)))
    }

    pub fn resolved_name_is(
        self,
        document: &EditDocument,
        key: &[u8],
        expected: &[u8],
    ) -> Result<bool> {
        let Some(value) = self.raw_value(key) else {
            return Ok(false);
        };
        Ok(document
            .resolve_current_value(value)?
            .is_some_and(|value| value.is_name(expected)))
    }

    pub fn direct_dictionary_reference(
        self,
        dictionary_key: &[u8],
        reference_key: &[u8],
    ) -> Option<CowObjectHandle> {
        let dictionary = self.raw_value(dictionary_key)?;
        if !dictionary.is_dictionary() {
            return None;
        }
        dictionary.dictionary_entry(reference_key)?.reference()
    }

    pub fn has_direct_dictionary(self, key: &[u8]) -> bool {
        self.raw_value(key)
            .is_some_and(|value| value.is_dictionary())
    }
}

#[derive(Clone, Copy)]
pub enum DirectArrayView<'a, 'pdf> {
    Source(&'a HayroArray<'pdf>),
    Owned(&'a [OwnedObject]),
}

impl<'a, 'pdf: 'a> DirectArrayView<'a, 'pdf> {
    fn raw_value(self, index: usize) -> Option<CurrentValue<'a>> {
        match self {
            Self::Source(array) => array
                .raw_iter()
                .nth(index)
                .map(|value| CurrentValue::Source(value, None)),
            Self::Owned(values) => values.get(index).map(CurrentValue::Owned),
        }
    }

    pub fn resolved_name_is(
        self,
        document: &EditDocument,
        index: usize,
        expected: &[u8],
    ) -> Result<bool> {
        let Some(value) = self.raw_value(index) else {
            return Ok(false);
        };
        Ok(document
            .resolve_current_value(value)?
            .is_some_and(|value| value.is_name(expected)))
    }

    /// Match a directly stored name without resolving indirect references.
    pub fn direct_name_matches(self, index: usize, predicate: impl FnOnce(&[u8]) -> bool) -> bool {
        self.raw_value(index)
            .is_some_and(|value| value.name().is_some_and(predicate))
    }

    pub fn reference(self, index: usize) -> Option<CowObjectHandle> {
        self.raw_value(index).and_then(|value| value.reference())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectWalkControl {
    Descend,
    SkipChildren,
}

pub trait DirectObjectVisitor {
    fn visit_dictionary(
        &mut self,
        _document: &EditDocument,
        _root: CowObjectHandle,
        _path: &[DirectPathStep],
        _dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        Ok(DirectWalkControl::Descend)
    }

    fn visit_array(
        &mut self,
        _document: &EditDocument,
        _root: CowObjectHandle,
        _path: &[DirectPathStep],
        _array: DirectArrayView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        Ok(DirectWalkControl::Descend)
    }

    fn visit_reference(
        &mut self,
        _root: CowObjectHandle,
        _path: &[DirectPathStep],
        _target: CowObjectHandle,
    ) -> Result<()> {
        Ok(())
    }

    // The owned stream representation omits source /Length, which the
    // writer generates. Visitors planning edits to that representation
    // can opt out of visiting this source-only entry.
    fn visit_source_stream_length(&self) -> bool {
        true
    }

    fn descend_dictionary_value(&self, _key: &[u8]) -> bool {
        true
    }
}

fn visit_current_object<'a>(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &CurrentValue<'a>,
    path: &[DirectPathStep],
    visitor: &mut impl DirectObjectVisitor,
) -> Result<Option<CurrentChildren<'a>>> {
    if let Some(target) = object.reference() {
        visitor.visit_reference(root, path, target)?;
        return Ok(None);
    }
    let control = match object {
        CurrentValue::Source(HayroMaybeRef::NotRef(HayroObject::Dict(dictionary)), _) => visitor
            .visit_dictionary(
                document,
                root,
                path,
                DirectDictionaryView::Source(dictionary),
            )?,
        CurrentValue::Source(HayroMaybeRef::NotRef(HayroObject::Stream(stream)), _) => visitor
            .visit_dictionary(
                document,
                root,
                path,
                DirectDictionaryView::Source(stream.dict()),
            )?,
        CurrentValue::Owned(
            OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. },
        ) => visitor.visit_dictionary(
            document,
            root,
            path,
            DirectDictionaryView::Owned(dictionary),
        )?,
        CurrentValue::Source(HayroMaybeRef::NotRef(HayroObject::Array(array)), _) => {
            visitor.visit_array(document, root, path, DirectArrayView::Source(array))?
        }
        CurrentValue::Owned(OwnedObject::Array(array)) => {
            visitor.visit_array(document, root, path, DirectArrayView::Owned(array))?
        }
        _ => return Ok(None),
    };
    let skip_length = matches!(
        object,
        CurrentValue::Source(HayroMaybeRef::NotRef(HayroObject::Stream(_)), _)
    ) && !visitor.visit_source_stream_length();
    Ok((control == DirectWalkControl::Descend)
        .then(|| object.children(skip_length))
        .flatten())
}

pub fn walk_direct_object(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &CurrentValue<'_>,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    let mut path = Vec::new();
    let Some(children) = visit_current_object(document, root, object, &path, visitor)? else {
        return Ok(());
    };
    let mut frames = vec![children];
    while let Some(frame) = frames.last_mut() {
        if let Some((key, child)) = frame.next() {
            let step = match key {
                CurrentChildKey::Dictionary(key) => {
                    if child.reference().is_none()
                        && !visitor.descend_dictionary_value(key.as_ref())
                    {
                        continue;
                    }
                    DirectPathStep::DictKey(key.as_ref().to_vec())
                }
                CurrentChildKey::Array(index) => DirectPathStep::ArrayIndex(index),
            };
            path.push(step);
            if let Some(children) = visit_current_object(document, root, &child, &path, visitor)? {
                frames.push(children);
            } else {
                path.pop();
            }
        } else {
            frames.pop();
            path.pop();
        }
    }
    Ok(())
}

pub fn walk_direct_output_objects(
    document: &EditDocument,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    document
        .walk_output_objects(|root, object| walk_direct_object(document, root, &object, visitor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, ObjectId, test_support::ClassicPdfBuilder};

    #[derive(Debug, PartialEq, Eq)]
    enum Visit {
        Dictionary(Vec<DirectPathStep>),
        Array(Vec<DirectPathStep>),
        Reference(Vec<DirectPathStep>, CowObjectHandle),
    }

    #[derive(Default)]
    struct Trace {
        events: Vec<Visit>,
        include_source_length: bool,
    }

    impl DirectObjectVisitor for Trace {
        fn visit_dictionary(
            &mut self,
            _: &EditDocument,
            _: CowObjectHandle,
            path: &[DirectPathStep],
            _: DirectDictionaryView<'_, '_>,
        ) -> Result<DirectWalkControl> {
            self.events.push(Visit::Dictionary(path.to_vec()));
            if matches!(path.last(), Some(DirectPathStep::DictKey(key)) if key == b"Stop") {
                Ok(DirectWalkControl::SkipChildren)
            } else {
                Ok(DirectWalkControl::Descend)
            }
        }

        fn visit_array(
            &mut self,
            _: &EditDocument,
            _: CowObjectHandle,
            path: &[DirectPathStep],
            _: DirectArrayView<'_, '_>,
        ) -> Result<DirectWalkControl> {
            self.events.push(Visit::Array(path.to_vec()));
            Ok(DirectWalkControl::Descend)
        }

        fn visit_reference(
            &mut self,
            _: CowObjectHandle,
            path: &[DirectPathStep],
            target: CowObjectHandle,
        ) -> Result<()> {
            self.events.push(Visit::Reference(path.to_vec(), target));
            Ok(())
        }

        fn visit_source_stream_length(&self) -> bool {
            self.include_source_length
        }

        fn descend_dictionary_value(&self, key: &[u8]) -> bool {
            key != b"Ignore"
        }
    }

    #[test]
    fn current_traversal_preserves_paths_filters_and_source_length_policy() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>")?;
        pdf.object(4, b"<< /Length 5 0 R /Z [<< /Length 7 0 R /Ignore [8 0 R] >> 6 0 R] /A << /Stop << /Ref 9 0 R >> /Ignore 6 0 R >> >>
stream
q Q
endstream")?;
        pdf.object(5, b"3")?;
        for id in 6..=9 {
            pdf.object(id, b"null")?;
        }
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let id = ObjectId::new(4, 0);
        let handle = CowObjectHandle::Existing(id);
        let Some(source) = document.current_value(handle)? else {
            return Err(Error::Invalid("missing source stream fixture".to_owned()));
        };
        let owned = document.source().materialize(id)?;
        let mut source_trace = Trace::default();
        walk_direct_object(&document, handle, &source, &mut source_trace)?;
        let mut owned_trace = Trace::default();
        walk_direct_object(
            &document,
            handle,
            &CurrentValue::Owned(&owned),
            &mut owned_trace,
        )?;
        let key = |key: &[u8]| DirectPathStep::DictKey(key.to_vec());
        let reference = |id| CowObjectHandle::Existing(ObjectId::new(id, 0));
        let expected = vec![
            Visit::Dictionary(vec![]),
            Visit::Dictionary(vec![key(b"A")]),
            Visit::Reference(vec![key(b"A"), key(b"Ignore")], reference(6)),
            Visit::Dictionary(vec![key(b"A"), key(b"Stop")]),
            Visit::Array(vec![key(b"Z")]),
            Visit::Dictionary(vec![key(b"Z"), DirectPathStep::ArrayIndex(0)]),
            Visit::Reference(
                vec![key(b"Z"), DirectPathStep::ArrayIndex(0), key(b"Length")],
                reference(7),
            ),
            Visit::Reference(vec![key(b"Z"), DirectPathStep::ArrayIndex(1)], reference(6)),
        ];
        assert_eq!(source_trace.events, expected);
        assert_eq!(source_trace.events, owned_trace.events);
        let mut inclusive = Trace {
            include_source_length: true,
            ..Trace::default()
        };
        let Some(source) = document.current_value(handle)? else {
            return Err(Error::Invalid("missing source stream fixture".to_owned()));
        };
        walk_direct_object(&document, handle, &source, &mut inclusive)?;
        assert_eq!(
            inclusive.events.remove(4),
            Visit::Reference(vec![key(b"Length")], reference(5))
        );
        assert_eq!(inclusive.events, expected);
        assert_eq!(
            document.source().references(id)?,
            [6, 7, 8, 9].map(|id| ObjectId::new(id, 0))
        );
        assert_eq!(owned.references(), [6, 7, 8, 9].map(reference));
        Ok(())
    }

    #[test]
    fn direct_views_resolve_overlay_names_without_changing_raw_reference_semantics() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /Mode 4 0 R /Array [4 0 R /Fit] >>",
        )?;
        pdf.object(4, b"/Fit")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        *document.edit_handle(CowObjectHandle::Existing(ObjectId::new(4, 0)))? =
            OwnedObject::Name(b"XYZ".to_vec());
        let id = ObjectId::new(3, 0);
        let HayroObject::Dict(dictionary) = document.source().object(id)? else {
            return Err(Error::Invalid("missing fixture dictionary".to_owned()));
        };
        let view = DirectDictionaryView::Source(&dictionary);
        assert!(view.resolved_name_is(&document, b"Mode", b"XYZ")?);
        assert_eq!(
            view.reference(b"Mode"),
            Some(CowObjectHandle::Existing(ObjectId::new(4, 0)))
        );
        let Some(HayroMaybeRef::NotRef(HayroObject::Array(array))) = dictionary.get_raw(b"Array")
        else {
            return Err(Error::Invalid("missing fixture array".to_owned()));
        };
        let view = DirectArrayView::Source(&array);
        assert!(view.resolved_name_is(&document, 0, b"XYZ")?);
        assert!(!view.direct_name_matches(0, |name| name == b"XYZ"));
        assert!(view.direct_name_matches(1, |name| name == b"Fit"));
        let object = document.source().materialize(id)?;
        let Some(dictionary) = object.as_dictionary() else {
            return Err(Error::Invalid("missing owned dictionary".to_owned()));
        };
        assert!(
            DirectDictionaryView::Owned(dictionary).resolved_name_is(&document, b"Mode", b"XYZ")?
        );
        Ok(())
    }
}
