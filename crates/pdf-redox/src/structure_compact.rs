use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result,
    dedup::exact_dictionary_fingerprint,
    direct_object::{
        DirectArrayView, DirectDictionaryView, DirectObjectVisitor, DirectPathStep,
        DirectWalkControl, object_at_direct_path_mut, walk_direct_object,
    },
    source::{CurrentObject, PAGE_INHERITABLE_KEYS, owned_from_maybe_ref},
};
use hayro_syntax::object::{Dict as HayroDict, Name as HayroName, Object as HayroObject, Rect};
use std::collections::{BTreeMap, BTreeSet};

const PAGE_TREE_FANOUT: usize = 256;
const NAME_TREE_FANOUT: usize = 64;
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StructureCompactionStats {
    pub page_tree_nodes_before: usize,
    pub page_tree_nodes_after: usize,
    pub page_tree_nodes_removed: usize,
    pub page_tree_pages_reparented: usize,
    pub shadowed_page_tree_inherited_entries_removed: usize,
    pub name_trees_repacked: usize,
    pub name_tree_nodes_before: usize,
    pub name_tree_nodes_after: usize,
    pub name_tree_nodes_removed: usize,
    pub named_destination_wrappers_inlined: usize,
    pub named_destination_arrays_inlined: usize,
    pub stale_page_destination_references_rewritten: usize,
}

fn take_inherited_page_keys(
    dictionary: &HayroDict<'_>,
    inherited: &mut [Option<OwnedObject>; PAGE_INHERITABLE_KEYS.len()],
) {
    for (key, value) in PAGE_INHERITABLE_KEYS.into_iter().zip(inherited) {
        if value.is_some() {
            continue;
        }
        // Match the source reader's typed lookups, including inheritance
        // past direct/indirect nulls and invalid values. Retain the raw COS
        // value for conservative identity comparison, not its dereferenced
        // object graph (in particular, resource dictionaries).
        let valid = match key {
            b"Resources" => dictionary.get::<HayroDict<'_>>(key).is_some(),
            b"MediaBox" | b"CropBox" => dictionary.get::<Rect>(key).is_some(),
            b"Rotate" => dictionary.get::<i32>(key).is_some(),
            _ => false,
        };
        if valid {
            *value = dictionary
                .get_raw::<HayroObject<'_>>(key)
                .map(owned_from_maybe_ref);
        }
    }
}

fn source_page_identity(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<Option<OwnedObject>> {
    let ObjectHandle::Existing(id) = page else {
        return Ok(None);
    };
    let HayroObject::Dict(mut dictionary) = document.source().object(id)? else {
        return Ok(None);
    };
    // Compare the complete original local page semantics, not a whitelist of
    // visual attributes or the rewritten overlay. Only structural ancestry
    // and the four separately resolved inheritable properties are excluded.
    let mut properties = OwnedDictionary::new();
    for name in dictionary.keys() {
        let key = name.as_ref();
        if key == b"Parent" || PAGE_INHERITABLE_KEYS.contains(&key) {
            continue;
        }
        let Some(value) = dictionary.get_raw::<HayroObject<'_>>(key) else {
            // An unparseable property cannot prove equality with another page.
            return Ok(None);
        };
        properties.insert(key.to_vec(), owned_from_maybe_ref(value));
    }
    let mut inherited = std::array::from_fn(|_| None);
    let mut seen = BTreeSet::from([id]);
    loop {
        take_inherited_page_keys(&dictionary, &mut inherited);
        if inherited.iter().all(Option::is_some) {
            break;
        }

        let parent = match dictionary.get_raw::<HayroObject<'_>>(b"Parent") {
            None | Some(hayro_syntax::object::MaybeRef::NotRef(HayroObject::Null(_))) => break,
            Some(hayro_syntax::object::MaybeRef::Ref(parent)) => <crate::ObjectId>::from(parent),
            _ => return Ok(None),
        };
        if !seen.insert(parent) {
            return Ok(None);
        }
        dictionary = match document.source().object(parent) {
            Ok(HayroObject::Dict(dictionary)) => dictionary,
            Ok(_) | Err(Error::MissingSourceObject { .. }) => return Ok(None),
            Err(error) => return Err(error),
        };
    }
    properties.extend(
        PAGE_INHERITABLE_KEYS
            .into_iter()
            .zip(inherited)
            .filter_map(|(key, value)| value.map(|value| (key.to_vec(), value))),
    );
    Ok(Some(OwnedObject::Dictionary(properties)))
}

fn reachable_page_dictionaries(document: &EditDocument) -> Result<Vec<ObjectHandle>> {
    let mut pages = Vec::new();
    document.walk_output_objects(|handle, object| {
        let is_page = match object {
            CurrentObject::Source(HayroObject::Dict(dictionary)) => dictionary
                .get::<HayroName<'_>>(b"Type")
                .is_some_and(|name| name.as_ref() == b"Page"),
            CurrentObject::Owned(OwnedObject::Dictionary(dictionary)) => {
                let Some(kind) = dictionary.get(b"Type".as_slice()) else {
                    return Ok(());
                };
                matches!(
                    document.resolve_owned_value(kind)?,
                    Some(OwnedObject::Name(name)) if name == b"Page"
                )
            }
            CurrentObject::Source(_) | CurrentObject::Owned(_) => false,
        };
        if is_page {
            pages.push(handle);
        }
        Ok(())
    })?;
    Ok(pages)
}

struct LivePageIdentity {
    properties: OwnedObject,
    unique_page: Option<ObjectHandle>,
}

#[derive(Default)]
struct LivePageIdentityIndex {
    by_fingerprint: BTreeMap<[u8; 32], Vec<LivePageIdentity>>,
}

impl LivePageIdentityIndex {
    fn fingerprint(properties: &OwnedObject) -> Result<[u8; 32]> {
        // Source materialization already canonicalizes both signs of zero to
        // integer zero, so the exact COS fingerprint agrees with structural
        // equality. Hashing is only an index; compare values in the bucket.
        exact_dictionary_fingerprint(properties, b"immutable-source-page-identity")?
            .ok_or_else(|| Error::Invalid("page identity is not a dictionary".to_owned()))
    }

    fn insert(&mut self, page: ObjectHandle, properties: OwnedObject) -> Result<()> {
        let fingerprint = Self::fingerprint(&properties)?;
        let bucket = self.by_fingerprint.entry(fingerprint).or_default();
        if let Some(existing) = bucket
            .iter_mut()
            .find(|candidate| candidate.properties == properties)
        {
            existing.unique_page = None;
        } else {
            bucket.push(LivePageIdentity {
                properties,
                unique_page: Some(page),
            });
        }
        Ok(())
    }

    fn unique_match(&self, properties: &OwnedObject) -> Result<Option<ObjectHandle>> {
        let fingerprint = Self::fingerprint(properties)?;
        Ok(self
            .by_fingerprint
            .get(&fingerprint)
            .and_then(|bucket| {
                bucket
                    .iter()
                    .find(|candidate| candidate.properties == *properties)
            })
            .and_then(|candidate| candidate.unique_page))
    }
}

fn stale_page_destination_map_from_plan(
    document: &EditDocument,
    plan: &PageTreePlan,
) -> Result<BTreeMap<ObjectHandle, ObjectHandle>> {
    let stale_pages = reachable_page_dictionaries(document)?
        .into_iter()
        .filter(|handle| !plan.owners.contains_key(handle))
        .collect::<Vec<_>>();
    if stale_pages.is_empty() {
        return Ok(BTreeMap::new());
    }

    let mut live_identities = LivePageIdentityIndex::default();
    for page in &plan.pages {
        if let Some(identity) = source_page_identity(document, page.handle)? {
            live_identities.insert(page.handle, identity)?;
        }
    }

    let mut redirects = BTreeMap::new();
    for handle in stale_pages {
        let Some(identity) = source_page_identity(document, handle)? else {
            continue;
        };
        if let Some(canonical) = live_identities.unique_match(&identity)? {
            redirects.insert(handle, canonical);
        }
    }
    Ok(redirects)
}

const fn is_destination_mode(name: &[u8]) -> bool {
    matches!(
        name,
        b"XYZ" | b"Fit" | b"FitH" | b"FitV" | b"FitR" | b"FitB" | b"FitBH" | b"FitBV"
    )
}

struct DestinationEdit {
    path: Vec<DirectPathStep>,
    target: ObjectHandle,
    canonical: ObjectHandle,
}

struct StaleDestinationVisitor<'a> {
    redirects: &'a BTreeMap<ObjectHandle, ObjectHandle>,
    edits: Vec<DestinationEdit>,
}

impl DirectObjectVisitor for StaleDestinationVisitor<'_> {
    fn visit_source_stream_length(&self) -> bool {
        false
    }

    fn visit_array(
        &mut self,
        _document: &EditDocument,
        _root: ObjectHandle,
        path: &[DirectPathStep],
        array: DirectArrayView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        if let Some(target) = array.reference(0)
            && let Some(&canonical) = self.redirects.get(&target)
            && array.direct_name_matches(1, is_destination_mode)
        {
            self.edits.push(DestinationEdit {
                path: path.to_vec(),
                target,
                canonical,
            });
        }
        Ok(DirectWalkControl::Descend)
    }
}

fn apply_destination_edits(
    document: &mut EditDocument,
    handle: ObjectHandle,
    edits: Vec<DestinationEdit>,
) -> Result<usize> {
    if edits.is_empty() {
        return Ok(0);
    }
    let object = document.edit_handle(handle)?;
    let mut rewritten = 0usize;
    for edit in edits {
        let Some(OwnedObject::Array(values)) = object_at_direct_path_mut(object, &edit.path) else {
            return Err(Error::Invalid(
                "validated destination array disappeared".to_owned(),
            ));
        };
        if let [OwnedObject::Reference(target), OwnedObject::Name(mode), ..] = values.as_mut_slice()
            && *target == edit.target
            && is_destination_mode(mode)
        {
            *target = edit.canonical;
            rewritten += 1;
        } else {
            return Err(Error::Invalid(
                "validated destination array changed".to_owned(),
            ));
        }
    }
    Ok(rewritten)
}

fn canonicalize_stale_page_destinations_from_plan(
    document: &mut EditDocument,
    plan: &PageTreePlan,
) -> Result<usize> {
    let redirects = stale_page_destination_map_from_plan(document, plan)?;
    if redirects.is_empty() {
        return Ok(0);
    }
    let handles = document.reachable_objects()?;
    let mut rewritten = 0usize;
    for handle in handles {
        // The preceding reachability traversal already populated source
        // adjacency. Skip unchanged source objects that cannot contain a
        // candidate; owned objects are inspected directly, including their
        // existing stream dictionary values.
        if let ObjectHandle::Existing(id) = handle
            && document.overlay().change(id).is_none()
            && !document
                .source()
                .references(id)?
                .into_iter()
                .any(|reference| redirects.contains_key(&ObjectHandle::Existing(reference)))
        {
            continue;
        }
        let Some(object) = document.current_object(handle)? else {
            continue;
        };
        let mut visitor = StaleDestinationVisitor {
            redirects: &redirects,
            edits: Vec::new(),
        };
        walk_direct_object(document, handle, object, &mut visitor)?;
        rewritten =
            rewritten.saturating_add(apply_destination_edits(document, handle, visitor.edits)?);
    }
    Ok(rewritten)
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
        .current_owned_object_cow(handle)?
        .and_then(crate::source::into_owned_dictionary))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PageTreeNodeKind {
    Branch,
    Leaf,
}

fn page_tree_node_kind(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<PageTreeNodeKind>> {
    let Some(kind) = dictionary.get(b"Type".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(kind)? {
        Some(OwnedObject::Name(name)) if name == b"Pages" => Some(PageTreeNodeKind::Branch),
        Some(OwnedObject::Name(name)) if name == b"Page" => Some(PageTreeNodeKind::Leaf),
        _ => None,
    })
}

/// One validated view of the current page tree, used by both destructive
/// structural passes. Never substitute the reader's recovery page list.
struct PageTreePlan {
    root: ObjectHandle,
    branches: BTreeSet<ObjectHandle>,
    owners: BTreeMap<ObjectHandle, InheritedOwners>,
    pages: Vec<PageTreeLeaf>,
    shadowed_entries: Vec<(ObjectHandle, Vec<Vec<u8>>)>,
}

type InheritedOwners = [Option<ObjectHandle>; PAGE_INHERITABLE_KEYS.len()];

fn nearest_page_tree_owners(
    node: ObjectHandle,
    defined: [bool; PAGE_INHERITABLE_KEYS.len()],
    inherited_from: InheritedOwners,
) -> InheritedOwners {
    std::array::from_fn(|index| {
        if defined[index] {
            Some(node)
        } else {
            inherited_from[index]
        }
    })
}

struct PageTreeLeaf {
    handle: ObjectHandle,
    inherited_from: InheritedOwners,
}

// A post-order traversal computes which inherited keys are defined on
// every path beneath each /Pages node. Keep only the small per-node state
// on the heap; recursive page-tree traversal can overflow the thread stack
// even when the source parser accepts every individual dictionary.
struct PageTreePruneFrame {
    node: ObjectHandle,
    defined: [bool; PAGE_INHERITABLE_KEYS.len()],
    nearest_owners: InheritedOwners,
    kids: Vec<ObjectHandle>,
    next_child: usize,
    covered_by_children: [bool; PAGE_INHERITABLE_KEYS.len()],
}

impl PageTreePruneFrame {
    fn new(
        document: &EditDocument,
        node: ObjectHandle,
        dictionary: &OwnedDictionary,
        inherited_from: InheritedOwners,
    ) -> Result<Option<Self>> {
        let Some(kids) = dictionary.get(b"Kids".as_slice()) else {
            return Ok(None);
        };
        let resolved = document.resolve_owned_value_cow(kids)?;
        let Some(OwnedObject::Array(kids)) = resolved.as_deref() else {
            return Ok(None);
        };
        let Some(kids) = kids
            .iter()
            .map(|kid| match kid {
                OwnedObject::Reference(handle) => Some(*handle),
                _ => None,
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(None);
        };
        let defined = document.effective_page_inheritance_mask(dictionary)?;
        Ok(Some(Self {
            node,
            defined,
            nearest_owners: nearest_page_tree_owners(node, defined, inherited_from),
            kids,
            next_child: 0,
            covered_by_children: [true; PAGE_INHERITABLE_KEYS.len()],
        }))
    }

    fn include_child(&mut self, coverage: [bool; PAGE_INHERITABLE_KEYS.len()]) {
        for (covered, child) in self.covered_by_children.iter_mut().zip(coverage) {
            *covered &= child;
        }
    }

    fn finish(
        self,
        removals: &mut Vec<(ObjectHandle, Vec<Vec<u8>>)>,
    ) -> [bool; PAGE_INHERITABLE_KEYS.len()] {
        // A /Pages node with no kids proves nothing about descendant
        // coverage, and its own keys must not be pruned.
        let covered_below = if self.kids.is_empty() {
            [false; PAGE_INHERITABLE_KEYS.len()]
        } else {
            self.covered_by_children
        };
        let remove = PAGE_INHERITABLE_KEYS
            .into_iter()
            .enumerate()
            .filter(|(index, _)| self.defined[*index] && covered_below[*index])
            .map(|(_, key)| key.to_vec())
            .collect::<Vec<_>>();
        if !remove.is_empty() {
            removals.push((self.node, remove));
        }
        std::array::from_fn(|index| self.defined[index] || covered_below[index])
    }
}

fn plan_page_tree(document: &EditDocument) -> Result<Option<PageTreePlan>> {
    let catalog = ObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = current_dictionary(document, catalog)? else {
        return Ok(None);
    };
    let Some(root) = dictionary_reference(&catalog, b"Pages") else {
        return Ok(None);
    };
    let Some(root_object) = document.current_owned_object_cow(root)? else {
        return Ok(None);
    };
    let OwnedObject::Dictionary(root_dictionary) = root_object.as_ref() else {
        return Ok(None);
    };
    if page_tree_node_kind(document, root_dictionary)? != Some(PageTreeNodeKind::Branch)
        || !matches!(
            root_dictionary.get(b"Parent".as_slice()),
            None | Some(OwnedObject::Null)
        )
    {
        return Ok(None);
    }
    let Some(root_frame) = PageTreePruneFrame::new(
        document,
        root,
        root_dictionary,
        [None; PAGE_INHERITABLE_KEYS.len()],
    )?
    else {
        return Ok(None);
    };
    let mut plan = PageTreePlan {
        root,
        branches: BTreeSet::from([root]),
        owners: BTreeMap::from([(root, root_frame.nearest_owners)]),
        pages: Vec::new(),
        shadowed_entries: Vec::new(),
    };
    let mut pending = vec![root_frame];

    while let Some(frame) = pending.last_mut() {
        if let Some(&kid) = frame.kids.get(frame.next_child) {
            frame.next_child += 1;
            // The two directions of the page tree must agree before we can
            // infer inheritance from /Kids or change a page's /Parent.
            if plan.owners.contains_key(&kid) {
                return Ok(None);
            }
            let Some(child) = document.current_owned_object_cow(kid)? else {
                return Ok(None);
            };
            let OwnedObject::Dictionary(child) = child.as_ref() else {
                return Ok(None);
            };
            if dictionary_reference(child, b"Parent") != Some(frame.node) {
                return Ok(None);
            }
            match page_tree_node_kind(document, child)? {
                Some(PageTreeNodeKind::Leaf) => {
                    let defined = document.effective_page_inheritance_mask(child)?;
                    plan.pages.push(PageTreeLeaf {
                        handle: kid,
                        inherited_from: std::array::from_fn(|index| {
                            if defined[index] {
                                None
                            } else {
                                frame.nearest_owners[index]
                            }
                        }),
                    });
                    plan.owners.insert(
                        kid,
                        nearest_page_tree_owners(kid, defined, frame.nearest_owners),
                    );
                    frame.include_child(defined);
                }
                Some(PageTreeNodeKind::Branch) => {
                    let Some(child_frame) =
                        PageTreePruneFrame::new(document, kid, child, frame.nearest_owners)?
                    else {
                        return Ok(None);
                    };
                    plan.branches.insert(kid);
                    plan.owners.insert(kid, child_frame.nearest_owners);
                    pending.push(child_frame);
                }
                None => return Ok(None),
            }
        } else {
            let Some(completed) = pending.pop() else {
                break;
            };
            let coverage = completed.finish(&mut plan.shadowed_entries);
            if let Some(parent) = pending.last_mut() {
                parent.include_child(coverage);
            }
        }
    }

    Ok(Some(plan))
}

fn apply_shadowed_page_tree_entries(
    document: &mut EditDocument,
    removals: Vec<(ObjectHandle, Vec<Vec<u8>>)>,
) -> Result<usize> {
    let mut removed = 0usize;
    for (node, keys) in removals {
        let Some(dictionary) = document.edit_handle(node)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "validated page-tree node stopped being a dictionary".to_owned(),
            ));
        };
        for key in keys {
            removed = removed.saturating_add(usize::from(dictionary.remove(&key).is_some()));
        }
    }
    Ok(removed)
}

const fn page_tree_required_nodes(mut leaves: usize) -> usize {
    let mut nodes = 1usize;
    while leaves > PAGE_TREE_FANOUT {
        leaves = leaves.div_ceil(PAGE_TREE_FANOUT);
        nodes = nodes.saturating_add(leaves);
    }
    nodes
}

#[derive(Clone, Copy)]
struct PageTreeChild {
    handle: ObjectHandle,
    pages: usize,
}

fn set_page_tree_parent(
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

fn rebuild_compact_page_tree_levels(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    root: ObjectHandle,
) -> Result<Vec<PageTreeChild>> {
    let mut level = pages
        .iter()
        .copied()
        .map(|handle| PageTreeChild { handle, pages: 1 })
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
                set_page_tree_parent(document, child.handle, node)?;
            }
            next.push(PageTreeChild {
                handle: node,
                pages: count,
            });
        }
        level = next;
    }

    for child in &level {
        set_page_tree_parent(document, child.handle, root)?;
    }

    Ok(level)
}

// The validated traversal already knows the nearest effective definitions.
// Materialize each referenced ancestor once, before changing anything.
fn materialize_page_inheritance(
    document: &EditDocument,
    pages: &[PageTreeLeaf],
) -> Result<Vec<(ObjectHandle, OwnedDictionary)>> {
    let mut cached = BTreeMap::<ObjectHandle, OwnedDictionary>::new();
    let mut result = Vec::with_capacity(pages.len());
    for page in pages {
        let mut values = OwnedDictionary::new();
        for (key, owner) in PAGE_INHERITABLE_KEYS.into_iter().zip(page.inherited_from) {
            let Some(owner) = owner else {
                continue;
            };
            let dictionary = match cached.entry(owner) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => {
                    let Some(object) = document.current_owned_object_cow(owner)? else {
                        return Err(Error::Invalid(
                            "validated inheritance owner disappeared".to_owned(),
                        ));
                    };
                    let Some(dictionary) = object.as_dictionary() else {
                        return Err(Error::Invalid(
                            "validated inheritance owner is not a dictionary".to_owned(),
                        ));
                    };
                    entry.insert(
                        PAGE_INHERITABLE_KEYS
                            .into_iter()
                            .filter_map(|key| {
                                dictionary
                                    .get(key)
                                    .map(|value| (key.to_vec(), value.clone()))
                            })
                            .collect(),
                    )
                }
            };
            let Some(value) = dictionary.get(key) else {
                return Err(Error::Invalid(
                    "validated inherited value disappeared".to_owned(),
                ));
            };
            values.insert(key.to_vec(), value.clone());
        }
        result.push((page.handle, values));
    }
    Ok(result)
}

type ProtectedInheritance = BTreeSet<(ObjectHandle, usize)>;

fn protect_inherited_owners(owners: InheritedOwners, protected: &mut ProtectedInheritance) {
    protected.extend(
        owners
            .into_iter()
            .enumerate()
            .filter_map(|(index, owner)| owner.map(|owner| (owner, index))),
    );
}

struct ExternalParentVisitor<'a> {
    owners: &'a BTreeMap<ObjectHandle, InheritedOwners>,
    protected: &'a mut ProtectedInheritance,
}

impl DirectObjectVisitor for ExternalParentVisitor<'_> {
    fn visit_source_stream_length(&self) -> bool {
        false
    }

    fn visit_dictionary(
        &mut self,
        document: &EditDocument,
        root: ObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        if !(path.is_empty() && self.owners.contains_key(&root))
            && let Some(parent) = dictionary.reference(b"Parent")
            && let Some(owners) = self.owners.get(&parent)
        {
            // Only validated member Parent links are covered by the
            // post-order proof. Protect the effective properties needed by
            // every independent boundary, including nested dictionaries.
            let mut needed = *owners;
            for (index, key) in PAGE_INHERITABLE_KEYS.into_iter().enumerate() {
                if needed[index].is_none() {
                    continue;
                }
                let effective = match dictionary {
                    DirectDictionaryView::Owned(dictionary) => match dictionary.get(key) {
                        Some(value) => document.effective_page_inherited_value(key, value)?,
                        None => false,
                    },
                    DirectDictionaryView::Source(dictionary) => match dictionary
                        .get_raw::<HayroObject<'_>>(key)
                    {
                        Some(value) => document
                            .effective_page_inherited_value(key, &owned_from_maybe_ref(value))?,
                        None => false,
                    },
                };
                if effective {
                    needed[index] = None;
                }
            }
            protect_inherited_owners(needed, self.protected);
        }
        Ok(DirectWalkControl::Descend)
    }
}

fn external_parent_dependencies(
    document: &EditDocument,
    plan: &PageTreePlan,
) -> Result<ProtectedInheritance> {
    let mut protected = ProtectedInheritance::new();
    for handle in document.reachable_output_objects()? {
        if !document
            .references_for_handle(handle)?
            .iter()
            .any(|target| plan.owners.contains_key(target))
        {
            continue;
        }
        if let Some(object) = document.current_object(handle)? {
            walk_direct_object(
                document,
                handle,
                object,
                &mut ExternalParentVisitor {
                    owners: &plan.owners,
                    protected: &mut protected,
                },
            )?;
        }
    }
    // Direct trailer dictionaries are not themselves indirect object roots.
    for value in document.trailer().values() {
        for target in value.references() {
            if let Some(owners) = plan.owners.get(&target) {
                protect_inherited_owners(*owners, &mut protected);
            }
        }
    }
    Ok(protected)
}

struct RebuildReferenceVisitor<'a> {
    replaced_key: &'a [u8],
    references: Vec<ObjectHandle>,
}

impl DirectObjectVisitor for RebuildReferenceVisitor<'_> {
    fn visit_source_stream_length(&self) -> bool {
        false
    }

    fn visit_reference(
        &mut self,
        _root: ObjectHandle,
        path: &[DirectPathStep],
        target: ObjectHandle,
    ) -> Result<()> {
        if !matches!(path.first(), Some(DirectPathStep::DictKey(key)) if key == self.replaced_key) {
            self.references.push(target);
        }
        Ok(())
    }
}

// Follow the references surviving replacement of root Kids and live leaf
// Parent, including future children and newly materialized inheritance.
fn retained_page_tree_branches(
    document: &EditDocument,
    plan: &PageTreePlan,
    inherited: &[(ObjectHandle, OwnedDictionary)],
) -> Result<BTreeSet<ObjectHandle>> {
    let mut pending = document.output_roots();
    pending.extend(plan.pages.iter().map(|page| page.handle));
    for (_, dictionary) in inherited {
        pending.extend(dictionary.values().flat_map(OwnedObject::references));
    }
    let mut visited = BTreeSet::new();
    let mut retained = BTreeSet::new();
    while let Some(handle) = pending.pop() {
        if !visited.insert(handle) {
            continue;
        }
        if handle != plan.root && plan.branches.contains(&handle) {
            retained.insert(handle);
        }
        let replaced_key = if handle == plan.root {
            Some(b"Kids".as_slice())
        } else if plan.owners.contains_key(&handle) && !plan.branches.contains(&handle) {
            Some(b"Parent".as_slice())
        } else {
            None
        };
        if let Some(replaced_key) = replaced_key {
            if let Some(object) = document.current_object(handle)? {
                let mut visitor = RebuildReferenceVisitor {
                    replaced_key,
                    references: Vec::new(),
                };
                walk_direct_object(document, handle, object, &mut visitor)?;
                pending.extend(visitor.references);
            }
        } else {
            match document.references_for_handle(handle) {
                Ok(references) => pending.extend(references),
                Err(Error::MissingSourceObject { .. })
                    if matches!(handle, ObjectHandle::Existing(_)) => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(retained)
}

fn remove_unprotected_inheritance(
    document: &mut EditDocument,
    removals: Vec<(ObjectHandle, Vec<Vec<u8>>)>,
    protected: &ProtectedInheritance,
) -> Result<usize> {
    let removals = removals
        .into_iter()
        .filter_map(|(owner, mut keys)| {
            keys.retain(|key| {
                PAGE_INHERITABLE_KEYS
                    .iter()
                    .position(|candidate| *candidate == key.as_slice())
                    .is_none_or(|index| !protected.contains(&(owner, index)))
            });
            (!keys.is_empty()).then_some((owner, keys))
        })
        .collect();
    apply_shadowed_page_tree_entries(document, removals)
}

fn compact_page_tree_from_plan(
    document: &mut EditDocument,
    stats: &mut StructureCompactionStats,
    plan: PageTreePlan,
) -> Result<()> {
    let before_nodes = plan.branches.len();
    let after_nodes = page_tree_required_nodes(plan.pages.len());
    stats.page_tree_nodes_before = before_nodes;
    stats.page_tree_nodes_after = before_nodes;
    let rebuild = !plan.pages.is_empty() && after_nodes < before_nodes;
    if !rebuild && plan.shadowed_entries.is_empty() {
        return Ok(());
    }

    let mut protected = external_parent_dependencies(document, &plan)?;
    let inherited = if rebuild {
        materialize_page_inheritance(document, &plan.pages)?
    } else {
        Vec::new()
    };
    let retained = if rebuild {
        retained_page_tree_branches(document, &plan, &inherited)?
    } else {
        BTreeSet::new()
    };
    for branch in &retained {
        if let Some(owners) = plan.owners.get(branch) {
            protect_inherited_owners(*owners, &mut protected);
        }
    }
    stats.shadowed_page_tree_inherited_entries_removed =
        remove_unprotected_inheritance(document, plan.shadowed_entries, &protected)?;
    if !rebuild || !retained.is_empty() {
        return Ok(());
    }
    stats.page_tree_nodes_after = after_nodes;
    let root = plan.root;
    let pages = plan.pages;

    for (page, values) in inherited {
        let Some(dictionary) = document.edit_handle(page)?.as_dictionary_mut() else {
            return Err(Error::Invalid("validated page disappeared".to_owned()));
        };
        dictionary.extend(values);
    }

    let pages = pages
        .into_iter()
        .map(|page| page.handle)
        .collect::<Vec<_>>();
    let level = rebuild_compact_page_tree_levels(document, &pages, root)?;

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
    for (index, key) in PAGE_INHERITABLE_KEYS.into_iter().enumerate() {
        if !protected.contains(&(root, index)) {
            root_dictionary.remove(key);
        }
    }

    stats.page_tree_pages_reparented = pages.len();
    stats.page_tree_nodes_removed = before_nodes.saturating_sub(after_nodes);
    Ok(())
}

struct NameEntry {
    key: Vec<u8>,
    value: OwnedObject,
}

struct NameLeaf {
    handle: ObjectHandle,
    entries: Vec<NameEntry>,
    changed: bool,
}

struct NameTreeSnapshot {
    root: ObjectHandle,
    node_count: usize,
    entry_count: usize,
    leaves: Vec<NameLeaf>,
}

impl NameLeaf {
    fn from_names(handle: ObjectHandle, names: Vec<OwnedObject>) -> Option<Self> {
        if names.len() % 2 != 0 {
            return None;
        }
        let mut entries = Vec::with_capacity(names.len() / 2);
        let mut names = names.into_iter();
        while let Some(key) = names.next() {
            let OwnedObject::String(key) = key else {
                return None;
            };
            entries.push(NameEntry {
                key,
                value: names.next()?,
            });
        }
        Some(Self {
            handle,
            entries,
            changed: false,
        })
    }
}

fn collect_name_tree(
    document: &EditDocument,
    root: ObjectHandle,
) -> Result<Option<NameTreeSnapshot>> {
    let mut nodes = BTreeSet::new();
    let mut leaves = Vec::new();
    let mut entry_count = 0;
    let mut pending = vec![root];

    while let Some(handle) = pending.pop() {
        if !nodes.insert(handle) {
            return Ok(None);
        }
        let Some(mut dictionary) = current_dictionary(document, handle)? else {
            return Ok(None);
        };

        if let Some(OwnedObject::Array(names)) = dictionary.remove(b"Names".as_slice()) {
            let Some(leaf) = NameLeaf::from_names(handle, names) else {
                return Ok(None);
            };
            entry_count += leaf.entries.len();
            leaves.push(leaf);
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

    Ok(Some(NameTreeSnapshot {
        root,
        node_count: nodes.len(),
        entry_count,
        leaves,
    }))
}

fn inline_destination_objects(
    document: &EditDocument,
    snapshot: &mut NameTreeSnapshot,
) -> Result<(usize, usize)> {
    let mut wrappers_inlined = 0usize;
    let mut arrays_inlined = 0usize;
    for leaf in &mut snapshot.leaves {
        for entry in &mut leaf.entries {
            let OwnedObject::Reference(target) = entry.value else {
                continue;
            };
            let Some(object) = document.current_owned_object_cow(target)? else {
                continue;
            };
            match object.as_ref() {
                OwnedObject::Array(_) => {
                    arrays_inlined = arrays_inlined.saturating_add(1);
                }
                OwnedObject::Dictionary(dictionary)
                    if dictionary.len() == 1 && dictionary.contains_key(b"D".as_slice()) =>
                {
                    wrappers_inlined = wrappers_inlined.saturating_add(1);
                }
                _ => continue,
            }
            entry.value = object.into_owned();
            leaf.changed = true;
        }
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

fn names_array(entries: Vec<NameEntry>) -> Vec<OwnedObject> {
    let mut out = Vec::with_capacity(entries.len().saturating_mul(2));
    for entry in entries {
        out.push(OwnedObject::String(entry.key));
        out.push(entry.value);
    }
    out
}

#[derive(Clone)]
struct NameChild {
    handle: ObjectHandle,
    first: Vec<u8>,
    last: Vec<u8>,
}

fn replace_name_tree(document: &mut EditDocument, snapshot: NameTreeSnapshot) -> Result<usize> {
    // Only a repack needs a global sort. Move the leaf entries into that
    // representation rather than retaining a duplicate tree of COS values.
    let mut entries = Vec::with_capacity(snapshot.entry_count);
    for leaf in snapshot.leaves {
        entries.extend(leaf.entries);
    }
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    if entries.len() <= NAME_TREE_FANOUT {
        let Some(root) = document.edit_handle(snapshot.root)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "name-tree root is not a dictionary".to_owned(),
            ));
        };
        root.clear();
        root.insert(b"Names".to_vec(), OwnedObject::Array(names_array(entries)));
        return Ok(1);
    }

    let mut level = Vec::with_capacity(entries.len().div_ceil(NAME_TREE_FANOUT));
    let mut entries = entries.into_iter();
    while entries.len() != 0 {
        let chunk = entries.by_ref().take(NAME_TREE_FANOUT).collect::<Vec<_>>();
        let first = chunk.first().map_or_default(|entry| entry.key.clone());
        let last = chunk.last().map_or_default(|entry| entry.key.clone());
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
            let first = chunk.first().map_or_default(|child| child.first.clone());
            let last = chunk.last().map_or_default(|child| child.last.clone());
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

fn apply_leaf_inlining(document: &mut EditDocument, snapshot: NameTreeSnapshot) -> Result<()> {
    for leaf in snapshot.leaves.into_iter().filter(|leaf| leaf.changed) {
        let Some(dictionary) = document.edit_handle(leaf.handle)?.as_dictionary_mut() else {
            return Err(Error::Invalid(
                "name-tree leaf is not a dictionary".to_owned(),
            ));
        };
        dictionary.insert(
            b"Names".to_vec(),
            OwnedObject::Array(names_array(leaf.entries)),
        );
    }
    Ok(())
}

fn compact_catalog_name_trees(
    document: &mut EditDocument,
    stats: &mut StructureCompactionStats,
) -> Result<bool> {
    let catalog_handle = ObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = current_dictionary(document, catalog_handle)? else {
        return Ok(false);
    };
    let Some(names_handle) = dictionary_reference(&catalog, b"Names") else {
        return Ok(false);
    };
    let Some(names_dictionary) = current_dictionary(document, names_handle)? else {
        return Ok(false);
    };

    let roots = names_dictionary
        .iter()
        .filter_map(|(kind, value)| match value {
            OwnedObject::Reference(root) => Some((kind.clone(), *root)),
            _ => None,
        })
        .collect::<Vec<_>>();

    let mut changed = false;
    for (kind, root) in roots {
        let Some(mut snapshot) = collect_name_tree(document, root)? else {
            continue;
        };
        let before = snapshot.node_count;
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

        let required = name_tree_required_nodes(snapshot.entry_count);
        stats.name_tree_nodes_before = stats.name_tree_nodes_before.saturating_add(before);

        if required < before {
            let after = replace_name_tree(document, snapshot)?;
            changed = true;
            stats.name_trees_repacked = stats.name_trees_repacked.saturating_add(1);
            stats.name_tree_nodes_after = stats.name_tree_nodes_after.saturating_add(after);
            stats.name_tree_nodes_removed = stats
                .name_tree_nodes_removed
                .saturating_add(before.saturating_sub(after));
        } else {
            if wrappers_inlined != 0 || arrays_inlined != 0 {
                apply_leaf_inlining(document, snapshot)?;
                changed = true;
            }
            stats.name_tree_nodes_after = stats.name_tree_nodes_after.saturating_add(before);
        }
    }
    Ok(changed)
}

pub fn compact_structure(document: &mut EditDocument) -> Result<StructureCompactionStats> {
    // Destructive destination rewrites and page compaction use the same
    // strictly validated membership evidence, never reader recovery pages.
    let mut plan = plan_page_tree(document)?;
    let mut stats = StructureCompactionStats::default();
    if let Some(plan) = &plan {
        stats.stale_page_destination_references_rewritten =
            canonicalize_stale_page_destinations_from_plan(document, plan)?;
    }

    let names_changed = compact_catalog_name_trees(document, &mut stats)?;
    if names_changed || stats.stale_page_destination_references_rewritten != 0 {
        // Name trees may alias other COS objects. Revalidate after an actual
        // intervening mutation, including changes that keep reference edges
        // stable; the reachability cache generation alone is insufficient.
        plan = plan_page_tree(document)?;
    }
    if let Some(plan) = plan {
        compact_page_tree_from_plan(document, &mut stats, plan)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{content::replace_page_content, test_support::ClassicPdfBuilder};

    fn stale_page_destination_map(
        document: &EditDocument,
    ) -> Result<BTreeMap<ObjectHandle, ObjectHandle>> {
        plan_page_tree(document)?.map_or_else(
            || Ok(BTreeMap::new()),
            |plan| stale_page_destination_map_from_plan(document, &plan),
        )
    }

    fn canonicalize_stale_page_destinations(document: &mut EditDocument) -> Result<usize> {
        plan_page_tree(document)?.map_or(Ok(0), |plan| {
            canonicalize_stale_page_destinations_from_plan(document, &plan)
        })
    }

    fn compact_page_tree(
        document: &mut EditDocument,
        stats: &mut StructureCompactionStats,
    ) -> Result<()> {
        if let Some(plan) = plan_page_tree(document)? {
            compact_page_tree_from_plan(document, stats, plan)?;
        }
        Ok(())
    }

    fn prune_shadowed_page_tree_inheritance(document: &mut EditDocument) -> Result<usize> {
        let Some(plan) = plan_page_tree(document)? else {
            return Ok(0);
        };
        let protected = external_parent_dependencies(document, &plan)?;
        remove_unprotected_inheritance(document, plan.shadowed_entries, &protected)
    }

    fn ordered_page_tree_fixture() -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [5 0 R 3 0 R] /Count 2 /Rotate 0 >>",
        )?;
        pdf.object(
            3,
            b"<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 1 /Rotate 90 >>",
        )?;
        pdf.object(4, b"<< /Type /Page /Parent 3 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(
            5,
            b"<< /Type 7 0 R /Parent 2 0 R /Kids 8 0 R /Count 1 /Rotate 180 >>",
        )?;
        pdf.object(6, b"<< /Type /Page /Parent 5 0 R /MediaBox [0 0 20 20] >>")?;
        pdf.object(7, b"/Pages")?;
        pdf.object(8, b"[6 0 R]")?;
        pdf.object(9, b"<< /Type /NotAPageTreeNode >>")?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    const fn existing(number: u32) -> ObjectHandle {
        ObjectHandle::Existing(crate::ObjectId::new(number, 0))
    }

    fn malformed_inheritance_fixture(nested: bool, invalid: &str) -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        let kid = if nested { 5 } else { 3 };
        pdf.object(2, format!("<< /Type /Pages /Kids [{kid} 0 R] /Count 1 /Resources << /Marker /Root >> /MediaBox [0 0 30 40] /CropBox [1 2 20 30] /Rotate 90 >>").as_bytes())?;
        let parent = if nested { 5 } else { 2 };
        pdf.object(3, format!("<< /Type /Page /Parent {parent} 0 R /Resources {invalid} /MediaBox {invalid} /CropBox {invalid} /Rotate {invalid} /Contents 4 0 R >>").as_bytes())?;
        pdf.stream(4, b"", b"q Q")?;
        if nested {
            pdf.object(5, b"<< /Type /Pages /Parent 2 0 R /Kids [3 0 R] /Count 1 /Rotate 180 /Resources null >>")?;
        }
        pdf.object(7, b"null")?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn page_tree_pruning_ignores_ineffective_local_inheritance_entries() -> Result<()> {
        for invalid in ["null", "7 0 R", "/WrongType", "[null 0 1 1]"] {
            let mut document = malformed_inheritance_fixture(false, invalid)?;
            let before =
                PAGE_INHERITABLE_KEYS.map(|key| document.inherited_page_value(existing(3), key));
            let Some(plan) = plan_page_tree(&document)? else {
                return Err(Error::Invalid("valid fixture topology rejected".to_owned()));
            };
            assert!(plan.shadowed_entries.is_empty(), "{invalid}");
            assert_eq!(plan.pages[0].inherited_from, [Some(existing(2)); 4]);
            let mut stats = StructureCompactionStats::default();
            compact_page_tree(&mut document, &mut stats)?;
            assert_eq!(stats.shadowed_page_tree_inherited_entries_removed, 0);
            for (key, expected) in PAGE_INHERITABLE_KEYS.into_iter().zip(before) {
                let expected = expected?;
                assert!(expected.is_some());
                assert_eq!(document.inherited_page_value(existing(3), key)?, expected);
            }
        }
        Ok(())
    }

    #[test]
    fn page_tree_rebuild_materializes_nearest_effective_owners() -> Result<()> {
        let mut document = malformed_inheritance_fixture(true, "7 0 R")?;
        let before =
            PAGE_INHERITABLE_KEYS.map(|key| document.inherited_page_value(existing(3), key));
        let Some(plan) = plan_page_tree(&document)? else {
            return Err(Error::Invalid("valid fixture topology rejected".to_owned()));
        };
        assert_eq!(
            plan.pages[0].inherited_from,
            [
                Some(existing(2)),
                Some(existing(2)),
                Some(existing(2)),
                Some(existing(5))
            ]
        );
        let mut stats = StructureCompactionStats::default();
        compact_page_tree(&mut document, &mut stats)?;
        assert_eq!(stats.page_tree_nodes_removed, 1);
        assert_eq!(stats.shadowed_page_tree_inherited_entries_removed, 1);
        let Some(leaf) = current_dictionary(&document, existing(3))? else {
            return Err(Error::Invalid("missing rebuilt leaf".to_owned()));
        };
        for (key, expected) in PAGE_INHERITABLE_KEYS.into_iter().zip(before) {
            let expected = expected?;
            assert!(expected.is_some());
            assert_eq!(leaf.get(key), expected.as_ref());
        }
        assert_eq!(
            leaf.get(b"Rotate".as_slice()),
            Some(&OwnedObject::Integer(180))
        );
        Ok(())
    }

    fn detached_inheritance_fixture(nested: bool, parent: u32) -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /OpenAction [9 0 R /Fit] >>",
        )?;
        let child = if nested { 5 } else { 3 };
        pdf.object(
            2,
            format!(
                "<< /Type /Pages /Kids [{child} 0 R] /Count 1 /Rotate 90 /MediaBox [0 0 10 20] >>"
            )
            .as_bytes(),
        )?;
        pdf.object(3, format!("<< /Type /Page /Parent {child_parent} 0 R /Rotate 180 /MediaBox [0 0 20 10] /Contents 4 0 R >>", child_parent = if nested { 5 } else { 2 }).as_bytes())?;
        if nested {
            pdf.object(
                5,
                b"<< /Type /Pages /Parent 2 0 R /Kids [3 0 R] /Count 1 /Rotate 270 >>",
            )?;
        }
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(
            9,
            format!("<< /Type /Page /Parent {parent} 0 R /MediaBox [0 0 30 40] /Contents 4 0 R >>")
                .as_bytes(),
        )?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn detached_parent_preserves_only_needed_root_inheritance() -> Result<()> {
        for nested in [false, true] {
            let mut document = detached_inheritance_fixture(nested, 2)?;
            let before = document.inherited_page_value(existing(9), b"Rotate")?;
            assert_eq!(before, Some(OwnedObject::Integer(90)));
            let stats = compact_structure(&mut document)?;
            assert_eq!(stats.stale_page_destination_references_rewritten, 0);
            assert_eq!(stats.page_tree_nodes_removed, usize::from(nested));
            assert_eq!(stats.page_tree_nodes_after, 1);
            assert_eq!(document.page_handles()?, vec![existing(3)]);
            assert_eq!(
                document.inherited_page_value(existing(9), b"Rotate")?,
                before
            );
            assert_eq!(
                document.inherited_page_value(existing(3), b"Rotate")?,
                Some(OwnedObject::Integer(180))
            );
            let Some(root) = current_dictionary(&document, existing(2))? else {
                return Err(Error::Invalid("missing fixture root".to_owned()));
            };
            assert_eq!(root.get(b"Rotate".as_slice()), before.as_ref());
            assert!(!root.contains_key(b"MediaBox".as_slice()));
        }
        Ok(())
    }

    #[test]
    fn retained_intermediate_branch_prevents_inconsistent_rebuild() -> Result<()> {
        let mut document = detached_inheritance_fixture(true, 5)?;
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.page_tree_nodes_before, 2);
        assert_eq!(stats.page_tree_nodes_after, 2);
        assert_eq!(stats.page_tree_nodes_removed, 0);
        assert_eq!(stats.page_tree_pages_reparented, 0);
        assert_eq!(
            document.inherited_page_value(existing(9), b"Rotate")?,
            Some(OwnedObject::Integer(270))
        );
        let Some(leaf) = current_dictionary(&document, existing(3))? else {
            return Err(Error::Invalid("missing fixture page".to_owned()));
        };
        assert_eq!(dictionary_reference(&leaf, b"Parent"), Some(existing(5)));
        Ok(())
    }

    #[test]
    fn independently_reachable_indirect_kids_keep_old_branches() -> Result<()> {
        let mut document = ordered_page_tree_fixture()?;
        let root = ObjectHandle::New(document.add_object(OwnedObject::Array(vec![
            OwnedObject::Reference(existing(5)),
            OwnedObject::Reference(existing(3)),
        ])));
        let Some(tree) = document.edit_handle(existing(2))?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture tree".to_owned()));
        };
        tree.insert(b"Kids".to_vec(), OwnedObject::Reference(root));
        let Some(catalog) = document.edit_handle(existing(1))?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture catalog".to_owned()));
        };
        catalog.insert(b"ExternalKids".to_vec(), OwnedObject::Reference(root));
        let mut stats = StructureCompactionStats::default();
        compact_page_tree(&mut document, &mut stats)?;
        assert_eq!(stats.page_tree_nodes_after, 3);
        assert_eq!(stats.page_tree_nodes_removed, 0);
        assert_eq!(document.page_handles()?, vec![existing(6), existing(4)]);
        Ok(())
    }

    #[test]
    fn compaction_uses_validated_order_and_indirect_kids() -> Result<()> {
        let mut document = ordered_page_tree_fixture()?;
        let mut stats = StructureCompactionStats::default();
        compact_page_tree(&mut document, &mut stats)?;
        assert_eq!(stats.page_tree_nodes_before, 3);
        assert_eq!(stats.page_tree_nodes_after, 1);
        assert_eq!(stats.page_tree_nodes_removed, 2);
        assert_eq!(stats.page_tree_pages_reparented, 2);
        assert_eq!(stats.shadowed_page_tree_inherited_entries_removed, 1);
        assert_eq!(document.page_handles()?, vec![existing(6), existing(4)]);
        for (page, rotation) in [(existing(6), 180), (existing(4), 90)] {
            let Some(dictionary) = current_dictionary(&document, page)? else {
                return Err(Error::Invalid("compacted page missing".to_owned()));
            };
            assert_eq!(
                dictionary_reference(&dictionary, b"Parent"),
                Some(existing(2))
            );
            assert_eq!(
                dictionary.get(b"Rotate".as_slice()),
                Some(&OwnedObject::Integer(rotation))
            );
        }
        Ok(())
    }

    #[test]
    fn malformed_page_tree_never_prunes_or_rebuilds_from_recovery_pages() -> Result<()> {
        let edits = [
            (
                2,
                b"Kids".as_slice(),
                OwnedObject::Array(
                    [5, 3, 9]
                        .map(|n| OwnedObject::Reference(existing(n)))
                        .to_vec(),
                ),
            ),
            (
                3,
                b"Kids".as_slice(),
                OwnedObject::Array(vec![OwnedObject::Reference(existing(4)); 2]),
            ),
            (6, b"Parent".as_slice(), OwnedObject::Reference(existing(3))),
            (
                5,
                b"Kids".as_slice(),
                OwnedObject::Array(vec![OwnedObject::Reference(existing(2))]),
            ),
        ];
        for (number, key, value) in edits {
            let mut document = ordered_page_tree_fixture()?;
            let Some(dictionary) = document.edit_handle(existing(number))?.as_dictionary_mut()
            else {
                return Err(Error::Invalid("fixture tree node missing".to_owned()));
            };
            dictionary.insert(key.to_vec(), value);
            let before = [2, 3, 4, 5, 6].map(|n| document.current_owned_object(existing(n)));
            let mut stats = StructureCompactionStats::default();
            compact_page_tree(&mut document, &mut stats)?;
            assert_eq!(stats, StructureCompactionStats::default());
            for (n, before) in [2, 3, 4, 5, 6].into_iter().zip(before) {
                assert_eq!(document.current_owned_object(existing(n))?, before?);
            }
        }
        Ok(())
    }

    fn stale_destination_fixture(ambiguous: bool, destination_mode: &[u8]) -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        let open_action = format!("[9 0 R /{}]", String::from_utf8_lossy(destination_mode));
        pdf.object(
            1,
            format!("<< /Type /Catalog /Pages 2 0 R /OpenAction {open_action} >>").as_bytes(),
        )?;
        if ambiguous {
            pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 6 0 R] /Count 2 >>")?;
        } else {
            pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        }
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources <<>> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"0 0 m 1 1 l S")?;
        if ambiguous {
            pdf.object(
                6,
                b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources <<>> /Contents 4 0 R >>",
            )?;
        }
        pdf.object(8, b"<< /Type /Pages /Kids [9 0 R] /Count 1 >>")?;
        pdf.object(
            9,
            b"<< /Type /Page /Parent 8 0 R /MediaBox [0 0 10 10] /Resources <<>> /Contents 4 0 R >>",
        )?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn drops_shadowed_root_resources_without_rebuilding_page_tree() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject << /Old 5 0 R >> >> >>",
        )?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /XObject << /New 6 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /Marker /Old >>")?;
        pdf.object(6, b"<< /Marker /New >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;

        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.page_tree_nodes_before, 1);
        assert_eq!(stats.page_tree_nodes_after, 1);
        assert_eq!(stats.shadowed_page_tree_inherited_entries_removed, 1);

        let root = current_dictionary(
            &document,
            ObjectHandle::Existing(crate::ObjectId::new(2, 0)),
        )?
        .ok_or_else(|| Error::Invalid("page-tree root is missing".to_owned()))?;
        assert!(!root.contains_key(b"Resources".as_slice()));
        let reachable = document.reachable_objects()?;
        assert!(!reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(5, 0))));
        assert!(reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(6, 0))));
        Ok(())
    }

    #[test]
    fn keeps_inherited_resources_when_any_descendant_relies_on_them() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [3 0 R 7 0 R] /Count 2 /Resources << /XObject << /Old 5 0 R >> >> >>",
        )?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /XObject << /New 6 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /Marker /Old >>")?;
        pdf.object(6, b"<< /Marker /New >>")?;
        pdf.object(
            7,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 8 0 R >>",
        )?;
        pdf.stream(8, b"", b"q Q")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;

        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.shadowed_page_tree_inherited_entries_removed, 0);
        let root = current_dictionary(
            &document,
            ObjectHandle::Existing(crate::ObjectId::new(2, 0)),
        )?
        .ok_or_else(|| Error::Invalid("page-tree root is missing".to_owned()))?;
        assert!(root.contains_key(b"Resources".as_slice()));
        assert!(
            document
                .reachable_objects()?
                .contains(&ObjectHandle::Existing(crate::ObjectId::new(5, 0)))
        );
        Ok(())
    }

    #[test]
    fn drops_shadowed_inheritance_across_nested_page_tree() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject << /Old 5 0 R >> >> >>",
        )?;
        pdf.object(
            3,
            b"<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 1 /Resources << /XObject << /New 6 0 R >> >> >>",
        )?;
        pdf.object(
            4,
            b"<< /Type /Page /Parent 3 0 R /MediaBox [0 0 10 10] /Contents 7 0 R >>",
        )?;
        pdf.object(5, b"<< /Marker /Old >>")?;
        pdf.object(6, b"<< /Marker /New >>")?;
        pdf.stream(7, b"", b"q Q")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;

        assert_eq!(prune_shadowed_page_tree_inheritance(&mut document)?, 1);
        let root = current_dictionary(
            &document,
            ObjectHandle::Existing(crate::ObjectId::new(2, 0)),
        )?
        .ok_or_else(|| Error::Invalid("page-tree root is missing".to_owned()))?;
        let child = current_dictionary(
            &document,
            ObjectHandle::Existing(crate::ObjectId::new(3, 0)),
        )?
        .ok_or_else(|| Error::Invalid("page-tree child is missing".to_owned()))?;
        assert!(!root.contains_key(b"Resources".as_slice()));
        assert!(child.contains_key(b"Resources".as_slice()));
        let reachable = document.reachable_objects()?;
        assert!(!reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(5, 0))));
        assert!(reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(6, 0))));
        Ok(())
    }

    #[test]
    fn empty_page_tree_does_not_vacuously_shadow_inherited_keys() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /Rotate 90 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let root = ObjectHandle::Existing(crate::ObjectId::new(2, 0));
        let Some(dictionary) = document.edit_handle(root)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing page-tree root".to_owned()));
        };
        dictionary.insert(b"Kids".to_vec(), OwnedObject::Array(Vec::new()));

        assert_eq!(prune_shadowed_page_tree_inheritance(&mut document)?, 0);
        let Some(dictionary) = current_dictionary(&document, root)? else {
            return Err(Error::Invalid("missing page-tree root".to_owned()));
        };
        assert_eq!(
            dictionary.get(b"Rotate".as_slice()),
            Some(&OwnedObject::Integer(90))
        );
        Ok(())
    }

    #[test]
    fn deep_page_tree_pruning_uses_bounded_call_stack() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 /Rotate 90 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /Rotate 180 /MediaBox [0 0 10 10] >>",
        )?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let root = ObjectHandle::Existing(crate::ObjectId::new(2, 0));
        let leaf = ObjectHandle::Existing(crate::ObjectId::new(3, 0));

        // Individual COS dictionaries remain shallow, but the indirect page
        // tree can exceed the stack available to recursively visit each node.
        let mut parent = root;
        for _ in 0..20_000 {
            let node = ObjectHandle::New(document.add_object(OwnedObject::Dictionary(
                OwnedDictionary::from([
                    (b"Type".to_vec(), OwnedObject::Name(b"Pages".to_vec())),
                    (b"Kids".to_vec(), OwnedObject::Array(Vec::new())),
                    (b"Count".to_vec(), OwnedObject::Integer(1)),
                    (b"Parent".to_vec(), OwnedObject::Reference(parent)),
                ]),
            )));
            let Some(dictionary) = document.edit_handle(parent)?.as_dictionary_mut() else {
                return Err(Error::Invalid("missing page-tree parent".to_owned()));
            };
            dictionary.insert(
                b"Kids".to_vec(),
                OwnedObject::Array(vec![OwnedObject::Reference(node)]),
            );
            parent = node;
        }
        let Some(dictionary) = document.edit_handle(parent)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing page-tree tail".to_owned()));
        };
        dictionary.insert(
            b"Kids".to_vec(),
            OwnedObject::Array(vec![OwnedObject::Reference(leaf)]),
        );
        let Some(dictionary) = document.edit_handle(leaf)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing page-tree leaf".to_owned()));
        };
        dictionary.insert(b"Parent".to_vec(), OwnedObject::Reference(parent));

        assert_eq!(prune_shadowed_page_tree_inheritance(&mut document)?, 1);
        let Some(root_dictionary) = current_dictionary(&document, root)? else {
            return Err(Error::Invalid("missing page-tree root".to_owned()));
        };
        assert!(!root_dictionary.contains_key(b"Rotate".as_slice()));
        assert_eq!(
            document.inherited_page_value(leaf, b"Rotate")?,
            Some(OwnedObject::Integer(180))
        );
        Ok(())
    }

    #[test]
    fn malformed_page_tree_aborts_shadow_pruning_transaction() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [3 0 R 9 0 R] /Count 2 /Resources << /XObject << /Root 5 0 R >> >> >>",
        )?;
        pdf.object(
            3,
            b"<< /Type /Pages /Parent 2 0 R /Kids [4 0 R] /Count 1 /Resources << /XObject << /Middle 6 0 R >> >> >>",
        )?;
        pdf.object(
            4,
            b"<< /Type /Page /Parent 3 0 R /MediaBox [0 0 10 10] /Resources << /XObject << /Leaf 7 0 R >> >> /Contents 8 0 R >>",
        )?;
        pdf.object(5, b"<< /Marker /Root >>")?;
        pdf.object(6, b"<< /Marker /Middle >>")?;
        pdf.object(7, b"<< /Marker /Leaf >>")?;
        pdf.stream(8, b"", b"q Q")?;
        pdf.object(9, b"<< /Type /NotAPageTreeNode >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;

        assert_eq!(prune_shadowed_page_tree_inheritance(&mut document)?, 0);
        let child = current_dictionary(
            &document,
            ObjectHandle::Existing(crate::ObjectId::new(3, 0)),
        )?
        .ok_or_else(|| Error::Invalid("page-tree child is missing".to_owned()))?;
        assert!(child.contains_key(b"Resources".as_slice()));

        // The sibling is now a /Pages node pointing back to the root. The
        // first subtree would justify removing /Resources from its parent,
        // but a later cycle must abort the entire batch without partial edits.
        let cycle = ObjectHandle::Existing(crate::ObjectId::new(9, 0));
        let root = ObjectHandle::Existing(crate::ObjectId::new(2, 0));
        *document.edit_handle(cycle)? = OwnedObject::Dictionary(OwnedDictionary::from([
            (b"Type".to_vec(), OwnedObject::Name(b"Pages".to_vec())),
            (b"Parent".to_vec(), OwnedObject::Reference(root)),
            (
                b"Kids".to_vec(),
                OwnedObject::Array(vec![OwnedObject::Reference(root)]),
            ),
        ]));
        assert_eq!(prune_shadowed_page_tree_inheritance(&mut document)?, 0);
        let Some(child) = current_dictionary(
            &document,
            ObjectHandle::Existing(crate::ObjectId::new(3, 0)),
        )?
        else {
            return Err(Error::Invalid("page-tree child is missing".to_owned()));
        };
        assert!(child.contains_key(b"Resources".as_slice()));
        Ok(())
    }

    #[test]
    fn canonicalizes_stale_page_destination_after_live_content_rewrite() -> Result<()> {
        let mut document = stale_destination_fixture(false, b"Fit")?;
        let live_page = ObjectHandle::Existing(crate::ObjectId::new(3, 0));
        replace_page_content(&mut document, live_page, b"q Q".to_vec())?;

        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.stale_page_destination_references_rewritten, 1);

        let catalog = current_dictionary(
            &document,
            ObjectHandle::Existing(document.source().catalog_id()),
        )?
        .ok_or_else(|| crate::Error::Invalid("catalog is missing".to_owned()))?;
        assert!(matches!(
            catalog.get(b"OpenAction".as_slice()),
            Some(OwnedObject::Array(destination))
                if matches!(destination.first(), Some(OwnedObject::Reference(target)) if *target == live_page)
        ));
        let reachable = document.reachable_objects()?;
        assert!(!reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(8, 0))));
        assert!(!reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(9, 0))));
        assert!(!reachable.contains(&ObjectHandle::Existing(crate::ObjectId::new(4, 0))));
        Ok(())
    }

    fn mixed_destination_fixture() -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /OpenAction [9 0 R /Fit] /Private 5 0 R /Other 6 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /Nested [[9 0 R /FitH 1] [9 0 R 7 0 R]] /NonDest [9 0 R /Other] /Length [9 0 R /Fit] >>")?;
        pdf.object(6, b"<< /NonDest [9 0 R /Other] >>")?;
        pdf.object(7, b"/Fit")?;
        pdf.object(8, b"<< /Type /Pages /Kids [9 0 R] /Count 1 >>")?;
        pdf.object(
            9,
            b"<< /Type /Page /Parent 8 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn stale_destination_edits_preserve_stream_payload_and_nonmatching_arrays() -> Result<()> {
        let mut document = mixed_destination_fixture()?;

        let payload = vec![42; 1024 * 1024];
        let payload_ptr = payload.as_ptr();
        let stream = ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary: OwnedDictionary::from([
                (
                    b"Dest".to_vec(),
                    OwnedObject::Array(vec![
                        OwnedObject::Reference(existing(9)),
                        OwnedObject::Name(b"XYZ".to_vec()),
                    ]),
                ),
                (b"Unchanged".to_vec(), OwnedObject::String(vec![1; 4096])),
            ]),
            data: crate::StreamData::Owned(payload),
        }));
        let Some(catalog) = document.edit_handle(existing(1))?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture catalog".to_owned()));
        };
        catalog.insert(b"Stream".to_vec(), OwnedObject::Reference(stream));
        assert_eq!(canonicalize_stale_page_destinations(&mut document)?, 4);
        let Some(object) = document.current_owned_object_cow(stream)? else {
            return Err(Error::Invalid("missing fixture stream".to_owned()));
        };
        let OwnedObject::Stream {
            dictionary,
            data: crate::StreamData::Owned(bytes),
        } = object.as_ref()
        else {
            return Err(Error::Invalid("wrong fixture stream".to_owned()));
        };
        assert_eq!(bytes.as_ptr(), payload_ptr);
        assert!(matches!(
            dictionary.get(b"Dest".as_slice()),
            Some(OwnedObject::Array(values)) if values.first() == Some(&OwnedObject::Reference(existing(3)))
        ));
        let Some(private) = document.current_owned_object(existing(5))? else {
            return Err(Error::Invalid("missing fixture private object".to_owned()));
        };
        let destination =
            |path: &[DirectPathStep]| crate::direct_object::object_at_direct_path(&private, path);
        for (path, target) in [
            (
                vec![
                    DirectPathStep::DictKey(b"Nested".to_vec()),
                    DirectPathStep::ArrayIndex(0),
                    DirectPathStep::ArrayIndex(0),
                ],
                3,
            ),
            (
                vec![
                    DirectPathStep::DictKey(b"Nested".to_vec()),
                    DirectPathStep::ArrayIndex(1),
                    DirectPathStep::ArrayIndex(0),
                ],
                9,
            ),
            (
                vec![
                    DirectPathStep::DictKey(b"NonDest".to_vec()),
                    DirectPathStep::ArrayIndex(0),
                ],
                9,
            ),
            (
                vec![
                    DirectPathStep::DictKey(b"Length".to_vec()),
                    DirectPathStep::ArrayIndex(0),
                ],
                3,
            ),
        ] {
            assert_eq!(
                destination(&path),
                Some(&OwnedObject::Reference(existing(target)))
            );
        }
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(6, 0))
                .is_none()
        );
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(4, 0))
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn stale_destination_visitor_handles_deep_owned_containers() -> Result<()> {
        let document = stale_destination_fixture(false, b"Fit")?;
        let redirects = BTreeMap::from([(existing(9), existing(3))]);
        let mut visitor = StaleDestinationVisitor {
            redirects: &redirects,
            edits: Vec::new(),
        };
        let depth = 20_000;
        let mut value = OwnedObject::Array(vec![
            OwnedObject::Reference(existing(9)),
            OwnedObject::Name(b"Fit".to_vec()),
        ]);
        for _ in 0..depth {
            value = OwnedObject::Array(vec![value]);
        }
        walk_direct_object(
            &document,
            existing(1),
            CurrentObject::Owned(&value),
            &mut visitor,
        )?;
        assert_eq!(visitor.edits.len(), 1);
        assert_eq!(visitor.edits[0].path.len(), depth);
        assert!(
            visitor.edits[0]
                .path
                .iter()
                .all(|step| *step == DirectPathStep::ArrayIndex(0))
        );
        // Dismantle the synthetic deep value iteratively as well: its
        // derived destructor is independent of the visitor's stack use.
        for _ in 0..depth {
            let OwnedObject::Array(mut children) = value else {
                return Err(Error::Invalid("unexpected fixture shape".to_owned()));
            };
            value = children
                .pop()
                .ok_or_else(|| Error::Invalid("missing fixture child".to_owned()))?;
        }
        Ok(())
    }

    #[test]
    fn stale_destination_rewrite_requires_validated_live_membership() -> Result<()> {
        let mut document = stale_destination_fixture(false, b"Fit")?;
        let Some(root) = document.edit_handle(existing(2))?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture root".to_owned()));
        };
        root.insert(
            b"Kids".to_vec(),
            OwnedObject::Array(vec![
                OwnedObject::Reference(existing(3)),
                OwnedObject::Reference(existing(3)),
            ]),
        );
        // The reader recovers one page, but duplicated membership is not
        // proof sufficient for redirecting a detached page to that target.
        assert_eq!(document.page_handles()?, vec![existing(3)]);
        assert!(plan_page_tree(&document)?.is_none());
        assert!(stale_page_destination_map(&document)?.is_empty());
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats, StructureCompactionStats::default());
        assert!(document.reachable_objects()?.contains(&existing(9)));
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(1, 0))
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn stale_page_destination_mapping_rejects_ambiguous_live_pages() -> Result<()> {
        let mut document = stale_destination_fixture(true, b"Fit")?;
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.stale_page_destination_references_rewritten, 0);
        assert!(
            document
                .reachable_objects()?
                .contains(&ObjectHandle::Existing(crate::ObjectId::new(9, 0)))
        );
        Ok(())
    }

    fn inherited_destination_fixture(
        live_parent: &str,
        stale_parent: &str,
        live_leaf: &str,
        stale_leaf: &str,
    ) -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /OpenAction [9 0 R /Fit] >>",
        )?;
        pdf.object(
            2,
            format!("<< /Type /Pages /Kids [3 0 R] /Count 1 {live_parent} >>").as_bytes(),
        )?;
        pdf.object(
            3,
            format!("<< /Type /Page /Parent 2 0 R /Contents 4 0 R {live_leaf} >>").as_bytes(),
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(
            8,
            format!("<< /Type /Pages /Kids [9 0 R] /Count 1 {stale_parent} >>").as_bytes(),
        )?;
        pdf.object(
            9,
            format!("<< /Type /Page /Parent 8 0 R /Contents 4 0 R {stale_leaf} >>").as_bytes(),
        )?;
        pdf.object(10, b"<< /Type /Pages /Parent 8 0 R >>")?;
        pdf.object(11, b"null")?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn stale_page_identity_includes_all_local_semantics() -> Result<()> {
        for (live, stale) in [
            (
                "/Annots [<< /Subtype /Text /Contents (one) >>]",
                "/Annots [<< /Subtype /Text /Contents (two) >>]",
            ),
            (
                "/AA << /O << /S /Named /N /NextPage >> >>",
                "/AA << /O << /S /Named /N /PrevPage >> >>",
            ),
            ("/StructParents 1", "/StructParents 2"),
            ("/Tabs /R", "/Tabs /C"),
            ("/Metadata 10 0 R", "/Metadata 11 0 R"),
            ("/PrivateExtension (one)", "/PrivateExtension (two)"),
            ("", "/PrivateExtension true"),
        ] {
            let mut document =
                inherited_destination_fixture("/Rotate 90", "/Rotate 90", live, stale)?;
            assert!(
                stale_page_destination_map(&document)?.is_empty(),
                "{live} vs {stale}"
            );
            assert_eq!(canonicalize_stale_page_destinations(&mut document)?, 0);
        }
        Ok(())
    }

    #[test]
    fn stale_page_identity_indexes_equal_local_dicts_and_effective_inheritance() -> Result<()> {
        let document = inherited_destination_fixture(
            "/Rotate 90 /Resources <<>>",
            "/Rotate 180",
            "/PrivateExtension << /Z [1.25 -0.0] /A (same) >>",
            "/Rotate 90 /Resources <<>> /PrivateExtension << /A (same) /Z [1.25 0] >>",
        )?;
        assert_eq!(
            stale_page_destination_map(&document)?,
            BTreeMap::from([(existing(9), existing(3))])
        );
        let Some(identity) = source_page_identity(&document, existing(3))? else {
            return Err(Error::Invalid("missing source identity".to_owned()));
        };
        let mut index = LivePageIdentityIndex::default();
        index.insert(existing(3), identity.clone())?;
        // A colliding bucket entry must never be eligible merely because of its hash.
        let fingerprint = LivePageIdentityIndex::fingerprint(&identity)?;
        index.by_fingerprint.entry(fingerprint).or_default().insert(
            0,
            LivePageIdentity {
                properties: OwnedObject::Dictionary(OwnedDictionary::new()),
                unique_page: Some(existing(10)),
            },
        );
        assert_eq!(index.unique_match(&identity)?, Some(existing(3)));
        index.insert(existing(12), identity.clone())?;
        assert_eq!(index.unique_match(&identity)?, None);
        assert_eq!(
            index.by_fingerprint.get(&fingerprint).map(Vec::len),
            Some(2)
        );
        Ok(())
    }

    #[test]
    fn stale_destination_identity_checks_every_inherited_key() -> Result<()> {
        for (live_parent, stale_parent) in [
            (
                "/Resources << /Font <<>> >>",
                "/Resources << /XObject <<>> >>",
            ),
            ("/MediaBox [0 0 10 20]", "/MediaBox [0 0 20 10]"),
            ("/CropBox [0 0 5 5]", "/CropBox [0 0 9 9]"),
            ("/Rotate 90", "/Rotate 180"),
        ] {
            let document = inherited_destination_fixture(live_parent, stale_parent, "", "")?;
            assert!(
                stale_page_destination_map(&document)?.is_empty(),
                "different inherited values must not match: {live_parent}, {stale_parent}"
            );
        }
        Ok(())
    }

    #[test]
    fn stale_destination_identity_follows_inheritance_past_invalid_local_values() -> Result<()> {
        for local in ["/Rotate null", "/Rotate 11 0 R", "/Rotate /Invalid"] {
            let document =
                inherited_destination_fixture("/Rotate 90", "/Rotate 180", local, local)?;
            assert!(
                stale_page_destination_map(&document)?.is_empty(),
                "invalid local rotation must not hide different inherited rotations: {local}"
            );
        }
        Ok(())
    }

    #[test]
    fn stale_destination_identity_uses_nearest_original_inheritance() -> Result<()> {
        let attributes = "/Resources <<>> /MediaBox [0 0 10 20] /CropBox [0 0 5 5] /Rotate 90";
        let mut document = inherited_destination_fixture(
            attributes,
            "/Resources <<>> /MediaBox [0 0 10 20] /CropBox [0 0 5 5] /Rotate 180",
            "/Rotate null",
            "/Rotate 90",
        )?;
        // Despite distinct storage and a shadowed ancestor, the source pages
        // have the same inherited values. An earlier edit must not affect that
        // immutable evidence.
        let Some(parent) = document.edit_handle(existing(2))?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing live-page parent".to_owned()));
        };
        parent.insert(b"Rotate".to_vec(), OwnedObject::Integer(270));
        replace_page_content(&mut document, existing(3), b"0 0 m 1 1 l S".to_vec())?;
        assert_eq!(
            stale_page_destination_map(&document)?,
            BTreeMap::from([(existing(9), existing(3))])
        );
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.stale_page_destination_references_rewritten, 1);
        Ok(())
    }

    #[test]
    fn stale_destination_identity_rejects_unresolved_ancestry() -> Result<()> {
        for parent in ["/Parent 10 0 R", "/Parent 99 0 R", "/Parent 4 0 R"] {
            let document = inherited_destination_fixture("", parent, "", "")?;
            assert!(source_page_identity(&document, existing(9))?.is_none());
        }
        Ok(())
    }

    #[test]
    fn stale_page_destination_mapping_requires_same_page_identity() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /OpenAction [9 0 R /Fit] >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources <<>> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"0 0 m 1 1 l S")?;
        pdf.object(8, b"<< /Type /Pages /Kids [9 0 R] /Count 1 >>")?;
        pdf.object(9, b"<< /Type /Page /Parent 8 0 R /MediaBox [0 0 20 10] /Resources <<>> /Contents 4 0 R >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.stale_page_destination_references_rewritten, 0);
        assert!(
            document
                .reachable_objects()?
                .contains(&ObjectHandle::Existing(crate::ObjectId::new(9, 0)))
        );
        Ok(())
    }

    #[test]
    fn stale_page_destination_rewrite_requires_standard_destination_mode() -> Result<()> {
        let mut document = stale_destination_fixture(false, b"NotADest")?;
        let stats = compact_structure(&mut document)?;
        assert_eq!(stats.stale_page_destination_references_rewritten, 0);
        assert!(
            document
                .reachable_objects()?
                .contains(&ObjectHandle::Existing(crate::ObjectId::new(9, 0)))
        );
        Ok(())
    }

    fn destination_tree_fixture(leaf_sizes: &[usize]) -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Names 7 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(7, b"<< /Dests 8 0 R >>")?;
        pdf.object(9, b"[3 0 R /Fit]")?;
        pdf.object(10, b"<< /D [3 0 R /Fit] >>")?;
        let mut kids = Vec::new();
        let mut offset = 0;
        for (&size, id) in leaf_sizes.iter().zip(20_u32..) {
            let entries = (offset..offset + size)
                .map(|index| {
                    let value = match index {
                        0 => "9 0 R",
                        1 => "10 0 R",
                        _ => "[3 0 R /Fit]",
                    };
                    format!("(key{index:03}) {value}")
                })
                .collect::<Vec<_>>()
                .join(" ");
            pdf.object(id, format!("<< /Names [{entries}] >>").as_bytes())?;
            kids.push(format!("{id} 0 R"));
            offset += size;
        }
        pdf.object(8, format!("<< /Kids [{}] >>", kids.join(" ")).as_bytes())?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    #[test]
    fn name_tree_inlining_edits_only_changed_leaves() -> Result<()> {
        let mut document = destination_tree_fixture(&[33, 32])?;
        let mut stats = StructureCompactionStats::default();
        assert!(compact_catalog_name_trees(&mut document, &mut stats)?);
        assert_eq!(stats.name_tree_nodes_before, 3);
        assert_eq!(stats.name_tree_nodes_after, 3);
        assert_eq!(stats.name_trees_repacked, 0);
        assert_eq!(stats.named_destination_arrays_inlined, 1);
        assert_eq!(stats.named_destination_wrappers_inlined, 1);
        let mut second = StructureCompactionStats::default();
        assert!(!compact_catalog_name_trees(&mut document, &mut second)?);
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(20, 0))
                .is_some()
        );
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(21, 0))
                .is_none()
        );
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(8, 0))
                .is_none()
        );
        let Some(tree) = collect_name_tree(&document, existing(8))? else {
            return Err(Error::Invalid("missing inlined name tree".to_owned()));
        };
        assert_eq!(tree.entry_count, 65);
        let Some(leaf) = tree.leaves.first() else {
            return Err(Error::Invalid("missing inlined leaf".to_owned()));
        };
        assert!(matches!(
            leaf.entries.first(),
            Some(NameEntry {
                value: OwnedObject::Array(_),
                ..
            })
        ));
        assert!(
            matches!(leaf.entries.get(1), Some(NameEntry { value: OwnedObject::Dictionary(dictionary), .. }) if dictionary.contains_key(b"D".as_slice()))
        );
        Ok(())
    }

    #[test]
    fn name_tree_repack_moves_entries_and_preserves_order_and_limits() -> Result<()> {
        let mut document = destination_tree_fixture(&[22, 22, 21])?;
        let Some(mut snapshot) = collect_name_tree(&document, existing(8))? else {
            return Err(Error::Invalid("missing original name tree".to_owned()));
        };
        assert_eq!(snapshot.node_count, 4);
        assert_eq!(snapshot.entry_count, 65);
        assert_eq!(
            inline_destination_objects(&document, &mut snapshot)?,
            (1, 1)
        );

        // Repacking must move the key allocations, even across a global sort.
        let mut keys = snapshot
            .leaves
            .iter()
            .flat_map(|leaf| &leaf.entries)
            .map(|entry| (entry.key.clone(), entry.key.as_ptr()))
            .collect::<BTreeMap<_, _>>();
        snapshot.leaves.reverse();
        assert_eq!(replace_name_tree(&mut document, snapshot)?, 3);

        let Some(root) = current_dictionary(&document, existing(8))? else {
            return Err(Error::Invalid("missing rebuilt root".to_owned()));
        };
        let Some(OwnedObject::Array(kids)) = root.get(b"Kids".as_slice()) else {
            return Err(Error::Invalid("missing rebuilt name-tree kids".to_owned()));
        };
        assert_eq!(kids.len(), 2);
        let mut index = 0;
        for kid in kids {
            let OwnedObject::Reference(handle) = kid else {
                return Err(Error::Invalid("invalid rebuilt child".to_owned()));
            };
            let Some(object) = document.current_owned_object_cow(*handle)? else {
                return Err(Error::Invalid("missing rebuilt child".to_owned()));
            };
            let Some(dictionary) = object.as_dictionary() else {
                return Err(Error::Invalid("invalid rebuilt dictionary".to_owned()));
            };
            let Some(OwnedObject::Array(names)) = dictionary.get(b"Names".as_slice()) else {
                return Err(Error::Invalid("missing name-tree leaf entries".to_owned()));
            };
            let first = format!("key{index:03}").into_bytes();
            for pair in names.as_chunks::<2>().0 {
                let OwnedObject::String(key) = &pair[0] else {
                    return Err(Error::Invalid("invalid rebuilt name-tree key".to_owned()));
                };
                assert_eq!(key, format!("key{index:03}").as_bytes());
                assert_eq!(keys.remove(key), Some(key.as_ptr()));
                if index == 1 {
                    assert!(
                        matches!(&pair[1], OwnedObject::Dictionary(dictionary) if dictionary.contains_key(b"D".as_slice()))
                    );
                } else {
                    assert_eq!(
                        pair[1],
                        OwnedObject::Array(vec![
                            OwnedObject::Reference(existing(3)),
                            OwnedObject::Name(b"Fit".to_vec())
                        ])
                    );
                }
                index += 1;
            }
            let last = format!("key{:03}", index - 1).into_bytes();
            assert_eq!(
                dictionary.get(b"Limits".as_slice()),
                Some(&OwnedObject::Array(vec![
                    OwnedObject::String(first),
                    OwnedObject::String(last)
                ]))
            );
        }
        assert_eq!(index, 65);
        assert!(keys.is_empty());
        Ok(())
    }

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
