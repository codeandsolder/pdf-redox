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
        || cfg.remove_active_content
        || cfg.remove_signatures
    {
        return Err(crate::Error::Invalid(
            "Hayro COS privacy migration does not yet support JPEG, attachment, active-content, or signature scrubbing"
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
/// Specialized JPEG, attachment, active-content, and signature operations stay
/// on the existing flpdf path for now.
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

        let (references, needs_edit) = match handle {
            CowObjectHandle::Existing(id) => match document.overlay().change(id) {
                Some(ExistingObjectChange::Replace(object)) => (
                    object.references(),
                    owned_dictionary_needs_cos_privacy_scrub(object, cfg),
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
                    (
                        references
                            .into_iter()
                            .map(CowObjectHandle::Existing)
                            .collect(),
                        needs_edit,
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
                )
            }
        };

        seen.insert(handle);
        for reference in references {
            if !seen.contains(&reference) {
                pending.push(reference);
            }
        }

        if !needs_edit {
            continue;
        }
        match handle {
            CowObjectHandle::Existing(id) => {
                if let Some(dictionary) = document.edit_object(id)?.as_dictionary_mut() {
                    scrub_owned_cos_privacy_dictionary(dictionary, cfg, &mut stats);
                }
            }
            CowObjectHandle::New(id) => {
                if let Some(object) = document.overlay_mut().added_mut(id)
                    && let Some(dictionary) = object.as_dictionary_mut()
                {
                    scrub_owned_cos_privacy_dictionary(dictionary, cfg, &mut stats);
                }
            }
        }
    }

    Ok(stats)
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
