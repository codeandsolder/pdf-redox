use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{form_content, form_resources, page_content, page_resources},
};
use hayro_syntax::{content::UntypedIter, object::Object as HayroObject};
use sha2::{Digest as _, Sha256};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::LazyLock,
};

static DEBUG_RASTER: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("PDF_REDOX_DEBUG_RASTER").is_some());

type InlineImageFingerprint = [u8; 32];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DuplicateInlineImageStats {
    pub fingerprints_selected: usize,
    pub occurrences_externalized: usize,
    pub xobjects_created: usize,
    pub xobject_references_reused: usize,
    pub duplicate_payload_bytes: usize,
}

#[derive(Debug, Clone)]
struct DetachedInlineImage {
    name: Vec<u8>,
    dictionary: OwnedDictionary,
    data: Vec<u8>,
    fingerprint: InlineImageFingerprint,
}

#[derive(Debug, Clone, Default)]
struct DetachedInlineImageRewrite {
    content: Vec<u8>,
    images: Vec<DetachedInlineImage>,
    externalized_occurrences: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContentTarget {
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
        names.extend(dictionary.keys().cloned());
    }
    Ok(names)
}

fn expand_filter_name(value: &mut OwnedObject) {
    let OwnedObject::Name(name) = value else {
        if let OwnedObject::Array(values) = value {
            values.iter_mut().for_each(expand_filter_name);
        }
        return;
    };
    let expanded = match name.as_slice() {
        b"AHx" => Some(b"ASCIIHexDecode".as_slice()),
        b"A85" => Some(b"ASCII85Decode".as_slice()),
        b"LZW" => Some(b"LZWDecode".as_slice()),
        b"Fl" => Some(b"FlateDecode".as_slice()),
        b"RL" => Some(b"RunLengthDecode".as_slice()),
        b"CCF" => Some(b"CCITTFaxDecode".as_slice()),
        b"DCT" => Some(b"DCTDecode".as_slice()),
        _ => None,
    };
    if let Some(expanded) = expanded {
        *name = expanded.to_vec();
    }
}

fn converted_color_space(
    document: &EditDocument,
    resources: &OwnedDictionary,
    value: OwnedObject,
) -> Result<(OwnedObject, bool)> {
    let OwnedObject::Name(name) = &value else {
        return Ok((value, true));
    };
    let builtin = match name.as_slice() {
        b"G" => Some(b"DeviceGray".as_slice()),
        b"RGB" => Some(b"DeviceRGB".as_slice()),
        b"CMYK" => Some(b"DeviceCMYK".as_slice()),
        b"I" => Some(b"Indexed".as_slice()),
        _ => None,
    };
    if let Some(name) = builtin {
        return Ok((OwnedObject::Name(name.to_vec()), true));
    }
    let Some(color_spaces) = resources.get(b"ColorSpace".as_slice()) else {
        return Ok((value, false));
    };
    let Some(OwnedObject::Dictionary(color_spaces)) = document.resolve_owned_value(color_spaces)?
    else {
        return Ok((value, false));
    };
    let Some(resolved) = color_spaces.get(name.as_slice()) else {
        return Ok((value, false));
    };
    Ok((resolved.clone(), true))
}

fn converted_inline_dictionary(
    document: &EditDocument,
    resources: &OwnedDictionary,
    dictionary: &hayro_syntax::object::Dict<'_>,
) -> Result<(OwnedDictionary, bool)> {
    let source = crate::source::owned_stream_dictionary(dictionary);
    let mut out = OwnedDictionary::new();
    let mut color_space_resolved = true;
    for (key, mut value) in source {
        let target = match key.as_slice() {
            b"BPC" => b"BitsPerComponent".as_slice(),
            b"CS" => b"ColorSpace".as_slice(),
            b"D" => b"Decode".as_slice(),
            b"DP" => b"DecodeParms".as_slice(),
            b"F" => b"Filter".as_slice(),
            b"H" => b"Height".as_slice(),
            b"IM" => b"ImageMask".as_slice(),
            b"I" => b"Interpolate".as_slice(),
            b"W" => b"Width".as_slice(),
            _ => key.as_slice(),
        };
        if target == b"Filter" {
            expand_filter_name(&mut value);
        } else if target == b"ColorSpace" {
            let (resolved, complete) = converted_color_space(document, resources, value)?;
            value = resolved;
            color_space_resolved &= complete;
        }
        if target != b"Length" {
            out.insert(target.to_vec(), value);
        }
    }
    out.insert(b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec()));
    out.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec()));
    Ok((out, color_space_resolved))
}

fn hash_owned_object(
    document: &EditDocument,
    object: &OwnedObject,
    hasher: &mut Sha256,
    depth: usize,
) -> Result<bool> {
    if depth > 128 {
        return Ok(false);
    }
    match object {
        OwnedObject::Reference(_) => {
            let Some(resolved) = document.resolve_owned_value(object)? else {
                return Ok(false);
            };
            hasher.update(b"R");
            hash_owned_object(document, &resolved, hasher, depth + 1)
        }
        OwnedObject::Null => {
            hasher.update(b"N");
            Ok(true)
        }
        OwnedObject::Boolean(value) => {
            hasher.update([b'B', u8::from(*value)]);
            Ok(true)
        }
        OwnedObject::Integer(value) => {
            hasher.update(b"I");
            hasher.update(value.to_le_bytes());
            Ok(true)
        }
        OwnedObject::Real(value) => {
            hasher.update(b"F");
            hasher.update(value.to_bits().to_le_bytes());
            Ok(true)
        }
        OwnedObject::Name(value) => {
            hasher.update(b"/");
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value);
            Ok(true)
        }
        OwnedObject::String(value) => {
            hasher.update(b"S");
            hasher.update((value.len() as u64).to_le_bytes());
            hasher.update(value);
            Ok(true)
        }
        OwnedObject::Array(values) => {
            hasher.update(b"A");
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                if !hash_owned_object(document, value, hasher, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        OwnedObject::Dictionary(dictionary) => {
            hasher.update(b"D");
            hasher.update((dictionary.len() as u64).to_le_bytes());
            for (key, value) in dictionary {
                hasher.update((key.len() as u64).to_le_bytes());
                hasher.update(key);
                if !hash_owned_object(document, value, hasher, depth + 1)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        OwnedObject::Stream { .. } => Ok(false),
    }
}

fn semantic_fingerprint(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    data: &[u8],
) -> Result<Option<InlineImageFingerprint>> {
    let mut hasher = Sha256::new();
    if !hash_owned_object(
        document,
        &OwnedObject::Dictionary(dictionary.clone()),
        &mut hasher,
        0,
    )? {
        return Ok(None);
    }
    hasher.update((data.len() as u64).to_le_bytes());
    hasher.update(data);
    Ok(Some(hasher.finalize().into()))
}

fn collect_targets(document: &EditDocument) -> Result<Vec<ContentTarget>> {
    let mut targets = BTreeSet::new();
    for page in document.page_handles()? {
        targets.insert(ContentTarget::Page(page));
    }
    for handle in document.reachable_streams_with_subtype(b"Form")? {
        targets.insert(ContentTarget::Form(handle));
    }
    Ok(targets.into_iter().collect())
}

fn inspect_inline_images(
    document: &EditDocument,
    resources: &OwnedDictionary,
    content: &[u8],
    min_size: usize,
) -> Result<HashMap<InlineImageFingerprint, (usize, usize)>> {
    let mut counts = HashMap::new();
    let mut iter = UntypedIter::new(content);
    while let Some(instruction) = iter.next() {
        if &instruction.operator[..] != b"BI" {
            continue;
        }
        let Some(HayroObject::Stream(stream)) = instruction.operands().next() else {
            continue;
        };
        let data = stream.raw_data();
        if data.len() < min_size {
            continue;
        }
        let (dictionary, color_space_resolved) =
            converted_inline_dictionary(document, resources, stream.dict())?;
        if !color_space_resolved {
            continue;
        }
        if let Some(fingerprint) = semantic_fingerprint(document, &dictionary, data.as_ref())? {
            let entry = counts.entry(fingerprint).or_insert((0usize, data.len()));
            entry.0 = entry.0.saturating_add(1);
        }
    }
    Ok(counts)
}

fn next_inline_name(resource_names: &mut BTreeSet<Vec<u8>>, suffix: &mut usize) -> Vec<u8> {
    loop {
        let name = format!("IIm{}", *suffix).into_bytes();
        *suffix = suffix.saturating_add(1);
        if resource_names.insert(name.clone()) {
            return name;
        }
    }
}

#[expect(
    clippy::option_if_let_else,
    reason = "the cache-miss branch atomically updates the generated name and staged image collections"
)]
fn rewrite_inline_images(
    document: &EditDocument,
    resources: &OwnedDictionary,
    content: &[u8],
    min_size: usize,
    mut resource_names: BTreeSet<Vec<u8>>,
    selected: Option<&HashSet<InlineImageFingerprint>>,
) -> Result<DetachedInlineImageRewrite> {
    let mut iter = UntypedIter::new(content);
    let mut output = Vec::with_capacity(content.len());
    let mut cursor = 0usize;
    let mut local_names = HashMap::<InlineImageFingerprint, Vec<u8>>::new();
    let mut images = Vec::new();
    let mut externalized_occurrences = 0usize;
    let mut suffix = 1usize;

    while let Some(instruction) = iter.next() {
        if &instruction.operator[..] != b"BI" {
            continue;
        }
        let Some(HayroObject::Stream(stream)) = instruction.operands().next() else {
            continue;
        };
        let data = stream.raw_data();
        if data.len() < min_size {
            continue;
        }
        let (dictionary, color_space_resolved) =
            converted_inline_dictionary(document, resources, stream.dict())?;
        let fingerprint = if selected.is_none() || color_space_resolved {
            semantic_fingerprint(document, &dictionary, data.as_ref())?
        } else {
            None
        };
        let Some(fingerprint) = fingerprint else {
            continue;
        };
        if selected.is_some_and(|selected| !selected.contains(&fingerprint)) {
            continue;
        }
        let span = instruction.span();
        if span.start < cursor || span.end > content.len() {
            continue;
        }
        output.extend_from_slice(&content[cursor..span.start]);
        let name = if let Some(name) = local_names.get(&fingerprint) {
            name.clone()
        } else {
            let name = next_inline_name(&mut resource_names, &mut suffix);
            local_names.insert(fingerprint, name.clone());
            images.push(DetachedInlineImage {
                name: name.clone(),
                dictionary,
                data: data.into_owned(),
                fingerprint,
            });
            name
        };
        output.push(b'/');
        output.extend_from_slice(&name);
        output.extend_from_slice(b" Do\n");
        cursor = span.end;
        externalized_occurrences = externalized_occurrences.saturating_add(1);
    }
    output.extend_from_slice(&content[cursor..]);
    Ok(DetachedInlineImageRewrite {
        content: output,
        images,
        externalized_occurrences,
    })
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
    let counts = inspect_inline_images(document, &resources, &content, min_size)?;
    Ok(Some(TargetSummary {
        target,
        resources,
        content,
        counts,
    }))
}

fn image_stream(document: &mut EditDocument, image: DetachedInlineImage) -> ObjectHandle {
    ObjectHandle::New(document.add_object(OwnedObject::Stream {
        dictionary: image.dictionary,
        data: StreamData::Owned(image.data),
    }))
}

fn install_rewrite(
    document: &mut EditDocument,
    target: ContentTarget,
    mut resources: OwnedDictionary,
    rewrite: DetachedInlineImageRewrite,
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
        let name = image.name.clone();
        let handle = if let Some(handle) = xobjects_by_fingerprint.get(&fingerprint).copied() {
            stats.xobject_references_reused = stats.xobject_references_reused.saturating_add(1);
            handle
        } else {
            let handle = image_stream(document, image);
            xobjects_by_fingerprint.insert(fingerprint, handle);
            stats.xobjects_created = stats.xobjects_created.saturating_add(1);
            handle
        };
        xobjects.insert(name, OwnedObject::Reference(handle));
    }
    resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));
    match target {
        ContentTarget::Page(page) => {
            let stream = ObjectHandle::New(document.add_object(OwnedObject::Stream {
                dictionary: OwnedDictionary::new(),
                data: StreamData::Owned(rewrite.content),
            }));
            let object = document.edit_handle(page)?;
            if let Some(dictionary) = object.as_dictionary_mut() {
                dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                dictionary.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
            }
        }
        ContentTarget::Form(form) => {
            let object = document.edit_handle(form)?;
            if let OwnedObject::Stream { dictionary, data } = object {
                dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                dictionary.remove(b"Filter".as_slice());
                dictionary.remove(b"DecodeParms".as_slice());
                dictionary.remove(b"Length".as_slice());
                *data = StreamData::Owned(rewrite.content);
            }
        }
    }
    stats.occurrences_externalized = stats
        .occurrences_externalized
        .saturating_add(rewrite.externalized_occurrences);
    Ok(())
}

pub fn externalize_duplicate_inline_images_hayro(
    document: &mut EditDocument,
    min_size: usize,
    min_duplicate_payload_bytes: usize,
) -> Result<DuplicateInlineImageStats> {
    let mut summaries = Vec::new();
    let mut aggregate: HashMap<InlineImageFingerprint, (usize, usize, usize)> = HashMap::new();
    for target in collect_targets(document)? {
        let Some(summary) = summary_for_target(document, target, min_size)? else {
            continue;
        };
        for (&fingerprint, &(count, bytes)) in &summary.counts {
            let entry = aggregate.entry(fingerprint).or_insert((0, bytes, 0));
            entry.0 = entry.0.saturating_add(count);
            entry.1 = bytes;
            entry.2 = entry.2.saturating_add(1);
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
        let rewrite = rewrite_inline_images(
            document,
            &summary.resources,
            &summary.content,
            min_size,
            names,
            Some(&selected),
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
pub struct FragmentedInlineExternalizationStats {
    pub scopes_rewritten: usize,
    pub occurrences_externalized: usize,
    pub xobjects_created: usize,
    pub xobject_references_reused: usize,
}

#[derive(Debug, Default)]
pub struct FragmentedInlineExternalization {
    pub stats: FragmentedInlineExternalizationStats,
    pub staged_xobjects: HashSet<ObjectHandle>,
}

pub fn externalize_fragmented_inline_target_hayro(
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
        return Ok(result);
    };
    let content = match target {
        ContentTarget::Page(page) => page_content(document, page)?,
        ContentTarget::Form(form) => form_content(document, form)?,
    };
    let names = direct_resource_names(document, &resources)?;
    let rewrite = rewrite_inline_images(document, &resources, &content, 0, names, None)?;
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

fn used_xobject_names(content: &[u8]) -> Option<BTreeSet<Vec<u8>>> {
    let mut used = BTreeSet::new();
    let incomplete = crate::content_stream::visit_instructions(content, |instruction| {
        if &instruction.operator[..] == b"Do"
            && let Some(name) = instruction
                .operands()
                .next()
                .and_then(crate::content_stream::operand_name)
        {
            used.insert(name.to_vec());
        }
        Ok(())
    })
    .ok()?;
    (!incomplete).then_some(used)
}

/// Remove only temporary `XObject` resource entries created by fragmented-inline
/// staging that no longer have a `Do` reference after raster reconstruction.
/// Other pre-existing resource entries are left untouched.
pub fn cleanup_fragmented_inline_staging_hayro(
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
        let Some(used_xobjects) = used_xobject_names(&content) else {
            continue;
        };
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
                let object = document.edit_handle(page)?;
                if let Some(dictionary) = object.as_dictionary_mut() {
                    dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                }
            }
            ContentTarget::Form(form) => {
                let object = document.edit_handle(form)?;
                if let Some(dictionary) = object.as_dictionary_mut() {
                    dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                }
            }
        }
    }
    Ok(removed)
}
