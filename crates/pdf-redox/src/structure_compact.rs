use crate::{EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result};
use std::collections::{BTreeMap, BTreeSet};

const PAGE_TREE_FANOUT: usize = 256;
const NAME_TREE_FANOUT: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructureCompactionStats {
    pub page_tree_nodes_before: usize,
    pub page_tree_nodes_after: usize,
    pub page_tree_nodes_removed: usize,
    pub page_tree_pages_reparented: usize,
    pub name_trees_repacked: usize,
    pub name_tree_nodes_before: usize,
    pub name_tree_nodes_after: usize,
    pub name_tree_nodes_removed: usize,
    pub named_destination_wrappers_inlined: usize,
    pub named_destination_arrays_inlined: usize,
}

fn dictionary_reference(dictionary: &OwnedDictionary, key: &[u8]) -> Option<ObjectHandle> {
    match dictionary.get(key) {
        Some(OwnedObject::Reference(handle)) => Some(*handle),
        _ => None,
    }
}

fn current_dictionary(
    document: &EditDocument,
    handle: ObjectHandle,
) -> Result<Option<OwnedDictionary>> {
    Ok(document
        .current_owned_object(handle)?
        .and_then(|object| object.as_dictionary().cloned()))
}

fn page_tree_node_count(document: &EditDocument, root: ObjectHandle) -> Result<Option<usize>> {
    let mut seen = BTreeSet::new();
    let mut pending = vec![root];
    while let Some(handle) = pending.pop() {
        if !seen.insert(handle) {
            return Ok(None);
        }
        let Some(dictionary) = current_dictionary(document, handle)? else {
            return Ok(None);
        };
        if !matches!(
            dictionary.get(b"Type".as_slice()),
            Some(OwnedObject::Name(name)) if name == b"Pages"
        ) {
            return Ok(None);
        }
        let Some(OwnedObject::Array(kids)) = dictionary.get(b"Kids".as_slice()) else {
            return Ok(None);
        };
        for kid in kids {
            let OwnedObject::Reference(kid) = kid else {
                return Ok(None);
            };
            let Some(child) = current_dictionary(document, *kid)? else {
                return Ok(None);
            };
            if matches!(
                child.get(b"Type".as_slice()),
                Some(OwnedObject::Name(name)) if name == b"Pages"
            ) {
                pending.push(*kid);
            }
        }
    }
    Ok(Some(seen.len()))
}

#[expect(
    clippy::too_many_lines,
    reason = "page-tree rebuilding is one recursive structural transaction with shared parent and count invariants"
)]
fn compact_page_tree(
    document: &mut EditDocument,
    stats: &mut StructureCompactionStats,
) -> Result<()> {
    const fn required_nodes(mut leaves: usize) -> usize {
        let mut nodes = 1usize;
        while leaves > PAGE_TREE_FANOUT {
            leaves = leaves.div_ceil(PAGE_TREE_FANOUT);
            nodes = nodes.saturating_add(leaves);
        }
        nodes
    }

    #[derive(Clone, Copy)]
    struct Child {
        handle: ObjectHandle,
        pages: usize,
    }

    fn set_parent(
        document: &mut EditDocument,
        child: ObjectHandle,
        parent: ObjectHandle,
    ) -> Result<()> {
        let Some(dictionary) = document.edit_handle(child)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "page-tree child is not a dictionary".to_owned(),
            ));
        };
        dictionary.insert(b"Parent".to_vec(), OwnedObject::Reference(parent));
        Ok(())
    }

    let catalog_handle = ObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = current_dictionary(document, catalog_handle)? else {
        return Ok(());
    };
    let Some(root) = dictionary_reference(&catalog, b"Pages") else {
        return Ok(());
    };
    let Some(before_nodes) = page_tree_node_count(document, root)? else {
        return Ok(());
    };
    stats.page_tree_nodes_before = before_nodes;

    let pages = document.page_handles()?;
    if pages.is_empty() {
        stats.page_tree_nodes_after = before_nodes;
        return Ok(());
    }

    let after_nodes = required_nodes(pages.len());
    stats.page_tree_nodes_after = after_nodes;
    if after_nodes >= before_nodes {
        return Ok(());
    }

    let inheritable_keys = [
        b"Resources".as_slice(),
        b"MediaBox".as_slice(),
        b"CropBox".as_slice(),
        b"Rotate".as_slice(),
    ];
    let mut inherited = Vec::with_capacity(pages.len());
    for &page in &pages {
        let Some(page_dictionary) = current_dictionary(document, page)? else {
            return Ok(());
        };
        let mut materialized = Vec::new();
        for key in inheritable_keys {
            if !page_dictionary.contains_key(key)
                && let Some(value) = document.inherited_page_value(page, key)?
            {
                materialized.push((key.to_vec(), value));
            }
        }
        inherited.push((page, materialized));
    }

    for (page, values) in &inherited {
        let Some(dictionary) = document.edit_handle(*page)?.as_dictionary_mut() else {
            return Ok(());
        };
        for (key, value) in values {
            dictionary.insert(key.clone(), value.clone());
        }
    }

    let mut level = pages
        .iter()
        .copied()
        .map(|handle| Child { handle, pages: 1 })
        .collect::<Vec<_>>();

    while level.len() > PAGE_TREE_FANOUT {
        let mut next = Vec::with_capacity(level.len().div_ceil(PAGE_TREE_FANOUT));
        for chunk in level.chunks(PAGE_TREE_FANOUT) {
            let count = chunk.iter().map(|child| child.pages).sum::<usize>();
            let node = ObjectHandle::New(
                document.add_object(OwnedObject::Dictionary(BTreeMap::from([
                    (b"Type".to_vec(), OwnedObject::Name(b"Pages".to_vec())),
                    (
                        b"Kids".to_vec(),
                        OwnedObject::Array(
                            chunk
                                .iter()
                                .map(|child| OwnedObject::Reference(child.handle))
                                .collect(),
                        ),
                    ),
                    (
                        b"Count".to_vec(),
                        OwnedObject::Integer(i64::try_from(count).unwrap_or(i64::MAX)),
                    ),
                ]))),
            );
            for child in chunk {
                set_parent(document, child.handle, node)?;
            }
            next.push(Child {
                handle: node,
                pages: count,
            });
        }
        level = next;
    }

    for child in &level {
        set_parent(document, child.handle, root)?;
    }

    let Some(root_dictionary) = document.edit_handle(root)?.as_dictionary_mut() else {
        return Ok(());
    };
    root_dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"Pages".to_vec()));
    root_dictionary.insert(
        b"Kids".to_vec(),
        OwnedObject::Array(
            level
                .iter()
                .map(|child| OwnedObject::Reference(child.handle))
                .collect(),
        ),
    );
    root_dictionary.insert(
        b"Count".to_vec(),
        OwnedObject::Integer(i64::try_from(pages.len()).unwrap_or(i64::MAX)),
    );
    root_dictionary.remove(b"Parent".as_slice());
    for key in inheritable_keys {
        root_dictionary.remove(key);
    }

    stats.page_tree_pages_reparented = pages.len();
    stats.page_tree_nodes_removed = before_nodes.saturating_sub(after_nodes);
    Ok(())
}

#[derive(Debug, Clone)]
struct NameLeaf {
    handle: ObjectHandle,
    names: Vec<OwnedObject>,
}

#[derive(Debug, Clone)]
struct NameTreeSnapshot {
    root: ObjectHandle,
    nodes: BTreeSet<ObjectHandle>,
    leaves: Vec<NameLeaf>,
    pairs: Vec<(Vec<u8>, OwnedObject)>,
}

fn collect_name_tree(
    document: &EditDocument,
    root: ObjectHandle,
) -> Result<Option<NameTreeSnapshot>> {
    let mut nodes = BTreeSet::new();
    let mut leaves = Vec::new();
    let mut pairs = Vec::new();
    let mut pending = vec![root];

    while let Some(handle) = pending.pop() {
        if !nodes.insert(handle) {
            return Ok(None);
        }
        let Some(dictionary) = current_dictionary(document, handle)? else {
            return Ok(None);
        };

        if let Some(OwnedObject::Array(names)) = dictionary.get(b"Names".as_slice()) {
            if names.len() % 2 != 0 {
                return Ok(None);
            }
            let mut leaf_names = names.clone();
            for pair in leaf_names.as_chunks_mut::<2>().0 {
                let OwnedObject::String(key) = &pair[0] else {
                    return Ok(None);
                };
                pairs.push((key.clone(), pair[1].clone()));
            }
            leaves.push(NameLeaf {
                handle,
                names: leaf_names,
            });
            continue;
        }

        let Some(OwnedObject::Array(kids)) = dictionary.get(b"Kids".as_slice()) else {
            return Ok(None);
        };
        for kid in kids.iter().rev() {
            let OwnedObject::Reference(kid) = kid else {
                return Ok(None);
            };
            pending.push(*kid);
        }
    }

    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Some(NameTreeSnapshot {
        root,
        nodes,
        leaves,
        pairs,
    }))
}

fn inline_destination_objects(
    document: &EditDocument,
    snapshot: &mut NameTreeSnapshot,
) -> Result<(usize, usize)> {
    let mut wrappers_inlined = 0usize;
    let mut arrays_inlined = 0usize;
    for leaf in &mut snapshot.leaves {
        for pair in leaf.names.as_chunks_mut::<2>().0 {
            let OwnedObject::Reference(target) = pair[1] else {
                continue;
            };
            let Some(object) = document.current_owned_object(target)? else {
                continue;
            };
            match object {
                OwnedObject::Array(array) => {
                    pair[1] = OwnedObject::Array(array);
                    arrays_inlined = arrays_inlined.saturating_add(1);
                }
                OwnedObject::Dictionary(dictionary)
                    if dictionary.len() == 1 && dictionary.contains_key(b"D".as_slice()) =>
                {
                    pair[1] = OwnedObject::Dictionary(dictionary);
                    wrappers_inlined = wrappers_inlined.saturating_add(1);
                }
                _ => {}
            }
        }
    }

    if wrappers_inlined != 0 || arrays_inlined != 0 {
        snapshot.pairs.clear();
        for leaf in &snapshot.leaves {
            for pair in leaf.names.as_chunks::<2>().0 {
                let OwnedObject::String(key) = &pair[0] else {
                    continue;
                };
                snapshot.pairs.push((key.clone(), pair[1].clone()));
            }
        }
        snapshot.pairs.sort_by(|a, b| a.0.cmp(&b.0));
    }
    Ok((wrappers_inlined, arrays_inlined))
}

const fn name_tree_required_nodes(pairs: usize) -> usize {
    if pairs <= NAME_TREE_FANOUT {
        return 1;
    }
    let mut children = pairs.div_ceil(NAME_TREE_FANOUT);
    let mut nodes = 1usize.saturating_add(children);
    while children > NAME_TREE_FANOUT {
        children = children.div_ceil(NAME_TREE_FANOUT);
        nodes = nodes.saturating_add(children);
    }
    nodes
}

fn names_array(pairs: &[(Vec<u8>, OwnedObject)]) -> Vec<OwnedObject> {
    let mut out = Vec::with_capacity(pairs.len().saturating_mul(2));
    for (key, value) in pairs {
        out.push(OwnedObject::String(key.clone()));
        out.push(value.clone());
    }
    out
}

#[derive(Clone)]
struct NameChild {
    handle: ObjectHandle,
    first: Vec<u8>,
    last: Vec<u8>,
}

#[expect(
    clippy::too_many_lines,
    reason = "name-tree replacement keeps collection, rebuilding, and parent-object rewrite invariants together"
)]
fn replace_name_tree(document: &mut EditDocument, snapshot: &NameTreeSnapshot) -> Result<usize> {
    if snapshot.pairs.is_empty() {
        let Some(root) = document.edit_handle(snapshot.root)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "name-tree root is not a dictionary".to_owned(),
            ));
        };
        root.clear();
        root.insert(b"Names".to_vec(), OwnedObject::Array(Vec::new()));
        return Ok(1);
    }

    if snapshot.pairs.len() <= NAME_TREE_FANOUT {
        let Some(root) = document.edit_handle(snapshot.root)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "name-tree root is not a dictionary".to_owned(),
            ));
        };
        root.clear();
        root.insert(
            b"Names".to_vec(),
            OwnedObject::Array(names_array(&snapshot.pairs)),
        );
        return Ok(1);
    }

    let mut level = Vec::new();
    for chunk in snapshot.pairs.chunks(NAME_TREE_FANOUT) {
        let first = chunk.first().map(|pair| pair.0.clone()).unwrap_or_default();
        let last = chunk.last().map(|pair| pair.0.clone()).unwrap_or_default();
        let handle =
            ObjectHandle::New(document.add_object(OwnedObject::Dictionary(BTreeMap::from([
                (
                    b"Limits".to_vec(),
                    OwnedObject::Array(vec![
                        OwnedObject::String(first.clone()),
                        OwnedObject::String(last.clone()),
                    ]),
                ),
                (b"Names".to_vec(), OwnedObject::Array(names_array(chunk))),
            ]))));
        level.push(NameChild {
            handle,
            first,
            last,
        });
    }
    let mut created = level.len();

    while level.len() > NAME_TREE_FANOUT {
        let mut next = Vec::with_capacity(level.len().div_ceil(NAME_TREE_FANOUT));
        for chunk in level.chunks(NAME_TREE_FANOUT) {
            let first = chunk
                .first()
                .map(|child| child.first.clone())
                .unwrap_or_default();
            let last = chunk
                .last()
                .map(|child| child.last.clone())
                .unwrap_or_default();
            let handle = ObjectHandle::New(
                document.add_object(OwnedObject::Dictionary(BTreeMap::from([
                    (
                        b"Limits".to_vec(),
                        OwnedObject::Array(vec![
                            OwnedObject::String(first.clone()),
                            OwnedObject::String(last.clone()),
                        ]),
                    ),
                    (
                        b"Kids".to_vec(),
                        OwnedObject::Array(
                            chunk
                                .iter()
                                .map(|child| OwnedObject::Reference(child.handle))
                                .collect(),
                        ),
                    ),
                ]))),
            );
            next.push(NameChild {
                handle,
                first,
                last,
            });
        }
        created = created.saturating_add(next.len());
        level = next;
    }

    let Some(root) = document.edit_handle(snapshot.root)?.as_dictionary_mut() else {
        return Err(Error::Invalid(
            "name-tree root is not a dictionary".to_owned(),
        ));
    };
    root.clear();
    root.insert(
        b"Kids".to_vec(),
        OwnedObject::Array(
            level
                .iter()
                .map(|child| OwnedObject::Reference(child.handle))
                .collect(),
        ),
    );
    Ok(created.saturating_add(1))
}

fn apply_leaf_inlining(document: &mut EditDocument, snapshot: &NameTreeSnapshot) -> Result<()> {
    for leaf in &snapshot.leaves {
        let Some(dictionary) = document.edit_handle(leaf.handle)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "name-tree leaf is not a dictionary".to_owned(),
            ));
        };
        dictionary.insert(b"Names".to_vec(), OwnedObject::Array(leaf.names.clone()));
    }
    Ok(())
}

fn compact_catalog_name_trees(
    document: &mut EditDocument,
    stats: &mut StructureCompactionStats,
) -> Result<()> {
    let catalog_handle = ObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = current_dictionary(document, catalog_handle)? else {
        return Ok(());
    };
    let Some(names_handle) = dictionary_reference(&catalog, b"Names") else {
        return Ok(());
    };
    let Some(names_dictionary) = current_dictionary(document, names_handle)? else {
        return Ok(());
    };

    let roots = names_dictionary
        .iter()
        .filter_map(|(kind, value)| match value {
            OwnedObject::Reference(root) => Some((kind.clone(), *root)),
            _ => None,
        })
        .collect::<Vec<_>>();

    for (kind, root) in roots {
        let Some(mut snapshot) = collect_name_tree(document, root)? else {
            continue;
        };
        let before = snapshot.nodes.len();
        if before == 0 {
            continue;
        }
        let (wrappers_inlined, arrays_inlined) = if kind.as_slice() == b"Dests" {
            inline_destination_objects(document, &mut snapshot)?
        } else {
            (0, 0)
        };
        stats.named_destination_wrappers_inlined = stats
            .named_destination_wrappers_inlined
            .saturating_add(wrappers_inlined);
        stats.named_destination_arrays_inlined = stats
            .named_destination_arrays_inlined
            .saturating_add(arrays_inlined);

        let required = name_tree_required_nodes(snapshot.pairs.len());
        stats.name_tree_nodes_before = stats.name_tree_nodes_before.saturating_add(before);

        if required < before {
            let after = replace_name_tree(document, &snapshot)?;
            stats.name_trees_repacked = stats.name_trees_repacked.saturating_add(1);
            stats.name_tree_nodes_after = stats.name_tree_nodes_after.saturating_add(after);
            stats.name_tree_nodes_removed = stats
                .name_tree_nodes_removed
                .saturating_add(before.saturating_sub(after));
        } else {
            if wrappers_inlined != 0 || arrays_inlined != 0 {
                apply_leaf_inlining(document, &snapshot)?;
            }
            stats.name_tree_nodes_after = stats.name_tree_nodes_after.saturating_add(before);
        }
    }
    Ok(())
}

pub fn compact_structure(document: &mut EditDocument) -> Result<StructureCompactionStats> {
    let mut stats = StructureCompactionStats::default();
    compact_catalog_name_trees(document, &mut stats)?;
    compact_page_tree(document, &mut stats)?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ClassicPdfBuilder;

    #[test]
    fn inlines_indirect_named_destination_arrays() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Names 7 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"")?;
        pdf.object(7, b"<< /Dests 8 0 R >>")?;
        pdf.object(8, b"<< /Names [(target) 9 0 R] >>")?;
        pdf.object(9, b"[3 0 R /Fit]")?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.named_destination_arrays_inlined, 1);

        let catalog = current_dictionary(
            &document,
            ObjectHandle::Existing(document.source().catalog_id()),
        )?
        .ok_or_else(|| crate::Error::Invalid("catalog is missing".to_owned()))?;
        let names = dictionary_reference(&catalog, b"Names")
            .ok_or_else(|| crate::Error::Invalid("Names reference is missing".to_owned()))?;
        let names = current_dictionary(&document, names)?
            .ok_or_else(|| crate::Error::Invalid("Names dictionary is missing".to_owned()))?;
        let dests = dictionary_reference(&names, b"Dests")
            .ok_or_else(|| crate::Error::Invalid("Dests reference is missing".to_owned()))?;
        let dests = current_dictionary(&document, dests)?
            .ok_or_else(|| crate::Error::Invalid("Dests name tree is missing".to_owned()))?;
        let Some(OwnedObject::Array(entries)) = dests.get(b"Names".as_slice()) else {
            return Err(crate::Error::Invalid(
                "Dests root has no direct Names array".to_owned(),
            ));
        };
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], OwnedObject::String(b"target".to_vec()));
        assert!(matches!(
            &entries[1],
            OwnedObject::Array(destination)
                if destination.len() == 2
                    && matches!(destination[0], OwnedObject::Reference(_))
                    && destination[1] == OwnedObject::Name(b"Fit".to_vec())
        ));
        Ok(())
    }
}
