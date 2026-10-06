use crate::geometry::{Matrix, Rectangle};
use crate::{
    AnnotationPolicy, EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary,
    OwnedObject, PreservationConfig, Result, StreamData,
};
use std::collections::{BTreeMap, BTreeSet};

const SCREEN_HIDDEN_ANNOTATION_FLAGS: i64 = 0x02 | 0x20;
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
    pub dropped_authoring_metadata_keys: BTreeMap<String, usize>,
    pub spliced_unknown_wrapper_keys: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum PreservationPathStep {
    DictKey(Vec<u8>),
    ArrayIndex(usize),
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PreservationDictionaryTarget {
    root: CowObjectHandle,
    path: Vec<PreservationPathStep>,
}

fn preservation_object_at_path<'a>(
    mut object: &'a OwnedObject,
    path: &[PreservationPathStep],
) -> Option<&'a OwnedObject> {
    for step in path {
        object = match step {
            PreservationPathStep::DictKey(key) => object.as_dictionary()?.get(key.as_slice())?,
            PreservationPathStep::ArrayIndex(index) => match object {
                OwnedObject::Array(values) => values.get(*index)?,
                _ => return None,
            },
        };
    }
    Some(object)
}

fn preservation_object_at_path_mut<'a>(
    mut object: &'a mut OwnedObject,
    path: &[PreservationPathStep],
) -> Option<&'a mut OwnedObject> {
    for step in path {
        object = match step {
            PreservationPathStep::DictKey(key) => {
                object.as_dictionary_mut()?.get_mut(key.as_slice())?
            }
            PreservationPathStep::ArrayIndex(index) => match object {
                OwnedObject::Array(values) => values.get_mut(*index)?,
                _ => return None,
            },
        };
    }
    Some(object)
}

fn preservation_target_snapshot(
    document: &EditDocument,
    target: &PreservationDictionaryTarget,
) -> Result<Option<OwnedObject>> {
    let Some(root) = document.current_owned_object(target.root)? else {
        return Ok(None);
    };
    Ok(preservation_object_at_path(&root, &target.path).cloned())
}

fn preservation_target_mut<'a>(
    document: &'a mut EditDocument,
    target: &PreservationDictionaryTarget,
) -> Result<Option<&'a mut OwnedDictionary>> {
    let root = document.edit_handle(target.root)?;
    Ok(
        preservation_object_at_path_mut(root, &target.path)
            .and_then(OwnedObject::as_dictionary_mut),
    )
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

fn collect_page_tree_value(
    document: &EditDocument,
    root: CowObjectHandle,
    value: &OwnedObject,
    path: Vec<PreservationPathStep>,
    seen: &mut BTreeSet<CowObjectHandle>,
    pages: &mut BTreeSet<PreservationDictionaryTarget>,
    nodes: &mut BTreeSet<PreservationDictionaryTarget>,
) -> Result<()> {
    if let OwnedObject::Reference(handle) = value {
        if !seen.insert(*handle) {
            return Ok(());
        }
        let Some(object) = document.current_owned_object(*handle)? else {
            return Ok(());
        };
        return collect_page_tree_value(document, *handle, &object, Vec::new(), seen, pages, nodes);
    }
    match value {
        OwnedObject::Dictionary(dictionary) => {
            let target = PreservationDictionaryTarget {
                root,
                path: path.clone(),
            };
            let kind = owned_name(document, dictionary.get(b"Type".as_slice()))?;
            if kind.as_deref() == Some(b"Pages") {
                nodes.insert(target);
                if let Some(kids) = dictionary.get(b"Kids".as_slice()) {
                    let mut kid_path = path;
                    kid_path.push(PreservationPathStep::DictKey(b"Kids".to_vec()));
                    collect_page_tree_value(document, root, kids, kid_path, seen, pages, nodes)?;
                }
            } else if kind.as_deref() == Some(b"Page") {
                pages.insert(target);
            }
        }
        OwnedObject::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                let mut child_path = path.clone();
                child_path.push(PreservationPathStep::ArrayIndex(index));
                collect_page_tree_value(document, root, child, child_path, seen, pages, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn preservation_page_tree_targets(
    document: &EditDocument,
) -> Result<(
    Vec<PreservationDictionaryTarget>,
    Vec<PreservationDictionaryTarget>,
)> {
    let catalog_handle = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog_object) = document.current_owned_object(catalog_handle)? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let Some(catalog) = catalog_object.as_dictionary() else {
        return Ok((Vec::new(), Vec::new()));
    };
    let Some(pages_value) = catalog.get(b"Pages".as_slice()) else {
        return Ok((Vec::new(), Vec::new()));
    };
    let mut pages = BTreeSet::new();
    let mut nodes = BTreeSet::new();
    let mut seen = BTreeSet::new();
    collect_page_tree_value(
        document,
        catalog_handle,
        pages_value,
        vec![PreservationPathStep::DictKey(b"Pages".to_vec())],
        &mut seen,
        &mut pages,
        &mut nodes,
    )?;
    for id in document.source().page_ids() {
        pages.insert(PreservationDictionaryTarget {
            root: CowObjectHandle::Existing(id),
            path: Vec::new(),
        });
    }
    Ok((pages.into_iter().collect(), nodes.into_iter().collect()))
}

fn page_annotations(
    document: &EditDocument,
    page: &PreservationDictionaryTarget,
) -> Result<Option<Vec<OwnedObject>>> {
    let Some(page_object) = preservation_target_snapshot(document, page)? else {
        return Ok(None);
    };
    let Some(dictionary) = page_object.as_dictionary() else {
        return Ok(None);
    };
    let Some(annots) = dictionary.get(b"Annots".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(annots)? {
        Some(OwnedObject::Array(values)) => Some(values),
        _ => Some(Vec::new()),
    })
}

fn replace_page_annotations(
    document: &mut EditDocument,
    page: &PreservationDictionaryTarget,
    annotations: Vec<OwnedObject>,
) -> Result<()> {
    let Some(dictionary) = preservation_target_mut(document, page)? else {
        return Ok(());
    };
    if annotations.is_empty() {
        dictionary.remove(b"Annots".as_slice());
    } else {
        dictionary.insert(b"Annots".to_vec(), OwnedObject::Array(annotations));
    }
    Ok(())
}

fn annotation_subtype(
    document: &EditDocument,
    annotation: &OwnedObject,
) -> Result<Option<Vec<u8>>> {
    let Some(annotation) = document.resolve_owned_value(annotation)? else {
        return Ok(None);
    };
    let Some(dictionary) = annotation.as_dictionary() else {
        return Ok(None);
    };
    owned_name(document, dictionary.get(b"Subtype".as_slice()))
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
    pages: &[PreservationDictionaryTarget],
) -> Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for page in pages {
        let Some(annotations) = page_annotations(document, page)? else {
            continue;
        };
        if annotations.is_empty() {
            let Some(page_object) = preservation_target_snapshot(document, page)? else {
                continue;
            };
            let Some(dictionary) = page_object.as_dictionary() else {
                continue;
            };
            if dictionary.contains_key(b"Annots".as_slice()) {
                *counts.entry("(non-array /Annots)".to_owned()).or_default() += 1;
            }
            continue;
        }
        for annotation in annotations {
            let label = annotation_subtype(document, &annotation)?.map_or_else(
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
    pages: &[PreservationDictionaryTarget],
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
    pages: &[PreservationDictionaryTarget],
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
    pages: &[PreservationDictionaryTarget],
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
    pages: &[PreservationDictionaryTarget],
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

const fn keep_page_key_hayro(key: &[u8], policy: &PreservationConfig) -> bool {
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
        b"Metadata" | b"PieceInfo" | b"LastModified" | b"Thumb" => policy.metadata,
        b"SeparationInfo" | b"PresSteps" => policy.output_intents,
        _ => policy.unknown_objects,
    }
}

const fn keep_page_tree_key_hayro(key: &[u8], policy: &PreservationConfig) -> bool {
    match key {
        b"Type" | b"Parent" | b"Kids" | b"Count" | b"Resources" | b"MediaBox" | b"CropBox"
        | b"Rotate" => true,
        b"Metadata" | b"PieceInfo" | b"LastModified" | b"Thumb" => policy.metadata,
        b"StructParents" | b"Tabs" => policy.structure,
        b"B" => policy.navigation,
        b"Dur" | b"Trans" | b"AA" => policy.viewer_preferences,
        b"SeparationInfo" | b"PresSteps" => policy.output_intents,
        _ => policy.unknown_objects,
    }
}

const fn keep_catalog_key_hayro(key: &[u8], policy: &PreservationConfig) -> bool {
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
        b"Metadata" | b"PieceInfo" | b"LastModified" => policy.metadata,
        _ => policy.unknown_objects,
    }
}

fn collect_direct_authoring_metadata_removals(
    root: CowObjectHandle,
    object: &OwnedObject,
    path: &mut Vec<PreservationPathStep>,
    removals: &mut BTreeMap<PreservationDictionaryTarget, BTreeSet<Vec<u8>>>,
) {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if dictionary.contains_key(b"Metadata".as_slice()) {
                removals
                    .entry(PreservationDictionaryTarget {
                        root,
                        path: path.clone(),
                    })
                    .or_default()
                    .insert(b"Metadata".to_vec());
            }
            for (key, value) in dictionary {
                if key.as_slice() == b"Metadata" || matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(PreservationPathStep::DictKey(key.clone()));
                collect_direct_authoring_metadata_removals(root, value, path, removals);
                path.pop();
            }
        }
        OwnedObject::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                path.push(PreservationPathStep::ArrayIndex(index));
                collect_direct_authoring_metadata_removals(root, value, path, removals);
                path.pop();
            }
        }
        OwnedObject::Reference(_)
        | OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
}

fn drop_authoring_metadata(
    document: &mut EditDocument,
    stats: &mut PreservationStats,
) -> Result<()> {
    // Discover every removable authoring-metadata key in one reachability walk.
    // Recurse only through direct dictionaries/arrays; referenced objects are
    // visited separately as roots, so discovery cannot follow graph cycles.
    let mut removals = BTreeMap::<PreservationDictionaryTarget, BTreeSet<Vec<u8>>>::new();
    for root in document.reachable_output_objects()? {
        let Some(snapshot) = document.current_owned_object(root)? else {
            continue;
        };
        collect_direct_authoring_metadata_removals(root, &snapshot, &mut Vec::new(), &mut removals);

        let Some(dictionary) = snapshot.as_dictionary() else {
            continue;
        };
        let is_form = matches!(snapshot, OwnedObject::Stream { .. })
            && match dictionary.get(b"Subtype".as_slice()) {
                Some(value) => matches!(
                    document.resolve_owned_value(value)?,
                    Some(OwnedObject::Name(name)) if name == b"Form"
                ),
                None => false,
            };
        if is_form {
            let target = PreservationDictionaryTarget {
                root,
                path: Vec::new(),
            };
            for key in [b"PieceInfo".as_slice(), b"LastModified".as_slice()] {
                if dictionary.contains_key(key) {
                    removals
                        .entry(target.clone())
                        .or_default()
                        .insert(key.to_vec());
                }
            }
        }
    }

    for (target, keys) in removals {
        let root = document.edit_handle(target.root)?;
        let Some(dictionary) = preservation_object_at_path_mut(root, &target.path)
            .and_then(OwnedObject::as_dictionary_mut)
        else {
            continue;
        };
        for key in keys {
            if dictionary.remove(key.as_slice()).is_some() {
                *stats
                    .dropped_authoring_metadata_keys
                    .entry(format!("/{}", String::from_utf8_lossy(&key)))
                    .or_default() += 1;
            }
        }
    }
    Ok(())
}

fn prune_dictionary_target(
    document: &mut EditDocument,
    target: &PreservationDictionaryTarget,
    keep: impl Fn(&[u8]) -> bool,
    splice_unknown_wrappers: bool,
    stats: &mut BTreeMap<String, usize>,
    spliced: &mut BTreeMap<String, usize>,
) -> Result<()> {
    let Some(snapshot) = preservation_target_snapshot(document, target)? else {
        return Ok(());
    };
    let Some(dictionary) = snapshot.as_dictionary() else {
        return Ok(());
    };
    let remove = dictionary
        .keys()
        .filter(|key| !keep(key))
        .cloned()
        .collect::<Vec<_>>();
    if remove.is_empty() {
        return Ok(());
    }

    let mut promotions = Vec::new();
    if splice_unknown_wrappers {
        let existing = dictionary.keys().cloned().collect::<BTreeSet<_>>();
        let mut planned = BTreeSet::new();
        for wrapper_key in &remove {
            let Some(wrapper) = dictionary.get(wrapper_key.as_slice()) else {
                continue;
            };
            let Some(resolved) = document.resolve_owned_value(wrapper)? else {
                continue;
            };
            let Some(wrapper_dict) = resolved.as_dictionary() else {
                continue;
            };
            for (child_key, child_value) in wrapper_dict {
                if keep(child_key)
                    && !existing.contains(child_key)
                    && planned.insert(child_key.clone())
                {
                    promotions.push((child_key.clone(), child_value.clone()));
                    *spliced
                        .entry(format!(
                            "/{} -> /{}",
                            String::from_utf8_lossy(wrapper_key),
                            String::from_utf8_lossy(child_key)
                        ))
                        .or_default() += 1;
                }
            }
        }
    }

    let Some(dictionary) = preservation_target_mut(document, target)? else {
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

fn resolved_number(document: &EditDocument, value: &OwnedObject) -> Result<Option<f64>> {
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => crate::source::exact_i64_to_f64(value),
        Some(OwnedObject::Real(value)) => Some(value),
        _ => None,
    })
}

fn resolved_number_array<const N: usize>(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<[f64; N]>> {
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != N {
        return Ok(None);
    }
    let mut out = [0.0; N];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = resolved_number(document, value)? else {
            return Ok(None);
        };
        out[index] = number;
    }
    Ok(Some(out))
}

fn inherited_page_value_hayro(
    document: &EditDocument,
    page: &PreservationDictionaryTarget,
    key: &[u8],
) -> Result<Option<OwnedObject>> {
    let Some(mut object) = preservation_target_snapshot(document, page)? else {
        return Ok(None);
    };
    let mut seen = BTreeSet::new();
    for _ in 0..=100 {
        let Some(dictionary) = object.as_dictionary() else {
            return Ok(None);
        };
        if let Some(value) = dictionary.get(key) {
            return Ok(Some(value.clone()));
        }
        let Some(OwnedObject::Reference(parent)) = dictionary.get(b"Parent".as_slice()) else {
            return Ok(None);
        };
        if !seen.insert(*parent) {
            return Ok(None);
        }
        let Some(parent_object) = document.current_owned_object(*parent)? else {
            return Ok(None);
        };
        object = parent_object;
    }
    Ok(None)
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
    Ok(resolved_number_array::<6>(document, value)?
        .map(Matrix::from)
        .unwrap_or_default())
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

fn page_resources_hayro(
    document: &EditDocument,
    page: &PreservationDictionaryTarget,
) -> Result<OwnedDictionary> {
    let Some(resources) = inherited_page_value_hayro(document, page, b"Resources")? else {
        return Ok(OwnedDictionary::new());
    };
    Ok(match document.resolve_owned_value(&resources)? {
        Some(OwnedObject::Dictionary(dictionary)) => dictionary,
        _ => OwnedDictionary::new(),
    })
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
    page: &PreservationDictionaryTarget,
    append_bytes: &[u8],
) -> Result<()> {
    let old = preservation_target_snapshot(document, page)?.and_then(|object| {
        object
            .as_dictionary()
            .and_then(|dictionary| dictionary.get(b"Contents".as_slice()).cloned())
    });
    let before = new_content_stream(document, b"q\n".to_vec());
    let mut after_bytes = b"\nQ\n".to_vec();
    after_bytes.extend_from_slice(append_bytes);
    let after = new_content_stream(document, after_bytes);
    let mut contents = vec![OwnedObject::Reference(before)];
    contents.extend(content_references(document, old)?);
    contents.push(OwnedObject::Reference(after));
    if let Some(dictionary) = preservation_target_mut(document, page)? {
        dictionary.insert(b"Contents".to_vec(), OwnedObject::Array(contents));
    }
    Ok(())
}

fn page_rotate(document: &EditDocument, page: &PreservationDictionaryTarget) -> Result<i32> {
    let Some(value) = inherited_page_value_hayro(document, page, b"Rotate")? else {
        return Ok(0);
    };
    Ok(match document.resolve_owned_value(&value)? {
        Some(OwnedObject::Integer(value)) => i32::try_from(value).unwrap_or(0),
        _ => 0,
    })
}

fn flatten_annotations(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
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
        let mut resources = page_resources_hayro(document, page)?;
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
        if let Some(page_dictionary) = preservation_target_mut(document, page)? {
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
    pages: &[PreservationDictionaryTarget],
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

pub fn apply_preservation_policy(
    document: &mut EditDocument,
    policy: &PreservationConfig,
) -> Result<PreservationStats> {
    let (pages, page_tree_nodes) = preservation_page_tree_targets(document)?;
    let mut stats = PreservationStats {
        pages: pages.len(),
        ..PreservationStats::default()
    };
    process_annotations(document, &pages, policy, &mut stats)?;
    if !policy.metadata {
        drop_authoring_metadata(document, &mut stats)?;
    }
    for page in &pages {
        prune_dictionary_target(
            document,
            page,
            |key| keep_page_key_hayro(key, policy),
            policy.splice_unknown_wrappers,
            &mut stats.dropped_page_keys,
            &mut stats.spliced_unknown_wrapper_keys,
        )?;
    }
    for node in &page_tree_nodes {
        prune_dictionary_target(
            document,
            node,
            |key| keep_page_tree_key_hayro(key, policy),
            policy.splice_unknown_wrappers,
            &mut stats.dropped_page_tree_keys,
            &mut stats.spliced_unknown_wrapper_keys,
        )?;
    }
    let catalog = PreservationDictionaryTarget {
        root: CowObjectHandle::Existing(document.source().catalog_id()),
        path: Vec::new(),
    };
    prune_dictionary_target(
        document,
        &catalog,
        |key| keep_catalog_key_hayro(key, policy),
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
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R /Thumb 5 0 R /UnknownPage 7 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[0])?;
        pdf.stream(6, b"/Type /Metadata /Subtype /XML", b"<xmp/>")?;
        pdf.object(7, b"<< /Private true >>")?;
        pdf.finish(1)
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
