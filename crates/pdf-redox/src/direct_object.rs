use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    source::CurrentObject,
};
use hayro_syntax::object::{
    Array as HayroArray, Dict as HayroDict, MaybeRef as HayroMaybeRef, Name as HayroName,
    Object as HayroObject,
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

#[derive(Clone, Copy)]
pub enum DirectDictionaryView<'a, 'pdf> {
    Source(&'a HayroDict<'pdf>),
    Owned(&'a OwnedDictionary),
}

impl DirectDictionaryView<'_, '_> {
    pub fn reference(self, key: &[u8]) -> Option<CowObjectHandle> {
        match self {
            Self::Source(dictionary) => dictionary
                .get_ref(key)
                .map(|reference| CowObjectHandle::Existing(reference.into())),
            Self::Owned(dictionary) => match dictionary.get(key) {
                Some(OwnedObject::Reference(reference)) => Some(*reference),
                _ => None,
            },
        }
    }

    pub fn resolved_name(self, document: &EditDocument, key: &[u8]) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Source(dictionary) => Ok(dictionary
                .get::<HayroName<'_>>(key)
                .map(|name| name.as_ref().to_vec())),
            Self::Owned(dictionary) => {
                let Some(value) = dictionary.get(key) else {
                    return Ok(None);
                };
                Ok(match document.resolve_owned_value(value)? {
                    Some(OwnedObject::Name(name)) => Some(name),
                    _ => None,
                })
            }
        }
    }

    pub fn resolved_name_is(
        self,
        document: &EditDocument,
        key: &[u8],
        expected: &[u8],
    ) -> Result<bool> {
        Ok(self
            .resolved_name(document, key)?
            .is_some_and(|name| name == expected))
    }

    pub fn direct_dictionary_reference(
        self,
        dictionary_key: &[u8],
        reference_key: &[u8],
    ) -> Option<CowObjectHandle> {
        match self {
            Self::Source(dictionary) => match dictionary.get_raw::<HayroObject<'_>>(dictionary_key)
            {
                Some(HayroMaybeRef::NotRef(HayroObject::Dict(dictionary))) => dictionary
                    .get_ref(reference_key)
                    .map(|reference| CowObjectHandle::Existing(reference.into())),
                _ => None,
            },
            Self::Owned(dictionary) => match dictionary.get(dictionary_key) {
                Some(OwnedObject::Dictionary(dictionary)) => match dictionary.get(reference_key) {
                    Some(OwnedObject::Reference(reference)) => Some(*reference),
                    _ => None,
                },
                _ => None,
            },
        }
    }

    pub fn has_direct_dictionary(self, key: &[u8]) -> bool {
        match self {
            Self::Source(dictionary) => matches!(
                dictionary.get_raw::<HayroObject<'_>>(key),
                Some(HayroMaybeRef::NotRef(HayroObject::Dict(_)))
            ),
            Self::Owned(dictionary) => {
                matches!(dictionary.get(key), Some(OwnedObject::Dictionary(_)))
            }
        }
    }
}

#[derive(Clone, Copy)]
pub enum DirectArrayView<'a, 'pdf> {
    Source(&'a HayroArray<'pdf>),
    Owned(&'a [OwnedObject]),
}

impl DirectArrayView<'_, '_> {
    pub fn resolved_name_is(
        self,
        document: &EditDocument,
        index: usize,
        expected: &[u8],
    ) -> Result<bool> {
        match self {
            Self::Source(array) => Ok(matches!(
                array.iter::<HayroObject<'_>>().nth(index),
                Some(HayroObject::Name(name)) if name.as_ref() == expected
            )),
            Self::Owned(values) => {
                let Some(value) = values.get(index) else {
                    return Ok(false);
                };
                Ok(matches!(
                    document.resolve_owned_value(value)?,
                    Some(OwnedObject::Name(name)) if name == expected
                ))
            }
        }
    }

    /// Match a directly stored name without resolving indirect references.
    pub fn direct_name_matches(self, index: usize, predicate: impl FnOnce(&[u8]) -> bool) -> bool {
        match self {
            Self::Source(array) => match array.raw_iter().nth(index) {
                Some(HayroMaybeRef::NotRef(HayroObject::Name(name))) => predicate(name.as_ref()),
                _ => false,
            },
            Self::Owned(values) => match values.get(index) {
                Some(OwnedObject::Name(name)) => predicate(name),
                _ => false,
            },
        }
    }

    pub fn reference(self, index: usize) -> Option<CowObjectHandle> {
        match self {
            Self::Source(array) => match array.raw_iter().nth(index) {
                Some(HayroMaybeRef::Ref(reference)) => {
                    Some(CowObjectHandle::Existing(reference.into()))
                }
                _ => None,
            },
            Self::Owned(values) => match values.get(index) {
                Some(OwnedObject::Reference(reference)) => Some(*reference),
                _ => None,
            },
        }
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
