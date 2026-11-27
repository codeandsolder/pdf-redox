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

#[cfg(test)]
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
        dictionary.insert(b"Annots".to_vec(), OwnedObject::array(annotations));
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

fn annotation_is_protected(subtype: Option<&[u8]>, policy: &PreservationConfig) -> bool {
    match subtype {
        Some(b"Link") => policy.links,
        Some(b"Widget") => policy.forms,
        _ => false,
    }
}

fn subtype_label(subtype: Option<&[u8]>) -> String {
    subtype.map_or_else(
        || "(missing/non-name subtype)".to_owned(),
        |name| format!("/{}", String::from_utf8_lossy(name)),
    )
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
            let label = subtype_label(current_annotation_subtype(document, annotation)?.as_deref());
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
        let Some(entries) = page_annotation_entries(document, page)? else {
            continue;
        };
        let AnnotationEntries::Array(items) = entries else {
            if preserve_other_annotations {
                // Preserve reader recovery behavior for malformed annotation
                // containers when the caller requests preservation.
                kept_total += 1;
            } else {
                replace_page_annotations(document, page, Vec::new())?;
            }
            continue;
        };
        let mut kept = Vec::new();
        let mut changed = false;
        for annotation in items {
            let subtype = current_annotation_subtype(document, annotation.clone())?;
            let protected = annotation_is_protected(subtype.as_deref(), policy);
            let explicitly_disabled = matches!(subtype.as_deref(), Some(b"Link")) && !policy.links
                || matches!(subtype.as_deref(), Some(b"Widget")) && !policy.forms;
            if protected || (preserve_other_annotations && !explicitly_disabled) {
                kept.push(annotation.into_cow().into_owned());
            } else {
                changed = true;
            }
        }
        kept_total += kept.len();
        if changed {
            replace_page_annotations(document, page, kept)?;
        }
    }
    Ok(kept_total)
}

fn link_visual_shell(dictionary: &OwnedDictionary) -> OwnedObject {
    const LINK_VISUAL_KEYS: &[&[u8]] = &[b"Rect", b"Border", b"BS", b"C", b"F", b"CA", b"OC"];
    let mut shell = OwnedDictionary::from([
        (b"Type".to_vec(), OwnedObject::Name(b"Annot".to_vec())),
        (b"Subtype".to_vec(), OwnedObject::Name(b"Link".to_vec())),
    ]);
    for &key in LINK_VISUAL_KEYS {
        if let Some(value) = dictionary.get(key) {
            shell.insert(key.to_vec(), value.clone());
        }
    }
    OwnedObject::Dictionary(shell)
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
        data: StreamData::Owned(data.into()),
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
    let mut current = CurrentValue::Owned(value);
    let mut seen = BTreeSet::new();
    let mut source = None;
    while let Some(handle) = current.reference() {
        if !seen.insert(handle) {
            return Ok(None);
        }
        let Some(next) = document.current_value(handle)? else {
            return Ok(None);
        };
        current = next;
        source = Some(handle);
    }
    Ok(current.is_stream().then(|| {
        source.map_or_else(
            || AppearanceSource::Direct(current.into_cow().into_owned()),
            AppearanceSource::Handle,
        )
    }))
}

fn selected_normal_appearance(
    document: &EditDocument,
    annotation: &OwnedDictionary,
) -> Result<Option<AppearanceSource>> {
    let Some(ap_value) = annotation.get(b"AP".as_slice()) else {
        return Ok(None);
    };
    let Some(ap_object) = document.resolve_owned_value_cow(ap_value)? else {
        return Ok(None);
    };
    let OwnedObject::Dictionary(ap) = ap_object.as_ref() else {
        return Ok(None);
    };
    let Some(normal) = ap.get(b"N".as_slice()) else {
        return Ok(None);
    };
    if let Some(stream) = stream_source_from_value(document, normal)? {
        return Ok(Some(stream));
    }
    let Some(normal_object) = document.resolve_owned_value_cow(normal)? else {
        return Ok(None);
    };
    let OwnedObject::Dictionary(states) = normal_object.as_ref() else {
        return Ok(None);
    };
    if let Some(state) = owned_name(document, annotation.get(b"AS".as_slice()))?
        && let Some(value) = states.get(state.as_slice())
        && let Some(stream) = stream_source_from_value(document, value)?
    {
        return Ok(Some(stream));
    }
    // Match the source interpreter's fallback to an explicit Off state.
    match states.get(b"Off".as_slice()) {
        Some(value) => stream_source_from_value(document, value),
        None => Ok(None),
    }
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

enum PlannedForm {
    Existing(CowObjectHandle),
    New(OwnedObject),
}

struct PlannedAppearance {
    form: PlannedForm,
    content: Vec<u8>,
    needs_resources: bool,
}

fn prepare_appearance(
    document: &EditDocument,
    annotation: &OwnedDictionary,
    source: AppearanceSource,
    resource_name: &str,
    rotate: i32,
    flags: i64,
) -> Result<Option<PlannedAppearance>> {
    let (handle, object) = match source {
        AppearanceSource::Handle(handle) => {
            let Some(object) = document.current_owned_object_cow(handle)? else {
                return Ok(None);
            };
            (Some(handle), object)
        }
        AppearanceSource::Direct(object) => (None, std::borrow::Cow::Owned(object)),
    };
    let OwnedObject::Stream { dictionary, .. } = object.as_ref() else {
        return Ok(None);
    };
    let content = appearance_content(
        document,
        annotation,
        dictionary,
        resource_name,
        rotate,
        flags,
    )?;
    if content.is_empty() {
        return Ok(None);
    }
    let needs_resources = match dictionary.get(b"Resources".as_slice()) {
        None => true,
        Some(value) => match document.resolve_owned_value_cow(value)?.as_deref() {
            Some(OwnedObject::Dictionary(_)) => false,
            Some(OwnedObject::Null) => true,
            _ => return Ok(None),
        },
    };
    let form = if let Some(handle) = handle
        && !needs_resources
        && owned_name(document, dictionary.get(b"Subtype".as_slice()))?.as_deref() == Some(b"Form")
    {
        PlannedForm::Existing(handle)
    } else {
        // Never normalize a shared appearance object in-place. Its other
        // users may still depend on the exact original dictionary.
        let mut object = object.into_owned();
        if let OwnedObject::Stream { dictionary, .. } = &mut object {
            dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec()));
        }
        PlannedForm::New(object)
    };
    Ok(Some(PlannedAppearance {
        form,
        content,
        needs_resources,
    }))
}

fn normalized_rectangle(document: &EditDocument, value: &OwnedObject) -> Result<Option<Rectangle>> {
    let Some([x0, y0, x1, y1]) = resolved_number_array::<4>(document, CurrentValue::Owned(value))?
    else {
        return Ok(None);
    };
    if ![x0, y0, x1, y1].into_iter().all(f64::is_finite) {
        return Ok(None);
    }
    Ok(Some(Rectangle::new(
        x0.min(x1),
        y0.min(y1),
        x0.max(x1),
        y0.max(y1),
    )))
}

fn appearance_matrix(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<Matrix>> {
    let Some(value) = dictionary.get(b"Matrix".as_slice()) else {
        return Ok(Some(Matrix::default()));
    };
    Ok(
        resolved_number_array::<6>(document, CurrentValue::Owned(value))?
            .map(Matrix::from)
            .filter(|matrix| matrix.is_finite()),
    )
}

fn appearance_content(
    document: &EditDocument,
    annotation: &OwnedDictionary,
    appearance_dictionary: &OwnedDictionary,
    resource_name: &str,
    rotate: i32,
    flags: i64,
) -> Result<Vec<u8>> {
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
    let Some(matrix) = appearance_matrix(document, appearance_dictionary)? else {
        return Ok(Vec::new());
    };
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
    let Some(transformed_bbox) = matrix.transform_rectangle(bbox) else {
        return Ok(Vec::new());
    };
    let Some(rect) = crate::geometry::Rect::from_rectangle(rect) else {
        return Ok(Vec::new());
    };
    let width = transformed_bbox.x1 - transformed_bbox.x0;
    let height = transformed_bbox.y1 - transformed_bbox.y0;
    if width <= 0.0 || height <= 0.0 || rect.area() <= 0.0 || !rect.area().is_finite() {
        return Ok(Vec::new());
    }
    let mut placement = Matrix::default();
    placement.translate(rect.x0, rect.y0);
    placement.scale((rect.x1 - rect.x0) / width, (rect.y1 - rect.y0) / height);
    placement.translate(-transformed_bbox.x0, -transformed_bbox.y0);
    if do_rotate {
        placement.rotatex90(rotate);
    }
    if !placement.is_finite() {
        return Ok(Vec::new());
    }
    let mut content = b"q\n".to_vec();
    for (index, value) in [
        placement.a,
        placement.b,
        placement.c,
        placement.d,
        placement.e,
        placement.f,
    ]
    .into_iter()
    .enumerate()
    {
        if index != 0 {
            content.push(b' ');
        }
        crate::writer::write_pdf_real(&mut content, value)?;
    }
    content.extend_from_slice(format!(" cm\n/{resource_name} Do\nQ\n").as_bytes());
    Ok(content)
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

fn resolved_resource_category(
    document: &EditDocument,
    resources: &OwnedDictionary,
    key: &[u8],
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = resources.get(key) else {
        return Ok(Some(OwnedDictionary::new()));
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Dictionary(dictionary)) => Some(dictionary),
        Some(OwnedObject::Null) => Some(OwnedDictionary::new()),
        _ => None,
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

fn merge_resource_dictionaries(
    document: &EditDocument,
    destination: &mut OwnedDictionary,
    source: &OwnedDictionary,
) -> Result<()> {
    for (category, source_value) in source {
        let Some(destination_value) = destination.get(category.as_slice()) else {
            destination.insert(category.clone(), source_value.clone());
            continue;
        };
        let merged = match (
            document.resolve_owned_value(destination_value)?,
            document.resolve_owned_value(source_value)?,
        ) {
            (
                Some(OwnedObject::Dictionary(mut destination_dict)),
                Some(OwnedObject::Dictionary(source_dict)),
            ) => {
                for (name, value) in source_dict {
                    destination_dict.entry(name).or_insert(value);
                }
                Some(OwnedObject::Dictionary(destination_dict))
            }
            (
                Some(OwnedObject::Array(mut destination_items)),
                Some(OwnedObject::Array(source_items)),
            ) => {
                for item in source_items {
                    if !destination_items.contains(&item) {
                        destination_items.push(item);
                    }
                }
                Some(OwnedObject::Array(destination_items))
            }
            _ => None,
        };
        if let Some(merged) = merged {
            destination.insert(category.clone(), merged);
        }
    }
    Ok(())
}

#[derive(Default)]
struct AppearanceScopes {
    page: Option<CowObjectHandle>,
    widget: Option<CowObjectHandle>,
}

impl AppearanceScopes {
    fn resource(
        &mut self,
        document: &mut EditDocument,
        page_resources: &OwnedDictionary,
        widget_defaults: Option<&OwnedDictionary>,
    ) -> Result<CowObjectHandle> {
        let slot = if widget_defaults.is_some() {
            &mut self.widget
        } else {
            &mut self.page
        };
        if let Some(handle) = *slot {
            return Ok(handle);
        }
        let mut resources = page_resources.clone();
        if let Some(defaults) = widget_defaults {
            merge_resource_dictionaries(document, &mut resources, defaults)?;
        }
        let handle = CowObjectHandle::New(document.add_object(OwnedObject::Dictionary(resources)));
        *slot = Some(handle);
        Ok(handle)
    }
}

struct PageContentPlan {
    parts: Vec<OwnedObject>,
    saves_to_restore: usize,
}

enum ContentTask<'a> {
    Visit(CurrentValue<'a>),
    Array(CurrentArrayIter<'a>),
    Leave(CowObjectHandle),
}

fn content_references(
    document: &EditDocument,
    root: CurrentValue<'_>,
) -> Result<Option<Vec<OwnedObject>>> {
    let mut output = Vec::new();
    let mut pending = vec![ContentTask::Visit(root)];
    let mut active = BTreeSet::new();
    while let Some(task) = pending.pop() {
        let value = match task {
            ContentTask::Visit(value) => value,
            ContentTask::Array(mut items) => {
                if let Some(value) = items.next() {
                    pending.push(ContentTask::Array(items));
                    pending.push(ContentTask::Visit(value));
                }
                continue;
            }
            ContentTask::Leave(handle) => {
                active.remove(&handle);
                continue;
            }
        };
        if let Some(handle) = value.reference() {
            if !active.insert(handle) {
                return Ok(None);
            }
            let Some(resolved) = document.current_value(handle)? else {
                return Ok(None);
            };
            if resolved.is_stream() {
                output.push(OwnedObject::Reference(handle));
                active.remove(&handle);
            } else {
                pending.push(ContentTask::Leave(handle));
                pending.push(ContentTask::Visit(resolved));
            }
        } else if let Some(items) = value.array_items() {
            pending.push(ContentTask::Array(items));
        } else if value.is_stream() {
            output.push(value.into_cow().into_owned());
        } else if !value.is_null() {
            return Ok(None);
        }
    }
    Ok(Some(output))
}

/// Existing content must finish outside text, marked-content and
/// compatibility scopes and never underflow its graphics-state stack.
/// Unmatched saves can be closed after the original paint without changing it.
fn content_isolation_depth(bytes: &[u8]) -> Result<Option<usize>> {
    let (mut saves, mut marked, mut compatibility) = (0_usize, 0_usize, 0_usize);
    let mut text = false;
    let mut valid = true;
    let incomplete = crate::content_stream::visit_instructions(bytes, |instruction| {
        match &instruction.operator[..] {
            b"q" => saves += 1,
            b"Q" if saves != 0 => saves -= 1,
            b"BT" if !text => text = true,
            b"ET" if text => text = false,
            b"BMC" | b"BDC" => marked += 1,
            b"EMC" if marked != 0 => marked -= 1,
            b"BX" => compatibility += 1,
            b"EX" if compatibility != 0 => compatibility -= 1,
            b"Q" | b"BT" | b"ET" | b"EMC" | b"EX" => valid = false,
            _ => {}
        }
        Ok(())
    })?;
    Ok((!incomplete && valid && !text && marked == 0 && compatibility == 0).then_some(saves + 1))
}

fn plan_page_contents(
    document: &EditDocument,
    page: &DirectObjectTarget,
) -> Result<Option<PageContentPlan>> {
    let Some(old) = page
        .current(document)?
        .and_then(|object| object.dictionary_entry(b"Contents"))
    else {
        return Ok(Some(PageContentPlan {
            parts: Vec::new(),
            saves_to_restore: 0,
        }));
    };
    let Some(parts) = content_references(document, old.clone())? else {
        return Ok(None);
    };
    if parts.is_empty() {
        return Ok(Some(PageContentPlan {
            parts,
            saves_to_restore: 0,
        }));
    }
    let decoded = match crate::content::decoded_content_value(document, old) {
        Ok(decoded) => decoded,
        Err(Error::Invalid(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(
        content_isolation_depth(&decoded)?.map(|saves_to_restore| PageContentPlan {
            parts,
            saves_to_restore,
        }),
    )
}

fn wrap_page_contents(
    document: &mut EditDocument,
    page: &DirectObjectTarget,
    plan: PageContentPlan,
    append_bytes: Vec<u8>,
) -> Result<()> {
    let contents = if plan.parts.is_empty() {
        OwnedObject::Reference(new_content_stream(document, append_bytes))
    } else {
        let before = new_content_stream(document, b"q\n".to_vec());
        let mut after_bytes = b"\nn\n".to_vec();
        for _ in 0..plan.saves_to_restore {
            after_bytes.extend_from_slice(b"Q\n");
        }
        after_bytes.extend(append_bytes);
        let after = new_content_stream(document, after_bytes);
        let mut contents = Vec::with_capacity(plan.parts.len() + 2);
        contents.push(OwnedObject::Reference(before));
        contents.extend(
            plan.parts
                .into_iter()
                .map(|value| OwnedObject::Reference(ensure_indirect_owned(document, value))),
        );
        contents.push(OwnedObject::Reference(after));
        OwnedObject::array(contents)
    };
    if let Some(dictionary) = page.dictionary_mut(document)? {
        dictionary.insert(b"Contents".to_vec(), contents);
    }
    Ok(())
}

fn page_rotate(document: &EditDocument, page: &DirectObjectTarget) -> Result<i32> {
    let Some(object) = page.current(document)? else {
        return Ok(0);
    };
    document.inherited_page_rotation_from_view(object)
}

struct PageAppearancePlan {
    content: Option<PageContentPlan>,
    original_resources: OwnedDictionary,
    xobjects: OwnedDictionary,
    properties: OwnedDictionary,
    properties_valid: bool,
    properties_changed: bool,
    scopes: AppearanceScopes,
    append_bytes: Vec<u8>,
    counter: usize,
}

impl PageAppearancePlan {
    fn new(document: &EditDocument, page: &DirectObjectTarget) -> Result<Self> {
        let original_resources = page_resources(document, page)?;
        let xobjects = resolved_resource_category(document, &original_resources, b"XObject")?;
        let properties = resolved_resource_category(document, &original_resources, b"Properties")?;
        let content = if xobjects.is_some() {
            plan_page_contents(document, page)?
        } else {
            None
        };
        Ok(Self {
            content,
            original_resources,
            xobjects: xobjects.unwrap_or_default(),
            properties_valid: properties.is_some(),
            properties: properties.unwrap_or_default(),
            properties_changed: false,
            scopes: AppearanceScopes::default(),
            append_bytes: Vec::new(),
            counter: 1,
        })
    }

    fn append(
        &mut self,
        document: &mut EditDocument,
        annotation: &OwnedDictionary,
        rotate: i32,
        widget_defaults: Option<&OwnedDictionary>,
    ) -> Result<bool> {
        if self.content.is_none() {
            return Ok(false);
        }
        let flags = annotation_flags(document, annotation)?;
        if flags & SCREEN_HIDDEN_ANNOTATION_FLAGS != 0 {
            return Ok(false);
        }
        let Some(source) = selected_normal_appearance(document, annotation)? else {
            return Ok(false);
        };
        let optional_content = annotation.get(b"OC".as_slice());
        if let Some(value) = optional_content
            && (!self.properties_valid
                || !matches!(
                    document.resolve_owned_value_cow(value)?.as_deref(),
                    Some(OwnedObject::Dictionary(_))
                ))
        {
            return Ok(false);
        }
        let resource_name = loop {
            let candidate = format!("Fxo{}", self.counter);
            self.counter += 1;
            if !self.xobjects.contains_key(candidate.as_bytes())
                && (optional_content.is_none()
                    || !self.properties.contains_key(candidate.as_bytes()))
            {
                break candidate;
            }
        };
        let Some(plan) =
            prepare_appearance(document, annotation, source, &resource_name, rotate, flags)?
        else {
            return Ok(false);
        };
        let appearance = match plan.form {
            PlannedForm::Existing(handle) => handle,
            PlannedForm::New(mut object) => {
                if plan.needs_resources {
                    let scope = self.scopes.resource(
                        document,
                        &self.original_resources,
                        widget_defaults,
                    )?;
                    if let OwnedObject::Stream { dictionary, .. } = &mut object {
                        dictionary.insert(b"Resources".to_vec(), OwnedObject::Reference(scope));
                    }
                }
                CowObjectHandle::New(document.add_object(object))
            }
        };
        if let Some(oc) = optional_content {
            self.properties
                .insert(resource_name.as_bytes().to_vec(), oc.clone());
            self.properties_changed = true;
            self.append_bytes
                .extend_from_slice(format!("/OC /{resource_name} BDC\n").as_bytes());
        }
        self.xobjects.insert(
            resource_name.into_bytes(),
            OwnedObject::Reference(appearance),
        );
        self.append_bytes.extend(plan.content);
        if optional_content.is_some() {
            self.append_bytes.extend_from_slice(b"EMC\n");
        }
        Ok(true)
    }

    fn commit(self, document: &mut EditDocument, page: &DirectObjectTarget) -> Result<()> {
        if self.append_bytes.is_empty() {
            return Ok(());
        }
        let Some(content) = self.content else {
            return Err(Error::Invalid(
                "annotation paint lacks page content plan".into(),
            ));
        };
        let mut resources = self.original_resources;
        resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(self.xobjects));
        if self.properties_changed {
            resources.insert(
                b"Properties".to_vec(),
                OwnedObject::Dictionary(self.properties),
            );
        }
        if let Some(dictionary) = page.dictionary_mut(document)? {
            dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
        }
        wrap_page_contents(document, page, content, self.append_bytes)
    }
}

fn flatten_annotations(
    document: &mut EditDocument,
    pages: &[DirectObjectTarget],
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    let need_appearances = acroform_need_appearances(document)?;
    let default_resources = if need_appearances || policy.forms {
        None
    } else {
        acroform_default_resources(document)?
    };
    for page in pages {
        let Some(entries) = page_annotation_entries(document, page)? else {
            continue;
        };
        let AnnotationEntries::Array(items) = entries else {
            *stats
                .unflattened_annotation_subtypes
                .entry("(non-array /Annots)".into())
                .or_default() += 1;
            stats.annotation_entries_dropped_unflattened += 1;
            replace_page_annotations(document, page, Vec::new())?;
            continue;
        };
        let annotations = items
            .map(|value| value.into_cow().into_owned())
            .collect::<Vec<_>>();
        let mut kept = Vec::new();
        let mut page_plan = None;
        let mut changed = false;
        let rotate = page_rotate(document, page)?;
        for annotation_value in annotations {
            let subtype = annotation_subtype(document, &annotation_value)?;
            if annotation_is_protected(subtype.as_deref(), policy) {
                kept.push(annotation_value);
                continue;
            }
            changed = true;
            let resolved = document.resolve_owned_value(&annotation_value)?;
            let dictionary = match resolved.as_ref() {
                Some(OwnedObject::Dictionary(dictionary)) => Some(dictionary),
                _ => None,
            };
            let widget = subtype.as_deref() == Some(b"Widget");
            if let Some(annotation) = dictionary
                && !(widget && need_appearances)
                && annotation.contains_key(b"AP".as_slice())
            {
                if page_plan.is_none() {
                    page_plan = Some(PageAppearancePlan::new(document, page)?);
                }
                if let Some(plan) = &mut page_plan
                    && plan.append(
                        document,
                        annotation,
                        rotate,
                        if widget {
                            default_resources.as_ref()
                        } else {
                            None
                        },
                    )?
                {
                    stats.annotation_entries_flattened += 1;
                    continue;
                }
            }
            *stats
                .unflattened_annotation_subtypes
                .entry(subtype_label(subtype.as_deref()))
                .or_default() += 1;
            if subtype.as_deref() == Some(b"Link")
                && let Some(dictionary) = dictionary
            {
                kept.push(link_visual_shell(dictionary));
                stats.link_visual_shells_retained += 1;
            } else {
                stats.annotation_entries_dropped_unflattened += 1;
            }
        }
        if let Some(plan) = page_plan {
            plan.commit(document, page)?;
        }
        if changed {
            replace_page_annotations(document, page, kept)?;
        }
    }
    // The catalog's AcroForm is handled by the independent forms policy.
    Ok(())
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
        AnnotationPolicy::Preserve if policy.links && policy.forms => {}
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
            flatten_annotations(document, pages, policy, stats)?;
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

    fn annotation_fixture(contents: &str, old_content: &[u8]) -> Result<EditDocument> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /AcroForm 12 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font 13 0 R >> /Contents {contents} /Annots [5 0 R 6 0 R 7 0 R 8 0 R 9 0 R] >>").as_bytes())?;
        pdf.stream(4, b"", old_content)?;
        pdf.object(
            5,
            b"<< /Type /Annot /Subtype /Widget /Rect [10 10 20 20] /AP << /N 10 0 R >> >>",
        )?;
        pdf.object(6, b"<< /Type /Annot /Subtype /Stamp /Rect [40 40 60 60] /AP << /N 11 0 R >> /OC 15 0 R >>")?;
        pdf.object(
            7,
            b"<< /Type /Annot /Subtype /Stamp /F 2 /Rect [40 40 60 60] /AP << /N 11 0 R >> >>",
        )?;
        pdf.object(
            8,
            b"<< /Type /Annot /Subtype /Stamp /Rect [0 0 0 0] /AP << /N 11 0 R >> >>",
        )?;
        pdf.object(9, b"<< /Type /Annot /Subtype /Link /Rect [70 70 80 80] /A << /S /URI /URI (https://example.invalid) >> >>")?;
        pdf.stream(10, b"/Subtype /Form /BBox [0 0 10 10]", b"0 0 10 10 re f")?;
        // Untyped stream shared outside its appearance use: never edit it.
        pdf.stream(
            11,
            b"/BBox [0 0 10000000 10000000]",
            b"0 0 10000000 10000000 re f",
        )?;
        pdf.object(12, b"<< /Fields [5 0 R] /DR << /Font 14 0 R >> >>")?;
        pdf.object(
            13,
            b"<< /PageFont << /Type /Font /Subtype /Type1 /BaseFont /Helvetica >> >>",
        )?;
        pdf.object(
            14,
            b"<< /DefaultFont << /Type /Font /Subtype /Type1 /BaseFont /Courier >> >>",
        )?;
        pdf.object(15, b"<< /Type /OCG /Name (Annotation layer) >>")?;
        pdf.object(16, b"[4 0 R [4 0 R]]")?;
        pdf.object(17, b"16 0 R")?;
        EditDocument::from_bytes(pdf.finish(1)?)
    }

    fn fixture_page() -> DirectObjectTarget {
        DirectObjectTarget {
            root: CowObjectHandle::Existing(crate::ObjectId::new(3, 0)),
            path: Vec::new(),
        }
    }

    #[test]
    fn annotation_flattening_preserves_forms_counts_real_paint_and_isolates_resources() -> Result<()>
    {
        let mut document = annotation_fixture("17 0 R", b"q 0 0 1 1 re f")?;
        let mut policy = PreservationConfig::visible_surface();
        policy.forms = true;
        let stats = apply_preservation_policy(&mut document, &policy)?;
        assert_eq!(stats.annotation_entries_seen, 5);
        assert_eq!(stats.annotation_entries_flattened, 1);
        assert_eq!(stats.annotation_entries_dropped_unflattened, 2);
        assert_eq!(stats.link_visual_shells_retained, 1);
        assert_eq!(
            stats.unflattened_annotation_subtypes.get("/Stamp"),
            Some(&2)
        );
        let catalog = document
            .current_value(CowObjectHandle::Existing(document.source().catalog_id()))?
            .ok_or_else(|| Error::Invalid("missing fixture catalog".into()))?;
        assert!(catalog.dictionary_entry(b"AcroForm").is_some());
        assert!(
            document
                .overlay()
                .change(crate::ObjectId::new(11, 0))
                .is_none()
        );
        let target = fixture_page();
        let annotations = page_annotations(&document, &target)?
            .ok_or_else(|| Error::Invalid("missing retained annotations".into()))?;
        assert_eq!(annotations.len(), 2);
        assert_eq!(
            annotations[0],
            OwnedObject::Reference(CowObjectHandle::Existing(crate::ObjectId::new(5, 0)))
        );
        assert!(
            annotations[1]
                .as_dictionary()
                .is_some_and(|shell| !shell.contains_key(b"A".as_slice()))
        );
        let page = target
            .current(&document)?
            .ok_or_else(|| Error::Invalid("missing fixture page".into()))?;
        let contents = page
            .dictionary_entry(b"Contents")
            .ok_or_else(|| Error::Invalid("missing flattened contents".into()))?;
        let parts = content_references(&document, contents.clone())?
            .ok_or_else(|| Error::Invalid("invalid content parts".into()))?;
        assert_eq!(parts.len(), 4);
        assert_eq!(parts[1], parts[2]);
        let decoded = crate::content::decoded_content_value(&document, contents)?;
        let decoded = String::from_utf8_lossy(&decoded);
        assert!(decoded.contains("n\nQ\nQ\nQ\n"));
        assert!(decoded.contains("/OC /Fxo1 BDC"));
        assert!(decoded.contains("0.000002 0 0 0.000002 40 40 cm"));
        assert!(decoded.contains("/Fxo1 Do\nQ\nEMC"));
        let resources = page_resources(&document, &target)?;
        let props = resolved_resource_category(&document, &resources, b"Properties")?
            .ok_or_else(|| Error::Invalid("missing Properties".into()))?;
        assert_eq!(
            props.get(b"Fxo1".as_slice()),
            Some(&OwnedObject::Reference(CowObjectHandle::Existing(
                crate::ObjectId::new(15, 0)
            )))
        );
        let xobjects = resolved_resource_category(&document, &resources, b"XObject")?
            .ok_or_else(|| Error::Invalid("missing XObjects".into()))?;
        let appearance = xobjects
            .get(b"Fxo1".as_slice())
            .ok_or_else(|| Error::Invalid("missing appearance".into()))?;
        let appearance = document
            .resolve_owned_value(appearance)?
            .ok_or_else(|| Error::Invalid("missing appearance stream".into()))?;
        let dictionary = appearance
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("missing appearance dictionary".into()))?;
        let scope = document
            .resolve_owned_value(
                dictionary
                    .get(b"Resources".as_slice())
                    .ok_or_else(|| Error::Invalid("missing appearance resources".into()))?,
            )?
            .ok_or_else(|| Error::Invalid("missing scope".into()))?;
        assert!(
            scope
                .as_dictionary()
                .is_some_and(|scope| !scope.contains_key(b"XObject".as_slice())
                    && !scope.contains_key(b"Properties".as_slice()))
        );
        Ok(())
    }

    #[test]
    fn unsafe_content_state_does_not_mutate_original_contents_or_appearances() -> Result<()> {
        for old_content in [
            b"Q".as_slice(),
            b"BT".as_slice(),
            b"/OC /hidden BDC".as_slice(),
        ] {
            let mut document = annotation_fixture("4 0 R", old_content)?;
            let target = fixture_page();
            let before = target
                .current(&document)?
                .and_then(|page| page.dictionary_entry(b"Contents"))
                .map(|value| value.into_cow().into_owned());
            let stats =
                apply_preservation_policy(&mut document, &PreservationConfig::visible_surface())?;
            assert_eq!(stats.annotation_entries_flattened, 0);
            assert_eq!(stats.annotation_entries_dropped_unflattened, 4);
            assert_eq!(stats.link_visual_shells_retained, 1);
            let after = target
                .current(&document)?
                .and_then(|page| page.dictionary_entry(b"Contents"))
                .map(|value| value.into_cow().into_owned());
            assert_eq!(before, after);
            assert!(
                document
                    .overlay()
                    .change(crate::ObjectId::new(10, 0))
                    .is_none()
            );
            assert!(
                document
                    .overlay()
                    .change(crate::ObjectId::new(11, 0))
                    .is_none()
            );
        }
        Ok(())
    }

    #[test]
    fn widget_defaults_do_not_change_page_font_bindings() -> Result<()> {
        let mut document = annotation_fixture("4 0 R", b"q Q")?;
        let stats =
            apply_preservation_policy(&mut document, &PreservationConfig::visible_surface())?;
        assert_eq!(stats.annotation_entries_flattened, 2);
        let resources = page_resources(&document, &fixture_page())?;
        let page_fonts = resolved_resource_category(&document, &resources, b"Font")?
            .ok_or_else(|| Error::Invalid("missing page fonts".into()))?;
        assert!(page_fonts.contains_key(b"PageFont".as_slice()));
        assert!(!page_fonts.contains_key(b"DefaultFont".as_slice()));
        let xobjects = resolved_resource_category(&document, &resources, b"XObject")?
            .ok_or_else(|| Error::Invalid("missing XObjects".into()))?;
        let widget = document
            .resolve_owned_value(
                xobjects
                    .get(b"Fxo1".as_slice())
                    .ok_or_else(|| Error::Invalid("missing widget appearance".into()))?,
            )?
            .ok_or_else(|| Error::Invalid("missing widget stream".into()))?;
        let scope = document
            .resolve_owned_value(
                widget
                    .as_dictionary()
                    .and_then(|dict| dict.get(b"Resources".as_slice()))
                    .ok_or_else(|| Error::Invalid("missing widget resources".into()))?,
            )?
            .ok_or_else(|| Error::Invalid("missing widget scope".into()))?;
        let fonts = resolved_resource_category(
            &document,
            scope
                .as_dictionary()
                .ok_or_else(|| Error::Invalid("invalid widget scope".into()))?,
            b"Font",
        )?
        .ok_or_else(|| Error::Invalid("missing widget fonts".into()))?;
        assert!(fonts.contains_key(b"PageFont".as_slice()));
        assert!(fonts.contains_key(b"DefaultFont".as_slice()));
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
                    OwnedObject::array(vec![direct_page, OwnedObject::Reference(leaf)]),
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
                        OwnedObject::array(vec![OwnedObject::Reference(child)]),
                    ),
                ]),
            )));
        }
        let Some(root_dictionary) = document.edit_handle(root)?.as_dictionary_mut() else {
            return Err(Error::Invalid("missing fixture root".to_owned()));
        };
        root_dictionary.insert(
            b"Kids".to_vec(),
            OwnedObject::array(vec![OwnedObject::Reference(child)]),
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
        let direct = CowObjectHandle::New(document.add_object(OwnedObject::array(vec![
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
