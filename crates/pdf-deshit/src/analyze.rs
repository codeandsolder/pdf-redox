use crate::{PdfAnalysis, Result, RiskFinding, RiskKind, hidden_text::analyze_hidden_text};
use flate2::{Compression, write::ZlibEncoder};
use flpdf::{
    DecodeLevel, ObjectHandle, ObjectHandleParserCallbacks, ObjectRef, PageObjectHelper,
    ParseControl, Pdf,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{Cursor, Read, Seek, Write};

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|w| *w == needle)
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
    let e = map.entry(kind).or_insert((0, note.to_owned()));
    e.0 += count;
}

fn document_info_text<R: Read + Seek>(pdf: &mut Pdf<R>, key: &[u8]) -> Result<Option<String>> {
    let info = pdf.trailer().try_get_key(b"/Info")?;
    if info.is_null() {
        return Ok(None);
    }
    let value = info.try_get_key(key)?;
    if value.is_null() {
        return Ok(None);
    }
    let bytes = value.try_get_utf8_value()?;
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

fn collect_page_content_refs<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<HashSet<ObjectRef>> {
    let mut refs = HashSet::new();
    for page_ref in flpdf::pages::page_refs(pdf)? {
        let mut helper = PageObjectHelper::new(page_ref, pdf);
        for stream in helper.get_page_contents()? {
            if let Some(object_ref) = stream.object_ref() {
                refs.insert(object_ref);
            }
        }
    }
    Ok(refs)
}

fn incoming_role_for_key(key: &[u8]) -> Option<&'static str> {
    match key {
        b"/Metadata" => Some("metadata"),
        b"/ToUnicode" => Some("to-unicode"),
        b"/CIDToGIDMap" => Some("cid-to-gid"),
        b"/ColorSpace" | b"/DestOutputProfile" => Some("color-space-support"),
        b"/XFA" => Some("xfa"),
        b"/Function" => Some("function"),
        b"/Shading" => Some("shading"),
        b"/Pattern" => Some("pattern"),
        _ => None,
    }
}

fn collect_incoming_roles(objects: &[ObjectHandle]) -> HashMap<ObjectRef, HashSet<&'static str>> {
    let objects_by_ref: HashMap<ObjectRef, ObjectHandle> = objects
        .iter()
        .filter_map(|object| {
            object
                .object_ref()
                .map(|object_ref| (object_ref, object.clone()))
        })
        .collect();
    let mut roles: HashMap<ObjectRef, HashSet<&'static str>> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();

    for object in objects {
        let dictionary = object.as_stream_dict().or_else(|| {
            let is_dictionary = object.try_is_dictionary().ok()?;
            if is_dictionary {
                Some(object.clone())
            } else {
                object.as_stream_dict()
            }
        });
        let Some(dictionary) = dictionary else {
            continue;
        };
        if let Some(entries) = dictionary.as_dictionary() {
            for (key, value) in entries {
                if let Some(role) = incoming_role_for_key(&key) {
                    queue.push_back((value, role));
                }
            }
        }
    }

    while let Some((value, role)) = queue.pop_front() {
        if let Some(object_ref) = value.object_ref() {
            if !seen.insert((object_ref, role)) {
                continue;
            }
            let Some(target) = objects_by_ref.get(&object_ref) else {
                continue;
            };
            let is_array = target.try_is_array().ok().unwrap_or(false);
            if target.as_stream_dict().is_some() {
                roles.entry(object_ref).or_default().insert(role);
                continue;
            }
            if is_array {
                if let Some(items) = target.as_array() {
                    for item in items {
                        queue.push_back((item, role));
                    }
                }
            } else if target.try_is_dictionary().ok().unwrap_or(false)
                && let Some(entries) = target.as_dictionary()
            {
                for (key, child) in entries {
                    queue.push_back((child, incoming_role_for_key(&key).unwrap_or(role)));
                }
            }
            continue;
        }
        if let Some(items) = value.as_array() {
            for item in items {
                queue.push_back((item, role));
            }
        } else if let Some(entries) = value.as_dictionary() {
            for (key, child) in entries {
                queue.push_back((child, incoming_role_for_key(&key).unwrap_or(role)));
            }
        }
    }

    roles
}

fn collect_direct_icc_profile_refs(
    value: &ObjectHandle,
    include_indirect_root: bool,
    refs: &mut HashSet<ObjectRef>,
) {
    if !include_indirect_root && value.object_ref().is_some() {
        return;
    }
    let Ok(is_array) = value.try_is_array() else {
        return;
    };
    if is_array {
        let Some(items) = value.as_array() else {
            return;
        };
        if items.len() >= 2 && matches!(items[0].try_is_name_and_equals(b"ICCBased"), Ok(true)) {
            if let Some(profile_ref) = items[1].object_ref() {
                refs.insert(profile_ref);
            }
            return;
        }
        for item in items {
            collect_direct_icc_profile_refs(&item, false, refs);
        }
        return;
    }
    if let Some(dict) = value.as_stream_dict() {
        if let Some(entries) = dict.as_dictionary() {
            for (_, child) in entries {
                collect_direct_icc_profile_refs(&child, false, refs);
            }
        }
        return;
    }
    let Ok(is_dictionary) = value.try_is_dictionary() else {
        return;
    };
    if is_dictionary && let Some(entries) = value.as_dictionary() {
        for (_, child) in entries {
            collect_direct_icc_profile_refs(&child, false, refs);
        }
    }
}

fn collect_icc_profile_refs(objects: &[ObjectHandle]) -> HashSet<ObjectRef> {
    let mut refs = HashSet::new();
    for object in objects {
        collect_direct_icc_profile_refs(object, true, &mut refs);
    }
    refs
}

fn stream_role(
    object: &ObjectHandle,
    object_ref: ObjectRef,
    font_refs: &HashSet<ObjectRef>,
    page_content_refs: &HashSet<ObjectRef>,
    icc_profile_refs: &HashSet<ObjectRef>,
    incoming_roles: &HashMap<ObjectRef, HashSet<&'static str>>,
) -> Result<&'static str> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok("other");
    };
    let type_object = dict.try_get_key(b"/Type")?;
    for (name, role) in [
        (b"Metadata".as_slice(), "metadata"),
        (b"EmbeddedFile".as_slice(), "embedded-file"),
        (b"ObjStm".as_slice(), "object-stream"),
        (b"XRef".as_slice(), "xref-stream"),
    ] {
        if type_object.try_is_name_and_equals(name)? {
            return Ok(role);
        }
    }
    let subtype = dict.try_get_key(b"/Subtype")?;
    if subtype.try_is_name_and_equals(b"Image")? {
        return Ok("image");
    }
    if subtype.try_is_name_and_equals(b"Form")? {
        return Ok("form");
    }
    if font_refs.contains(&object_ref) {
        return Ok("font-program");
    }
    if page_content_refs.contains(&object_ref) {
        return Ok("page-content");
    }
    if icc_profile_refs.contains(&object_ref) {
        return Ok("icc-profile");
    }
    if let Some(roles) = incoming_roles.get(&object_ref) {
        if roles.len() == 1 {
            return Ok(roles.iter().next().copied().unwrap_or("other"));
        }
        if roles.len() > 1 {
            return Ok("multi-reference");
        }
    }
    if type_object.try_is_name_and_equals(b"CMap")? {
        return Ok("cmap");
    }
    Ok("other")
}

fn duplicate_payload_role_stats(
    payloads: &HashMap<[u8; 32], (usize, usize)>,
    members: &HashMap<[u8; 32], Vec<ObjectRef>>,
    roles: &HashMap<ObjectRef, &'static str>,
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
            .filter_map(|object_ref| roles.get(object_ref).copied())
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

fn analyze_inline_images<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<(usize, usize, usize, usize)> {
    let page_refs = flpdf::pages::page_refs(pdf)?;
    let mut counter = InlineImageCounter::default();
    let mut seen_forms = HashSet::new();

    for page_ref in page_refs {
        let page = pdf.get_object_handle(page_ref);
        pdf.resolve(&page)?;
        page.parse_page_contents(&mut counter)?;

        let mut forms = Vec::new();
        {
            let mut helper = PageObjectHelper::new(page_ref, pdf);
            helper.for_each_form_xobject(true, |form, _, _| {
                if form
                    .object_ref()
                    .is_none_or(|object_ref| seen_forms.insert(object_ref))
                {
                    forms.push(form);
                }
                Ok(())
            })?;
        }
        for form in forms {
            form.parse_as_contents(&mut counter)?;
        }
    }

    let (duplicate_groups, duplicate_wasted_bytes) = duplicate_payload_stats(counter.payloads);
    Ok((
        counter.count,
        counter.bytes,
        duplicate_groups,
        duplicate_wasted_bytes,
    ))
}

pub fn analyze_pdf(input: &[u8]) -> Result<PdfAnalysis> {
    let mut pdf = Pdf::open(Cursor::new(input.to_vec()))?;
    let page_count = flpdf::pages::page_refs(&mut pdf)?.len();
    let objects = pdf.get_all_objects()?;

    let mut out = PdfAnalysis {
        input_bytes: input.len(),
        page_count,
        object_count: objects.len(),
        ..PdfAnalysis::default()
    };
    match document_info_text(&mut pdf, b"/Producer") {
        Ok(value) => out.producer = value,
        Err(error) => out
            .warnings
            .push(format!("Producer metadata analysis skipped: {error}")),
    }
    match document_info_text(&mut pdf, b"/Creator") {
        Ok(value) => out.creator = value,
        Err(error) => out
            .warnings
            .push(format!("Creator metadata analysis skipped: {error}")),
    }
    let page_content_refs = match collect_page_content_refs(&mut pdf) {
        Ok(refs) => refs,
        Err(error) => {
            out.warnings
                .push(format!("page-content role analysis skipped: {error}"));
            HashSet::new()
        }
    };
    let mut risks: BTreeMap<RiskKind, (usize, String)> = BTreeMap::new();
    let mut stream_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut stream_payload_members: HashMap<[u8; 32], Vec<ObjectRef>> = HashMap::new();
    let mut metadata_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut image_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut form_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_refs = HashSet::new();

    for object in &objects {
        let dict = if let Some(d) = object.as_stream_dict() {
            out.stream_count += 1;
            let raw = object.get_raw_stream_data()?;
            out.stream_raw_bytes += raw.len();
            let hash = record_payload(&mut stream_payloads, raw.as_ref());
            if let Some(object_ref) = object.object_ref() {
                stream_payload_members
                    .entry(hash)
                    .or_default()
                    .push(object_ref);
            }
            d
        } else if object.try_is_dictionary()? {
            object.clone()
        } else {
            continue;
        };

        let keys = dict.try_get_keys()?;
        if keys.contains(b"/PieceInfo".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::PieceInfo,
                1,
                "Adobe/private-piece metadata can contain authoring provenance and source paths",
            );
        }
        if keys.contains(b"/Thumb".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::Thumbnail,
                1,
                "page thumbnail stream is separate hidden raster content",
            );
        }
        if keys.contains(b"/AA".as_slice()) || keys.contains(b"/OpenAction".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::AutomaticAction,
                1,
                "automatic PDF actions are executable/interactive hidden state",
            );
        }
        if keys.contains(b"/OCProperties".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::HiddenLayer,
                1,
                "optional-content groups can hide content from the default view",
            );
        }
        if keys.contains(b"/ActualText".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::SuspiciousHiddenText,
                1,
                "ActualText may differ from visible glyphs and should be retained but audited in privacy mode",
            );
        }
        if keys.contains(b"/V".as_slice()) && keys.contains(b"/FT".as_slice()) {
            push_risk(
                &mut risks,
                RiskKind::FormValue,
                1,
                "interactive form field contains a stored value",
            );
        }

        for key in [
            b"/FontFile".as_slice(),
            b"/FontFile2".as_slice(),
            b"/FontFile3".as_slice(),
        ] {
            if !keys.contains(key) {
                continue;
            }
            let font = dict.try_get_key(key)?;
            if let Some(r) = font.object_ref()
                && !font_refs.insert(r)
            {
                continue;
            }
            if let Ok(data) = font.get_raw_stream_data() {
                out.font_program_count += 1;
                out.font_program_bytes += data.len();
                let duplicate_basis = font
                    .get_stream_data(DecodeLevel::Generalized)
                    .unwrap_or_else(|_| data.clone());
                record_payload(&mut font_payloads, duplicate_basis.as_ref());
            }
        }

        if object.as_stream_dict().is_none() {
            continue;
        }
        let filter = dict.try_get_key(b"/Filter")?.unparse_resolved();
        let filter_name = String::from_utf8_lossy(&filter).into_owned();
        *out.filter_counts.entry(filter_name.clone()).or_default() += 1;

        let subtype = dict.try_get_key(b"/Subtype")?;
        let is_image = subtype.try_is_name_and_equals(b"Image")?;
        let is_form = subtype.try_is_name_and_equals(b"Form")?;
        if is_image {
            out.image_count += 1;
            let raw = object.get_raw_stream_data()?;
            out.image_raw_bytes += raw.len();
            record_payload(&mut image_payloads, raw.as_ref());
            if filter_name.contains("DCTDecode") {
                // APP1/APP13/COM are cheap byte-level privacy candidates; exact
                // parsing/removal is performed by the scrubber.
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
            let raw = object.get_raw_stream_data()?;
            let duplicate_basis = object
                .get_stream_data(DecodeLevel::Generalized)
                .unwrap_or_else(|_| raw.clone());
            record_payload(&mut form_payloads, duplicate_basis.as_ref());
        }
        let type_obj = dict.try_get_key(b"/Type")?;
        if type_obj.try_is_name_and_equals(b"Metadata")? {
            let raw = object.get_raw_stream_data()?;
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

        if filter_name == "/FlateDecode" {
            out.flate_stream_count += 1;
            if !is_image {
                out.non_image_flate_stream_count += 1;
            }
            let raw = object.get_raw_stream_data()?;
            if let Ok(decoded) = object.get_stream_data(DecodeLevel::Generalized) {
                let mut encoder = ZlibEncoder::new(Vec::new(), Compression::best());
                if encoder.write_all(decoded.as_ref()).is_ok()
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
    let icc_profile_refs = collect_icc_profile_refs(&objects);
    let mut stream_roles = HashMap::new();
    for object in &objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        if object.as_stream_dict().is_none() {
            continue;
        }
        let role = stream_role(
            object,
            object_ref,
            &font_refs,
            &page_content_refs,
            &icc_profile_refs,
            &incoming_roles,
        )?;
        stream_roles.insert(object_ref, role);
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

    match analyze_inline_images(&mut pdf) {
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
    match flpdf::should_remove_unreferenced_resources(&mut pdf) {
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

    match analyze_hidden_text(&mut pdf) {
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
