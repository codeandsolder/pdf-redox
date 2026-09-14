use crate::{AnnotationPolicy, PreservationConfig, Result};
use flpdf::{ObjectHandle, ObjectRef, PageDocumentHelper, Pdf};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
};

const SCREEN_HIDDEN_ANNOTATION_FLAGS: i64 = 0x02 | 0x20;
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

/// Apply semantic-preservation policy while keeping the source Catalog/page tree.
/// The final writer still emits a fresh full PDF rewrite.
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
