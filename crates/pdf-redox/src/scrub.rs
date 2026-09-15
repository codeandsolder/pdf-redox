use crate::jpeg::strip_jpeg_metadata;
use crate::{
    EditDocument, ExistingObjectChange, ObjectHandle as CowObjectHandle, OwnedDictionary,
    OwnedObject, PrivacyConfig, PrivacyLevel, Result,
};
use flpdf::{ObjectHandle, Pdf};
use hayro_syntax::object::Object as HayroObject;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek};
use std::rc::Rc;

#[derive(Debug, Default)]
pub(crate) struct ScrubStats {
    pub removed: BTreeMap<String, usize>,
    pub jpeg_metadata_bytes_removed: usize,
}

impl ScrubStats {
    fn bump(&mut self, key: &str) {
        *self.removed.entry(key.to_owned()).or_default() += 1;
    }
}

fn dict_view(handle: &ObjectHandle) -> Option<ObjectHandle> {
    if let Some(d) = handle.as_stream_dict() {
        Some(d)
    } else if handle.as_dictionary().is_some() {
        Some(handle.clone())
    } else {
        None
    }
}

fn remove_key<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    dict: &ObjectHandle,
    key: &[u8],
    label: &str,
    stats: &mut ScrubStats,
) -> Result<()> {
    if dict.try_get_keys()?.contains(key) {
        dict.remove_key(key);
        pdf.mark_object_handle_dirty(dict)?;
        stats.bump(label);
    }
    Ok(())
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

fn current_object(document: &EditDocument, handle: CowObjectHandle) -> Result<Option<OwnedObject>> {
    match handle {
        CowObjectHandle::Existing(id) => match document.overlay().change(id) {
            Some(ExistingObjectChange::Replace(object)) => Ok(Some(object.clone())),
            Some(ExistingObjectChange::Delete) => Err(crate::Error::DeletedReferencedObject {
                number: id.number(),
                generation: id.generation(),
            }),
            None => match document.source().materialize(id) {
                Ok(object) => Ok(Some(object)),
                Err(crate::Error::MissingSourceObject { .. }) => Ok(None),
                Err(error) => Err(error),
            },
        },
        CowObjectHandle::New(id) => document
            .overlay()
            .added(id)
            .cloned()
            .ok_or(crate::Error::MissingNewObject { index: id.index() })
            .map(Some),
    }
}

fn resolve_owned_value(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<OwnedObject>> {
    let mut value = value.clone();
    let mut seen = BTreeSet::new();
    loop {
        let OwnedObject::Reference(handle) = value else {
            return Ok(Some(value));
        };
        if !seen.insert(handle) {
            return Ok(None);
        }
        let Some(next) = current_object(document, handle)? else {
            return Ok(None);
        };
        value = next;
    }
}

fn owned_action_is_dangerous(document: &EditDocument, action: &OwnedObject) -> Result<bool> {
    let Some(action) = resolve_owned_value(document, action)? else {
        return Ok(false);
    };
    let OwnedObject::Dictionary(dictionary) = action else {
        return Ok(false);
    };
    let Some(kind) = dictionary.get(b"S".as_slice()) else {
        return Ok(false);
    };
    let Some(kind) = resolve_owned_value(document, kind)? else {
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
    let Some(object) = current_object(document, handle)? else {
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

fn hayro_object_has_active_action_candidate(object: &HayroObject<'_>, cfg: &PrivacyConfig) -> bool {
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

fn hayro_object_needs_cos_privacy_scrub(object: &HayroObject<'_>, cfg: &PrivacyConfig) -> bool {
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

fn validate_hayro_cos_privacy_config(cfg: &PrivacyConfig) -> Result<()> {
    if cfg.strip_jpeg_metadata
        || cfg.aggressive_jpeg_app_scrub
        || cfg.remove_attachments
        || cfg.remove_signatures
    {
        return Err(crate::Error::Invalid(
            "Hayro COS privacy migration does not yet support JPEG, attachment, or signature scrubbing"
                .to_owned(),
        ));
    }
    Ok(())
}

/// Remove the migrated dictionary-only privacy state from the Hayro/COW graph
/// without materializing unaffected source objects.
///
/// Supported today: Metadata-level `/Info`, `/ID`, `/Metadata`, `/PieceInfo`,
/// and `/LastModified`; BestEffort additionally removes `/Thumb` and, when
/// requested, `/V`, `/DV`, and `/RV` from field dictionaries carrying `/FT`.
/// Active-content mode removes `/AA`, dangerous `/A` and `/OpenAction` actions,
/// and the Catalog JavaScript name tree. Specialized JPEG, attachment, and
/// signature operations stay on the existing flpdf path for now.
pub(crate) fn scrub_edit_document_cos_privacy(
    document: &mut EditDocument,
    cfg: &PrivacyConfig,
) -> Result<ScrubStats> {
    validate_hayro_cos_privacy_config(cfg)?;
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
                    let needs_edit = hayro_object_needs_cos_privacy_scrub(&object, cfg);
                    let inspect_actions = hayro_object_has_active_action_candidate(&object, cfg);
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
                    .ok_or(crate::Error::MissingNewObject { index: id.index() })?;
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
                if let Some(object) = document.overlay_mut().added_mut(id)
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

    Ok(stats)
}

fn scrub_catalog_javascript_name_tree(
    document: &mut EditDocument,
    stats: &mut ScrubStats,
) -> Result<()> {
    let catalog = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog_object) = current_object(document, catalog)? else {
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
            let Some(names_object) = current_object(document, handle)? else {
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
                    if let Some(object) = document.overlay_mut().added_mut(id)
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

pub(crate) fn scrub_edit_document_metadata(document: &mut EditDocument) -> Result<ScrubStats> {
    scrub_edit_document_cos_privacy(
        document,
        &PrivacyConfig {
            level: PrivacyLevel::Metadata,
            ..PrivacyConfig::default()
        },
    )
}

fn dangerous_action(action: &ObjectHandle) -> Result<bool> {
    if !action.try_is_dictionary()? {
        return Ok(false);
    }
    let s = action.try_get_key(b"/S")?;
    for name in [
        b"JavaScript".as_slice(),
        b"Launch".as_slice(),
        b"SubmitForm".as_slice(),
        b"ImportData".as_slice(),
        b"Rendition".as_slice(),
        b"RichMediaExecute".as_slice(),
    ] {
        if s.try_is_name_and_equals(name)? {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn scrub_pdf<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    cfg: &PrivacyConfig,
) -> Result<ScrubStats> {
    let mut stats = ScrubStats::default();
    if cfg.level == PrivacyLevel::None {
        return Ok(stats);
    }

    // Trailer is live state. A fresh writer later emits no /Prev chain and no
    // unreachable historical revisions.
    let trailer = pdf.trailer();
    remove_key(pdf, &trailer, b"/Info", "info-dictionary", &mut stats)?;
    remove_key(pdf, &trailer, b"/ID", "document-id", &mut stats)?;

    let objects = pdf.get_all_objects()?;
    for object in &objects {
        let Some(dict) = dict_view(object) else {
            continue;
        };
        for (key, label) in [
            (b"/Metadata".as_slice(), "xmp-reference"),
            (b"/PieceInfo".as_slice(), "piece-info"),
            (b"/LastModified".as_slice(), "last-modified"),
        ] {
            remove_key(pdf, &dict, key, label, &mut stats)?;
        }

        if cfg.level == PrivacyLevel::BestEffort {
            remove_key(pdf, &dict, b"/Thumb", "thumbnail", &mut stats)?;
            // /AA is, by definition, automatic additional actions.
            if cfg.remove_active_content {
                remove_key(pdf, &dict, b"/AA", "additional-actions", &mut stats)?;
                if dict.try_get_keys()?.contains(b"/A".as_slice()) {
                    let action = dict.try_get_key(b"/A")?;
                    if dangerous_action(&action)? {
                        remove_key(pdf, &dict, b"/A", "dangerous-action", &mut stats)?;
                    }
                }
                if dict.try_get_keys()?.contains(b"/OpenAction".as_slice()) {
                    let action = dict.try_get_key(b"/OpenAction")?;
                    if dangerous_action(&action)? {
                        remove_key(
                            pdf,
                            &dict,
                            b"/OpenAction",
                            "dangerous-open-action",
                            &mut stats,
                        )?;
                    }
                }
            }
            if cfg.remove_form_values && dict.try_get_keys()?.contains(b"/FT".as_slice()) {
                for key in [b"/V".as_slice(), b"/DV".as_slice(), b"/RV".as_slice()] {
                    remove_key(pdf, &dict, key, "form-value", &mut stats)?;
                }
            }
        }
    }

    if cfg.level == PrivacyLevel::BestEffort && cfg.remove_active_content {
        let root = trailer.try_get_key(b"/Root")?;
        if root.try_is_dictionary()? {
            remove_key(pdf, &root, b"/AA", "catalog-additional-actions", &mut stats)?;
            if root.try_get_keys()?.contains(b"/OpenAction".as_slice()) {
                let action = root.try_get_key(b"/OpenAction")?;
                if dangerous_action(&action)? {
                    remove_key(
                        pdf,
                        &root,
                        b"/OpenAction",
                        "catalog-open-action",
                        &mut stats,
                    )?;
                }
            }
            let names = root.try_get_key(b"/Names")?;
            if names.try_is_dictionary()? {
                remove_key(
                    pdf,
                    &names,
                    b"/JavaScript",
                    "javascript-name-tree",
                    &mut stats,
                )?;
                if cfg.remove_attachments {
                    remove_key(
                        pdf,
                        &names,
                        b"/EmbeddedFiles",
                        "embedded-file-name-tree",
                        &mut stats,
                    )?;
                }
            }
            if cfg.remove_attachments {
                remove_key(pdf, &root, b"/AF", "associated-files", &mut stats)?;
            }
        }
    }

    if cfg.level == PrivacyLevel::BestEffort && cfg.remove_attachments {
        // Use flpdf's name-tree aware helper as well; this catches balanced
        // /EmbeddedFiles trees rather than relying only on catalog surgery.
        let attachments = flpdf::list_embedded_files(pdf).unwrap_or_default();
        for (key, _) in attachments {
            if flpdf::remove_attachment(pdf, &key).unwrap_or(false) {
                stats.bump("embedded-file");
            }
        }
    }

    if cfg.level == PrivacyLevel::BestEffort
        && cfg.remove_signatures
        && flpdf::strip_signature_values(pdf).unwrap_or(false)
    {
        stats.bump("signature-values");
    }

    if cfg.strip_jpeg_metadata {
        let objects = pdf.get_all_objects()?;
        for object in objects {
            let Some(dict) = object.as_stream_dict() else {
                continue;
            };
            let filter = dict.try_get_key(b"/Filter")?.unparse_resolved();
            if filter != b"/DCTDecode" {
                continue;
            }
            let raw = object.get_raw_stream_data()?;
            if let Some((clean, removed)) =
                strip_jpeg_metadata(raw.as_ref(), cfg.aggressive_jpeg_app_scrub)
            {
                object.replace_stream_data(Rc::new(clean), None, None);
                pdf.mark_object_handle_dirty(&object)?;
                stats.bump("jpeg-metadata-stream");
                stats.jpeg_metadata_bytes_removed += removed;
            }
        }
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectHandle as CowObjectHandle, OwnedObject};
    use std::io::Cursor;

    #[test]
    fn hayro_metadata_scrub_matches_flpdf_and_stays_sparse() -> Result<()> {
        let input = metadata_fixture();
        let config = PrivacyConfig {
            level: PrivacyLevel::Metadata,
            ..PrivacyConfig::default()
        };

        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = scrub_pdf(&mut flpdf, &config)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = scrub_edit_document_metadata(&mut document)?;
        assert_eq!(actual.removed, expected.removed);
        assert_eq!(actual.jpeg_metadata_bytes_removed, 0);
        assert_eq!(document.overlay().changes().count(), 3);
        assert!(!document.trailer().contains_key(b"Info".as_slice()));
        assert!(!document.trailer().contains_key(b"ID".as_slice()));

        let output = document.write_compact_experimental()?;
        let rewritten = EditDocument::from_bytes(output)?;
        assert_eq!(rewritten.source().object_count(), 5);
        assert!(!rewritten.trailer().contains_key(b"Info".as_slice()));
        assert!(!rewritten.trailer().contains_key(b"ID".as_slice()));

        for handle in rewritten.reachable_objects()? {
            let CowObjectHandle::Existing(id) = handle else {
                continue;
            };
            let object = rewritten.source().materialize(id)?;
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };
            for (key, _) in METADATA_PRIVACY_KEYS {
                assert!(
                    !dictionary.contains_key(key),
                    "rewritten object retained metadata key {}",
                    String::from_utf8_lossy(key)
                );
            }
        }

        let custom = match rewritten.trailer().get(b"Custom".as_slice()) {
            Some(custom) => custom,
            None => panic!("custom trailer root should survive"),
        };
        let custom_id = match custom {
            OwnedObject::Reference(CowObjectHandle::Existing(id)) => *id,
            other => panic!("expected custom trailer reference, got {other:?}"),
        };
        let custom = rewritten.source().materialize(custom_id)?;
        let custom = match custom.as_dictionary() {
            Some(dictionary) => dictionary,
            None => panic!("custom trailer object should remain a dictionary"),
        };
        assert_eq!(
            custom.get(b"Keep".as_slice()),
            Some(&OwnedObject::Boolean(true))
        );
        Ok(())
    }

    #[test]
    fn hayro_best_effort_thumbnail_and_form_values_match_flpdf() -> Result<()> {
        let input = best_effort_fixture();
        let config = PrivacyConfig {
            level: PrivacyLevel::BestEffort,
            remove_form_values: true,
            ..PrivacyConfig::default()
        };

        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = scrub_pdf(&mut flpdf, &config)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = scrub_edit_document_cos_privacy(&mut document, &config)?;
        assert_eq!(actual.removed, expected.removed);
        assert_eq!(actual.jpeg_metadata_bytes_removed, 0);
        assert_eq!(document.overlay().changes().count(), 3);

        let output = document.write_compact_experimental()?;
        let rewritten = EditDocument::from_bytes(output)?;
        assert_eq!(rewritten.source().object_count(), 6);

        let mut saw_field = false;
        let mut saw_non_field = false;
        for handle in rewritten.reachable_objects()? {
            let CowObjectHandle::Existing(id) = handle else {
                continue;
            };
            let object = rewritten.source().materialize(id)?;
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };
            assert!(!dictionary.contains_key(b"Thumb".as_slice()));
            match dictionary.get(b"Marker".as_slice()) {
                Some(OwnedObject::Name(name)) if name == b"Field" => {
                    saw_field = true;
                    assert!(dictionary.contains_key(b"FT".as_slice()));
                    for key in FORM_VALUE_KEYS {
                        assert!(!dictionary.contains_key(key));
                    }
                }
                Some(OwnedObject::Name(name)) if name == b"NonField" => {
                    saw_non_field = true;
                    assert!(!dictionary.contains_key(b"FT".as_slice()));
                    for key in FORM_VALUE_KEYS {
                        assert!(dictionary.contains_key(key));
                    }
                }
                _ => {}
            }
        }
        assert!(saw_field);
        assert!(saw_non_field);
        Ok(())
    }

    #[test]
    fn hayro_active_content_scrub_matches_flpdf_and_preserves_safe_actions() -> Result<()> {
        let input = active_content_fixture();
        let config = PrivacyConfig {
            level: PrivacyLevel::BestEffort,
            remove_active_content: true,
            ..PrivacyConfig::default()
        };

        let mut flpdf = Pdf::open(Cursor::new(input.clone()))?;
        let expected = scrub_pdf(&mut flpdf, &config)?;

        let mut document = EditDocument::from_bytes(input)?;
        let actual = scrub_edit_document_cos_privacy(&mut document, &config)?;
        assert_eq!(actual.removed, expected.removed);

        let output = document.write_compact_experimental()?;
        let rewritten = EditDocument::from_bytes(output)?;
        let catalog_id = rewritten.source().catalog_id();
        let catalog = rewritten.source().materialize(catalog_id)?;
        let catalog = match catalog.as_dictionary() {
            Some(dictionary) => dictionary,
            None => panic!("catalog should remain a dictionary"),
        };
        assert!(!catalog.contains_key(b"AA".as_slice()));
        assert!(!catalog.contains_key(b"OpenAction".as_slice()));

        let names = match catalog.get(b"Names".as_slice()) {
            Some(OwnedObject::Dictionary(dictionary)) => dictionary,
            other => panic!("expected direct Names dictionary, got {other:?}"),
        };
        assert!(!names.contains_key(b"JavaScript".as_slice()));
        assert!(names.contains_key(b"Dests".as_slice()));

        let mut saw_safe_action = false;
        let mut saw_page = false;
        for handle in rewritten.reachable_objects()? {
            let CowObjectHandle::Existing(id) = handle else {
                continue;
            };
            let object = rewritten.source().materialize(id)?;
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };
            assert!(!dictionary.contains_key(b"AA".as_slice()));
            if dictionary.get(b"Marker".as_slice())
                == Some(&OwnedObject::Name(b"SafeHolder".to_vec()))
            {
                saw_safe_action = true;
                assert!(dictionary.contains_key(b"A".as_slice()));
            }
            if dictionary.get(b"Marker".as_slice()) == Some(&OwnedObject::Name(b"Page".to_vec())) {
                saw_page = true;
                assert!(!dictionary.contains_key(b"A".as_slice()));
                assert!(dictionary.contains_key(b"OpenAction".as_slice()));
            }
        }
        assert!(saw_safe_action);
        assert!(saw_page);
        Ok(())
    }

    fn active_content_fixture() -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /AA << /WC 11 0 R >> /OpenAction 11 0 R /Names << /JavaScript 14 0 R /Dests 15 0 R >> >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Marker /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << >> /Contents 4 0 R /AA << /O 11 0 R >> /A 11 0 R /OpenAction 12 0 R /Annots [8 0 R] >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Producer (pdf-redox-test) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Unused true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Unused true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Type /Annot /Subtype /Link /Marker /SafeHolder /Rect [0 0 1 1] /A 12 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"9 0 obj\n<< /Unused true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"10 0 obj\n<< /Unused true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"11 0 obj\n<< /S /JavaScript /JS (app.alert('x')) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"12 0 obj\n<< /S /URI /URI (https://example.test/) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"13 0 obj\n<< /S /Launch /F (calc.exe) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"14 0 obj\n<< /Names [(script) 11 0 R] >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"15 0 obj\n<< /Names [(dest) [3 0 R /Fit]] >>\nendobj\n",
        );

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 16 /Root 1 0 R /Info 5 0 R /ID [(left) (right)] >>\nstartxref\n{xref_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    fn best_effort_fixture() -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Metadata 6 0 R /AcroForm << /Fields [8 0 R 9 0 R] >> >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << >> /Contents 4 0 R /Thumb 7 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Producer (pdf-redox-test) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Metadata /Subtype /XML /Length 4 >>\nstream\n<x/>\nendstream\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /ThumbnailPayload true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /FT /Tx /Marker /Field /V (secret) /DV (default) /RV (rich) /Keep true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"9 0 obj\n<< /Marker /NonField /V (keep-v) /DV (keep-dv) /RV (keep-rv) /Keep true >>\nendobj\n",
        );

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 10 /Root 1 0 R /Info 5 0 R /ID [(left) (right)] >>\nstartxref\n{xref_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    fn metadata_fixture() -> Vec<u8> {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R /Metadata 6 0 R >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << >> /Contents 4 0 R /PieceInfo 7 0 R /LastModified (yesterday) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Producer (pdf-redox-test) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Metadata /Subtype /XML /Length 4 >>\nstream\n<x/>\nendstream\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Private (secret) >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Metadata 6 0 R /Keep true >>\nendobj\n",
        );
        append_object(
            &mut pdf,
            &mut offsets,
            b"9 0 obj\n<< /Keep true >>\nendobj\n",
        );

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size 10 /Root 1 0 R /Info 5 0 R /ID [(left) (right)] /Custom 8 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    fn append_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, object: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(object);
    }
}
