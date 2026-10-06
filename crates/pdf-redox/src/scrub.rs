use crate::jpeg::strip_jpeg_metadata;
use crate::{
    EditDocument, ExistingObjectChange, ObjectHandle as CowObjectHandle, OwnedDictionary,
    OwnedObject, PrivacyConfig, PrivacyLevel, Result, StreamData,
};
use hayro_syntax::object::Object as HayroObject;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Default)]
pub struct ScrubStats {
    pub removed: BTreeMap<String, usize>,
    pub jpeg_metadata_bytes_removed: usize,
}

impl ScrubStats {
    fn bump(&mut self, key: &str) {
        *self.removed.entry(key.to_owned()).or_default() += 1;
    }
}

const METADATA_PRIVACY_KEYS: [(&[u8], &str); 3] = [
    (b"Metadata", "xmp-reference"),
    (b"PieceInfo", "piece-info"),
    (b"LastModified", "last-modified"),
];

const FORM_VALUE_KEYS: [&[u8]; 3] = [b"V", b"DV", b"RV"];

const DANGEROUS_ACTION_NAMES: [&[u8]; 6] = [
    b"JavaScript",
    b"Launch",
    b"SubmitForm",
    b"ImportData",
    b"Rendition",
    b"RichMediaExecute",
];

#[derive(Debug, Clone, Copy, Default)]
struct ActiveContentPlan {
    remove_action: bool,
    remove_open_action: bool,
}

fn owned_action_is_dangerous(document: &EditDocument, action: &OwnedObject) -> Result<bool> {
    let Some(action) = document.resolve_owned_value(action)? else {
        return Ok(false);
    };
    let OwnedObject::Dictionary(dictionary) = action else {
        return Ok(false);
    };
    let Some(kind) = dictionary.get(b"S".as_slice()) else {
        return Ok(false);
    };
    let Some(kind) = document.resolve_owned_value(kind)? else {
        return Ok(false);
    };
    let OwnedObject::Name(name) = kind else {
        return Ok(false);
    };
    Ok(DANGEROUS_ACTION_NAMES.contains(&name.as_slice()))
}

fn active_content_plan_for_handle(
    document: &EditDocument,
    handle: CowObjectHandle,
) -> Result<ActiveContentPlan> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(ActiveContentPlan::default());
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(ActiveContentPlan::default());
    };
    Ok(ActiveContentPlan {
        remove_action: match dictionary.get(b"A".as_slice()) {
            Some(action) => owned_action_is_dangerous(document, action)?,
            None => false,
        },
        remove_open_action: match dictionary.get(b"OpenAction".as_slice()) {
            Some(action) => owned_action_is_dangerous(document, action)?,
            None => false,
        },
    })
}

fn dictionary_has_active_action_candidate(
    contains_key: impl Fn(&[u8]) -> bool,
    cfg: &PrivacyConfig,
) -> bool {
    cfg.level == PrivacyLevel::BestEffort
        && cfg.remove_active_content
        && (contains_key(b"A") || contains_key(b"OpenAction"))
}

fn owned_object_has_active_action_candidate(object: &OwnedObject, cfg: &PrivacyConfig) -> bool {
    object.as_dictionary().is_some_and(|dictionary| {
        dictionary_has_active_action_candidate(|key| dictionary.contains_key(key), cfg)
    })
}

fn source_object_has_active_action_candidate(
    object: &HayroObject<'_>,
    cfg: &PrivacyConfig,
) -> bool {
    let has_candidate = |dictionary: &hayro_syntax::object::Dict<'_>| {
        dictionary_has_active_action_candidate(|key| dictionary.contains_key(key), cfg)
    };
    match object {
        HayroObject::Dict(dictionary) => has_candidate(dictionary),
        HayroObject::Stream(stream) => has_candidate(stream.dict()),
        _ => false,
    }
}

fn dictionary_needs_cos_privacy_scrub(
    contains_key: impl Fn(&[u8]) -> bool,
    cfg: &PrivacyConfig,
) -> bool {
    if cfg.level == PrivacyLevel::None {
        return false;
    }
    if METADATA_PRIVACY_KEYS
        .iter()
        .any(|(key, _)| contains_key(key))
    {
        return true;
    }
    if cfg.level != PrivacyLevel::BestEffort {
        return false;
    }
    if contains_key(b"Thumb") {
        return true;
    }
    if cfg.remove_active_content && contains_key(b"AA") {
        return true;
    }
    cfg.remove_form_values
        && contains_key(b"FT")
        && FORM_VALUE_KEYS.iter().any(|key| contains_key(key))
}

fn owned_dictionary_needs_cos_privacy_scrub(object: &OwnedObject, cfg: &PrivacyConfig) -> bool {
    object.as_dictionary().is_some_and(|dictionary| {
        dictionary_needs_cos_privacy_scrub(|key| dictionary.contains_key(key), cfg)
    })
}

fn source_object_needs_cos_privacy_scrub(object: &HayroObject<'_>, cfg: &PrivacyConfig) -> bool {
    let needs_scrub = |dictionary: &hayro_syntax::object::Dict<'_>| {
        dictionary_needs_cos_privacy_scrub(|key| dictionary.contains_key(key), cfg)
    };
    match object {
        HayroObject::Dict(dictionary) => needs_scrub(dictionary),
        HayroObject::Stream(stream) => needs_scrub(stream.dict()),
        _ => false,
    }
}

fn scrub_owned_cos_privacy_dictionary(
    dictionary: &mut OwnedDictionary,
    cfg: &PrivacyConfig,
    active_content: ActiveContentPlan,
    stats: &mut ScrubStats,
) {
    for (key, label) in METADATA_PRIVACY_KEYS {
        if dictionary.remove(key).is_some() {
            stats.bump(label);
        }
    }

    if cfg.level != PrivacyLevel::BestEffort {
        return;
    }
    if dictionary.remove(b"Thumb".as_slice()).is_some() {
        stats.bump("thumbnail");
    }
    if cfg.remove_active_content {
        if dictionary.remove(b"AA".as_slice()).is_some() {
            stats.bump("additional-actions");
        }
        if active_content.remove_action && dictionary.remove(b"A".as_slice()).is_some() {
            stats.bump("dangerous-action");
        }
        if active_content.remove_open_action
            && dictionary.remove(b"OpenAction".as_slice()).is_some()
        {
            stats.bump("dangerous-open-action");
        }
    }
    if cfg.remove_form_values && dictionary.contains_key(b"FT".as_slice()) {
        for key in FORM_VALUE_KEYS {
            if dictionary.remove(key).is_some() {
                stats.bump("form-value");
            }
        }
    }
}

/// Apply COS-level privacy cleanup directly to the Hayro/COW graph without
/// materializing unaffected source objects.
///
/// Metadata cleanup removes `/Info`, `/ID`, `/Metadata`, `/PieceInfo`, and
/// `/LastModified`. `BestEffort` can additionally remove thumbnails and form
/// values, active content, attachment roots, signature values, and JPEG metadata.
#[expect(
    clippy::too_many_lines,
    reason = "privacy scrubbing applies an ordered set of related catalog, trailer, stream, and object-graph transformations"
)]
pub fn scrub_edit_document_cos_privacy(
    document: &mut EditDocument,
    cfg: &PrivacyConfig,
) -> Result<ScrubStats> {
    let mut stats = ScrubStats::default();
    if cfg.level == PrivacyLevel::None {
        return Ok(stats);
    }

    for (key, label) in [
        (b"Info".as_slice(), "info-dictionary"),
        (b"ID".as_slice(), "document-id"),
    ] {
        if document.trailer_mut().remove(key).is_some() {
            stats.bump(label);
        }
    }

    // Walk the post-trailer-removal output graph once. For untouched source
    // objects, inspect privacy keys and collect outgoing references from the
    // same Hayro parse. Only the sparse set of dictionaries that actually need
    // mutation is parsed a second time when materialized into the COW overlay.
    let mut seen = BTreeSet::new();
    let mut pending = document.output_roots();
    while let Some(handle) = pending.pop() {
        if seen.contains(&handle) {
            continue;
        }

        let (references, needs_edit, inspect_actions) = match handle {
            CowObjectHandle::Existing(id) => match document.overlay().change(id) {
                Some(ExistingObjectChange::Replace(object)) => (
                    object.references(),
                    owned_dictionary_needs_cos_privacy_scrub(object, cfg),
                    owned_object_has_active_action_candidate(object, cfg),
                ),
                Some(ExistingObjectChange::Delete) => {
                    return Err(crate::Error::DeletedReferencedObject {
                        number: id.number(),
                        generation: id.generation(),
                    });
                }
                None => {
                    let (object, references) = match document.source().object_with_references(id) {
                        Ok(value) => value,
                        Err(crate::Error::MissingSourceObject { .. }) => {
                            // PDF semantics treat a missing indirect object as null.
                            seen.insert(handle);
                            continue;
                        }
                        Err(error) => return Err(error),
                    };
                    let needs_edit = source_object_needs_cos_privacy_scrub(&object, cfg);
                    let inspect_actions = source_object_has_active_action_candidate(&object, cfg);
                    (
                        references
                            .into_iter()
                            .map(CowObjectHandle::Existing)
                            .collect(),
                        needs_edit,
                        inspect_actions,
                    )
                }
            },
            CowObjectHandle::New(id) => {
                let object = document
                    .overlay()
                    .added(id)
                    .ok_or_else(|| crate::Error::MissingNewObject { index: id.index() })?;
                (
                    object.references(),
                    owned_dictionary_needs_cos_privacy_scrub(object, cfg),
                    owned_object_has_active_action_candidate(object, cfg),
                )
            }
        };

        seen.insert(handle);
        for reference in references {
            if !seen.contains(&reference) {
                pending.push(reference);
            }
        }

        let active_content = if inspect_actions {
            active_content_plan_for_handle(document, handle)?
        } else {
            ActiveContentPlan::default()
        };
        let needs_edit =
            needs_edit || active_content.remove_action || active_content.remove_open_action;
        if !needs_edit {
            continue;
        }
        match handle {
            CowObjectHandle::Existing(id) => {
                if let Some(dictionary) = document.edit_object(id)?.as_dictionary_mut() {
                    scrub_owned_cos_privacy_dictionary(dictionary, cfg, active_content, &mut stats);
                }
            }
            CowObjectHandle::New(id) => {
                if let Ok(object) = document.edit_added_object(id)
                    && let Some(dictionary) = object.as_dictionary_mut()
                {
                    scrub_owned_cos_privacy_dictionary(dictionary, cfg, active_content, &mut stats);
                }
            }
        }
    }

    if cfg.level == PrivacyLevel::BestEffort && cfg.remove_active_content {
        scrub_catalog_javascript_name_tree(document, &mut stats)?;
    }
    if cfg.level == PrivacyLevel::BestEffort && cfg.remove_attachments {
        scrub_catalog_attachments(document, &mut stats)?;
    }
    if cfg.level == PrivacyLevel::BestEffort
        && cfg.remove_signatures
        && strip_signature_values(document)?
    {
        stats.bump("signature-values");
    }
    if cfg.strip_jpeg_metadata {
        scrub_jpeg_metadata(document, cfg.aggressive_jpeg_app_scrub, &mut stats)?;
    }

    Ok(stats)
}

fn count_embedded_file_name_tree_entries(
    document: &EditDocument,
    value: &OwnedObject,
    seen: &mut BTreeSet<CowObjectHandle>,
) -> Result<usize> {
    let object = match value {
        OwnedObject::Reference(handle) => {
            if !seen.insert(*handle) {
                return Ok(0);
            }
            let Some(object) = document.current_owned_object(*handle)? else {
                return Ok(0);
            };
            object
        }
        other => other.clone(),
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(0);
    };
    let mut count = 0;
    if let Some(names) = dictionary.get(b"Names".as_slice())
        && let Some(OwnedObject::Array(values)) = document.resolve_owned_value(names)?
    {
        count += values.len() / 2;
    }
    if let Some(kids) = dictionary.get(b"Kids".as_slice())
        && let Some(OwnedObject::Array(values)) = document.resolve_owned_value(kids)?
    {
        for kid in &values {
            count += count_embedded_file_name_tree_entries(document, kid, seen)?;
        }
    }
    Ok(count)
}

fn scrub_catalog_attachments(document: &mut EditDocument, stats: &mut ScrubStats) -> Result<()> {
    let catalog_handle = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(snapshot) = document.current_owned_object(catalog_handle)? else {
        return Ok(());
    };
    let Some(catalog) = snapshot.as_dictionary() else {
        return Ok(());
    };
    let names = catalog.get(b"Names".as_slice()).cloned();
    let associated = catalog.contains_key(b"AF".as_slice());

    if let Some(names) = names {
        let embedded_value = match &names {
            OwnedObject::Reference(handle) => {
                document.current_owned_object(*handle)?.and_then(|object| {
                    object
                        .as_dictionary()
                        .and_then(|dict| dict.get(b"EmbeddedFiles".as_slice()).cloned())
                })
            }
            OwnedObject::Dictionary(dict) => dict.get(b"EmbeddedFiles".as_slice()).cloned(),
            _ => None,
        };
        let embedded_count = if let Some(value) = embedded_value.as_ref() {
            count_embedded_file_name_tree_entries(document, value, &mut BTreeSet::new())?
        } else {
            0
        };
        match names {
            OwnedObject::Reference(handle) => match handle {
                CowObjectHandle::Existing(id) => {
                    if let Some(dictionary) = document.edit_object(id)?.as_dictionary_mut()
                        && dictionary.remove(b"EmbeddedFiles".as_slice()).is_some()
                    {
                        stats.bump("embedded-file-name-tree");
                        for _ in 0..embedded_count {
                            stats.bump("embedded-file");
                        }
                    }
                }
                CowObjectHandle::New(id) => {
                    if let Ok(object) = document.edit_added_object(id)
                        && let Some(dictionary) = object.as_dictionary_mut()
                        && dictionary.remove(b"EmbeddedFiles".as_slice()).is_some()
                    {
                        stats.bump("embedded-file-name-tree");
                        for _ in 0..embedded_count {
                            stats.bump("embedded-file");
                        }
                    }
                }
            },
            OwnedObject::Dictionary(_) => {
                let catalog_id = document.source().catalog_id();
                if let Some(catalog) = document.edit_object(catalog_id)?.as_dictionary_mut()
                    && let Some(OwnedObject::Dictionary(names)) =
                        catalog.get_mut(b"Names".as_slice())
                    && names.remove(b"EmbeddedFiles".as_slice()).is_some()
                {
                    stats.bump("embedded-file-name-tree");
                    for _ in 0..embedded_count {
                        stats.bump("embedded-file");
                    }
                }
            }
            _ => {}
        }
    }
    if associated {
        let catalog_id = document.source().catalog_id();
        if let Some(catalog) = document.edit_object(catalog_id)?.as_dictionary_mut()
            && catalog.remove(b"AF".as_slice()).is_some()
        {
            stats.bump("associated-files");
        }
    }
    Ok(())
}

fn signature_field_type(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    inherited: Option<Vec<u8>>,
) -> Result<Option<Vec<u8>>> {
    let Some(value) = dictionary.get(b"FT".as_slice()) else {
        return Ok(inherited);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => inherited,
    })
}

fn pure_widget_field(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<bool> {
    let is_widget = match dictionary.get(b"Subtype".as_slice()) {
        Some(value) => {
            matches!(document.resolve_owned_value(value)?, Some(OwnedObject::Name(name)) if name == b"Widget")
        }
        None => false,
    };
    let has_field_entries = [
        b"T".as_slice(),
        b"FT",
        b"Kids",
        b"V",
        b"DV",
        b"Ff",
        b"TU",
        b"TM",
    ]
    .into_iter()
    .any(|key| dictionary.contains_key(key));
    Ok(is_widget && !has_field_entries)
}

fn strip_signature_field(
    document: &mut EditDocument,
    handle: CowObjectHandle,
    inherited_type: Option<Vec<u8>>,
    depth: usize,
    seen: &mut BTreeSet<CowObjectHandle>,
    changed: &mut bool,
) -> Result<()> {
    if depth > 100 || !seen.insert(handle) {
        return Ok(());
    }
    let Some(snapshot) = document.current_owned_object(handle)? else {
        return Ok(());
    };
    let Some(dictionary) = snapshot.as_dictionary() else {
        return Ok(());
    };
    let field_type = signature_field_type(document, dictionary, inherited_type)?;
    let kids = dictionary.get(b"Kids".as_slice()).cloned();
    let remove_value =
        field_type.as_deref() == Some(b"Sig") && dictionary.contains_key(b"V".as_slice());
    if remove_value {
        match handle {
            CowObjectHandle::Existing(id) => {
                if let Some(dictionary) = document.edit_object(id)?.as_dictionary_mut()
                    && dictionary.remove(b"V".as_slice()).is_some()
                {
                    *changed = true;
                }
            }
            CowObjectHandle::New(id) => {
                if let Ok(object) = document.edit_added_object(id)
                    && let Some(dictionary) = object.as_dictionary_mut()
                    && dictionary.remove(b"V".as_slice()).is_some()
                {
                    *changed = true;
                }
            }
        }
    }
    if depth == 100 {
        return Ok(());
    }
    let Some(kids) = kids else {
        return Ok(());
    };
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(&kids)? else {
        return Ok(());
    };
    for kid in values {
        let OwnedObject::Reference(kid_handle) = kid else {
            continue;
        };
        let Some(kid_object) = document.current_owned_object(kid_handle)? else {
            continue;
        };
        let Some(kid_dictionary) = kid_object.as_dictionary() else {
            continue;
        };
        if pure_widget_field(document, kid_dictionary)? {
            continue;
        }
        strip_signature_field(
            document,
            kid_handle,
            field_type.clone(),
            depth + 1,
            seen,
            changed,
        )?;
    }
    Ok(())
}

fn strip_signature_values(document: &mut EditDocument) -> Result<bool> {
    let catalog_handle = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog_object) = document.current_owned_object(catalog_handle)? else {
        return Ok(false);
    };
    let Some(catalog) = catalog_object.as_dictionary() else {
        return Ok(false);
    };
    let Some(acroform) = catalog.get(b"AcroForm".as_slice()) else {
        return Ok(false);
    };
    let Some(acroform_object) = document.resolve_owned_value(acroform)? else {
        return Ok(false);
    };
    let Some(acroform_dictionary) = acroform_object.as_dictionary() else {
        return Ok(false);
    };
    let Some(fields) = acroform_dictionary.get(b"Fields".as_slice()) else {
        return Ok(false);
    };
    let Some(OwnedObject::Array(fields)) = document.resolve_owned_value(fields)? else {
        return Ok(false);
    };
    let mut seen = BTreeSet::new();
    let mut changed = false;
    for field in fields {
        let OwnedObject::Reference(handle) = field else {
            continue;
        };
        strip_signature_field(document, handle, None, 0, &mut seen, &mut changed)?;
    }
    Ok(changed)
}

fn scrub_jpeg_metadata(
    document: &mut EditDocument,
    aggressive: bool,
    stats: &mut ScrubStats,
) -> Result<()> {
    let handles = document.reachable_output_objects()?;
    for handle in handles {
        let Some(OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
            continue;
        };
        if !matches!(document.resolve_owned_value(filter)?, Some(OwnedObject::Name(name)) if name == b"DCTDecode")
        {
            continue;
        }
        let raw = data.bytes(document.source())?;
        let Some((clean, removed)) = strip_jpeg_metadata(raw.as_ref(), aggressive) else {
            continue;
        };
        let object = document.edit_handle(handle)?;
        if let OwnedObject::Stream { data, .. } = object {
            *data = StreamData::Owned(clean);
            stats.bump("jpeg-metadata-stream");
            stats.jpeg_metadata_bytes_removed += removed;
        }
    }
    Ok(())
}

fn scrub_catalog_javascript_name_tree(
    document: &mut EditDocument,
    stats: &mut ScrubStats,
) -> Result<()> {
    let catalog = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog_object) = document.current_owned_object(catalog)? else {
        return Ok(());
    };
    let Some(catalog_dictionary) = catalog_object.as_dictionary() else {
        return Ok(());
    };
    let Some(names) = catalog_dictionary.get(b"Names".as_slice()).cloned() else {
        return Ok(());
    };

    match names {
        OwnedObject::Reference(handle) => {
            let Some(names_object) = document.current_owned_object(handle)? else {
                return Ok(());
            };
            let Some(names_dictionary) = names_object.as_dictionary() else {
                return Ok(());
            };
            if !names_dictionary.contains_key(b"JavaScript".as_slice()) {
                return Ok(());
            }
            match handle {
                CowObjectHandle::Existing(id) => {
                    if let Some(dictionary) = document.edit_object(id)?.as_dictionary_mut()
                        && dictionary.remove(b"JavaScript".as_slice()).is_some()
                    {
                        stats.bump("javascript-name-tree");
                    }
                }
                CowObjectHandle::New(id) => {
                    if let Ok(object) = document.edit_added_object(id)
                        && let Some(dictionary) = object.as_dictionary_mut()
                        && dictionary.remove(b"JavaScript".as_slice()).is_some()
                    {
                        stats.bump("javascript-name-tree");
                    }
                }
            }
        }
        OwnedObject::Dictionary(_) => {
            let catalog_id = document.source().catalog_id();
            if let Some(catalog_dictionary) = document.edit_object(catalog_id)?.as_dictionary_mut()
                && let Some(OwnedObject::Dictionary(names_dictionary)) =
                    catalog_dictionary.get_mut(b"Names".as_slice())
                && names_dictionary.remove(b"JavaScript".as_slice()).is_some()
            {
                stats.bump("javascript-name-tree");
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, SourcePdf, test_support::ClassicPdfBuilder};

    fn privacy_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Metadata 5 0 R /OpenAction 6 0 R >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R /PieceInfo << /Private true >> /LastModified (today) >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"/Type /Metadata /Subtype /XML", b"<xmp/>")?;
        pdf.object(6, b"<< /S /JavaScript /JS (app.alert('x')) >>")?;
        pdf.finish(1)
    }

    #[test]
    fn best_effort_scrub_removes_metadata_and_dangerous_open_action() -> Result<()> {
        let mut document = EditDocument::from_bytes(privacy_fixture()?)?;
        let stats = scrub_edit_document_cos_privacy(
            &mut document,
            &PrivacyConfig {
                level: PrivacyLevel::BestEffort,
                remove_active_content: true,
                ..PrivacyConfig::default()
            },
        )?;
        assert!(!stats.removed.is_empty());
        let source = SourcePdf::from_bytes(document.write_compact()?)?;
        let catalog = source.materialize(source.catalog_id())?;
        let catalog = catalog
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten catalog is not a dictionary".to_owned()))?;
        assert!(!catalog.contains_key(b"Metadata".as_slice()));
        assert!(!catalog.contains_key(b"OpenAction".as_slice()));
        let page_id = source
            .page_ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("rewritten fixture lost its page".to_owned()))?;
        let page = source.materialize(page_id)?;
        let page = page
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten page is not a dictionary".to_owned()))?;
        assert!(!page.contains_key(b"PieceInfo".as_slice()));
        assert!(!page.contains_key(b"LastModified".as_slice()));
        Ok(())
    }
}
