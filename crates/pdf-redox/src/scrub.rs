use crate::jpeg::strip_jpeg_metadata;
use crate::{PrivacyConfig, PrivacyLevel, Result};
use flpdf::{ObjectHandle, Pdf};
use std::collections::BTreeMap;
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
