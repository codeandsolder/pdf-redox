use crate::source::OwnedObject;
use smallvec::SmallVec;
use std::{
    collections::{BTreeMap, btree_map},
    mem,
    ops::{Deref, DerefMut},
};

/// Owned direct PDF array. Nested objects are destroyed iteratively.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnedArray(Vec<OwnedObject>);

impl OwnedArray {
    /// Construct an empty array.
    #[must_use = "use the array to construct a COS value"]
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    /// Consume the array and extract its immediate children.
    #[must_use = "use the extracted direct COS values"]
    pub fn into_vec(mut self) -> Vec<OwnedObject> {
        mem::take(&mut self.0)
    }
}

impl From<Vec<OwnedObject>> for OwnedArray {
    fn from(values: Vec<OwnedObject>) -> Self {
        Self(values)
    }
}

impl FromIterator<OwnedObject> for OwnedArray {
    fn from_iter<T: IntoIterator<Item = OwnedObject>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Deref for OwnedArray {
    type Target = Vec<OwnedObject>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OwnedArray {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl IntoIterator for OwnedArray {
    type Item = OwnedObject;
    type IntoIter = std::vec::IntoIter<OwnedObject>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_vec().into_iter()
    }
}

impl<'a> IntoIterator for &'a OwnedArray {
    type Item = &'a OwnedObject;
    type IntoIter = std::slice::Iter<'a, OwnedObject>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut OwnedArray {
    type Item = &'a mut OwnedObject;
    type IntoIter = std::slice::IterMut<'a, OwnedObject>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

/// Owned PDF dictionary keyed by decoded name bytes without the leading slash.
/// The container owns iterative destruction of direct descendants.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OwnedDictionary(BTreeMap<Vec<u8>, OwnedObject>);

impl OwnedDictionary {
    /// Construct an empty dictionary.
    #[must_use = "use the dictionary to construct a COS value"]
    pub const fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// Consume the dictionary and extract its immediate entries.
    #[must_use = "use the extracted direct COS entries"]
    pub fn into_map(mut self) -> BTreeMap<Vec<u8>, OwnedObject> {
        mem::take(&mut self.0)
    }
}

impl From<BTreeMap<Vec<u8>, OwnedObject>> for OwnedDictionary {
    fn from(values: BTreeMap<Vec<u8>, OwnedObject>) -> Self {
        Self(values)
    }
}

impl<const N: usize> From<[(Vec<u8>, OwnedObject); N]> for OwnedDictionary {
    fn from(values: [(Vec<u8>, OwnedObject); N]) -> Self {
        Self(BTreeMap::from(values))
    }
}

impl FromIterator<(Vec<u8>, OwnedObject)> for OwnedDictionary {
    fn from_iter<T: IntoIterator<Item = (Vec<u8>, OwnedObject)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl Deref for OwnedDictionary {
    type Target = BTreeMap<Vec<u8>, OwnedObject>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for OwnedDictionary {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl IntoIterator for OwnedDictionary {
    type Item = (Vec<u8>, OwnedObject);
    type IntoIter = btree_map::IntoIter<Vec<u8>, OwnedObject>;
    fn into_iter(self) -> Self::IntoIter {
        self.into_map().into_iter()
    }
}

impl<'a> IntoIterator for &'a OwnedDictionary {
    type Item = (&'a Vec<u8>, &'a OwnedObject);
    type IntoIter = btree_map::Iter<'a, Vec<u8>, OwnedObject>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<'a> IntoIterator for &'a mut OwnedDictionary {
    type Item = (&'a Vec<u8>, &'a mut OwnedObject);
    type IntoIter = btree_map::IterMut<'a, Vec<u8>, OwnedObject>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter_mut()
    }
}

enum DropChildren {
    Array(std::vec::IntoIter<OwnedObject>),
    Dictionary(btree_map::IntoValues<Vec<u8>, OwnedObject>),
}

impl DropChildren {
    fn next(&mut self) -> Option<OwnedObject> {
        match self {
            Self::Array(iter) => iter.next(),
            Self::Dictionary(iter) => iter.next(),
        }
    }
}

/// Drain one owning iterator per depth, rather than queueing all siblings.
/// Once a child's storage has been moved out, dropping its empty wrapper
/// takes the fast path, so destructor call depth remains bounded.
fn drop_children(root: DropChildren) {
    let mut frames = SmallVec::<[DropChildren; 4]>::new();
    frames.push(root);
    while let Some(frame) = frames.last_mut() {
        let Some(value) = frame.next() else {
            frames.pop();
            continue;
        };
        match value {
            OwnedObject::Array(mut array) if !array.is_empty() => {
                frames.push(DropChildren::Array(mem::take(&mut array.0).into_iter()));
            }
            OwnedObject::Dictionary(mut dictionary)
            | OwnedObject::Stream { mut dictionary, .. }
                if !dictionary.is_empty() =>
            {
                frames.push(DropChildren::Dictionary(
                    mem::take(&mut dictionary.0).into_values(),
                ));
            }
            _ => {}
        }
    }
}

impl Drop for OwnedArray {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            drop_children(DropChildren::Array(mem::take(&mut self.0).into_iter()));
        }
    }
}

impl Drop for OwnedDictionary {
    fn drop(&mut self) {
        if !self.0.is_empty() {
            drop_children(DropChildren::Dictionary(
                mem::take(&mut self.0).into_values(),
            ));
        }
    }
}
