use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    source::{CurrentObject, CurrentValue},
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

/// Project a COW root onto a direct child without copying the selected
/// subtree or unrelated root entries.
pub fn object_at_direct_path_cow<'a>(
    object: std::borrow::Cow<'a, OwnedObject>,
    path: &[DirectPathStep],
) -> Option<std::borrow::Cow<'a, OwnedObject>> {
    use std::borrow::Cow;
    match object {
        Cow::Borrowed(object) => object_at_direct_path(object, path).map(Cow::Borrowed),
        Cow::Owned(object) if path.is_empty() => Some(Cow::Owned(object)),
        Cow::Owned(mut object) => {
            let child = object_at_direct_path_mut(&mut object, path)?;
            Some(Cow::Owned(std::mem::replace(child, OwnedObject::Null)))
        }
    }
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

fn walk_source_direct_dictionary(
    document: &EditDocument,
    root: CowObjectHandle,
    dictionary: &HayroDict<'_>,
    skip_length: bool,
    path: &mut Vec<DirectPathStep>,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    if visitor.visit_dictionary(
        document,
        root,
        path,
        DirectDictionaryView::Source(dictionary),
    )? == DirectWalkControl::SkipChildren
    {
        return Ok(());
    }
    for (name, value) in dictionary.entries() {
        let key = name.as_ref();
        if skip_length && key == b"Length" {
            continue;
        }
        path.push(DirectPathStep::DictKey(key.to_vec()));
        match value {
            HayroMaybeRef::Ref(target) => {
                visitor.visit_reference(root, path, CowObjectHandle::Existing(target.into()))?;
            }
            HayroMaybeRef::NotRef(value) if visitor.descend_dictionary_value(key) => {
                walk_source_direct_object(document, root, &value, path, visitor)?;
            }
            HayroMaybeRef::NotRef(_) => {}
        }
        path.pop();
    }
    Ok(())
}

fn walk_source_direct_object(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &HayroObject<'_>,
    path: &mut Vec<DirectPathStep>,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    match object {
        HayroObject::Dict(dictionary) => {
            walk_source_direct_dictionary(document, root, dictionary, false, path, visitor)?;
        }
        HayroObject::Stream(stream) => {
            walk_source_direct_dictionary(
                document,
                root,
                stream.dict(),
                !visitor.visit_source_stream_length(),
                path,
                visitor,
            )?;
        }
        HayroObject::Array(array) => {
            if visitor.visit_array(document, root, path, DirectArrayView::Source(array))?
                == DirectWalkControl::SkipChildren
            {
                return Ok(());
            }
            for (index, value) in array.raw_iter().enumerate() {
                path.push(DirectPathStep::ArrayIndex(index));
                match value {
                    HayroMaybeRef::Ref(target) => visitor.visit_reference(
                        root,
                        path,
                        CowObjectHandle::Existing(target.into()),
                    )?,
                    HayroMaybeRef::NotRef(value) => {
                        walk_source_direct_object(document, root, &value, path, visitor)?;
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
    Ok(())
}

// Keep only an iterator per active container, not a stack of all of
// its children. The shared path tracks the exact pre-order traversal.
enum OwnedDirectChildren<'a> {
    Dictionary(std::collections::btree_map::Iter<'a, Vec<u8>, OwnedObject>),
    Array(std::iter::Enumerate<std::slice::Iter<'a, OwnedObject>>),
}

impl<'a> OwnedDirectChildren<'a> {
    fn next_child(
        &mut self,
        visitor: &impl DirectObjectVisitor,
    ) -> Option<(DirectPathStep, &'a OwnedObject)> {
        match self {
            Self::Dictionary(entries) => entries
                .find(|(name, value)| {
                    matches!(value, OwnedObject::Reference(_))
                        || visitor.descend_dictionary_value(name)
                })
                .map(|(name, value)| (DirectPathStep::DictKey(name.clone()), value)),
            Self::Array(entries) => entries
                .next()
                .map(|(index, value)| (DirectPathStep::ArrayIndex(index), value)),
        }
    }
}

fn visit_owned_direct_object<'a>(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &'a OwnedObject,
    path: &[DirectPathStep],
    visitor: &mut impl DirectObjectVisitor,
) -> Result<Option<OwnedDirectChildren<'a>>> {
    match object {
        OwnedObject::Reference(target) => visitor.visit_reference(root, path, *target)?,
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if visitor.visit_dictionary(
                document,
                root,
                path,
                DirectDictionaryView::Owned(dictionary),
            )? == DirectWalkControl::Descend
            {
                return Ok(Some(OwnedDirectChildren::Dictionary(dictionary.iter())));
            }
        }
        OwnedObject::Array(values) => {
            if visitor.visit_array(document, root, path, DirectArrayView::Owned(values))?
                == DirectWalkControl::Descend
            {
                return Ok(Some(OwnedDirectChildren::Array(values.iter().enumerate())));
            }
        }
        _ => {}
    }
    Ok(None)
}

fn walk_owned_direct_object(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &OwnedObject,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    let mut path = Vec::new();
    let Some(children) = visit_owned_direct_object(document, root, object, &path, visitor)? else {
        return Ok(());
    };
    let mut frames = vec![children];
    while let Some(frame) = frames.last_mut() {
        if let Some((step, child)) = frame.next_child(visitor) {
            path.push(step);
            if let Some(children) =
                visit_owned_direct_object(document, root, child, &path, visitor)?
            {
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

pub fn walk_direct_object(
    document: &EditDocument,
    root: CowObjectHandle,
    object: CurrentObject<'_>,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    match object {
        CurrentObject::Source(object) => {
            walk_source_direct_object(document, root, &object, &mut Vec::new(), visitor)
        }
        CurrentObject::Owned(object) => walk_owned_direct_object(document, root, object, visitor),
    }
}

pub fn walk_direct_output_objects(
    document: &EditDocument,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    document.walk_output_objects(|root, object| walk_direct_object(document, root, object, visitor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, ObjectId, test_support::ClassicPdfBuilder};

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
