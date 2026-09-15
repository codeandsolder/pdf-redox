use crate::{EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData};
use flpdf::{
    DetachedInlineImage, DuplicateInlineImageStats, InlineImageFingerprint,
    inspect_duplicate_inline_images_detached, rewrite_duplicate_inline_images_detached,
};
use std::collections::{BTreeSet, HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContentTarget {
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

fn page_resources(document: &EditDocument, page: ObjectHandle) -> Result<Option<OwnedDictionary>> {
    let Some(value) = document.inherited_page_value(page, b"Resources")? else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(&value)? {
        Some(OwnedObject::Dictionary(dictionary)) => Some(dictionary),
        _ => None,
    })
}

fn form_resources(document: &EditDocument, form: ObjectHandle) -> Result<Option<OwnedDictionary>> {
    let Some(object) = document.current_owned_object(form)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    let Some(value) = dictionary.get(b"Resources".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Dictionary(dictionary)) => Some(dictionary),
        _ => None,
    })
}

fn decoded_content_value(
    document: &EditDocument,
    value: &OwnedObject,
    out: &mut Vec<u8>,
) -> Result<()> {
    let value = match value {
        OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
            value
        }
        value => value.clone(),
    };
    match value {
        OwnedObject::Stream { .. } => {
            let bytes =
                document.decoded_owned_stream_data(&value, flpdf::DecodeLevel::Specialized)?;
            if !out.is_empty() && out.last() != Some(&b'\n') {
                out.push(b'\n');
            }
            out.extend_from_slice(&bytes);
        }
        OwnedObject::Array(values) => {
            for value in values {
                decoded_content_value(document, &value, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn page_content(document: &EditDocument, page: ObjectHandle) -> Result<Vec<u8>> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(Vec::new());
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    decoded_content_value(document, contents, &mut out)?;
    Ok(out)
}

fn form_content(document: &EditDocument, form: ObjectHandle) -> Result<Vec<u8>> {
    document.decoded_stream_data(form, flpdf::DecodeLevel::Specialized)
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
    let mut dictionary = match crate::source::owned_from_flpdf(&image.dictionary, 0)? {
        OwnedObject::Dictionary(dictionary) => dictionary,
        _ => {
            return Err(Error::Invalid(
                "detached inline image dictionary is not a dictionary".to_owned(),
            ));
        }
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
                    .ok_or(Error::MissingNewObject { index: id.index() })?,
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
                    .ok_or(Error::MissingNewObject { index: id.index() })?,
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
