use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedObject, PdfAnalysis, Result, RiskFinding,
    RiskKind, hidden_text::analyze_hidden_text_hayro, prune::should_prune_resources_hayro,
};
use flate2::{Compression, write::ZlibEncoder};
use flpdf::{DecodeLevel, ObjectHandle, ObjectHandleParserCallbacks, ParseControl};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::io::Write;

pub(crate) fn input_sha256(input: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(input);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn push_risk(
    map: &mut BTreeMap<RiskKind, (usize, String)>,
    kind: RiskKind,
    count: usize,
    note: &str,
) {
    if count == 0 {
        return;
    }
    let entry = map.entry(kind).or_insert((0, note.to_owned()));
    entry.0 += count;
}

fn document_info_text(document: &EditDocument, key: &[u8]) -> Result<Option<String>> {
    let Some(info) = document.trailer().get(b"Info".as_slice()) else {
        return Ok(None);
    };
    let Some(info) = document.resolve_owned_value(info)? else {
        return Ok(None);
    };
    let Some(dictionary) = info.as_dictionary() else {
        return Ok(None);
    };
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    let handle = document.detached_flpdf_object(&value)?;
    let bytes = handle.try_get_utf8_value()?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    Ok((!text.is_empty()).then_some(text))
}

fn record_payload(map: &mut HashMap<[u8; 32], (usize, usize)>, bytes: &[u8]) -> [u8; 32] {
    let hash: [u8; 32] = Sha256::digest(bytes).into();
    let entry = map.entry(hash).or_insert((0, bytes.len()));
    entry.0 += 1;
    hash
}

fn duplicate_payload_stats(map: HashMap<[u8; 32], (usize, usize)>) -> (usize, usize) {
    map.into_values()
        .filter(|(count, _)| *count > 1)
        .fold((0, 0), |(groups, wasted), (count, bytes)| {
            (groups + 1, wasted + (count - 1) * bytes)
        })
}

fn all_source_objects(document: &EditDocument) -> Result<Vec<(CowObjectHandle, OwnedObject)>> {
    let mut objects = Vec::new();
    for id in document.source().object_ids() {
        let handle = CowObjectHandle::Existing(id);
        if let Some(object) = document.current_owned_object(handle)? {
            objects.push((handle, object));
        }
    }
    Ok(objects)
}

fn collect_content_stream_refs(
    document: &EditDocument,
    value: &OwnedObject,
    refs: &mut HashSet<CowObjectHandle>,
    seen: &mut HashSet<CowObjectHandle>,
) -> Result<()> {
    match value {
        OwnedObject::Reference(handle) => {
            if !seen.insert(*handle) {
                return Ok(());
            }
            let Some(target) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
            match target {
                OwnedObject::Stream { .. } => {
                    refs.insert(*handle);
                }
                OwnedObject::Array(values) => {
                    for value in &values {
                        collect_content_stream_refs(document, value, refs, seen)?;
                    }
                }
                _ => {}
            }
        }
        OwnedObject::Array(values) => {
            for value in values {
                collect_content_stream_refs(document, value, refs, seen)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_page_content_refs(document: &EditDocument) -> Result<HashSet<CowObjectHandle>> {
    let mut refs = HashSet::new();
    for page in document.page_handles()? {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        collect_content_stream_refs(document, contents, &mut refs, &mut HashSet::new())?;
    }
    Ok(refs)
}

fn incoming_role_for_key(key: &[u8]) -> Option<&'static str> {
    match key {
        b"Metadata" => Some("metadata"),
        b"ToUnicode" => Some("to-unicode"),
        b"CIDToGIDMap" => Some("cid-to-gid"),
        b"ColorSpace" | b"DestOutputProfile" => Some("color-space-support"),
        b"XFA" => Some("xfa"),
        b"Function" => Some("function"),
        b"Shading" => Some("shading"),
        b"Pattern" => Some("pattern"),
        _ => None,
    }
}

fn collect_incoming_roles(
    objects: &[(CowObjectHandle, OwnedObject)],
) -> HashMap<CowObjectHandle, HashSet<&'static str>> {
    let objects_by_handle: HashMap<_, _> = objects.iter().cloned().collect();
    let mut roles: HashMap<CowObjectHandle, HashSet<&'static str>> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();

    for (_, object) in objects {
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        for (key, value) in dictionary {
            if let Some(role) = incoming_role_for_key(key) {
                queue.push_back((value.clone(), role));
            }
        }
    }

    while let Some((value, role)) = queue.pop_front() {
        match value {
            OwnedObject::Reference(handle) => {
                if !seen.insert((handle, role)) {
                    continue;
                }
                let Some(target) = objects_by_handle.get(&handle) else {
                    continue;
                };
                match target {
                    OwnedObject::Stream { .. } => {
                        roles.entry(handle).or_default().insert(role);
                    }
                    OwnedObject::Array(values) => {
                        for value in values {
                            queue.push_back((value.clone(), role));
                        }
                    }
                    OwnedObject::Dictionary(dictionary) => {
                        for (key, child) in dictionary {
                            queue.push_back((
                                child.clone(),
                                incoming_role_for_key(key).unwrap_or(role),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            OwnedObject::Array(values) => {
                for value in values {
                    queue.push_back((value, role));
                }
            }
            OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
                for (key, child) in dictionary {
                    queue.push_back((child, incoming_role_for_key(&key).unwrap_or(role)));
                }
            }
            _ => {}
        }
    }

    roles
}

fn is_name(document: &EditDocument, value: &OwnedObject, expected: &[u8]) -> Result<bool> {
    Ok(matches!(
        document.resolve_owned_value(value)?,
        Some(OwnedObject::Name(name)) if name == expected
    ))
}

fn collect_direct_icc_profile_refs(
    document: &EditDocument,
    value: &OwnedObject,
    refs: &mut HashSet<CowObjectHandle>,
) -> Result<()> {
    match value {
        OwnedObject::Array(values) => {
            if values.len() >= 2 && is_name(document, &values[0], b"ICCBased")? {
                if let OwnedObject::Reference(handle) = values[1] {
                    refs.insert(handle);
                }
                return Ok(());
            }
            for value in values {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_direct_icc_profile_refs(document, value, refs)?;
                }
            }
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            for value in dictionary.values() {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_direct_icc_profile_refs(document, value, refs)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn collect_icc_profile_refs(
    document: &EditDocument,
    objects: &[(CowObjectHandle, OwnedObject)],
) -> Result<HashSet<CowObjectHandle>> {
    let mut refs = HashSet::new();
    for (_, object) in objects {
        collect_direct_icc_profile_refs(document, object, &mut refs)?;
    }
    Ok(refs)
}

fn dictionary_name(
    document: &EditDocument,
    dictionary: &BTreeMap<Vec<u8>, OwnedObject>,
    key: &[u8],
) -> Result<Option<Vec<u8>>> {
    let Some(value) = dictionary.get(key) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn stream_role(
    document: &EditDocument,
    object: &OwnedObject,
    handle: CowObjectHandle,
    font_refs: &HashSet<CowObjectHandle>,
    page_content_refs: &HashSet<CowObjectHandle>,
    icc_profile_refs: &HashSet<CowObjectHandle>,
    incoming_roles: &HashMap<CowObjectHandle, HashSet<&'static str>>,
) -> Result<&'static str> {
    let OwnedObject::Stream { dictionary, .. } = object else {
        return Ok("other");
    };
    if let Some(kind) = dictionary_name(document, dictionary, b"Type")? {
        match kind.as_slice() {
            b"Metadata" => return Ok("metadata"),
            b"EmbeddedFile" => return Ok("embedded-file"),
            b"ObjStm" => return Ok("object-stream"),
            b"XRef" => return Ok("xref-stream"),
            _ => {}
        }
    }
    if let Some(subtype) = dictionary_name(document, dictionary, b"Subtype")? {
        match subtype.as_slice() {
            b"Image" => return Ok("image"),
            b"Form" => return Ok("form"),
            _ => {}
        }
    }
    if font_refs.contains(&handle) {
        return Ok("font-program");
    }
    if page_content_refs.contains(&handle) {
        return Ok("page-content");
    }
    if icc_profile_refs.contains(&handle) {
        return Ok("icc-profile");
    }
    if let Some(roles) = incoming_roles.get(&handle) {
        if roles.len() == 1 {
            return Ok(roles.iter().next().copied().unwrap_or("other"));
        }
        if roles.len() > 1 {
            return Ok("multi-reference");
        }
    }
    if dictionary_name(document, dictionary, b"Type")?.as_deref() == Some(b"CMap") {
        return Ok("cmap");
    }
    Ok("other")
}

fn duplicate_payload_role_stats(
    payloads: &HashMap<[u8; 32], (usize, usize)>,
    members: &HashMap<[u8; 32], Vec<CowObjectHandle>>,
    roles: &HashMap<CowObjectHandle, &'static str>,
) -> (BTreeMap<String, usize>, BTreeMap<String, usize>) {
    let mut groups = BTreeMap::new();
    let mut wasted = BTreeMap::new();
    for (hash, (count, bytes)) in payloads {
        if *count <= 1 {
            continue;
        }
        let role_set: HashSet<&'static str> = members
            .get(hash)
            .into_iter()
            .flatten()
            .filter_map(|handle| roles.get(handle).copied())
            .collect();
        let role = if role_set.len() == 1 {
            role_set.into_iter().next().unwrap_or("other")
        } else if role_set.is_empty() {
            "other"
        } else {
            "mixed"
        };
        *groups.entry(role.to_owned()).or_default() += 1;
        *wasted.entry(role.to_owned()).or_default() += (count - 1) * bytes;
    }
    (groups, wasted)
}

#[derive(Debug, Default)]
struct InlineImageCounter {
    count: usize,
    bytes: usize,
    payloads: HashMap<[u8; 32], (usize, usize)>,
}

impl ObjectHandleParserCallbacks for InlineImageCounter {
    fn handle_object(
        &mut self,
        object: ObjectHandle,
        _offset: usize,
        _length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(bytes) = object.as_inline_image() {
            self.count += 1;
            self.bytes += bytes.len();
            record_payload(&mut self.payloads, &bytes);
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn decoded_content_value(
    document: &EditDocument,
    value: &OwnedObject,
    output: &mut Vec<u8>,
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
            let bytes = document.decoded_owned_stream_data(&value, DecodeLevel::Specialized)?;
            if !output.is_empty() && output.last() != Some(&b'\n') {
                output.push(b'\n');
            }
            output.extend_from_slice(&bytes);
        }
        OwnedObject::Array(values) => {
            for value in values {
                decoded_content_value(document, &value, output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn page_content(document: &EditDocument, page: CowObjectHandle) -> Result<Vec<u8>> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(Vec::new());
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    decoded_content_value(document, contents, &mut output)?;
    Ok(output)
}

fn analyze_inline_images(document: &EditDocument) -> Result<(usize, usize, usize, usize)> {
    let mut counter = InlineImageCounter::default();
    for page in document.page_handles()? {
        let content = page_content(document, page)?;
        flpdf::parse_detached_content_stream(
            &content,
            "Hayro analysis page content",
            &mut counter,
        )?;
    }

    let mut seen_forms = BTreeSet::new();
    for handle in document.reachable_output_objects()? {
        if !seen_forms.insert(handle) {
            continue;
        }
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = &object else {
            continue;
        };
        if dictionary_name(document, dictionary, b"Subtype")?.as_deref() != Some(b"Form") {
            continue;
        }
        let content = document.decoded_owned_stream_data(&object, DecodeLevel::Specialized)?;
        flpdf::parse_detached_content_stream(
            &content,
            "Hayro analysis Form content",
            &mut counter,
        )?;
    }

    let (duplicate_groups, duplicate_wasted_bytes) = duplicate_payload_stats(counter.payloads);
    Ok((
        counter.count,
        counter.bytes,
        duplicate_groups,
        duplicate_wasted_bytes,
    ))
}

fn raw_stream_bytes<'a>(
    document: &'a EditDocument,
    object: &'a OwnedObject,
) -> Result<std::borrow::Cow<'a, [u8]>> {
    let OwnedObject::Stream { data, .. } = object else {
        return Err(crate::Error::Invalid("object is not a stream".to_owned()));
    };
    data.bytes(document.source())
}

fn filter_name(
    document: &EditDocument,
    dictionary: &BTreeMap<Vec<u8>, OwnedObject>,
) -> Result<String> {
    let Some(value) = dictionary.get(b"Filter".as_slice()) else {
        return Ok("null".to_owned());
    };
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok("null".to_owned());
    };
    let detached = document.detached_flpdf_object(&value)?;
    Ok(String::from_utf8_lossy(&detached.unparse_resolved()).into_owned())
}

pub fn analyze_pdf(input: &[u8]) -> Result<PdfAnalysis> {
    let document = EditDocument::from_bytes(input.to_vec())?;
    let objects = all_source_objects(&document)?;
    let mut out = PdfAnalysis {
        input_bytes: input.len(),
        input_sha256: input_sha256(input),
        page_count: document.source().page_count(),
        object_count: document.source().object_count(),
        ..PdfAnalysis::default()
    };

    match document_info_text(&document, b"Producer") {
        Ok(value) => out.producer = value,
        Err(error) => out
            .warnings
            .push(format!("Producer metadata analysis skipped: {error}")),
    }
    match document_info_text(&document, b"Creator") {
        Ok(value) => out.creator = value,
        Err(error) => out
            .warnings
            .push(format!("Creator metadata analysis skipped: {error}")),
    }

    let page_content_refs = match collect_page_content_refs(&document) {
        Ok(refs) => refs,
        Err(error) => {
            out.warnings
                .push(format!("page-content role analysis skipped: {error}"));
            HashSet::new()
        }
    };
    let mut risks: BTreeMap<RiskKind, (usize, String)> = BTreeMap::new();
    let mut stream_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut stream_payload_members: HashMap<[u8; 32], Vec<CowObjectHandle>> = HashMap::new();
    let mut metadata_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut image_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut form_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_refs = HashSet::new();

    for (handle, object) in &objects {
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let is_stream = matches!(object, OwnedObject::Stream { .. });
        if is_stream {
            out.stream_count += 1;
            let raw = raw_stream_bytes(&document, object)?;
            out.stream_raw_bytes += raw.len();
            let hash = record_payload(&mut stream_payloads, raw.as_ref());
            stream_payload_members
                .entry(hash)
                .or_default()
                .push(*handle);
        }

        if dictionary.contains_key(b"PieceInfo".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::PieceInfo,
                1,
                "Adobe/private-piece metadata can contain authoring provenance and source paths",
            );
        }
        if dictionary.contains_key(b"Thumb".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::Thumbnail,
                1,
                "page thumbnail stream is separate hidden raster content",
            );
        }
        if dictionary.contains_key(b"AA".as_slice())
            || dictionary.contains_key(b"OpenAction".as_slice())
        {
            push_risk(
                &mut risks,
                RiskKind::AutomaticAction,
                1,
                "automatic PDF actions are executable/interactive hidden state",
            );
        }
        if dictionary.contains_key(b"OCProperties".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::HiddenLayer,
                1,
                "optional-content groups can hide content from the default view",
            );
        }
        if dictionary.contains_key(b"ActualText".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::SuspiciousHiddenText,
                1,
                "ActualText may differ from visible glyphs and should be retained but audited in privacy mode",
            );
        }
        if dictionary.contains_key(b"V".as_slice()) && dictionary.contains_key(b"FT".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::FormValue,
                1,
                "interactive form field contains a stored value",
            );
        }

        for key in [
            b"FontFile".as_slice(),
            b"FontFile2".as_slice(),
            b"FontFile3".as_slice(),
        ] {
            let Some(value) = dictionary.get(key) else {
                continue;
            };
            let reference = match value {
                OwnedObject::Reference(handle) => Some(*handle),
                _ => None,
            };
            if let Some(reference) = reference
                && !font_refs.insert(reference)
            {
                continue;
            }
            let Some(font) = document.resolve_owned_value(value)? else {
                continue;
            };
            if !matches!(font, OwnedObject::Stream { .. }) {
                continue;
            }
            let raw = raw_stream_bytes(&document, &font)?;
            out.font_program_count += 1;
            out.font_program_bytes += raw.len();
            let duplicate_basis = document
                .decoded_owned_stream_data(&font, DecodeLevel::Generalized)
                .unwrap_or_else(|_| raw.as_ref().to_vec());
            record_payload(&mut font_payloads, &duplicate_basis);
        }

        if !is_stream {
            continue;
        }
        let filter = filter_name(&document, dictionary)?;
        *out.filter_counts.entry(filter.clone()).or_default() += 1;

        let subtype = dictionary_name(&document, dictionary, b"Subtype")?;
        let is_image = subtype.as_deref() == Some(b"Image");
        let is_form = subtype.as_deref() == Some(b"Form");
        if is_image {
            out.image_count += 1;
            let raw = raw_stream_bytes(&document, object)?;
            out.image_raw_bytes += raw.len();
            record_payload(&mut image_payloads, raw.as_ref());
            if filter.contains("DCTDecode") {
                let marker_hits = count_bytes(raw.as_ref(), &[0xff, 0xe1])
                    + count_bytes(raw.as_ref(), &[0xff, 0xed])
                    + count_bytes(raw.as_ref(), &[0xff, 0xfe]);
                push_risk(
                    &mut risks,
                    RiskKind::JpegMetadata,
                    marker_hits,
                    "embedded JPEG carries EXIF/XMP/IPTC/comment marker candidates",
                );
            }
        }
        if is_form {
            out.form_xobject_count += 1;
            let raw = raw_stream_bytes(&document, object)?;
            let duplicate_basis = document
                .decoded_owned_stream_data(object, DecodeLevel::Generalized)
                .unwrap_or_else(|_| raw.as_ref().to_vec());
            record_payload(&mut form_payloads, &duplicate_basis);
        }
        if dictionary_name(&document, dictionary, b"Type")?.as_deref() == Some(b"Metadata") {
            let raw = raw_stream_bytes(&document, object)?;
            out.metadata_stream_count += 1;
            out.metadata_stream_bytes += raw.len();
            record_payload(&mut metadata_payloads, raw.as_ref());
            push_risk(
                &mut risks,
                RiskKind::XmpMetadata,
                1,
                "XMP metadata stream may contain edit history, usernames, paths, thumbnails, and tool provenance",
            );
        }

        if filter == "/FlateDecode" {
            out.flate_stream_count += 1;
            if !is_image {
                out.non_image_flate_stream_count += 1;
            }
            let raw = raw_stream_bytes(&document, object)?;
            if let Ok(decoded) =
                document.decoded_owned_stream_data(object, DecodeLevel::Generalized)
            {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
                if encoder.write_all(&decoded).is_ok()
                    && let Ok(repacked) = encoder.finish()
                {
                    let saving = raw.len().saturating_sub(repacked.len());
                    if saving >= 1024 && saving * 100 >= raw.len().saturating_mul(5) {
                        out.flate_recompress_candidate_count += 1;
                        out.flate_recompress_potential_saving_bytes += saving;
                        if !is_image {
                            out.non_image_flate_recompress_candidate_count += 1;
                            out.non_image_flate_recompress_potential_saving_bytes += saving;
                        }
                    }
                }
            }
        }
    }

    let incoming_roles = collect_incoming_roles(&objects);
    let icc_profile_refs = collect_icc_profile_refs(&document, &objects)?;
    let mut stream_roles = HashMap::new();
    for (handle, object) in &objects {
        if !matches!(object, OwnedObject::Stream { .. }) {
            continue;
        }
        let role = stream_role(
            &document,
            object,
            *handle,
            &font_refs,
            &page_content_refs,
            &icc_profile_refs,
            &incoming_roles,
        )?;
        *out.stream_role_counts.entry(role.to_owned()).or_default() += 1;
        *out.stream_role_raw_bytes
            .entry(role.to_owned())
            .or_default() += raw_stream_bytes(&document, object)?.len();
        stream_roles.insert(*handle, role);
    }
    (
        out.duplicate_stream_role_groups,
        out.duplicate_stream_role_wasted_bytes,
    ) = duplicate_payload_role_stats(&stream_payloads, &stream_payload_members, &stream_roles);

    (
        out.duplicate_metadata_payload_groups,
        out.duplicate_metadata_payload_wasted_bytes,
    ) = duplicate_payload_stats(metadata_payloads);
    (
        out.duplicate_stream_payload_groups,
        out.duplicate_stream_payload_wasted_bytes,
    ) = duplicate_payload_stats(stream_payloads);
    (
        out.duplicate_image_payload_groups,
        out.duplicate_image_payload_wasted_bytes,
    ) = duplicate_payload_stats(image_payloads);
    (
        out.duplicate_form_payload_groups,
        out.duplicate_form_payload_wasted_bytes,
    ) = duplicate_payload_stats(form_payloads);
    (
        out.duplicate_font_payload_groups,
        out.duplicate_font_payload_wasted_bytes,
    ) = duplicate_payload_stats(font_payloads);

    match analyze_inline_images(&document) {
        Ok((count, bytes, duplicate_groups, duplicate_wasted_bytes)) => {
            out.inline_image_count = count;
            out.inline_image_bytes = bytes;
            out.duplicate_inline_image_payload_groups = duplicate_groups;
            out.duplicate_inline_image_payload_wasted_bytes = duplicate_wasted_bytes;
        }
        Err(error) => out
            .warnings
            .push(format!("inline-image analysis skipped: {error}")),
    }
    match should_prune_resources_hayro(&document) {
        Ok(candidate) => out.resource_pruning_auto_triggered = candidate,
        Err(error) => out
            .warnings
            .push(format!("resource-pruning preflight skipped: {error}")),
    }

    let startxrefs = count_bytes(input, b"startxref");
    out.incremental_update_count = startxrefs.saturating_sub(1);
    push_risk(
        &mut risks,
        RiskKind::IncrementalHistory,
        out.incremental_update_count,
        "incremental revisions can retain previous object bodies, old metadata, or pre-redaction content; a fresh rewrite drops the /Prev chain",
    );
    push_risk(
        &mut risks,
        RiskKind::InfoDictionary,
        count_bytes(input, b"/Info"),
        "document Info dictionary commonly stores author, creator, producer, and timestamps",
    );
    push_risk(
        &mut risks,
        RiskKind::EmbeddedFile,
        count_bytes(input, b"/EmbeddedFiles") + count_bytes(input, b"/EmbeddedFile"),
        "PDF can carry arbitrary attached files invisibly to normal page rendering",
    );
    push_risk(
        &mut risks,
        RiskKind::Javascript,
        count_bytes(input, b"/JavaScript") + count_bytes(input, b"/JS"),
        "document or annotation JavaScript is active hidden content",
    );
    push_risk(
        &mut risks,
        RiskKind::Signature,
        count_bytes(input, b"/Sig"),
        "digital signatures can contain signer identity, certificates, timestamps, reason, and location",
    );

    match analyze_hidden_text_hayro(&document) {
        Ok(findings) => {
            let suspicious = findings
                .iter()
                .filter(|finding| {
                    !matches!(
                        finding.category,
                        crate::HiddenTextCategory::OcrOverlay
                            | crate::HiddenTextCategory::Accessibility
                    )
                })
                .count();
            push_risk(
                &mut risks,
                RiskKind::SuspiciousHiddenText,
                suspicious,
                "text that is not visible in the default page appearance; inspect the categorized findings before removing it",
            );
            out.hidden_text = findings;
        }
        Err(error) => out
            .warnings
            .push(format!("hidden-text analysis skipped: {error}")),
    }

    out.risks = risks
        .into_iter()
        .map(|(kind, (count, note))| RiskFinding { kind, count, note })
        .collect();
    Ok(out)
}
