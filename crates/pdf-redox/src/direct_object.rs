use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    source::CurrentObject,
};
use hayro_syntax::object::{
    Array as HayroArray, Dict as HayroDict, MaybeRef as HayroMaybeRef, Name as HayroName,
    Object as HayroObject,
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum DirectPathStep {
    DictKey(Vec<u8>),
    ArrayIndex(usize),
}

#[derive(Clone, Copy)]
pub(crate) enum DirectDictionaryView<'a, 'pdf> {
    Source(&'a HayroDict<'pdf>),
    Owned(&'a OwnedDictionary),
}

impl DirectDictionaryView<'_, '_> {
    pub(crate) fn reference(self, key: &[u8]) -> Option<CowObjectHandle> {
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

    pub(crate) fn resolved_name(
        self,
        document: &EditDocument,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
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

    pub(crate) fn resolved_name_is(
        self,
        document: &EditDocument,
        key: &[u8],
        expected: &[u8],
    ) -> Result<bool> {
        Ok(self
            .resolved_name(document, key)?
            .is_some_and(|name| name == expected))
    }

    pub(crate) fn direct_dictionary_reference(
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

    pub(crate) fn has_direct_dictionary(self, key: &[u8]) -> bool {
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
pub(crate) enum DirectArrayView<'a, 'pdf> {
    Source(&'a HayroArray<'pdf>),
    Owned(&'a [OwnedObject]),
}

impl DirectArrayView<'_, '_> {
    pub(crate) fn resolved_name_is(
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

    pub(crate) fn reference(self, index: usize) -> Option<CowObjectHandle> {
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
pub(crate) enum DirectWalkControl {
    Descend,
    SkipChildren,
}

pub(crate) trait DirectObjectVisitor {
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

    fn descend_dictionary_value(&self, _key: &[u8]) -> bool {
        true
    }
}

fn walk_source_direct_dictionary(
    document: &EditDocument,
    root: CowObjectHandle,
    dictionary: &HayroDict<'_>,
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
        path.push(DirectPathStep::DictKey(key.to_vec()));
        match value {
            HayroMaybeRef::Ref(target) => {
                visitor.visit_reference(root, path, CowObjectHandle::Existing(target.into()))?
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
            walk_source_direct_dictionary(document, root, dictionary, path, visitor)?;
        }
        HayroObject::Stream(stream) => {
            walk_source_direct_dictionary(document, root, &stream.dict(), path, visitor)?;
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

fn walk_owned_direct_object(
    document: &EditDocument,
    root: CowObjectHandle,
    object: &OwnedObject,
    path: &mut Vec<DirectPathStep>,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    match object {
        OwnedObject::Reference(target) => visitor.visit_reference(root, path, *target)?,
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if visitor.visit_dictionary(
                document,
                root,
                path,
                DirectDictionaryView::Owned(dictionary),
            )? == DirectWalkControl::SkipChildren
            {
                return Ok(());
            }
            for (name, value) in dictionary {
                path.push(DirectPathStep::DictKey(name.clone()));
                if matches!(value, OwnedObject::Reference(_))
                    || visitor.descend_dictionary_value(name)
                {
                    walk_owned_direct_object(document, root, value, path, visitor)?;
                }
                path.pop();
            }
        }
        OwnedObject::Array(values) => {
            if visitor.visit_array(document, root, path, DirectArrayView::Owned(values))?
                == DirectWalkControl::SkipChildren
            {
                return Ok(());
            }
            for (index, value) in values.iter().enumerate() {
                path.push(DirectPathStep::ArrayIndex(index));
                walk_owned_direct_object(document, root, value, path, visitor)?;
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
    Ok(())
}

pub(crate) fn walk_direct_output_objects(
    document: &EditDocument,
    visitor: &mut impl DirectObjectVisitor,
) -> Result<()> {
    document.walk_output_objects(|root, object| {
        match object {
            CurrentObject::Source(object) => {
                walk_source_direct_object(document, root, &object, &mut Vec::new(), visitor)?;
            }
            CurrentObject::Owned(object) => {
                walk_owned_direct_object(document, root, object, &mut Vec::new(), visitor)?;
            }
        }
        Ok(())
    })
}
