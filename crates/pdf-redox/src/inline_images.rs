use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{form_content, form_resources, page_content, page_resources},
};
use flpdf::{
    DetachedInlineImage, DuplicateInlineImageStats, InlineImageFingerprint,
    find_resources_detached, inspect_duplicate_inline_images_detached,
    rewrite_duplicate_inline_images_detached,
};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::LazyLock,
};

static DEBUG_RASTER: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("PDF_REDOX_DEBUG_RASTER").is_some());

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ContentTarget {
    Page(ObjectHandle),
    Form(ObjectHandle),
}

#[derive(Debug)]
struct TargetSummary {
    target: ContentTarget,
    resources: OwnedDictionary,
    content: Vec<u8>,
    counts: HashMap<InlineImageFingerprint, (usize, usize)>,
}

fn direct_resource_names(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeSet<Vec<u8>>> {
    let mut names = BTreeSet::new();
    for value in resources.values() {
        let Some(value) = document.resolve_owned_value(value)? else {
            continue;
        };
        let Some(dictionary) = value.as_dictionary() else {
            continue;
        };
        for key in dictionary.keys() {
            names.insert([b"/".as_slice(), key.as_slice()].concat());
        }
    }
    Ok(names)
}

fn detached_color_spaces(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<Option<flpdf::ObjectHandle>> {
    let Some(value) = resources.get(b"ColorSpace".as_slice()) else {
        return Ok(None);
    };
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if value.as_dictionary().is_none() {
        return Ok(None);
    }
    Ok(Some(document.detached_flpdf_object(&value)?))
}

fn collect_targets(document: &EditDocument) -> Result<Vec<ContentTarget>> {
    let mut targets = BTreeSet::new();
    for page in document.page_handles()? {
        targets.insert(ContentTarget::Page(page));
    }
    for handle in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
            continue;
        };
        if matches!(document.resolve_owned_value(subtype)?, Some(OwnedObject::Name(name)) if name == b"Form")
            && matches!(object, OwnedObject::Stream { .. })
        {
            targets.insert(ContentTarget::Form(handle));
        }
    }
    Ok(targets.into_iter().collect())
}

fn summary_for_target(
    document: &EditDocument,
    target: ContentTarget,
    min_size: usize,
) -> Result<Option<TargetSummary>> {
    let resources = match target {
        ContentTarget::Page(page) => page_resources(document, page)?,
        ContentTarget::Form(form) => form_resources(document, form)?,
    };
    let Some(resources) = resources else {
        return Ok(None);
    };
    let content = match target {
        ContentTarget::Page(page) => page_content(document, page)?,
        ContentTarget::Form(form) => form_content(document, form)?,
    };
    let color_spaces = detached_color_spaces(document, &resources)?;
    let counts = inspect_duplicate_inline_images_detached(&content, min_size, color_spaces)?;
    Ok(Some(TargetSummary {
        target,
        resources,
        content,
        counts,
    }))
}

fn image_stream(document: &mut EditDocument, image: DetachedInlineImage) -> Result<ObjectHandle> {
    let OwnedObject::Dictionary(mut dictionary) =
        crate::source::owned_from_flpdf(&image.dictionary, 0)?
    else {
        return Err(Error::Invalid(
            "detached inline image dictionary is not a dictionary".to_owned(),
        ));
    };
    dictionary.remove(b"Length".as_slice());
    Ok(ObjectHandle::New(document.overlay_mut().add(
        OwnedObject::Stream {
            dictionary,
            data: StreamData::Owned(image.data),
        },
    )))
}

fn install_rewrite(
    document: &mut EditDocument,
    target: ContentTarget,
    mut resources: OwnedDictionary,
    rewrite: flpdf::DetachedInlineImageRewrite,
    xobjects_by_fingerprint: &mut HashMap<InlineImageFingerprint, ObjectHandle>,
    stats: &mut DuplicateInlineImageStats,
) -> Result<()> {
    if rewrite.externalized_occurrences == 0 {
        return Ok(());
    }
    let mut xobjects = match resources.get(b"XObject".as_slice()) {
        Some(value) => match document.resolve_owned_value(value)? {
            Some(OwnedObject::Dictionary(dictionary)) => dictionary,
            _ => OwnedDictionary::new(),
        },
        None => OwnedDictionary::new(),
    };
    for image in rewrite.images {
        let fingerprint = image.fingerprint;
        let name = image
            .name
            .strip_prefix(b"/")
            .unwrap_or(&image.name)
            .to_vec();
        let handle = if let Some(handle) = xobjects_by_fingerprint.get(&fingerprint).copied() {
            stats.xobject_references_reused += 1;
            handle
        } else {
            let handle = image_stream(document, image)?;
            xobjects_by_fingerprint.insert(fingerprint, handle);
            stats.xobjects_created += 1;
            handle
        };
        xobjects.insert(name, OwnedObject::Reference(handle));
    }
    resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));

    match target {
        ContentTarget::Page(page) => {
            let stream = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
                dictionary: OwnedDictionary::new(),
                data: StreamData::Owned(rewrite.content),
            }));
            let object = match page {
                ObjectHandle::Existing(id) => document.edit_object(id)?,
                ObjectHandle::New(id) => document
                    .overlay_mut()
                    .added_mut(id)
                    .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
            };
            if let Some(dictionary) = object.as_dictionary_mut() {
                dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                dictionary.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
            }
        }
        ContentTarget::Form(form) => {
            let object = match form {
                ObjectHandle::Existing(id) => document.edit_object(id)?,
                ObjectHandle::New(id) => document
                    .overlay_mut()
                    .added_mut(id)
                    .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
            };
            if let OwnedObject::Stream { dictionary, data } = object {
                dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                dictionary.remove(b"Filter".as_slice());
                dictionary.remove(b"DecodeParms".as_slice());
                *data = StreamData::Owned(rewrite.content);
            }
        }
    }
    stats.occurrences_externalized += rewrite.externalized_occurrences;
    Ok(())
}

pub(crate) fn externalize_duplicate_inline_images_hayro(
    document: &mut EditDocument,
    min_size: usize,
    min_duplicate_payload_bytes: usize,
) -> Result<DuplicateInlineImageStats> {
    let targets = collect_targets(document)?;
    let mut summaries = Vec::new();
    let mut aggregate: HashMap<InlineImageFingerprint, (usize, usize, usize)> = HashMap::new();
    for target in targets {
        let Some(summary) = summary_for_target(document, target, min_size)? else {
            continue;
        };
        for (&fingerprint, &(count, bytes)) in &summary.counts {
            let entry = aggregate.entry(fingerprint).or_insert((0, bytes, 0));
            entry.0 += count;
            entry.1 = bytes;
            entry.2 += 1;
        }
        summaries.push(summary);
    }
    let selected: HashSet<_> = aggregate
        .iter()
        .filter_map(|(fingerprint, &(count, bytes, scopes))| {
            let wasted = count.saturating_sub(1).saturating_mul(bytes);
            (count >= 2 && scopes >= 2 && wasted >= min_duplicate_payload_bytes)
                .then_some(*fingerprint)
        })
        .collect();
    let mut stats = DuplicateInlineImageStats {
        fingerprints_selected: selected.len(),
        duplicate_payload_bytes: selected
            .iter()
            .filter_map(|fingerprint| aggregate.get(fingerprint))
            .map(|&(count, bytes, _)| count.saturating_sub(1).saturating_mul(bytes))
            .sum(),
        ..DuplicateInlineImageStats::default()
    };
    if selected.is_empty() {
        return Ok(stats);
    }
    let mut xobjects_by_fingerprint = HashMap::new();
    for summary in summaries {
        if !summary
            .counts
            .keys()
            .any(|fingerprint| selected.contains(fingerprint))
        {
            continue;
        }
        let names = direct_resource_names(document, &summary.resources)?;
        let color_spaces = detached_color_spaces(document, &summary.resources)?;
        let rewrite = rewrite_duplicate_inline_images_detached(
            &summary.content,
            min_size,
            color_spaces,
            names,
            selected.clone(),
        )?;
        install_rewrite(
            document,
            summary.target,
            summary.resources,
            rewrite,
            &mut xobjects_by_fingerprint,
            &mut stats,
        )?;
    }
    Ok(stats)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FragmentedInlineExternalizationStats {
    pub scopes_rewritten: usize,
    pub occurrences_externalized: usize,
    pub xobjects_created: usize,
    pub xobject_references_reused: usize,
}

#[derive(Debug, Default)]
pub(crate) struct FragmentedInlineExternalization {
    pub stats: FragmentedInlineExternalizationStats,
    pub staged_xobjects: HashSet<ObjectHandle>,
}

/// Externalize every inline image only in content scopes that are already
/// pathologically fragmented. This is a staging step for raster-layout
/// reconstruction, not a general PDF rewrite: normal pages/forms remain
/// untouched, while repeated inline sprites within one scope share a local
/// Image `XObject` immediately.
pub(crate) fn externalize_fragmented_inline_target_hayro(
    document: &mut EditDocument,
    target: ContentTarget,
    min_occurrences: usize,
) -> Result<FragmentedInlineExternalization> {
    let mut result = FragmentedInlineExternalization::default();
    let debug = *DEBUG_RASTER;
    let resources = match target {
        ContentTarget::Page(page) => page_resources(document, page)?,
        ContentTarget::Form(form) => form_resources(document, form)?,
    };
    let Some(resources) = resources else {
        if debug {
            eprintln!("raster-inline {target:?}: no resources");
        }
        return Ok(result);
    };
    let content = match target {
        ContentTarget::Page(page) => page_content(document, page)?,
        ContentTarget::Form(form) => form_content(document, form)?,
    };
    let names = direct_resource_names(document, &resources)?;
    let color_spaces = detached_color_spaces(document, &resources)?;
    let rewrite = flpdf::rewrite_all_inline_images_detached(&content, 0, color_spaces, names)?;
    if debug && rewrite.externalized_occurrences > 0 {
        eprintln!(
            "raster-inline {target:?}: content={} inline={} unique={}",
            content.len(),
            rewrite.externalized_occurrences,
            rewrite.images.len()
        );
    }
    if rewrite.externalized_occurrences < min_occurrences {
        return Ok(result);
    }

    // All-mode fingerprints are intentionally local to this resources
    // dictionary: an unresolved named color space can mean something else
    // in another scope even when its spelling and image bytes match.
    let mut local_xobjects = HashMap::new();
    let mut stats = DuplicateInlineImageStats::default();
    install_rewrite(
        document,
        target,
        resources,
        rewrite,
        &mut local_xobjects,
        &mut stats,
    )?;
    result
        .staged_xobjects
        .extend(local_xobjects.values().copied());
    result.stats.scopes_rewritten = 1;
    result.stats.occurrences_externalized = stats.occurrences_externalized;
    result.stats.xobjects_created = stats.xobjects_created;
    result.stats.xobject_references_reused = stats.xobject_references_reused;
    Ok(result)
}

/// Remove only temporary `XObject` resource entries created by fragmented-inline
/// staging that no longer have a `Do` reference after raster reconstruction.
/// Other pre-existing resource entries are left untouched.
pub(crate) fn cleanup_fragmented_inline_staging_hayro(
    document: &mut EditDocument,
    staged_xobjects: &HashSet<ObjectHandle>,
) -> Result<usize> {
    if staged_xobjects.is_empty() {
        return Ok(0);
    }
    let mut removed = 0usize;
    for target in collect_targets(document)? {
        let mut resources = match target {
            ContentTarget::Page(page) => page_resources(document, page)?,
            ContentTarget::Form(form) => form_resources(document, form)?,
        }
        .unwrap_or_default();
        let content = match target {
            ContentTarget::Page(page) => page_content(document, page)?,
            ContentTarget::Form(form) => form_content(document, form)?,
        };
        let Ok(usage) = find_resources_detached(&content) else {
            continue;
        };
        if usage.pending_operands {
            continue;
        }
        let used_xobjects = usage
            .names_by_resource_type
            .get(b"XObject".as_slice())
            .cloned()
            .unwrap_or_default();
        let Some(value) = resources.get(b"XObject".as_slice()).cloned() else {
            continue;
        };
        let Some(OwnedObject::Dictionary(mut xobjects)) = document.resolve_owned_value(&value)?
        else {
            continue;
        };
        let before = xobjects.len();
        xobjects.retain(|name, value| {
            let staged =
                matches!(value, OwnedObject::Reference(handle) if staged_xobjects.contains(handle));
            !staged || used_xobjects.contains(name)
        });
        let removed_here = before.saturating_sub(xobjects.len());
        if removed_here == 0 {
            continue;
        }
        removed += removed_here;
        resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));
        match target {
            ContentTarget::Page(page) => {
                let object = match page {
                    ObjectHandle::Existing(id) => document.edit_object(id)?,
                    ObjectHandle::New(id) => document
                        .overlay_mut()
                        .added_mut(id)
                        .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
                };
                if let Some(dictionary) = object.as_dictionary_mut() {
                    dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                }
            }
            ContentTarget::Form(form) => {
                let object = match form {
                    ObjectHandle::Existing(id) => document.edit_object(id)?,
                    ObjectHandle::New(id) => document
                        .overlay_mut()
                        .added_mut(id)
                        .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
                };
                if let Some(dictionary) = object.as_dictionary_mut() {
                    dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                }
            }
        }
    }
    Ok(removed)
}
