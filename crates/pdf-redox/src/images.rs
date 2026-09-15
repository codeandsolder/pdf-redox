use crate::{EditDocument, Error, ObjectHandle, OwnedObject, Result, StreamData};
use flpdf::{
    DetachedImageTransform, ImageOptimizationOptions, ImageOptimizationStats, ImageResizeTarget,
    optimize_image_detached,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

fn is_image(document: &EditDocument, handle: ObjectHandle) -> Result<bool> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(false);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(false);
    };
    Ok(
        matches!(document.resolve_owned_value(subtype)?, Some(OwnedObject::Name(name)) if name == b"Image")
            && matches!(object, OwnedObject::Stream { .. }),
    )
}

fn object_subtype(document: &EditDocument, handle: ObjectHandle) -> Result<Option<Vec<u8>>> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(subtype)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn xobject_bindings(
    document: &EditDocument,
    resources: Option<OwnedObject>,
) -> Result<BTreeMap<Vec<u8>, ObjectHandle>> {
    let Some(resources) = resources else {
        return Ok(BTreeMap::new());
    };
    let Some(resources) = document.resolve_owned_value(&resources)? else {
        return Ok(BTreeMap::new());
    };
    let Some(resources) = resources.as_dictionary() else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = resources.get(b"XObject".as_slice()) else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = document.resolve_owned_value(xobjects)? else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = xobjects.as_dictionary() else {
        return Ok(BTreeMap::new());
    };
    Ok(xobjects
        .iter()
        .filter_map(|(name, value)| match value {
            OwnedObject::Reference(handle) => Some((name.clone(), *handle)),
            _ => None,
        })
        .collect())
}

fn accumulate_xobject_bindings(
    document: &EditDocument,
    xobjects: &BTreeMap<Vec<u8>, ObjectHandle>,
    counts: &mut BTreeMap<ObjectHandle, usize>,
    forms: &mut VecDeque<(ObjectHandle, BTreeMap<Vec<u8>, ObjectHandle>)>,
) -> Result<()> {
    for handle in xobjects.values().copied() {
        match object_subtype(document, handle)?.as_deref() {
            Some(b"Image") => *counts.entry(handle).or_default() += 1,
            Some(b"Form") => forms.push_back((handle, xobjects.clone())),
            _ => {}
        }
    }
    Ok(())
}

fn image_binding_counts(document: &EditDocument) -> Result<BTreeMap<ObjectHandle, usize>> {
    let mut counts = BTreeMap::new();
    for page in document.page_handles()? {
        let page_xobjects =
            xobject_bindings(document, document.inherited_page_value(page, b"Resources")?)?;
        let mut forms = VecDeque::new();
        accumulate_xobject_bindings(document, &page_xobjects, &mut counts, &mut forms)?;
        let mut seen_forms = HashSet::new();
        while let Some((form, inherited_xobjects)) = forms.pop_front() {
            if !seen_forms.insert(form) {
                continue;
            }
            let Some(object) = document.current_owned_object(form)? else {
                continue;
            };
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };
            let xobjects = if let Some(resources) = dictionary.get(b"Resources".as_slice()) {
                xobject_bindings(document, Some(resources.clone()))?
            } else {
                inherited_xobjects
            };
            accumulate_xobject_bindings(document, &xobjects, &mut counts, &mut forms)?;
        }
    }
    Ok(counts)
}

fn install_transform(
    document: &mut EditDocument,
    handle: ObjectHandle,
    transform: DetachedImageTransform,
) -> Result<()> {
    let decode_params = if transform.decode_params.is_null() {
        None
    } else {
        Some(crate::source::owned_from_flpdf(
            &transform.decode_params,
            0,
        )?)
    };
    let object = match handle {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or(Error::MissingNewObject { index: id.index() })?,
    };
    let OwnedObject::Stream { dictionary, data } = object else {
        return Ok(());
    };
    dictionary.insert(
        b"Width".to_vec(),
        OwnedObject::Integer(i64::from(transform.width)),
    );
    dictionary.insert(
        b"Height".to_vec(),
        OwnedObject::Integer(i64::from(transform.height)),
    );
    dictionary.insert(b"Filter".to_vec(), OwnedObject::Name(transform.filter));
    match decode_params {
        Some(value) => {
            dictionary.insert(b"DecodeParms".to_vec(), value);
        }
        None => {
            dictionary.remove(b"DecodeParms".as_slice());
        }
    }
    dictionary.remove(b"Length".as_slice());
    *data = StreamData::Owned(transform.encoded);
    Ok(())
}

fn transform_one(
    document: &mut EditDocument,
    handle: ObjectHandle,
    options: ImageOptimizationOptions,
    target: Option<ImageResizeTarget>,
    stats: &mut ImageOptimizationStats,
) -> Result<bool> {
    if !is_image(document, handle)? {
        return Ok(false);
    }
    let Some(image) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    let detached = document.detached_flpdf_object(&image)?;
    let Some(transform) = optimize_image_detached(detached, options, target)? else {
        return Ok(false);
    };
    stats.images_optimized += 1;
    if transform.resized {
        stats.images_resized += 1;
        match target.map(|target| target.encoding) {
            Some(flpdf::ImageResizeEncoding::Jpeg) => stats.jpeg_images_resized += 1,
            Some(flpdf::ImageResizeEncoding::Flate) => stats.flate_images_resized += 1,
            None => {}
        }
    }
    stats.original_encoded_bytes += transform.original_encoded_bytes;
    stats.optimized_encoded_bytes += transform.encoded.len() as u64;
    stats.original_pixels += transform.original_pixels;
    stats.optimized_pixels += transform.optimized_pixels;
    install_transform(document, handle, transform)?;
    Ok(true)
}

pub(crate) fn optimize_images_hayro(
    document: &mut EditDocument,
    options: ImageOptimizationOptions,
) -> Result<ImageOptimizationStats> {
    let binding_counts = image_binding_counts(document)?;
    let mut stats = ImageOptimizationStats::default();
    let mut images = BTreeSet::new();
    for handle in document.reachable_output_objects()? {
        if is_image(document, handle)? {
            images.insert(handle);
        }
    }
    for image in images {
        if transform_one(document, image, options, None, &mut stats)? {
            stats.references_reused += binding_counts
                .get(&image)
                .copied()
                .unwrap_or(1)
                .saturating_sub(1);
        }
    }
    Ok(stats)
}

pub(crate) fn optimize_images_with_resize_targets_hayro(
    document: &mut EditDocument,
    options: ImageOptimizationOptions,
    targets: &HashMap<(ObjectHandle, ObjectHandle), ImageResizeTarget>,
) -> Result<ImageOptimizationStats> {
    let mut by_image = BTreeMap::<ObjectHandle, ImageResizeTarget>::new();
    let mut binding_counts = BTreeMap::<ObjectHandle, usize>::new();
    for (&(_page, image), &target) in targets {
        match by_image.get(&image).copied() {
            None => {
                by_image.insert(image, target);
            }
            Some(existing) if existing == target => {}
            Some(existing) => {
                // This should not happen because the placement planner derives one
                // target from the maximum observed use. Fall back to the less
                // aggressive dimensions if a caller supplies conflicting targets.
                let conservative = ImageResizeTarget {
                    width: existing.width.max(target.width),
                    height: existing.height.max(target.height),
                    encoding: existing.encoding,
                };
                by_image.insert(image, conservative);
            }
        }
        *binding_counts.entry(image).or_default() += 1;
    }

    let mut stats = ImageOptimizationStats::default();
    for (image, target) in by_image {
        if transform_one(document, image, options, Some(target), &mut stats)? {
            stats.references_reused += binding_counts
                .get(&image)
                .copied()
                .unwrap_or(1)
                .saturating_sub(1);
        }
    }
    Ok(stats)
}
