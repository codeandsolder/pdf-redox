use crate::{
    AnnotationPolicy, EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary,
    OwnedObject, PreservationConfig, Result, StreamData,
};
use flpdf::{Matrix, Rectangle};
#[cfg(test)]
use flpdf::{ObjectHandle, ObjectRef, PageDocumentHelper, Pdf};
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::io::Cursor;

const SCREEN_HIDDEN_ANNOTATION_FLAGS: i64 = 0x02 | 0x20;
#[cfg(test)]
const PAGE_SURFACE_KEYS: &[&[u8]] = &[
    b"/Type",
    b"/MediaBox",
    b"/CropBox",
    b"/BleedBox",
    b"/TrimBox",
    b"/ArtBox",
    b"/Rotate",
    b"/UserUnit",
    b"/Resources",
    b"/Contents",
    b"/Group",
    b"/Annots",
];

#[derive(Debug, Clone, Default)]
pub(crate) struct PreservationStats {
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
    let root = match target.root {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or(Error::MissingNewObject { index: id.index() })?,
    };
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

fn page_annotations_hayro(
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

fn replace_page_annotations_hayro(
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

fn annotation_subtype_hayro(
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

fn annotation_is_protected_hayro(
    document: &EditDocument,
    annotation: &OwnedObject,
    policy: &PreservationConfig,
) -> Result<bool> {
    Ok(
        match annotation_subtype_hayro(document, annotation)?.as_deref() {
            Some(b"Link") => policy.links,
            Some(b"Widget") => policy.forms,
            _ => false,
        },
    )
}

fn annotation_subtypes_hayro(
    document: &EditDocument,
    pages: &[PreservationDictionaryTarget],
) -> Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for page in pages {
        let Some(annotations) = page_annotations_hayro(document, page)? else {
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
            let label = annotation_subtype_hayro(document, &annotation)?
                .map(|name| format!("/{}", String::from_utf8_lossy(&name)))
                .unwrap_or_else(|| "(missing/non-name subtype)".to_owned());
            *counts.entry(label).or_default() += 1;
        }
    }
    Ok(counts)
}

fn filter_annotations_hayro(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
    policy: &PreservationConfig,
    preserve_other_annotations: bool,
) -> Result<usize> {
    let mut kept_total = 0;
    for page in pages {
        let Some(items) = page_annotations_hayro(document, page)? else {
            continue;
        };
        let mut kept = Vec::new();
        for annotation in items {
            let subtype = annotation_subtype_hayro(document, &annotation)?;
            let protected = annotation_is_protected_hayro(document, &annotation, policy)?;
            let explicitly_disabled = matches!(subtype.as_deref(), Some(b"Link")) && !policy.links
                || matches!(subtype.as_deref(), Some(b"Widget")) && !policy.forms;
            if protected || (preserve_other_annotations && !explicitly_disabled) {
                kept.push(annotation);
            }
        }
        kept_total += kept.len();
        replace_page_annotations_hayro(document, page, kept)?;
    }
    Ok(kept_total)
}

fn detach_protected_annotations_hayro(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
    policy: &PreservationConfig,
) -> Result<Vec<Vec<OwnedObject>>> {
    let mut protected_by_page = Vec::with_capacity(pages.len());
    for page in pages {
        let Some(items) = page_annotations_hayro(document, page)? else {
            protected_by_page.push(Vec::new());
            continue;
        };
        let mut protected = Vec::new();
        let mut processable = Vec::new();
        for annotation in items {
            if annotation_is_protected_hayro(document, &annotation, policy)? {
                protected.push(annotation);
            } else {
                processable.push(annotation);
            }
        }
        replace_page_annotations_hayro(document, page, processable)?;
        protected_by_page.push(protected);
    }
    Ok(protected_by_page)
}

fn restore_protected_annotations_hayro(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
    protected_by_page: Vec<Vec<OwnedObject>>,
) -> Result<()> {
    for (page, protected) in pages.iter().zip(protected_by_page) {
        if protected.is_empty() {
            continue;
        }
        let mut items = page_annotations_hayro(document, page)?.unwrap_or_default();
        items.extend(protected);
        replace_page_annotations_hayro(document, page, items)?;
    }
    Ok(())
}

fn retain_link_visual_shells_hayro(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
) -> Result<usize> {
    const LINK_VISUAL_KEYS: &[&[u8]] = &[b"Rect", b"Border", b"BS", b"C", b"F", b"CA"];
    let mut retained = 0;
    for page in pages {
        let Some(items) = page_annotations_hayro(document, page)? else {
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
        replace_page_annotations_hayro(document, page, shells)?;
    }
    Ok(retained)
}

fn keep_page_key_hayro(key: &[u8], policy: &PreservationConfig) -> bool {
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

fn keep_page_tree_key_hayro(key: &[u8], policy: &PreservationConfig) -> bool {
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

fn keep_catalog_key_hayro(key: &[u8], policy: &PreservationConfig) -> bool {
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

fn prune_dictionary_target_hayro(
    document: &mut EditDocument,
    target: &PreservationDictionaryTarget,
    keep: impl Fn(&[u8]) -> bool,
    stats: &mut BTreeMap<String, usize>,
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
    Ok(())
}

fn resolved_number(document: &EditDocument, value: &OwnedObject) -> Result<Option<f64>> {
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => Some(value as f64),
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
        object => CowObjectHandle::New(document.overlay_mut().add(object)),
    }
}

fn new_content_stream(document: &mut EditDocument, data: Vec<u8>) -> CowObjectHandle {
    CowObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
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

fn selected_normal_appearance_hayro(
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

fn annotation_flags_hayro(document: &EditDocument, annotation: &OwnedDictionary) -> Result<i64> {
    let Some(value) = annotation.get(b"F".as_slice()) else {
        return Ok(0);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => value,
        _ => 0,
    })
}

fn acroform_need_appearances_hayro(document: &EditDocument) -> Result<bool> {
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

fn appearance_as_form_hayro(
    document: &mut EditDocument,
    source: AppearanceSource,
) -> Result<CowObjectHandle> {
    match source {
        AppearanceSource::Handle(handle) => {
            let object = match handle {
                CowObjectHandle::Existing(id) => document.edit_object(id)?,
                CowObjectHandle::New(id) => document
                    .overlay_mut()
                    .added_mut(id)
                    .ok_or(Error::MissingNewObject { index: id.index() })?,
            };
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
            Ok(CowObjectHandle::New(document.overlay_mut().add(object)))
        }
    }
}

fn normalized_rectangle_hayro(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<Rectangle>> {
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

fn appearance_matrix_hayro(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Matrix> {
    let Some(value) = dictionary.get(b"Matrix".as_slice()) else {
        return Ok(Matrix::default());
    };
    Ok(resolved_number_array::<6>(document, value)?
        .map(Matrix::from)
        .unwrap_or_default())
}

fn appearance_content_hayro(
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
    let Some(bbox) = normalized_rectangle_hayro(document, bbox_value)? else {
        return Ok(Vec::new());
    };
    let Some(rect_value) = annotation.get(b"Rect".as_slice()) else {
        return Ok(Vec::new());
    };
    let Some(rect) = normalized_rectangle_hayro(document, rect_value)? else {
        return Ok(Vec::new());
    };
    let matrix = appearance_matrix_hayro(document, appearance_dictionary)?;
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
    Ok(format!("q\n{} cm\n/{} Do\nQ\n", placement.unparse(), resource_name).into_bytes())
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

fn resource_dictionary_hayro(
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

fn acroform_default_resources_hayro(document: &EditDocument) -> Result<Option<OwnedDictionary>> {
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

fn merge_resource_dictionaries_hayro(destination: &mut OwnedDictionary, source: OwnedDictionary) {
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

fn content_references_hayro(
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

fn wrap_page_contents_hayro(
    document: &mut EditDocument,
    page: &PreservationDictionaryTarget,
    append_bytes: Vec<u8>,
) -> Result<()> {
    let old = preservation_target_snapshot(document, page)?.and_then(|object| {
        object
            .as_dictionary()
            .and_then(|dictionary| dictionary.get(b"Contents".as_slice()).cloned())
    });
    let before = new_content_stream(document, b"q\n".to_vec());
    let mut after_bytes = b"\nQ\n".to_vec();
    after_bytes.extend_from_slice(&append_bytes);
    let after = new_content_stream(document, after_bytes);
    let mut contents = vec![OwnedObject::Reference(before)];
    contents.extend(content_references_hayro(document, old)?);
    contents.push(OwnedObject::Reference(after));
    if let Some(dictionary) = preservation_target_mut(document, page)? {
        dictionary.insert(b"Contents".to_vec(), OwnedObject::Array(contents));
    }
    Ok(())
}

fn page_rotate_hayro(document: &EditDocument, page: &PreservationDictionaryTarget) -> Result<i32> {
    let Some(value) = inherited_page_value_hayro(document, page, b"Rotate")? else {
        return Ok(0);
    };
    Ok(match document.resolve_owned_value(&value)? {
        Some(OwnedObject::Integer(value)) => i32::try_from(value).unwrap_or(0),
        _ => 0,
    })
}

fn flatten_annotations_hayro(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
    required_flags: i64,
    forbidden_flags: i64,
) -> Result<usize> {
    let need_appearances = acroform_need_appearances_hayro(document)?;
    let default_resources = if need_appearances {
        None
    } else {
        acroform_default_resources_hayro(document)?
    };
    let mut flattened_total = 0;
    for page in pages {
        let Some(annotations) = page_annotations_hayro(document, page)? else {
            continue;
        };
        let rotate = page_rotate_hayro(document, page)?;
        let mut resources = page_resources_hayro(document, page)?;
        let has_widget = annotations.iter().any(|annotation| {
            matches!(annotation_subtype_hayro(document, annotation), Ok(Some(name)) if name == b"Widget")
        });
        if has_widget && let Some(default_resources) = default_resources.clone() {
            merge_resource_dictionaries_hayro(&mut resources, default_resources);
        }
        let mut xobjects = resource_dictionary_hayro(document, &resources, b"XObject")?;
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
            let (has_appearance, appearance) =
                selected_normal_appearance_hayro(document, annotation)?;
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
            let flags = annotation_flags_hayro(document, annotation)?;
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
            let appearance = appearance_as_form_hayro(document, appearance)?;
            let content = appearance_content_hayro(
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
            wrap_page_contents_hayro(document, page, append_bytes)?;
            replace_page_annotations_hayro(document, page, kept)?;
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

fn process_annotations_hayro(
    document: &mut EditDocument,
    pages: &[PreservationDictionaryTarget],
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    stats.annotation_subtypes_seen = annotation_subtypes_hayro(document, pages)?;
    stats.annotation_entries_seen = stats.annotation_subtypes_seen.values().sum();
    match policy.annotations {
        AnnotationPolicy::Preserve => {
            let kept = filter_annotations_hayro(document, pages, policy, true)?;
            stats.annotation_entries_dropped_unflattened =
                stats.annotation_entries_seen.saturating_sub(kept);
        }
        AnnotationPolicy::Discard => {
            let kept = filter_annotations_hayro(document, pages, policy, false)?;
            stats.annotation_entries_dropped_unflattened =
                stats.annotation_entries_seen.saturating_sub(kept);
        }
        AnnotationPolicy::AppearanceOnly => {
            let protected = detach_protected_annotations_hayro(document, pages, policy)?;
            let processable_before: usize =
                annotation_subtypes_hayro(document, pages)?.values().sum();
            if processable_before > 0 {
                flatten_annotations_hayro(document, pages, 0, SCREEN_HIDDEN_ANNOTATION_FLAGS)?;
            }
            stats.unflattened_annotation_subtypes = annotation_subtypes_hayro(document, pages)?;
            let unflattened: usize = stats.unflattened_annotation_subtypes.values().sum();
            stats.annotation_entries_flattened = processable_before.saturating_sub(unflattened);
            stats.link_visual_shells_retained = retain_link_visual_shells_hayro(document, pages)?;
            stats.annotation_entries_dropped_unflattened =
                unflattened.saturating_sub(stats.link_visual_shells_retained);
            restore_protected_annotations_hayro(document, pages, protected)?;
        }
    }
    Ok(())
}

pub(crate) fn apply_preservation_policy_hayro(
    document: &mut EditDocument,
    policy: &PreservationConfig,
) -> Result<PreservationStats> {
    let (pages, page_tree_nodes) = preservation_page_tree_targets(document)?;
    let mut stats = PreservationStats {
        pages: pages.len(),
        ..PreservationStats::default()
    };
    process_annotations_hayro(document, &pages, policy, &mut stats)?;
    for page in &pages {
        prune_dictionary_target_hayro(
            document,
            page,
            |key| keep_page_key_hayro(key, policy),
            &mut stats.dropped_page_keys,
        )?;
    }
    for node in &page_tree_nodes {
        prune_dictionary_target_hayro(
            document,
            node,
            |key| keep_page_tree_key_hayro(key, policy),
            &mut stats.dropped_page_tree_keys,
        )?;
    }
    let catalog = PreservationDictionaryTarget {
        root: CowObjectHandle::Existing(document.source().catalog_id()),
        path: Vec::new(),
    };
    prune_dictionary_target_hayro(
        document,
        &catalog,
        |key| keep_catalog_key_hayro(key, policy),
        &mut stats.dropped_catalog_keys,
    )?;
    Ok(stats)
}

/// Apply semantic-preservation policy while keeping the source Catalog/page tree.
/// The final writer still emits a fresh full PDF rewrite.
#[cfg(test)]
pub(crate) fn apply_preservation_policy(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    policy: &PreservationConfig,
) -> Result<PreservationStats> {
    let page_refs = PageDocumentHelper::new(pdf).get_all_pages()?;
    let mut stats = PreservationStats {
        pages: page_refs.len(),
        ..PreservationStats::default()
    };
    process_annotations(pdf, &page_refs, policy, &mut stats)?;
    prune_pages(pdf, &page_refs, policy, &mut stats)?;
    prune_page_tree_nodes(pdf, policy, &mut stats)?;
    prune_catalog(pdf, policy, &mut stats)?;
    Ok(stats)
}

#[cfg(test)]
fn process_annotations(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    stats.annotation_subtypes_seen = annotation_subtypes(pdf, page_refs)?;
    stats.annotation_entries_seen = stats.annotation_subtypes_seen.values().sum();

    match policy.annotations {
        AnnotationPolicy::Preserve => {
            let kept = filter_annotations(pdf, page_refs, policy, true)?;
            stats.annotation_entries_dropped_unflattened =
                stats.annotation_entries_seen.saturating_sub(kept);
        }
        AnnotationPolicy::Discard => {
            let kept = filter_annotations(pdf, page_refs, policy, false)?;
            stats.annotation_entries_dropped_unflattened =
                stats.annotation_entries_seen.saturating_sub(kept);
        }
        AnnotationPolicy::AppearanceOnly => {
            let protected = detach_protected_annotations(pdf, page_refs, policy)?;
            let processable_before: usize = annotation_subtypes(pdf, page_refs)?.values().sum();
            if processable_before > 0 {
                PageDocumentHelper::new(pdf)
                    .flatten_annotations(0, SCREEN_HIDDEN_ANNOTATION_FLAGS)?;
            }
            stats.unflattened_annotation_subtypes = annotation_subtypes(pdf, page_refs)?;
            let unflattened_count: usize = stats.unflattened_annotation_subtypes.values().sum();
            stats.annotation_entries_flattened =
                processable_before.saturating_sub(unflattened_count);
            stats.link_visual_shells_retained = retain_link_visual_shells(pdf, page_refs)?;
            stats.annotation_entries_dropped_unflattened =
                unflattened_count.saturating_sub(stats.link_visual_shells_retained);
            restore_protected_annotations(pdf, page_refs, protected)?;
        }
    }
    Ok(())
}

#[cfg(test)]
fn annotation_is_protected(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    annotation: &ObjectHandle,
    policy: &PreservationConfig,
) -> Result<bool> {
    pdf.resolve(annotation)?;
    if annotation.as_dictionary().is_none() {
        return Ok(false);
    }
    let subtype = annotation.try_get_key(b"/Subtype")?;
    pdf.resolve(&subtype)?;
    Ok(match subtype.as_name().as_deref() {
        Some(b"Link") => policy.links,
        Some(b"Widget") => policy.forms,
        _ => false,
    })
}

#[cfg(test)]
fn filter_annotations(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
    policy: &PreservationConfig,
    preserve_other_annotations: bool,
) -> Result<usize> {
    let mut kept_total = 0;
    for &page_ref in page_refs {
        let page = pdf.get_object_handle(page_ref);
        if !page.try_has_key(b"/Annots")? {
            continue;
        }
        let annotations = page.try_get_key(b"/Annots")?;
        pdf.resolve(&annotations)?;
        let Some(items) = annotations.as_array() else {
            page.remove_key(b"/Annots");
            pdf.mark_object_handle_dirty(&page)?;
            continue;
        };
        let mut kept = Vec::new();
        for annotation in items {
            pdf.resolve(&annotation)?;
            let subtype = if annotation.as_dictionary().is_some() {
                let subtype = annotation.try_get_key(b"/Subtype")?;
                pdf.resolve(&subtype)?;
                subtype.as_name()
            } else {
                None
            };
            let protected = annotation_is_protected(pdf, &annotation, policy)?;
            let explicitly_disabled = matches!(subtype.as_deref(), Some(b"Link")) && !policy.links
                || matches!(subtype.as_deref(), Some(b"Widget")) && !policy.forms;
            if protected || (preserve_other_annotations && !explicitly_disabled) {
                kept.push(annotation);
            }
        }
        kept_total += kept.len();
        if kept.is_empty() {
            page.remove_key(b"/Annots");
        } else {
            page.replace_key(b"/Annots", ObjectHandle::array(kept))?;
        }
        pdf.mark_object_handle_dirty(&page)?;
    }
    Ok(kept_total)
}

#[cfg(test)]
fn detach_protected_annotations(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
    policy: &PreservationConfig,
) -> Result<Vec<Vec<ObjectHandle>>> {
    let mut protected_by_page = Vec::with_capacity(page_refs.len());
    for &page_ref in page_refs {
        let page = pdf.get_object_handle(page_ref);
        if !page.try_has_key(b"/Annots")? {
            protected_by_page.push(Vec::new());
            continue;
        }
        let annotations = page.try_get_key(b"/Annots")?;
        pdf.resolve(&annotations)?;
        let Some(items) = annotations.as_array() else {
            page.remove_key(b"/Annots");
            pdf.mark_object_handle_dirty(&page)?;
            protected_by_page.push(Vec::new());
            continue;
        };
        let mut protected = Vec::new();
        let mut processable = Vec::new();
        for annotation in items {
            if annotation_is_protected(pdf, &annotation, policy)? {
                protected.push(annotation);
            } else {
                processable.push(annotation);
            }
        }
        if processable.is_empty() {
            page.remove_key(b"/Annots");
        } else {
            page.replace_key(b"/Annots", ObjectHandle::array(processable))?;
        }
        pdf.mark_object_handle_dirty(&page)?;
        protected_by_page.push(protected);
    }
    Ok(protected_by_page)
}

#[cfg(test)]
fn restore_protected_annotations(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
    protected_by_page: Vec<Vec<ObjectHandle>>,
) -> Result<()> {
    for (&page_ref, protected) in page_refs.iter().zip(protected_by_page) {
        if protected.is_empty() {
            continue;
        }
        let page = pdf.get_object_handle(page_ref);
        let mut items = if page.try_has_key(b"/Annots")? {
            let annotations = page.try_get_key(b"/Annots")?;
            pdf.resolve(&annotations)?;
            annotations.as_array().unwrap_or_default()
        } else {
            Vec::new()
        };
        items.extend(protected);
        page.replace_key(b"/Annots", ObjectHandle::array(items))?;
        pdf.mark_object_handle_dirty(&page)?;
    }
    Ok(())
}

#[cfg(test)]
fn prune_pages(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    for &page_ref in page_refs {
        let page = pdf.get_object_handle(page_ref);
        for key in page.try_get_keys()? {
            if !keep_page_key(&key, policy) {
                page.remove_key(&key);
                *stats
                    .dropped_page_keys
                    .entry(String::from_utf8_lossy(&key).into_owned())
                    .or_default() += 1;
            }
        }
        pdf.mark_object_handle_dirty(&page)?;
    }
    Ok(())
}

#[cfg(test)]
fn keep_page_key(key: &[u8], policy: &PreservationConfig) -> bool {
    if PAGE_SURFACE_KEYS.contains(&key) {
        return true;
    }
    match key {
        b"/Parent" => true,
        b"/StructParents" | b"/Tabs" => policy.structure,
        b"/B" => policy.navigation,
        b"/Dur" | b"/Trans" | b"/AA" => policy.viewer_preferences,
        b"/Metadata" | b"/PieceInfo" | b"/LastModified" | b"/Thumb" => policy.metadata,
        b"/SeparationInfo" | b"/PresSteps" => policy.output_intents,
        _ => policy.unknown_objects,
    }
}

#[cfg(test)]
fn prune_page_tree_nodes(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    let catalog = pdf.root_handle()?;
    let root = catalog.try_get_key(b"/Pages")?;
    let mut stack = vec![root];
    let mut seen = BTreeSet::new();

    while let Some(node) = stack.pop() {
        pdf.resolve(&node)?;
        if let Some(object_ref) = node.object_ref()
            && !seen.insert(object_ref)
        {
            continue;
        }
        if node.as_dictionary().is_none() {
            continue;
        }
        let node_type = node.try_get_key(b"/Type")?;
        pdf.resolve(&node_type)?;
        if node_type.as_name().as_deref() != Some(b"Pages".as_slice()) {
            continue;
        }

        let kids = node.try_get_key(b"/Kids")?;
        pdf.resolve(&kids)?;
        if let Some(items) = kids.as_array() {
            for kid in items {
                pdf.resolve(&kid)?;
                if kid.as_dictionary().is_none() {
                    continue;
                }
                let kid_type = kid.try_get_key(b"/Type")?;
                pdf.resolve(&kid_type)?;
                if kid_type.as_name().as_deref() == Some(b"Pages".as_slice()) {
                    stack.push(kid);
                }
            }
        }

        for key in node.try_get_keys()? {
            if !keep_page_tree_key(&key, policy) {
                node.remove_key(&key);
                *stats
                    .dropped_page_tree_keys
                    .entry(String::from_utf8_lossy(&key).into_owned())
                    .or_default() += 1;
            }
        }
        pdf.mark_object_handle_dirty(&node)?;
    }
    Ok(())
}

#[cfg(test)]
fn keep_page_tree_key(key: &[u8], policy: &PreservationConfig) -> bool {
    match key {
        // Page-tree structure and the four inheritable page attributes.
        b"/Type" | b"/Parent" | b"/Kids" | b"/Count" | b"/Resources" | b"/MediaBox"
        | b"/CropBox" | b"/Rotate" => true,
        b"/Metadata" | b"/PieceInfo" | b"/LastModified" | b"/Thumb" => policy.metadata,
        b"/StructParents" | b"/Tabs" => policy.structure,
        b"/B" => policy.navigation,
        b"/Dur" | b"/Trans" | b"/AA" => policy.viewer_preferences,
        b"/SeparationInfo" | b"/PresSteps" => policy.output_intents,
        _ => policy.unknown_objects,
    }
}

#[cfg(test)]
fn prune_catalog(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    policy: &PreservationConfig,
    stats: &mut PreservationStats,
) -> Result<()> {
    let catalog = pdf.root_handle()?;
    for key in catalog.try_get_keys()? {
        if !keep_catalog_key(&key, policy) {
            catalog.remove_key(&key);
            *stats
                .dropped_catalog_keys
                .entry(String::from_utf8_lossy(&key).into_owned())
                .or_default() += 1;
        }
    }
    pdf.mark_object_handle_dirty(&catalog)?;
    Ok(())
}

#[cfg(test)]
fn keep_catalog_key(key: &[u8], policy: &PreservationConfig) -> bool {
    match key {
        b"/Type" | b"/Pages" | b"/Version" | b"/Extensions" => true,
        b"/AcroForm" => policy.forms,
        b"/Outlines" | b"/Names" | b"/Dests" | b"/PageLabels" | b"/OpenAction" | b"/Threads" => {
            policy.navigation
        }
        b"/OCProperties" => policy.optional_content,
        b"/StructTreeRoot" | b"/MarkInfo" | b"/Lang" => policy.structure,
        b"/OutputIntents" => policy.output_intents,
        b"/ViewerPreferences" | b"/PageMode" | b"/PageLayout" => policy.viewer_preferences,
        b"/Metadata" | b"/PieceInfo" | b"/LastModified" => policy.metadata,
        _ => policy.unknown_objects,
    }
}

#[cfg(test)]
fn retain_link_visual_shells(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
) -> Result<usize> {
    // Link annotations can paint a border even when they have no /AP. Keep an
    // inert annotation shell containing only keys that affect that default
    // appearance. Destinations, actions, URI targets, tooltips, and other
    // interaction semantics are deliberately excluded.
    const LINK_VISUAL_KEYS: &[&[u8]] = &[b"/Rect", b"/Border", b"/BS", b"/C", b"/F", b"/CA"];

    let mut retained = 0;
    for &page_ref in page_refs {
        let page = pdf.get_object_handle(page_ref);
        if !page.try_has_key(b"/Annots")? {
            continue;
        }
        let annotations = page.try_get_key(b"/Annots")?;
        pdf.resolve(&annotations)?;
        let Some(items) = annotations.as_array() else {
            page.remove_key(b"/Annots");
            pdf.mark_object_handle_dirty(&page)?;
            continue;
        };

        let mut shells = Vec::new();
        for annotation in items {
            pdf.resolve(&annotation)?;
            if annotation.as_dictionary().is_none() {
                continue;
            }
            let subtype = annotation.try_get_key(b"/Subtype")?;
            pdf.resolve(&subtype)?;
            if subtype.as_name().as_deref() != Some(b"Link".as_slice()) {
                continue;
            }

            let mut entries = vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Annot".to_vec())),
                (b"/Subtype".to_vec(), ObjectHandle::name(b"Link".to_vec())),
            ];
            for &key in LINK_VISUAL_KEYS {
                if annotation.try_has_key(key)? {
                    entries.push((key.to_vec(), annotation.try_get_key(key)?));
                }
            }
            shells.push(ObjectHandle::dictionary(entries));
            retained += 1;
        }

        if shells.is_empty() {
            page.remove_key(b"/Annots");
        } else {
            page.replace_key(b"/Annots", ObjectHandle::array(shells))?;
        }
        pdf.mark_object_handle_dirty(&page)?;
    }
    Ok(retained)
}

#[cfg(test)]
fn annotation_subtypes(
    pdf: &mut Pdf<Cursor<Vec<u8>>>,
    page_refs: &[ObjectRef],
) -> Result<BTreeMap<String, usize>> {
    let mut counts = BTreeMap::new();
    for &page_ref in page_refs {
        let page = pdf.get_object_handle(page_ref);
        if !page.try_has_key(b"/Annots")? {
            continue;
        }
        let annotations = page.try_get_key(b"/Annots")?;
        pdf.resolve(&annotations)?;
        let Some(items) = annotations.as_array() else {
            *counts.entry("(non-array /Annots)".to_owned()).or_default() += 1;
            continue;
        };
        for annotation in items {
            pdf.resolve(&annotation)?;
            let subtype = if annotation.as_dictionary().is_some() {
                let subtype = annotation.try_get_key(b"/Subtype")?;
                pdf.resolve(&subtype)?;
                subtype
                    .as_name()
                    .map(|name| format!("/{}", String::from_utf8_lossy(&name)))
                    .unwrap_or_else(|| "(missing/non-name subtype)".to_owned())
            } else {
                "(non-dictionary annotation)".to_owned()
            };
            *counts.entry(subtype).or_default() += 1;
        }
    }
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flpdf::ObjectHandle;
    use std::rc::Rc;

    #[test]
    fn visible_surface_policy_keeps_page_surface_and_prunes_auxiliary_keys() -> Result<()> {
        let mut source = Pdf::empty()?;
        let catalog = source.root_handle()?;
        catalog.replace_key(
            b"/Names",
            ObjectHandle::dictionary(vec![(
                b"/JavaScript".to_vec(),
                ObjectHandle::dictionary(Vec::new()),
            )]),
        )?;
        source.mark_object_handle_dirty(&catalog)?;

        let pages = catalog.try_get_key(b"/Pages")?;
        let content = source.new_stream_with_data(Rc::new(b"0 0 20 20 re f\n".to_vec()))?;
        let annotation = source.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Annot".to_vec())),
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Text".to_vec())),
            (
                b"/Rect".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(10),
                    ObjectHandle::integer(10),
                ]),
            ),
        ]))?;
        let page = source.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
            (b"/Parent".to_vec(), pages.clone()),
            (
                b"/MediaBox".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(100),
                    ObjectHandle::integer(100),
                ]),
            ),
            (b"/Resources".to_vec(), ObjectHandle::dictionary(Vec::new())),
            (b"/Contents".to_vec(), content),
            (b"/Annots".to_vec(), ObjectHandle::array(vec![annotation])),
            (b"/PieceInfo".to_vec(), ObjectHandle::dictionary(Vec::new())),
        ]))?;
        pages.replace_key(b"/Kids", ObjectHandle::array(vec![page]))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(1))?;
        pages.replace_key(b"/Rotate", ObjectHandle::integer(90))?;
        pages.replace_key(
            b"/Metadata",
            ObjectHandle::string(b"tree metadata".to_vec()),
        )?;
        pages.replace_key(b"/MadeUpVendorKey", ObjectHandle::integer(42))?;
        source.mark_object_handle_dirty(&pages)?;

        let stats = apply_preservation_policy(&mut source, &PreservationConfig::visible_surface())?;
        assert_eq!(stats.pages, 1);
        assert_eq!(stats.annotation_entries_seen, 1);
        assert_eq!(stats.annotation_entries_flattened, 0);
        assert_eq!(stats.annotation_entries_dropped_unflattened, 1);
        assert_eq!(stats.annotation_subtypes_seen.get("/Text"), Some(&1));
        assert_eq!(stats.unflattened_annotation_subtypes.get("/Text"), Some(&1));
        assert_eq!(stats.dropped_catalog_keys.get("/Names"), Some(&1));
        assert_eq!(stats.dropped_page_keys.get("/Annots"), None);
        assert_eq!(stats.dropped_page_keys.get("/PieceInfo"), Some(&1));
        assert_eq!(stats.dropped_page_keys.get("/Parent"), None);
        assert_eq!(stats.dropped_page_tree_keys.get("/Metadata"), Some(&1));
        assert_eq!(
            stats.dropped_page_tree_keys.get("/MadeUpVendorKey"),
            Some(&1)
        );
        assert!(pages.try_has_key(b"/Rotate")?);
        assert!(!pages.try_has_key(b"/Metadata")?);
        assert!(!pages.try_has_key(b"/MadeUpVendorKey")?);

        let page_refs = PageDocumentHelper::new(&mut source).get_all_pages()?;
        assert_eq!(page_refs.len(), 1);
        let page = source.get_object_handle(page_refs[0]);
        let keys = page.try_get_keys()?;
        assert!(keys.contains(b"/MediaBox".as_slice()));
        assert!(keys.contains(b"/Resources".as_slice()));
        assert!(keys.contains(b"/Contents".as_slice()));
        assert!(!keys.contains(b"/Annots".as_slice()));
        assert!(!keys.contains(b"/PieceInfo".as_slice()));
        Ok(())
    }
    #[test]
    fn appearance_only_retains_only_inert_link_visual_shell() -> Result<()> {
        let mut source = Pdf::empty()?;
        let catalog = source.root_handle()?;
        let pages = catalog.try_get_key(b"/Pages")?;
        let content = source.new_stream_with_data(Rc::new(Vec::new()))?;
        let action = ObjectHandle::dictionary(vec![
            (b"/S".to_vec(), ObjectHandle::name(b"URI".to_vec())),
            (
                b"/URI".to_vec(),
                ObjectHandle::string(b"https://example.invalid".to_vec()),
            ),
        ]);
        let link = ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Annot".to_vec())),
            (b"/Subtype".to_vec(), ObjectHandle::name(b"Link".to_vec())),
            (
                b"/Rect".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(10),
                    ObjectHandle::integer(10),
                    ObjectHandle::integer(40),
                    ObjectHandle::integer(30),
                ]),
            ),
            (
                b"/Border".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(1),
                ]),
            ),
            (
                b"/C".to_vec(),
                ObjectHandle::array(vec![ObjectHandle::integer(1)]),
            ),
            (b"/A".to_vec(), action),
            (
                b"/Contents".to_vec(),
                ObjectHandle::string(b"tooltip".to_vec()),
            ),
        ]);
        let page = source.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
            (b"/Parent".to_vec(), pages.clone()),
            (
                b"/MediaBox".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(100),
                    ObjectHandle::integer(100),
                ]),
            ),
            (b"/Resources".to_vec(), ObjectHandle::dictionary(Vec::new())),
            (b"/Contents".to_vec(), content),
            (b"/Annots".to_vec(), ObjectHandle::array(vec![link])),
        ]))?;
        pages.replace_key(b"/Kids", ObjectHandle::array(vec![page]))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(1))?;
        source.mark_object_handle_dirty(&pages)?;

        let stats = apply_preservation_policy(&mut source, &PreservationConfig::visible_surface())?;
        assert_eq!(stats.link_visual_shells_retained, 1);
        assert_eq!(stats.annotation_entries_dropped_unflattened, 0);

        let page_ref = PageDocumentHelper::new(&mut source).get_all_pages()?[0];
        let page = source.get_object_handle(page_ref);
        let annots = page.try_get_key(b"/Annots")?;
        source.resolve(&annots)?;
        let items = annots.as_array().ok_or_else(|| {
            crate::Error::Invalid("preserved /Annots must be an array".to_owned())
        })?;
        assert_eq!(items.len(), 1);
        let link = &items[0];
        source.resolve(link)?;
        assert!(
            link.try_get_key(b"/Subtype")?
                .try_is_name_and_equals(b"Link")?
        );
        assert!(link.try_has_key(b"/Rect")?);
        assert!(link.try_has_key(b"/Border")?);
        assert!(link.try_has_key(b"/C")?);
        assert!(!link.try_has_key(b"/A")?);
        assert!(!link.try_has_key(b"/Dest")?);
        assert!(!link.try_has_key(b"/Contents")?);
        Ok(())
    }

    #[test]
    fn unknown_catalog_entries_can_be_dropped() -> Result<()> {
        let mut source = Pdf::empty()?;
        let root = source.root_handle()?;
        root.replace_key(b"/MadeUpVendorKey", ObjectHandle::integer(42))?;
        source.mark_object_handle_dirty(&root)?;
        let mut policy = PreservationConfig::functional();
        policy.unknown_objects = false;
        apply_preservation_policy(&mut source, &policy)?;
        assert!(!source.root_handle()?.try_has_key(b"/MadeUpVendorKey")?);
        Ok(())
    }
}
