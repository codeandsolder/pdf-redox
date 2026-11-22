use crate::direct_object::{
    DirectDictionaryView, DirectObjectTarget, DirectObjectVisitor, DirectPathStep,
    DirectWalkControl, value_at_direct_path, walk_direct_output_objects,
};
use crate::geometry::{Matrix, Rectangle};
use crate::{
    AnnotationPolicy, EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary,
    OwnedObject, PreservationConfig, Result, StreamData,
    content::resolved_number_array,
    source::{CurrentArrayIter, CurrentValue},
};
use std::collections::{BTreeMap, BTreeSet};

const SCREEN_HIDDEN_ANNOTATION_FLAGS: i64 = 0x02 | 0x20;
#[derive(Debug, Clone, Copy, Default)]
pub struct PreservationScanHints {
    pub no_piece_info: bool,
    pub no_authoring_metadata: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PreservationStats {
    pub pages: usize,
    pub annotation_entries_seen: usize,
    pub annotation_entries_flattened: usize,
    pub annotation_entries_dropped_unflattened: usize,
    pub link_visual_shells_retained: usize,
    pub annotation_subtypes_seen: BTreeMap<String, usize>,
    pub unflattened_annotation_subtypes: BTreeMap<String, usize>,
    pub dropped_page_keys: BTreeMap<String, usize>,
    pub dropped_page_tree_keys: BTreeMap<String, usize>,
    pub dropped_catalog_keys: BTreeMap<String, usize>,
    pub dropped_authoring_keys: BTreeMap<String, usize>,
    pub spliced_unknown_wrapper_keys: BTreeMap<String, usize>,
}

fn owned_name(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<Vec<u8>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

struct PendingPageTreeValue<'a> {
    target: DirectObjectTarget,
    value: CurrentValue<'a>,
}

struct PreservationArrayFrame<'a> {
    target: DirectObjectTarget,
    children: std::iter::Enumerate<CurrentArrayIter<'a>>,
}

impl<'a> PreservationArrayFrame<'a> {
    fn next_child(&mut self) -> Option<PendingPageTreeValue<'a>> {
        let (index, value) = self.children.next()?;
        let mut path = self.target.path.clone();
        path.push(DirectPathStep::ArrayIndex(index));
        Some(PendingPageTreeValue {
            target: DirectObjectTarget {
                root: self.target.root,
                path,
            },
            value,
        })
    }
}

fn collect_page_tree_targets<'a>(
    document: &'a EditDocument,
    initial: PendingPageTreeValue<'a>,
    pages: &mut BTreeSet<DirectObjectTarget>,
    nodes: &mut BTreeSet<DirectObjectTarget>,
) -> Result<()> {
    let mut seen = BTreeSet::new();
    let mut arrays = Vec::<PreservationArrayFrame<'a>>::new();
    let mut next = Some(initial);
    while let Some(PendingPageTreeValue { mut target, value }) = next.take().or_else(|| {
        loop {
            match arrays.last_mut()?.next_child() {
                Some(child) => break Some(child),
                None => {
                    arrays.pop();
                }
            }
        }
    }) {
        if let Some(handle) = value.reference() {
            if seen.insert(handle)
                && let Some(value) = document.current_value(handle)?
            {
                next = Some(PendingPageTreeValue {
                    target: DirectObjectTarget {
                        root: handle,
                        path: Vec::new(),
                    },
                    value,
                });
            }
        } else if value.is_dictionary() {
            let kind = match value.dictionary_entry(b"Type") {
                Some(kind) => document.resolve_current_value(kind)?,
                None => None,
            };
            match kind.as_ref().and_then(CurrentValue::name) {
                Some(b"Pages") => {
                    nodes.insert(target.clone());
                    if let Some(value) = value.dictionary_entry(b"Kids") {
                        target.path.push(DirectPathStep::DictKey(b"Kids".to_vec()));
                        next = Some(PendingPageTreeValue { target, value });
                    }
                }
                Some(b"Page") => {
                    pages.insert(target);
                }
                _ => {}
            }
        } else if let Some(children) = value.array_items() {
            arrays.push(PreservationArrayFrame {
                target,
                children: children.enumerate(),
            });
        }
    }
    Ok(())
}

fn preservation_page_tree_targets(
    document: &EditDocument,
) -> Result<(Vec<DirectObjectTarget>, Vec<DirectObjectTarget>)> {
    let catalog_handle = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog_object) = document.current_value(catalog_handle)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let path = vec![DirectPathStep::DictKey(b"Pages".to_vec())];
    let Some(pages_value) = value_at_direct_path(catalog_object, &path) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut pages = BTreeSet::new();
    let mut nodes = BTreeSet::new();
    collect_page_tree_targets(
        document,
        PendingPageTreeValue {
            target: DirectObjectTarget {
                root: catalog_handle,
                path,
            },
            value: pages_value,
        },
        &mut pages,
        &mut nodes,
    )?;
    for id in document.source().page_ids() {
        pages.insert(DirectObjectTarget {
            root: CowObjectHandle::Existing(id),
            path: Vec::new(),
        });
    }
    Ok((pages.into_iter().collect(), nodes.into_iter().collect()))
}

enum AnnotationEntries<'a> {
    Array(CurrentArrayIter<'a>),
    Malformed,
}

fn page_annotation_entries<'a>(
    document: &'a EditDocument,
    page: &DirectObjectTarget,
) -> Result<Option<AnnotationEntries<'a>>> {
    let Some(page_object) = page.current(document)? else {
        return Ok(None);
    };
    let Some(annots) = page_object.dictionary_entry(b"Annots") else {
        return Ok(None);
    };
    let entries = document
        .resolve_current_value(annots)?
        .and_then(|value| value.array_items())
        .map_or(AnnotationEntries::Malformed, AnnotationEntries::Array);
    Ok(Some(entries))
}

fn page_annotations(
    document: &EditDocument,
    page: &DirectObjectTarget,
) -> Result<Option<Vec<OwnedObject>>> {
    Ok(
        page_annotation_entries(document, page)?.map(|entries| match entries {
            AnnotationEntries::Array(items) => {
                items.map(|value| value.into_cow().into_owned()).collect()
            }
            AnnotationEntries::Malformed => Vec::new(),
        }),
    )
}

fn replace_page_annotations(
    document: &mut EditDocument,
    page: &DirectObjectTarget,
    annotations: Vec<OwnedObject>,
) -> Result<()> {
    let Some(dictionary) = page.dictionary_mut(document)? else {
        return Ok(());
    };
    if annotations.is_empty() {
        dictionary.remove(b"Annots".as_slice());
    } else {
        dictionary.insert(b"Annots".to_vec(), OwnedObject::Array(annotations));
    }
    Ok(())
}

fn current_annotation_subtype(
    document: &EditDocument,
    annotation: CurrentValue<'_>,
) -> Result<Option<Vec<u8>>> {
    let Some(annotation) = document.resolve_current_value(annotation)? else {
        return Ok(None);
    };
    let Some(subtype) = annotation.dictionary_entry(b"Subtype") else {
        return Ok(None);
    };
    Ok(document
        .resolve_current_value(subtype)?
        .and_then(|value| value.name().map(<[u8]>::to_vec)))
}

fn annotation_subtype(
    document: &EditDocument,
    annotation: &OwnedObject,
) -> Result<Option<Vec<u8>>> {
    current_annotation_subtype(document, CurrentValue::Owned(annotation))
}

fn annotation_is_protected(
    document: &EditDocument,
    annotation: &OwnedObject,
    policy: &PreservationConfig,
) -> Result<bool> {
    Ok(match annotation_subtype(document, annotation)?.as_deref() {
        Some(b"Link") => policy.links,
        Some(b"Widget") => policy.forms,
        _ => false,
    })
}

fn annotation_subtypes(
    document: &EditDocument,
    pages: &[DirectObjectTarget],
) -> Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for page in pages {
        let Some(entries) = page_annotation_entries(document, page)? else {
            continue;
        };
        let AnnotationEntries::Array(annotations) = entries else {
            *counts.entry("(non-array /Annots)".to_owned()).or_default() += 1;
            continue;
        };
        for annotation in annotations {
            let label = current_annotation_subtype(document, annotation)?.map_or_else(
                || "(missing/non-name subtype)".to_owned(),
                |name| format!("/{}", String::from_utf8_lossy(&name)),
            );
            *counts.entry(label).or_default() += 1;
        }
    }
    Ok(counts)
}

fn filter_annotations(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
    policy: &PreservationConfig,
    preserve_other_annotations: bool,
) -> Result<usize> {
    let mut kept_total = 0;
    for page in pages {
        let Some(items) = page_annotations(document, page)? else {
            continue;
        };
        let mut kept = Vec::new();
        for annotation in items {
            let subtype = annotation_subtype(document, &annotation)?;
            let protected = annotation_is_protected(document, &annotation, policy)?;
            let explicitly_disabled = matches!(subtype.as_deref(), Some(b"Link")) && !policy.links
                || matches!(subtype.as_deref(), Some(b"Widget")) && !policy.forms;
            if protected || (preserve_other_annotations && !explicitly_disabled) {
                kept.push(annotation);
            }
        }
        kept_total += kept.len();
        replace_page_annotations(document, page, kept)?;
    }
    Ok(kept_total)
}

fn detach_protected_annotations(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
    policy: &PreservationConfig,
) -> Result<Vec<Vec<OwnedObject>>> {
    let mut protected_by_page = Vec::with_capacity(pages.len());
    for page in pages {
        let Some(items) = page_annotations(document, page)? else {
            protected_by_page.push(Vec::new());
            continue;
        };
        let mut protected = Vec::new();
        let mut processable = Vec::new();
        for annotation in items {
            if annotation_is_protected(document, &annotation, policy)? {
                protected.push(annotation);
            } else {
                processable.push(annotation);
            }
        }
        replace_page_annotations(document, page, processable)?;
        protected_by_page.push(protected);
    }
    Ok(protected_by_page)
}

fn restore_protected_annotations(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
    protected_by_page: Vec<Vec<OwnedObject>>,
) -> Result<()> {
    for (page, protected) in pages.iter().zip(protected_by_page) {
        if protected.is_empty() {
            continue;
        }
        let mut items = page_annotations(document, page)?.unwrap_or_default();
        items.extend(protected);
        replace_page_annotations(document, page, items)?;
    }
    Ok(())
}

fn retain_link_visual_shells(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
) -> Result<usize> {
    const LINK_VISUAL_KEYS: &[&[u8]] = &[b"Rect", b"Border", b"BS", b"C", b"F", b"CA"];
    let mut retained = 0;
    for page in pages {
        let Some(items) = page_annotations(document, page)? else {
            continue;
        };
        let mut shells = Vec::new();
        for annotation in items {
            let Some(resolved) = document.resolve_owned_value(&annotation)? else {
                continue;
            };
            let Some(dictionary) = resolved.as_dictionary() else {
                continue;
            };
            if owned_name(document, dictionary.get(b"Subtype".as_slice()))?.as_deref()
                != Some(b"Link")
            {
                continue;
            }
            let mut shell = OwnedDictionary::new();
            shell.insert(b"Type".to_vec(), OwnedObject::Name(b"Annot".to_vec()));
            shell.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Link".to_vec()));
            for &key in LINK_VISUAL_KEYS {
                if let Some(value) = dictionary.get(key) {
                    shell.insert(key.to_vec(), value.clone());
                }
            }
            shells.push(OwnedObject::Dictionary(shell));
            retained += 1;
        }
        replace_page_annotations(document, page, shells)?;
    }
    Ok(retained)
}

const fn keep_page_key(key: &[u8], policy: &PreservationConfig) -> bool {
    if matches!(
        key,
        b"Type"
            | b"MediaBox"
            | b"CropBox"
            | b"BleedBox"
            | b"TrimBox"
            | b"ArtBox"
            | b"Rotate"
            | b"UserUnit"
            | b"Resources"
            | b"Contents"
            | b"Group"
            | b"Annots"
            | b"Parent"
    ) {
        return true;
    }
    match key {
        b"StructParents" | b"Tabs" => policy.structure,
        b"B" => policy.navigation,
        b"Dur" | b"Trans" | b"AA" => policy.viewer_preferences,
        b"Thumb" => false,
        b"PieceInfo" => policy.private_authoring_data,
        b"Metadata" | b"LastModified" => policy.metadata,
        b"SeparationInfo" | b"PresSteps" => policy.output_intents,
        _ => policy.unknown_objects,
    }
}

const fn keep_page_tree_key(key: &[u8], policy: &PreservationConfig) -> bool {
    match key {
        b"Type" | b"Parent" | b"Kids" | b"Count" | b"Resources" | b"MediaBox" | b"CropBox"
        | b"Rotate" => true,
        b"Thumb" => false,
        b"PieceInfo" => policy.private_authoring_data,
        b"Metadata" | b"LastModified" => policy.metadata,
        b"StructParents" | b"Tabs" => policy.structure,
        b"B" => policy.navigation,
        b"Dur" | b"Trans" | b"AA" => policy.viewer_preferences,
        b"SeparationInfo" | b"PresSteps" => policy.output_intents,
        _ => policy.unknown_objects,
    }
}

const fn keep_catalog_key(key: &[u8], policy: &PreservationConfig) -> bool {
    match key {
        b"Type" | b"Pages" | b"Version" | b"Extensions" => true,
        b"AcroForm" => policy.forms,
        b"Outlines" | b"Names" | b"Dests" | b"PageLabels" | b"OpenAction" | b"Threads" => {
            policy.navigation
        }
        b"OCProperties" => policy.optional_content,
        b"StructTreeRoot" | b"MarkInfo" | b"Lang" => policy.structure,
        b"OutputIntents" => policy.output_intents,
        b"ViewerPreferences" | b"PageMode" | b"PageLayout" => policy.viewer_preferences,
        b"PieceInfo" => policy.private_authoring_data,
        b"Metadata" | b"LastModified" => policy.metadata,
        _ => policy.unknown_objects,
    }
}

struct AuthoringRemovalVisitor {
    key: &'static [u8],
    removals: BTreeMap<DirectObjectTarget, BTreeSet<Vec<u8>>>,
}

impl DirectObjectVisitor for AuthoringRemovalVisitor {
    fn visit_dictionary(
        &mut self,
        document: &EditDocument,
        root: CowObjectHandle,
        path: &[DirectPathStep],
        dictionary: DirectDictionaryView<'_, '_>,
    ) -> Result<DirectWalkControl> {
        let remove_key = dictionary.raw_value(self.key).is_some();
        let remove_modified = self.key == b"Metadata"
            && path.is_empty()
            && dictionary.raw_value(b"LastModified").is_some()
            && document.stream_subtype_is(root, b"Form")?;
        if remove_key || remove_modified {
            let keys = self
                .removals
                .entry(DirectObjectTarget {
                    root,
                    path: path.to_vec(),
                })
                .or_default();
            if remove_key {
                keys.insert(self.key.to_vec());
            }
            if remove_modified {
                keys.insert(b"LastModified".to_vec());
            }
        }
        Ok(DirectWalkControl::Descend)
    }

    fn visit_source_stream_length(&self) -> bool {
        false
    }

    fn descend_dictionary_value(&self, key: &[u8]) -> bool {
        key != self.key
    }
}

fn drop_authoring_key(
    document: &mut EditDocument,
    key: &'static [u8],
    stats: &mut PreservationStats,
) -> Result<()> {
    // Discover against the original reachable graph before editing. A removed
    // direct subtree is skipped, but independently reachable indirect targets
    // remain separate roots, preserving the existing removal counts.
    let mut visitor = AuthoringRemovalVisitor {
        key,
        removals: BTreeMap::new(),
    };
    walk_direct_output_objects(document, &mut visitor)?;
    apply_authoring_removals(document, visitor.removals, stats)
}

fn drop_authoring_metadata(
    document: &mut EditDocument,
    stats: &mut PreservationStats,
) -> Result<()> {
    drop_authoring_key(document, b"Metadata", stats)
}

fn apply_authoring_removals(
    document: &mut EditDocument,
    removals: BTreeMap<DirectObjectTarget, BTreeSet<Vec<u8>>>,
    stats: &mut PreservationStats,
) -> Result<()> {
    for (target, keys) in removals {
        let Some(dictionary) = target.dictionary_mut(document)? else {
            continue;
        };
        for key in keys {
            if dictionary.remove(key.as_slice()).is_some() {
                *stats
                    .dropped_authoring_keys
                    .entry(format!("/{}", String::from_utf8_lossy(&key)))
                    .or_default() += 1;
            }
        }
    }
    Ok(())
}

/// Drop producer-private authoring state such as `/PieceInfo` throughout the
/// reachable object graph while leaving ordinary document metadata intact.
pub fn drop_private_authoring_data(
    document: &mut EditDocument,
    stats: &mut PreservationStats,
) -> Result<()> {
    drop_authoring_key(document, b"PieceInfo", stats)
}

fn prune_dictionary_target(
    document: &mut EditDocument,
    target: &DirectObjectTarget,
    keep: impl Fn(&[u8]) -> bool,
    splice_unknown_wrappers: bool,
    stats: &mut BTreeMap<String, usize>,
    spliced: &mut BTreeMap<String, usize>,
) -> Result<()> {
    let Some(snapshot) = target.current(document)? else {
        return Ok(());
    };
    let Some(keys) = snapshot.dictionary_keys() else {
        return Ok(());
    };
    let remove = keys
        .iter()
        .filter(|key| !keep(key))
        .cloned()
        .collect::<Vec<_>>();
    if remove.is_empty() {
        return Ok(());
    }

    let mut promotions = Vec::new();
    if splice_unknown_wrappers {
        let existing = keys.into_iter().collect::<BTreeSet<_>>();
        let mut planned = BTreeSet::new();
        for wrapper_key in &remove {
            let Some(wrapper) = snapshot.dictionary_entry(wrapper_key) else {
                continue;
            };
            let Some(resolved) = document.resolve_current_value(wrapper)? else {
                continue;
            };
            let Some(children) = resolved.dictionary_keys() else {
                continue;
            };
            for child_key in children {
                if keep(&child_key)
                    && !existing.contains(&child_key)
                    && planned.insert(child_key.clone())
                    && let Some(child_value) = resolved.dictionary_entry(&child_key)
                {
                    *spliced
                        .entry(format!(
                            "/{} -> /{}",
                            String::from_utf8_lossy(wrapper_key),
                            String::from_utf8_lossy(&child_key)
                        ))
                        .or_default() += 1;
                    promotions.push((child_key, child_value.into_cow().into_owned()));
                }
            }
        }
    }

    let Some(dictionary) = target.dictionary_mut(document)? else {
        return Ok(());
    };
    for key in remove {
        if dictionary.remove(key.as_slice()).is_some() {
            *stats
                .entry(format!("/{}", String::from_utf8_lossy(&key)))
                .or_default() += 1;
        }
    }
    for (key, value) in promotions {
        dictionary.entry(key).or_insert(value);
    }
    Ok(())
}

#[cfg(test)]
fn inherited_page_value(
    document: &EditDocument,
    page: &DirectObjectTarget,
    key: &[u8],
) -> Result<Option<OwnedObject>> {
    let Some(object) = page.current(document)? else {
        return Ok(None);
    };
    Ok(document
        .inherited_page_value_from_view(object, key)?
        .map(|value| value.into_cow().into_owned()))
}

fn ensure_indirect_owned(document: &mut EditDocument, object: OwnedObject) -> CowObjectHandle {
    match object {
        OwnedObject::Reference(handle) => handle,
        object => CowObjectHandle::New(document.add_object(object)),
    }
}

fn new_content_stream(document: &mut EditDocument, data: Vec<u8>) -> CowObjectHandle {
    CowObjectHandle::New(document.add_object(OwnedObject::Stream {
        dictionary: OwnedDictionary::new(),
        data: StreamData::Owned(data),
    }))
}

#[derive(Debug, Clone)]
enum AppearanceSource {
    Handle(CowObjectHandle),
    Direct(OwnedObject),
}

fn stream_source_from_value(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<AppearanceSource>> {
    Ok(match value {
        OwnedObject::Reference(handle) => match document.current_owned_object(*handle)? {
            Some(OwnedObject::Stream { .. }) => Some(AppearanceSource::Handle(*handle)),
            _ => None,
        },
        OwnedObject::Stream { .. } => Some(AppearanceSource::Direct(value.clone())),
        _ => None,
    })
}

fn selected_normal_appearance(
    document: &EditDocument,
    annotation: &OwnedDictionary,
) -> Result<(bool, Option<AppearanceSource>)> {
    let Some(ap_value) = annotation.get(b"AP".as_slice()) else {
        return Ok((false, None));
    };
    let Some(ap_object) = document.resolve_owned_value(ap_value)? else {
        return Ok((false, None));
    };
    let Some(ap) = ap_object.as_dictionary() else {
        return Ok((true, None));
    };
    let Some(normal) = ap.get(b"N".as_slice()) else {
        return Ok((true, None));
    };
    if let Some(stream) = stream_source_from_value(document, normal)? {
        return Ok((true, Some(stream)));
    }
    let Some(normal_object) = document.resolve_owned_value(normal)? else {
        return Ok((true, None));
    };
    let Some(states) = normal_object.as_dictionary() else {
        return Ok((true, None));
    };
    let Some(state) = owned_name(document, annotation.get(b"AS".as_slice()))? else {
        return Ok((true, None));
    };
    let Some(value) = states.get(state.as_slice()) else {
        return Ok((true, None));
    };
    Ok((true, stream_source_from_value(document, value)?))
}

fn annotation_flags(document: &EditDocument, annotation: &OwnedDictionary) -> Result<i64> {
    let Some(value) = annotation.get(b"F".as_slice()) else {
        return Ok(0);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => value,
        _ => 0,
    })
}

fn acroform_need_appearances(document: &EditDocument) -> Result<bool> {
    let catalog = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = document.current_owned_object(catalog)? else {
        return Ok(false);
    };
    let Some(dictionary) = catalog.as_dictionary() else {
        return Ok(false);
    };
    let Some(acroform) = dictionary.get(b"AcroForm".as_slice()) else {
        return Ok(false);
    };
    let Some(acroform) = document.resolve_owned_value(acroform)? else {
        return Ok(false);
    };
    let Some(dictionary) = acroform.as_dictionary() else {
        return Ok(false);
    };
    let Some(value) = dictionary.get(b"NeedAppearances".as_slice()) else {
        return Ok(false);
    };
    Ok(matches!(
        document.resolve_owned_value(value)?,
        Some(OwnedObject::Boolean(true))
    ))
}

fn appearance_as_form(
    document: &mut EditDocument,
    source: AppearanceSource,
) -> Result<CowObjectHandle> {
    match source {
        AppearanceSource::Handle(handle) => {
            let object = document.edit_handle(handle)?;
            let Some(dictionary) = object.as_dictionary_mut() else {
                return Err(Error::Invalid(
                    "annotation appearance is not a stream".to_owned(),
                ));
            };
            dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec()));
            Ok(handle)
        }
        AppearanceSource::Direct(mut object) => {
            let Some(dictionary) = object.as_dictionary_mut() else {
                return Err(Error::Invalid(
                    "direct annotation appearance is not a stream".to_owned(),
                ));
            };
            dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec()));
            Ok(CowObjectHandle::New(document.add_object(object)))
        }
    }
}

fn normalized_rectangle(document: &EditDocument, value: &OwnedObject) -> Result<Option<Rectangle>> {
    let Some([x0, y0, x1, y1]) = resolved_number_array::<4>(document, value)? else {
        return Ok(None);
    };
    Ok(Some(Rectangle::new(
        x0.min(x1),
        y0.min(y1),
        x0.max(x1),
        y0.max(y1),
    )))
}

fn appearance_matrix(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<Matrix> {
    let Some(value) = dictionary.get(b"Matrix".as_slice()) else {
        return Ok(Matrix::default());
    };
    Ok(resolved_number_array::<6>(document, value)?.map_or_default(Matrix::from))
}

fn appearance_content(
    document: &EditDocument,
    annotation: &OwnedDictionary,
    appearance: CowObjectHandle,
    resource_name: &str,
    rotate: i32,
    flags: i64,
) -> Result<Vec<u8>> {
    let Some(appearance_object) = document.current_owned_object(appearance)? else {
        return Ok(Vec::new());
    };
    let Some(appearance_dictionary) = appearance_object.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(bbox_value) = appearance_dictionary.get(b"BBox".as_slice()) else {
        return Ok(Vec::new());
    };
    let Some(bbox) = normalized_rectangle(document, bbox_value)? else {
        return Ok(Vec::new());
    };
    let Some(rect_value) = annotation.get(b"Rect".as_slice()) else {
        return Ok(Vec::new());
    };
    let Some(rect) = normalized_rectangle(document, rect_value)? else {
        return Ok(Vec::new());
    };
    let matrix = appearance_matrix(document, appearance_dictionary)?;
    let do_rotate = rotate != 0 && (flags & 0x10) != 0;
    let (rect, matrix) = if do_rotate {
        let mut rotated_matrix = Matrix::default();
        rotated_matrix.rotatex90(rotate);
        rotated_matrix.concat(matrix);
        let rect_width = rect.urx - rect.llx;
        let rect_height = rect.ury - rect.lly;
        let rotated_rect = match rotate {
            90 => Rectangle::new(
                rect.llx,
                rect.ury,
                rect.llx + rect_height,
                rect.ury + rect_width,
            ),
            180 => Rectangle::new(
                rect.llx - rect_width,
                rect.ury,
                rect.llx,
                rect.ury + rect_height,
            ),
            270 => Rectangle::new(
                rect.llx - rect_height,
                rect.ury - rect_width,
                rect.llx,
                rect.ury,
            ),
            _ => rect,
        };
        (rotated_rect, rotated_matrix)
    } else {
        (rect, matrix)
    };
    let transformed_bbox = matrix.transform_rectangle(bbox);
    let width = transformed_bbox.urx - transformed_bbox.llx;
    let height = transformed_bbox.ury - transformed_bbox.lly;
    if width == 0.0 || height == 0.0 {
        return Ok(Vec::new());
    }
    let mut placement = Matrix::default();
    placement.translate(rect.llx, rect.lly);
    placement.scale(
        (rect.urx - rect.llx) / width,
        (rect.ury - rect.lly) / height,
    );
    placement.translate(-transformed_bbox.llx, -transformed_bbox.lly);
    if do_rotate {
        placement.rotatex90(rotate);
    }
    let placement = placement.unparse();
    Ok(format!("q\n{placement} cm\n/{resource_name} Do\nQ\n").into_bytes())
}

fn page_resources(document: &EditDocument, page: &DirectObjectTarget) -> Result<OwnedDictionary> {
    let Some(object) = page.current(document)? else {
        return Ok(OwnedDictionary::new());
    };
    Ok(document
        .inherited_page_resources_from_view(object)?
        .and_then(|resources| crate::source::into_owned_dictionary(resources.into_cow()))
        .unwrap_or_default())
}

fn resource_dictionary(
    document: &EditDocument,
    resources: &OwnedDictionary,
    key: &[u8],
) -> Result<OwnedDictionary> {
    let Some(value) = resources.get(key) else {
        return Ok(OwnedDictionary::new());
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Dictionary(dictionary)) => dictionary,
        _ => OwnedDictionary::new(),
    })
}

fn acroform_default_resources(document: &EditDocument) -> Result<Option<OwnedDictionary>> {
    let catalog = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = document.current_owned_object(catalog)? else {
        return Ok(None);
    };
    let Some(catalog) = catalog.as_dictionary() else {
        return Ok(None);
    };
    let Some(acroform) = catalog.get(b"AcroForm".as_slice()) else {
        return Ok(None);
    };
    let Some(acroform) = document.resolve_owned_value(acroform)? else {
        return Ok(None);
    };
    let Some(acroform) = acroform.as_dictionary() else {
        return Ok(None);
    };
    let Some(dr) = acroform.get(b"DR".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(dr)? {
        Some(OwnedObject::Dictionary(dictionary)) => Some(dictionary),
        _ => None,
    })
}

fn merge_resource_dictionaries(destination: &mut OwnedDictionary, source: OwnedDictionary) {
    for (category, source_value) in source {
        match (destination.get_mut(category.as_slice()), source_value) {
            (None, source_value) => {
                destination.insert(category, source_value);
            }
            (
                Some(OwnedObject::Dictionary(destination_dict)),
                OwnedObject::Dictionary(source_dict),
            ) => {
                for (name, value) in source_dict {
                    destination_dict.entry(name).or_insert(value);
                }
            }
            (Some(OwnedObject::Array(destination_items)), OwnedObject::Array(source_items)) => {
                for item in source_items {
                    if !destination_items.contains(&item) {
                        destination_items.push(item);
                    }
                }
            }
            _ => {}
        }
    }
}

fn content_references(
    document: &mut EditDocument,
    value: Option<OwnedObject>,
) -> Result<Vec<OwnedObject>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    match value {
        OwnedObject::Reference(handle) => match document.current_owned_object(handle)? {
            Some(OwnedObject::Array(values)) => {
                let mut output = Vec::new();
                for value in values {
                    match value {
                        OwnedObject::Reference(_) => output.push(value),
                        OwnedObject::Stream { .. } => {
                            output.push(OwnedObject::Reference(ensure_indirect_owned(
                                document, value,
                            )));
                        }
                        _ => {}
                    }
                }
                Ok(output)
            }
            Some(OwnedObject::Stream { .. }) => Ok(vec![OwnedObject::Reference(handle)]),
            _ => Ok(Vec::new()),
        },
        OwnedObject::Array(values) => {
            let mut output = Vec::new();
            for value in values {
                match value {
                    OwnedObject::Reference(_) => output.push(value),
                    OwnedObject::Stream { .. } => {
                        output.push(OwnedObject::Reference(ensure_indirect_owned(
                            document, value,
                        )));
                    }
                    _ => {}
                }
            }
            Ok(output)
        }
        OwnedObject::Stream { .. } => Ok(vec![OwnedObject::Reference(ensure_indirect_owned(
            document, value,
        ))]),
        _ => Ok(Vec::new()),
    }
}

fn wrap_page_contents(
    document: &mut EditDocument,
    page: &DirectObjectTarget,
    append_bytes: &[u8],
) -> Result<()> {
    let old = page
        .current(document)?
        .and_then(|object| object.dictionary_entry(b"Contents"))
        .map(|value| value.into_cow().into_owned());
    let before = new_content_stream(document, b"q\n".to_vec());
    let mut after_bytes = b"\nQ\n".to_vec();
    after_bytes.extend_from_slice(append_bytes);
    let after = new_content_stream(document, after_bytes);
    let mut contents = vec![OwnedObject::Reference(before)];
    contents.extend(content_references(document, old)?);
    contents.push(OwnedObject::Reference(after));
    if let Some(dictionary) = page.dictionary_mut(document)? {
        dictionary.insert(b"Contents".to_vec(), OwnedObject::Array(contents));
    }
    Ok(())
}

fn page_rotate(document: &EditDocument, page: &DirectObjectTarget) -> Result<i32> {
    let Some(object) = page.current(document)? else {
        return Ok(0);
    };
    document.inherited_page_rotation_from_view(object)
}

fn flatten_annotations(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
    required_flags: i64,
    forbidden_flags: i64,
) -> Result<usize> {
    let need_appearances = acroform_need_appearances(document)?;
    let default_resources = if need_appearances {
        None
    } else {
        acroform_default_resources(document)?
    };
    let mut flattened_total = 0;
    for page in pages {
        let Some(annotations) = page_annotations(document, page)? else {
            continue;
        };
        let rotate = page_rotate(document, page)?;
        let mut resources = page_resources(document, page)?;
        let has_widget = annotations.iter().any(|annotation| {
            matches!(annotation_subtype(document, annotation), Ok(Some(name)) if name == b"Widget")
        });
        if has_widget && let Some(default_resources) = default_resources.clone() {
            merge_resource_dictionaries(&mut resources, default_resources);
        }
        let mut xobjects = resource_dictionary(document, &resources, b"XObject")?;
        let mut kept = Vec::new();
        let mut append_bytes = Vec::new();
        let mut changed_annotations = false;
        let mut counter = 1_u32;

        for annotation_value in annotations {
            let Some(annotation_object) = document.resolve_owned_value(&annotation_value)? else {
                kept.push(annotation_value);
                continue;
            };
            let Some(annotation) = annotation_object.as_dictionary() else {
                kept.push(annotation_value);
                continue;
            };
            let (has_appearance, appearance) = selected_normal_appearance(document, annotation)?;
            let subtype = owned_name(document, annotation.get(b"Subtype".as_slice()))?;
            if need_appearances && subtype.as_deref() == Some(b"Widget") {
                kept.push(annotation_value);
                continue;
            }
            if !has_appearance {
                kept.push(annotation_value);
                continue;
            }
            changed_annotations = true;
            let Some(appearance) = appearance else {
                continue;
            };
            let flags = annotation_flags(document, annotation)?;
            if (flags & forbidden_flags) != 0 || (flags & required_flags) != required_flags {
                continue;
            }
            let resource_name = loop {
                let candidate = format!("Fxo{counter}");
                counter += 1;
                if !xobjects.contains_key(candidate.as_bytes()) {
                    break candidate;
                }
            };
            let appearance = appearance_as_form(document, appearance)?;
            let content = appearance_content(
                document,
                annotation,
                appearance,
                &resource_name,
                rotate,
                flags,
            )?;
            if content.is_empty() {
                continue;
            }
            xobjects.insert(
                resource_name.into_bytes(),
                OwnedObject::Reference(appearance),
            );
            append_bytes.extend_from_slice(&content);
            flattened_total += 1;
        }

        if !xobjects.is_empty() {
            resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));
        }
        if let Some(page_dictionary) = page.dictionary_mut(document)? {
            page_dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
        }
        if changed_annotations {
            wrap_page_contents(document, page, &append_bytes)?;
            replace_page_annotations(document, page, kept)?;
        }
    }
    if !need_appearances {
        let catalog_id = document.source().catalog_id();
        if let Some(catalog) = document.edit_object(catalog_id)?.as_dictionary_mut() {
            catalog.remove(b"AcroForm".as_slice());
        }
    }
    Ok(flattened_total)
}

fn process_annotations(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    stats.annotation_subtypes_seen = annotation_subtypes(document, pages)?;
    stats.annotation_entries_seen = stats.annotation_subtypes_seen.values().sum();
    match policy.annotations {
        AnnotationPolicy::Preserve => {
            let kept = filter_annotations(document, pages, policy, true)?;
            stats.annotation_entries_dropped_unflattened =
                stats.annotation_entries_seen.saturating_sub(kept);
        }
        AnnotationPolicy::Discard => {
            let kept = filter_annotations(document, pages, policy, false)?;
            stats.annotation_entries_dropped_unflattened =
                stats.annotation_entries_seen.saturating_sub(kept);
        }
        AnnotationPolicy::AppearanceOnly => {
            let protected = detach_protected_annotations(document, pages, policy)?;
            let processable_before: usize = annotation_subtypes(document, pages)?.values().sum();
            if processable_before > 0 {
                flatten_annotations(document, pages, 0, SCREEN_HIDDEN_ANNOTATION_FLAGS)?;
            }
            stats.unflattened_annotation_subtypes = annotation_subtypes(document, pages)?;
            let unflattened: usize = stats.unflattened_annotation_subtypes.values().sum();
            stats.annotation_entries_flattened = processable_before.saturating_sub(unflattened);
            stats.link_visual_shells_retained = retain_link_visual_shells(document, pages)?;
            stats.annotation_entries_dropped_unflattened =
                unflattened.saturating_sub(stats.link_visual_shells_retained);
            restore_protected_annotations(document, pages, protected)?;
        }
    }
    Ok(())
}

/// Drop embedded page-thumbnail cache entries regardless of preservation policy.
///
/// `/Thumb` is an optional page-preview cache and is not part of page rendering.
/// Normalized outputs deliberately do not carry producer thumbnail state; callers
/// that need thumbnails can regenerate them from the final normalized pages.
pub fn drop_page_thumbnails(
    document: &mut EditDocument,
    stats: &mut PreservationStats,
) -> Result<()> {
    // Use the source parser's recovered page list rather than recursively walking
    // `/Pages`. Default optimization historically skipped preservation traversal,
    // so thumbnail cleanup must not make malformed page-tree references a new
    // failure mode.
    for id in document.source().page_ids() {
        let page = DirectObjectTarget {
            root: CowObjectHandle::Existing(id),
            path: Vec::new(),
        };
        if page
            .current(document)?
            .is_none_or(|value| value.dictionary_entry(b"Thumb").is_none())
        {
            continue;
        }
        if let Some(dictionary) = page.dictionary_mut(document)?
            && dictionary.remove(b"Thumb".as_slice()).is_some()
        {
            *stats
                .dropped_page_keys
                .entry("/Thumb".to_owned())
                .or_default() += 1;
        }
    }
    Ok(())
}

pub fn apply_preservation_policy(
    document: &mut EditDocument,
    policy: &PreservationConfig,
) -> Result<PreservationStats> {
    apply_preservation_policy_with_hints(document, policy, PreservationScanHints::default())
}

pub fn apply_preservation_policy_with_hints(
    document: &mut EditDocument,
    policy: &PreservationConfig,
    hints: PreservationScanHints,
) -> Result<PreservationStats> {
    let (pages, page_tree_nodes) = preservation_page_tree_targets(document)?;
    let mut stats = PreservationStats {
        pages: pages.len(),
        ..PreservationStats::default()
    };
    process_annotations(document, &pages, policy, &mut stats)?;
    if !policy.private_authoring_data && !hints.no_piece_info {
        drop_private_authoring_data(document, &mut stats)?;
    }
    if !policy.metadata && !hints.no_authoring_metadata {
        drop_authoring_metadata(document, &mut stats)?;
    }
    for page in &pages {
        prune_dictionary_target(
            document,
            page,
            |key| keep_page_key(key, policy),
            policy.splice_unknown_wrappers,
            &mut stats.dropped_page_keys,
            &mut stats.spliced_unknown_wrapper_keys,
        )?;
    }
    for node in &page_tree_nodes {
        prune_dictionary_target(
            document,
            node,
            |key| keep_page_tree_key(key, policy),
            policy.splice_unknown_wrappers,
            &mut stats.dropped_page_tree_keys,
            &mut stats.spliced_unknown_wrapper_keys,
        )?;
    }
    let catalog = DirectObjectTarget {
        root: CowObjectHandle::Existing(document.source().catalog_id()),
        path: Vec::new(),
    };
    prune_dictionary_target(
        document,
        &catalog,
        |key| keep_catalog_key(key, policy),
        policy.splice_unknown_wrappers,
        &mut stats.dropped_catalog_keys,
        &mut stats.spliced_unknown_wrapper_keys,
    )?;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SourcePdf, test_support::ClassicPdfBuilder};

    fn auxiliary_state_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Metadata 6 0 R /UnknownCatalog 7 0 R >>",
        )?;
        pdf.object(
            2,
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 /UnknownTree 7 0 R >>",
        )?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R /Thumb 5 0 R /PieceInfo 8 0 R /LastModified (today) /UnknownPage 7 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[0])?;
        pdf.stream(6, b"/Type /Metadata /Subtype /XML", b"<xmp/>")?;
        pdf.object(7, b"<< /Private true >>")?;
        pdf.object(8, b"<< /Illustrator << /Private true >> >>")?;
        pdf.finish(1)
    }

    #[test]
    fn preservation_direct_source_paths_remain_lazy_and_promote_selected_keys() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R << /Type /Page /Parent 2 0 R /Annots [5 0 R] /Wrapper << /MediaBox [0 0 10 20] /Ignored (large) >> >>] /Count 2 /Rotate 90 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(5, b"<< /Type /Annot /Subtype /Text >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let root = CowObjectHandle::Existing(crate::ObjectId::new(2, 0));
        let target = DirectObjectTarget {
            root,
            path: vec![
                DirectPathStep::DictKey(b"Kids".to_vec()),
                DirectPathStep::ArrayIndex(1),
            ],
        };
        let (pages, _) = preservation_page_tree_targets(&document)?;
        assert!(pages.contains(&target));
        assert!(matches!(
            target.current(&document)?,
            Some(CurrentValue::Source(..))
        ));
        assert_eq!(
            inherited_page_value(&document, &target, b"Rotate")?,
            Some(OwnedObject::Integer(90))
        );
        assert_eq!(
            page_annotations(&document, &target)?,
            Some(vec![OwnedObject::Reference(CowObjectHandle::Existing(
                crate::ObjectId::new(5, 0)
            ))])
        );
        let mut removed = BTreeMap::new();
        let mut spliced = BTreeMap::new();
        prune_dictionary_target(
            &mut document,
            &target,
            |key| key != b"Wrapper" && key != b"Ignored",
            true,
            &mut removed,
            &mut spliced,
        )?;
        let Some(CurrentValue::Owned(object)) = target.current(&document)? else {
            return Err(Error::Invalid("missing edited direct page".to_owned()));
        };
        let Some(dictionary) = object.as_dictionary() else {
            return Err(Error::Invalid("wrong direct page".to_owned()));
        };
        assert!(!dictionary.contains_key(b"Wrapper".as_slice()));
        assert!(dictionary.contains_key(b"MediaBox".as_slice()));
        assert!(!dictionary.contains_key(b"Ignored".as_slice()));
        assert_eq!(spliced.get("/Wrapper -> /MediaBox"), Some(&1));
        assert_eq!(removed.get("/Wrapper"), Some(&1));
        Ok(())
    }

    #[test]
    fn annotation_statistics_distinguish_empty_arrays_and_malformed_values() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /Annots [] >>")?;
        pdf.object(4, b"<< /Type /Page /Parent 2 0 R /Annots null >>")?;
        pdf.object(
            5,
            b"<< /Type /Page /Parent 2 0 R /Annots [<< /Subtype /Text >>] >>",
        )?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let (pages, _) = preservation_page_tree_targets(&document)?;
        assert_eq!(
            annotation_subtypes(&document, &pages)?,
            BTreeMap::from([
                ("(non-array /Annots)".to_owned(), 1),
                ("/Text".to_owned(), 1),
            ])
        );
        Ok(())
    }

    #[test]
    fn thumbnail_cleanup_does_not_materialize_pages_without_thumbnails() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(
            4,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Thumb 5 0 R >>",
        )?;
        pdf.stream(5, b"", b"x")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let mut stats = PreservationStats::default();
        drop_page_thumbnails(&mut document, &mut stats)?;
        assert_eq!(stats.dropped_page_keys.get("/Thumb"), Some(&1));
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(3, 0))
                .is_none()
        );
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(4, 0))
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn preservation_tree_walk_handles_deep_indirect_nodes_and_direct_leaves() -> Result<()> {
        let mut document = EditDocument::from_bytes(auxiliary_state_fixture()?)?;
        let root = CowObjectHandle::Existing(crate::ObjectId::new(2, 0));
        let leaf = CowObjectHandle::Existing(crate::ObjectId::new(3, 0));
        let direct_name = b"UniquePageMarker";
        let direct_page = OwnedObject::Dictionary(OwnedDictionary::from([
            (b"Type".to_vec(), OwnedObject::Name(b"Page".to_vec())),
            (direct_name.to_vec(), OwnedObject::String(vec![11; 4096])),
        ]));
        let direct_branch = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(
            OwnedDictionary::from([
                (b"Type".to_vec(), OwnedObject::Name(b"Pages".to_vec())),
                (
                    b"Kids".to_vec(),
                    OwnedObject::Array(vec![direct_page, OwnedObject::Reference(leaf)]),
                ),
            ]),
        )));
        let mut child = direct_branch;
        let depth = 20_000;
        for _ in 0..depth {
            child = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(
                OwnedDictionary::from([
                    (b"Type".to_vec(), OwnedObject::Name(b"Pages".to_vec())),
                    (
                        b"Kids".to_vec(),
                        OwnedObject::Array(vec![OwnedObject::Reference(child)]),
                    ),
                ]),
            )));
        }
        let Some(root_dictionary) = document.edit_handle(root)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture root".to_owned()));
        };
        root_dictionary.insert(
            b"Kids".to_vec(),
            OwnedObject::Array(vec![OwnedObject::Reference(child)]),
        );

        let target = DirectObjectTarget {
            root: direct_branch,
            path: vec![
                DirectPathStep::DictKey(b"Kids".to_vec()),
                DirectPathStep::ArrayIndex(0),
            ],
        };
        let (pages, nodes) = preservation_page_tree_targets(&document)?;
        assert_eq!(nodes.len(), depth + 2);
        assert_eq!(pages.len(), 2);
        assert!(pages.contains(&target));
        let Some(CurrentValue::Owned(object)) = target.current(&document)? else {
            return Err(Error::Invalid(
                "direct overlay target was copied".to_owned(),
            ));
        };
        assert!(
            object
                .as_dictionary()
                .is_some_and(|dictionary| dictionary.contains_key(direct_name.as_slice()))
        );
        Ok(())
    }

    #[test]
    fn annotation_rotation_matches_hayro_including_real_and_wrapped_angles() -> Result<()> {
        use hayro_syntax::page::Rotation;
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R 6 0 R 7 0 R 8 0 R] /Count 6 /Rotate 90 /MediaBox [0 0 100 100] >>")?;
        for (number, rotation) in [
            (3, "-90.5"),
            (4, "450"),
            (5, "180.9"),
            (6, "45"),
            (7, "2147483648"),
            (8, "9 0 R"),
        ] {
            pdf.object(
                number,
                format!("<< /Type /Page /Parent 2 0 R /Rotate {rotation} >>").as_bytes(),
            )?;
        }
        pdf.object(9, b"null")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let existing = |number| CowObjectHandle::Existing(crate::ObjectId::new(number, 0));
        let target = |number| DirectObjectTarget {
            root: existing(number),
            path: Vec::new(),
        };
        let expected = [270, 90, 180, 0, 90, 90];
        let pages = document.source().syntax().pages();
        assert_eq!(pages.len(), expected.len());
        for ((number, expected), page) in (3..=8).zip(expected).zip(pages.iter()) {
            let renderer_rotation = match page.rotation() {
                Rotation::None => 0,
                Rotation::Horizontal => 90,
                Rotation::Flipped => 180,
                Rotation::FlippedHorizontal => 270,
            };
            assert_eq!(renderer_rotation, expected);
            assert_eq!(page_rotate(&document, &target(number))?, expected);
        }
        *document.edit_handle(existing(9))? = OwnedObject::Real(-450.5);
        assert_eq!(page_rotate(&document, &target(8))?, 270);
        *document.edit_handle(existing(9))? = OwnedObject::Real(f64::INFINITY);
        assert_eq!(page_rotate(&document, &target(8))?, 90);
        let direct = CowObjectHandle::New(document.add_object(OwnedObject::Array(vec![
            OwnedObject::Dictionary(OwnedDictionary::from([
                (b"Parent".to_vec(), OwnedObject::Reference(existing(2))),
                (b"Rotate".to_vec(), OwnedObject::Real(540.5)),
            ])),
        ])));
        assert_eq!(
            page_rotate(
                &document,
                &DirectObjectTarget {
                    root: direct,
                    path: vec![DirectPathStep::ArrayIndex(0)]
                }
            )?,
            180
        );
        Ok(())
    }

    #[test]
    fn inherited_page_values_follow_deep_chains_and_terminate_on_cycles() -> Result<()> {
        let mut document = EditDocument::from_bytes(auxiliary_state_fixture()?)?;
        let root = CowObjectHandle::Existing(crate::ObjectId::new(2, 0));
        let Some(root_dictionary) = document.edit_handle(root)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture page tree".to_owned()));
        };
        root_dictionary.insert(b"Rotate".to_vec(), OwnedObject::Integer(90));

        // Source and preservation inheritance must agree even when a valid
        // page tree exceeds the old hardcoded limit of 101 parent traversals.
        let mut parent = root;
        for _ in 0..160 {
            parent = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(
                OwnedDictionary::from([(b"Parent".to_vec(), OwnedObject::Reference(parent))]),
            )));
        }
        let wrapper = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(
            OwnedDictionary::from([(
                b"NestedPage".to_vec(),
                OwnedObject::Dictionary(OwnedDictionary::from([(
                    b"Parent".to_vec(),
                    OwnedObject::Reference(parent),
                )])),
            )]),
        )));
        let direct_target = DirectObjectTarget {
            root: wrapper,
            path: vec![DirectPathStep::DictKey(b"NestedPage".to_vec())],
        };
        assert_eq!(
            document.inherited_page_value(parent, b"Rotate")?,
            Some(OwnedObject::Integer(90))
        );
        assert_eq!(
            inherited_page_value(&document, &direct_target, b"Rotate")?,
            Some(OwnedObject::Integer(90))
        );

        // A closer ancestor always wins, regardless of the distant root.
        let Some(near_dictionary) = document.edit_handle(parent)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing nearest ancestor".to_owned()));
        };
        near_dictionary.insert(b"Rotate".to_vec(), OwnedObject::Integer(180));
        assert_eq!(
            document.inherited_page_value(parent, b"Rotate")?,
            Some(OwnedObject::Integer(180))
        );
        assert_eq!(
            inherited_page_value(&document, &direct_target, b"Rotate")?,
            Some(OwnedObject::Integer(180))
        );

        // A malformed indirect parent cycle with no value must terminate.
        let first = CowObjectHandle::New(
            document.add_object(OwnedObject::Dictionary(OwnedDictionary::new())),
        );
        let second = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(
            OwnedDictionary::from([(b"Parent".to_vec(), OwnedObject::Reference(first))]),
        )));
        let Some(first_dictionary) = document.edit_handle(first)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing cycle node".to_owned()));
        };
        first_dictionary.insert(b"Parent".to_vec(), OwnedObject::Reference(second));
        let cycle_target = DirectObjectTarget {
            root: second,
            path: Vec::new(),
        };
        assert_eq!(document.inherited_page_value(second, b"Rotate")?, None);
        assert_eq!(
            inherited_page_value(&document, &cycle_target, b"Rotate")?,
            None
        );
        Ok(())
    }

    #[test]
    fn authoring_removals_walk_source_and_overlay_without_editing_unaffected_roots() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Extra [4 0 R 5 0 R 6 0 R 7 0 R 9 0 R] >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.stream(4, b"/Subtype 8 0 R /LastModified (old) /Nested [<< /Metadata << /Metadata true >> /PieceInfo << /PieceInfo true >> >>] /Metadata 9 0 R", b"q Q")?;
        pdf.object(
            5,
            b"<< /Subtype /Form /LastModified (keep) /Nested << /LastModified (keep) >> >>",
        )?;
        pdf.stream(
            6,
            b"/Subtype /Image /LastModified (keep) /Metadata null",
            &[0],
        )?;
        pdf.object(7, b"<< /PieceInfo 9 0 R >>")?;
        pdf.object(8, b"/Form")?;
        pdf.object(9, b"<< /Metadata true /PieceInfo true >>")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let handle = |number| CowObjectHandle::Existing(crate::ObjectId::new(number, 0));
        // Force one source dictionary into the overlay and add a reachable new root.
        let new = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(
            OwnedDictionary::from([
                (b"Metadata".to_vec(), OwnedObject::Null),
                (b"PieceInfo".to_vec(), OwnedObject::Null),
            ]),
        )));
        document
            .edit_handle(handle(7))?
            .as_dictionary_mut()
            .ok_or_else(|| Error::Invalid("missing dictionary".to_owned()))?
            .insert(b"Extra".to_vec(), OwnedObject::Reference(new));

        let mut stats = PreservationStats::default();
        drop_private_authoring_data(&mut document, &mut stats)?;
        drop_authoring_metadata(&mut document, &mut stats)?;
        assert_eq!(stats.dropped_authoring_keys.get("/PieceInfo"), Some(&4));
        assert_eq!(stats.dropped_authoring_keys.get("/Metadata"), Some(&5));
        assert_eq!(stats.dropped_authoring_keys.get("/LastModified"), Some(&1));
        assert!(matches!(
            document.current_value(handle(5))?,
            Some(CurrentValue::Source(..))
        ));
        for number in [5, 6] {
            assert!(
                document
                    .current_value(handle(number))?
                    .is_some_and(|value| value.dictionary_entry(b"LastModified").is_some())
            );
        }
        assert!(
            document
                .current_value(handle(4))?
                .is_some_and(|value| value.dictionary_entry(b"LastModified").is_none())
        );
        Ok(())
    }

    #[test]
    fn functional_policy_drops_disposable_caches_but_keeps_ordinary_metadata() -> Result<()> {
        let mut document = EditDocument::from_bytes(auxiliary_state_fixture()?)?;
        let stats = apply_preservation_policy(&mut document, &PreservationConfig::functional())?;
        assert_eq!(stats.dropped_page_keys.get("/Thumb"), Some(&1));
        assert_eq!(stats.dropped_authoring_keys.get("/PieceInfo"), Some(&1));

        let source = SourcePdf::from_bytes(document.write_compact()?)?;
        let catalog = source.materialize(source.catalog_id())?;
        let catalog = catalog
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten catalog is not a dictionary".to_owned()))?;
        assert!(catalog.contains_key(b"Metadata".as_slice()));
        assert!(catalog.contains_key(b"UnknownCatalog".as_slice()));

        let page_id = source
            .page_ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("rewritten fixture lost its page".to_owned()))?;
        let page = source.materialize(page_id)?;
        let page = page
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten page is not a dictionary".to_owned()))?;
        assert!(!page.contains_key(b"Thumb".as_slice()));
        assert!(!page.contains_key(b"PieceInfo".as_slice()));
        assert!(page.contains_key(b"LastModified".as_slice()));
        assert!(page.contains_key(b"UnknownPage".as_slice()));
        Ok(())
    }

    #[test]
    fn explicit_private_authoring_policy_keeps_piece_info_but_not_thumbnails() -> Result<()> {
        let mut policy = PreservationConfig::functional();
        policy.private_authoring_data = true;
        let mut document = EditDocument::from_bytes(auxiliary_state_fixture()?)?;
        let stats = apply_preservation_policy(&mut document, &policy)?;
        assert_eq!(stats.dropped_page_keys.get("/Thumb"), Some(&1));
        assert!(!stats.dropped_authoring_keys.contains_key("/PieceInfo"));

        let source = SourcePdf::from_bytes(document.write_compact()?)?;
        let page_id = source
            .page_ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("rewritten fixture lost its page".to_owned()))?;
        let page = source.materialize(page_id)?;
        let page = page
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten page is not a dictionary".to_owned()))?;
        assert!(!page.contains_key(b"Thumb".as_slice()));
        assert!(page.contains_key(b"PieceInfo".as_slice()));
        assert!(page.contains_key(b"LastModified".as_slice()));
        Ok(())
    }

    #[test]
    fn visible_surface_drops_metadata_and_unknown_auxiliary_state() -> Result<()> {
        let mut document = EditDocument::from_bytes(auxiliary_state_fixture()?)?;
        let stats =
            apply_preservation_policy(&mut document, &PreservationConfig::visible_surface())?;
        assert_eq!(stats.pages, 1);
        let source = SourcePdf::from_bytes(document.write_compact()?)?;
        let catalog = source.materialize(source.catalog_id())?;
        let catalog = catalog
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten catalog is not a dictionary".to_owned()))?;
        assert!(!catalog.contains_key(b"Metadata".as_slice()));
        assert!(!catalog.contains_key(b"UnknownCatalog".as_slice()));
        let page_id = source
            .page_ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("rewritten fixture lost its page".to_owned()))?;
        let page = source.materialize(page_id)?;
        let page = page
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten page is not a dictionary".to_owned()))?;
        assert!(!page.contains_key(b"Thumb".as_slice()));
        assert!(!page.contains_key(b"UnknownPage".as_slice()));
        Ok(())
    }
}
