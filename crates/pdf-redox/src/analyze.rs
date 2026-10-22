use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedObject, PdfAnalysis, Result, RiskFinding,
    RiskKind, content::page_content, dedup::exact_dictionary_fingerprint,
    hidden_text::analyze_hidden_text,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

pub fn input_sha256(input: &[u8]) -> String {
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
    let entry = map.entry(kind).or_insert_with(|| (0, note.to_owned()));
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
    let OwnedObject::String(bytes) = value else {
        return Ok(None);
    };
    let text = crate::hidden_text::decode_pdf_text_string(&bytes)
        .trim()
        .to_owned();
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

fn collect_stream_dedup_refs_from_value(
    value: &OwnedObject,
    font_refs: &mut HashSet<CowObjectHandle>,
    to_unicode_refs: &mut HashSet<CowObjectHandle>,
) {
    match value {
        OwnedObject::Array(values) => {
            for value in values {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_stream_dedup_refs_from_value(value, font_refs, to_unicode_refs);
                }
            }
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            for (key, child) in dictionary {
                if matches!(key.as_slice(), b"FontFile" | b"FontFile2" | b"FontFile3") {
                    if let OwnedObject::Reference(handle) = child {
                        font_refs.insert(*handle);
                    }
                } else if key.as_slice() == b"ToUnicode" {
                    if let OwnedObject::Reference(handle) = child {
                        to_unicode_refs.insert(*handle);
                    }
                } else if !matches!(child, OwnedObject::Reference(_)) {
                    // Indirect dictionaries are visited as top-level objects. Only recurse
                    // through direct children here so nested direct holders are not missed
                    // and cyclic object graphs cannot recurse forever.
                    collect_stream_dedup_refs_from_value(child, font_refs, to_unicode_refs);
                }
            }
        }
        _ => {}
    }
}

fn collect_stream_dedup_refs(
    objects: &[(CowObjectHandle, OwnedObject)],
) -> (HashSet<CowObjectHandle>, HashSet<CowObjectHandle>) {
    let mut font_refs = HashSet::new();
    let mut to_unicode_refs = HashSet::new();
    for (_, object) in objects {
        collect_stream_dedup_refs_from_value(object, &mut font_refs, &mut to_unicode_refs);
    }
    (font_refs, to_unicode_refs)
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

const fn incoming_role_for_key(key: &[u8]) -> Option<&'static str> {
    match key {
        b"Metadata" => Some("metadata"),
        b"PieceInfo" => Some("authoring-private"),
        b"Thumb" => Some("thumbnail"),
        b"ToUnicode" => Some("to-unicode"),
        b"CIDToGIDMap" => Some("cid-to-gid"),
        b"CIDSet" => Some("font-support"),
        b"CharProcs" => Some("type3-charproc"),
        b"Mask" | b"SMask" => Some("image-mask"),
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
    // Several common stream classes are identified by their defining dictionary
    // keys rather than `/Type` or `/Subtype`. Keeping them out of the generic
    // `other` bucket makes corpus byte anatomy actionable.
    if dictionary.contains_key(b"PatternType".as_slice()) {
        return Ok("pattern");
    }
    if dictionary.contains_key(b"ShadingType".as_slice()) {
        return Ok("shading");
    }
    if dictionary.contains_key(b"FunctionType".as_slice()) {
        return Ok("function");
    }
    if dictionary.contains_key(b"Width".as_slice())
        && dictionary.contains_key(b"Height".as_slice())
        && dictionary.contains_key(b"BitsPerComponent".as_slice())
    {
        return Ok("image-like");
    }
    Ok("other")
}

fn structural_object_role(
    document: &EditDocument,
    object: &OwnedObject,
    stream_role_name: Option<&str>,
) -> Result<String> {
    if matches!(object, OwnedObject::Stream { .. }) {
        return Ok(format!("stream:{}", stream_role_name.unwrap_or("other")));
    }
    if let Some(dictionary) = object.as_dictionary() {
        let kind = dictionary_name(document, dictionary, b"Type")?;
        let subtype = dictionary_name(document, dictionary, b"Subtype")?;
        return Ok(match (kind, subtype) {
            (Some(kind), Some(subtype)) => format!(
                "dict:{}/{}",
                String::from_utf8_lossy(&kind),
                String::from_utf8_lossy(&subtype)
            ),
            (Some(kind), None) => format!("dict:{}", String::from_utf8_lossy(&kind)),
            (None, Some(subtype)) => {
                format!("dict:subtype:{}", String::from_utf8_lossy(&subtype))
            }
            (None, None) => "dict:untyped".to_owned(),
        });
    }
    Ok(match object {
        OwnedObject::Array(_) => "array".to_owned(),
        OwnedObject::Name(_) => "name".to_owned(),
        OwnedObject::String(_) => "string".to_owned(),
        OwnedObject::Integer(_) | OwnedObject::Real(_) => "number".to_owned(),
        OwnedObject::Boolean(_) => "boolean".to_owned(),
        OwnedObject::Null => "null".to_owned(),
        OwnedObject::Reference(_) => "reference".to_owned(),
        OwnedObject::Dictionary(_) | OwnedObject::Stream { .. } => unreachable!(),
    })
}

fn record_object_graph_inventory(
    document: &EditDocument,
    objects: &[(CowObjectHandle, OwnedObject)],
    stream_roles: &HashMap<CowObjectHandle, &'static str>,
    out: &mut PdfAnalysis,
) -> Result<()> {
    let mut roles = HashMap::new();
    for (handle, object) in objects {
        let role = structural_object_role(document, object, stream_roles.get(handle).copied())?;
        *out.object_role_counts.entry(role.clone()).or_default() += 1;
        roles.insert(*handle, role);
    }

    for (handle, object) in objects {
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let source_role = roles
            .get(handle)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned());
        for (key, value) in dictionary {
            let key_text = String::from_utf8_lossy(key);
            *out.object_key_counts
                .entry(format!("{source_role}/{key_text}"))
                .or_default() += 1;
            for target in value.references() {
                let target_role = roles.get(&target).map_or("missing", String::as_str);
                *out.object_reference_edge_counts
                    .entry(format!("{source_role}/{key_text}->{target_role}"))
                    .or_default() += 1;
            }
        }
    }
    Ok(())
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
struct ContentAnalysisStats {
    inline_count: usize,
    inline_bytes: usize,
    inline_payloads: HashMap<[u8; 32], (usize, usize)>,
    page_instruction_count: usize,
    page_max_instructions: usize,
    page_path_construction_count: usize,
    page_max_path_construction: usize,
    page_paint_count: usize,
    page_max_paints: usize,
    page_xobject_paint_count: usize,
    page_max_xobject_paints: usize,
    page_text_show_count: usize,
    page_max_text_shows: usize,
    page_incomplete_parse_count: usize,
    form_instruction_count: usize,
    form_max_instructions: usize,
    form_path_construction_count: usize,
    form_max_path_construction: usize,
    form_paint_count: usize,
    form_max_paints: usize,
    form_incomplete_parse_count: usize,
}

#[derive(Debug, Default)]
struct PageInstructionCounter {
    instructions: usize,
    path_construction: usize,
    paints: usize,
    xobject_paints: usize,
    text_shows: usize,
}

fn scan_content(
    content: &[u8],
    inline_count: &mut usize,
    inline_bytes: &mut usize,
    inline_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
    mut page: Option<&mut PageInstructionCounter>,
) -> Result<bool> {
    crate::content_stream::visit_instructions(content, |instruction| {
        let operator = &instruction.operator[..];
        if let Some(page) = page.as_deref_mut() {
            page.instructions = page.instructions.saturating_add(1);
            if matches!(operator, b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re") {
                page.path_construction = page.path_construction.saturating_add(1);
            }
            if matches!(
                operator,
                b"S" | b"s"
                    | b"f"
                    | b"F"
                    | b"f*"
                    | b"B"
                    | b"B*"
                    | b"b"
                    | b"b*"
                    | b"sh"
                    | b"Do"
                    | b"BI"
                    | b"Tj"
                    | b"TJ"
                    | b"'"
                    | b"\""
            ) {
                page.paints = page.paints.saturating_add(1);
            }
            if operator == b"Do" {
                page.xobject_paints = page.xobject_paints.saturating_add(1);
            }
            if matches!(operator, b"Tj" | b"TJ" | b"'" | b"\"") {
                page.text_shows = page.text_shows.saturating_add(1);
            }
        }
        if operator == b"BI"
            && let Some(hayro_syntax::object::Object::Stream(stream)) =
                instruction.operands().next()
        {
            let bytes = stream.raw_data();
            *inline_count = inline_count.saturating_add(1);
            *inline_bytes = inline_bytes.saturating_add(bytes.len());
            record_payload(inline_payloads, bytes.as_ref());
        }
        Ok(())
    })
}

fn analyze_content(document: &EditDocument) -> Result<ContentAnalysisStats> {
    let mut out = ContentAnalysisStats::default();
    for page in document.page_handles()? {
        let content = page_content(document, page)?;
        let mut page_stats = PageInstructionCounter::default();
        let incomplete = scan_content(
            &content,
            &mut out.inline_count,
            &mut out.inline_bytes,
            &mut out.inline_payloads,
            Some(&mut page_stats),
        )?;
        out.page_instruction_count = out
            .page_instruction_count
            .saturating_add(page_stats.instructions);
        out.page_max_instructions = out.page_max_instructions.max(page_stats.instructions);
        out.page_path_construction_count = out
            .page_path_construction_count
            .saturating_add(page_stats.path_construction);
        out.page_max_path_construction = out
            .page_max_path_construction
            .max(page_stats.path_construction);
        out.page_paint_count = out.page_paint_count.saturating_add(page_stats.paints);
        out.page_max_paints = out.page_max_paints.max(page_stats.paints);
        out.page_xobject_paint_count = out
            .page_xobject_paint_count
            .saturating_add(page_stats.xobject_paints);
        out.page_max_xobject_paints = out.page_max_xobject_paints.max(page_stats.xobject_paints);
        out.page_text_show_count = out
            .page_text_show_count
            .saturating_add(page_stats.text_shows);
        out.page_max_text_shows = out.page_max_text_shows.max(page_stats.text_shows);
        out.page_incomplete_parse_count = out
            .page_incomplete_parse_count
            .saturating_add(usize::from(incomplete));
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
        let content = document.decoded_owned_stream_data(&object)?;
        let mut form_stats = PageInstructionCounter::default();
        let incomplete = scan_content(
            &content,
            &mut out.inline_count,
            &mut out.inline_bytes,
            &mut out.inline_payloads,
            Some(&mut form_stats),
        )?;
        out.form_instruction_count = out
            .form_instruction_count
            .saturating_add(form_stats.instructions);
        out.form_max_instructions = out.form_max_instructions.max(form_stats.instructions);
        out.form_path_construction_count = out
            .form_path_construction_count
            .saturating_add(form_stats.path_construction);
        out.form_max_path_construction = out
            .form_max_path_construction
            .max(form_stats.path_construction);
        out.form_paint_count = out.form_paint_count.saturating_add(form_stats.paints);
        out.form_max_paints = out.form_max_paints.max(form_stats.paints);
        out.form_incomplete_parse_count = out
            .form_incomplete_parse_count
            .saturating_add(usize::from(incomplete));
    }
    Ok(out)
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

fn record_to_unicode_stream_payload(
    document: &EditDocument,
    object: &OwnedObject,
    raw_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
) -> Result<()> {
    if !matches!(object, OwnedObject::Stream { .. }) {
        return Ok(());
    }
    let raw = raw_stream_bytes(document, object)?;
    record_payload(raw_payloads, raw.as_ref());
    Ok(())
}

fn record_font_program_stream(
    document: &EditDocument,
    object: &OwnedObject,
    deep: bool,
    out: &mut PdfAnalysis,
    raw_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
    decoded_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
) -> Result<()> {
    if !matches!(object, OwnedObject::Stream { .. }) {
        return Ok(());
    }
    let raw = raw_stream_bytes(document, object)?;
    out.font_program_count += 1;
    out.font_program_bytes += raw.len();
    record_payload(raw_payloads, raw.as_ref());
    if document
        .decoded_owned_stream_data(object)
        .ok()
        .as_deref()
        .is_some_and(crate::font_subset::has_pdf_rendering_unused_tables)
    {
        out.font_rendering_table_candidate_count += 1;
    }
    if deep {
        let duplicate_basis = document
            .decoded_owned_stream_data(object)
            .unwrap_or_else(|_| raw.as_ref().to_vec());
        record_payload(decoded_payloads, &duplicate_basis);
    }
    Ok(())
}

fn filter_value_name(document: &EditDocument, value: &OwnedObject) -> Result<String> {
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok("null".to_owned());
    };
    Ok(match value {
        OwnedObject::Name(name) => format!("/{}", String::from_utf8_lossy(&name)),
        OwnedObject::Array(values) => {
            let mut parts = Vec::with_capacity(values.len());
            for value in &values {
                parts.push(filter_value_name(document, value)?);
            }
            format!("[{}]", parts.join(" "))
        }
        OwnedObject::Null => "null".to_owned(),
        other => format!("{other:?}"),
    })
}

fn filter_name(
    document: &EditDocument,
    dictionary: &BTreeMap<Vec<u8>, OwnedObject>,
) -> Result<String> {
    let Some(value) = dictionary.get(b"Filter".as_slice()) else {
        return Ok("null".to_owned());
    };
    filter_value_name(document, value)
}

/// Analyze a PDF without modifying it.
///
/// # Errors
///
/// Returns an error when the input cannot be parsed or a referenced stream cannot be decoded.
pub fn analyze_pdf(input: &[u8]) -> Result<PdfAnalysis> {
    analyze_pdf_impl(input, true, true)
}

/// Analyze inexpensive structural and content-density metrics without deep duplicate/compression work.
///
/// The returned report has `analysis_complete = false`.
///
/// # Errors
///
/// Returns an error when the input cannot be parsed or required structural streams cannot be decoded.
pub fn analyze_pdf_preflight(input: &[u8]) -> Result<PdfAnalysis> {
    analyze_pdf_impl(input, false, true)
}

pub fn analyze_document_for_optimization(
    input: &[u8],
    document: &EditDocument,
) -> Result<PdfAnalysis> {
    analyze_document_impl(input, document, false, false)
}

#[derive(Default)]
struct DirectKeyInventory {
    piece_info: usize,
    authoring_metadata: usize,
    to_unicode: usize,
    ext_gstate: usize,
    char_procs: usize,
    appearance: usize,
    icc_based_arrays: usize,
    type3_font_scopes: usize,
    tiling_pattern_scopes: usize,
    font_glyph_scope_inventory_complete: bool,
}

fn count_direct_preflight_keys(
    document: &EditDocument,
    object: &OwnedObject,
    inventory: &mut DirectKeyInventory,
    count_targeted_keys: bool,
) {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if dictionary.contains_key(b"Subtype".as_slice()) {
                match dictionary_name(document, dictionary, b"Subtype") {
                    Ok(Some(name)) if name == b"Type3" => {
                        inventory.type3_font_scopes = inventory.type3_font_scopes.saturating_add(1);
                    }
                    Ok(_) => {}
                    Err(_) => inventory.font_glyph_scope_inventory_complete = false,
                }
            }
            if matches!(object, OwnedObject::Stream { .. })
                && let Some(pattern_type) = dictionary.get(b"PatternType".as_slice())
            {
                match document.resolve_owned_value(pattern_type) {
                    Ok(Some(OwnedObject::Integer(1))) => {
                        inventory.tiling_pattern_scopes =
                            inventory.tiling_pattern_scopes.saturating_add(1);
                    }
                    Ok(_) => {}
                    Err(_) => inventory.font_glyph_scope_inventory_complete = false,
                }
            }

            if count_targeted_keys {
                inventory.piece_info = inventory.piece_info.saturating_add(usize::from(
                    dictionary.contains_key(b"PieceInfo".as_slice()),
                ));
                inventory.authoring_metadata = inventory
                    .authoring_metadata
                    .saturating_add(usize::from(dictionary.contains_key(b"Metadata".as_slice())));
                inventory.authoring_metadata = inventory.authoring_metadata.saturating_add(
                    usize::from(dictionary.contains_key(b"LastModified".as_slice())),
                );
                inventory.to_unicode = inventory.to_unicode.saturating_add(usize::from(
                    dictionary.contains_key(b"ToUnicode".as_slice()),
                ));
                inventory.ext_gstate = inventory.ext_gstate.saturating_add(usize::from(
                    dictionary.contains_key(b"ExtGState".as_slice()),
                ));
                inventory.char_procs = inventory.char_procs.saturating_add(usize::from(
                    dictionary.contains_key(b"CharProcs".as_slice()),
                ));
                inventory.appearance = inventory
                    .appearance
                    .saturating_add(usize::from(dictionary.contains_key(b"AP".as_slice())));
            }
            for value in dictionary.values() {
                if !matches!(value, OwnedObject::Reference(_)) {
                    count_direct_preflight_keys(document, value, inventory, count_targeted_keys);
                }
            }
        }
        OwnedObject::Array(values) => {
            let mut count_child_targeted_keys = count_targeted_keys;
            if count_targeted_keys {
                let icc_status =
                    values
                        .first()
                        .map(|first| match document.resolve_owned_value(first) {
                            Ok(Some(OwnedObject::Name(name))) => name == b"ICCBased",
                            Ok(_) => false,
                            Err(_) => true,
                        });
                if icc_status == Some(true)
                    && matches!(values.get(1), Some(OwnedObject::Reference(_)))
                {
                    inventory.icc_based_arrays = inventory.icc_based_arrays.saturating_add(1);
                    // Preserve the old targeted-key inventory's ICCBased terminal
                    // semantics, but continue the physical walk for the independent
                    // glyph-scope proof below.
                    count_child_targeted_keys = false;
                }
            }
            for value in values {
                if !matches!(value, OwnedObject::Reference(_)) {
                    count_direct_preflight_keys(
                        document,
                        value,
                        inventory,
                        count_child_targeted_keys,
                    );
                }
            }
        }
        OwnedObject::Reference(_)
        | OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
}

fn analyze_pdf_impl(input: &[u8], deep: bool, content_density: bool) -> Result<PdfAnalysis> {
    let document = EditDocument::from_bytes(input.to_vec())?;
    analyze_document_impl(input, &document, deep, content_density)
}

#[expect(
    clippy::too_many_lines,
    reason = "the analysis is a single ordered aggregation pass over related document metrics"
)]
fn analyze_document_impl(
    input: &[u8],
    document: &EditDocument,
    deep: bool,
    content_density: bool,
) -> Result<PdfAnalysis> {
    let objects = if deep {
        Some(all_source_objects(document)?)
    } else {
        None
    };
    let mut out = PdfAnalysis {
        input_bytes: input.len(),
        analysis_complete: deep,
        input_sha256: input_sha256(input),
        page_count: document.source().page_count(),
        object_count: document.source().object_count(),
        ..PdfAnalysis::default()
    };

    match document_info_text(document, b"Producer") {
        Ok(value) => out.producer = value,
        Err(error) => out
            .warnings
            .push(format!("Producer metadata analysis skipped: {error}")),
    }
    match document_info_text(document, b"Creator") {
        Ok(value) => out.creator = value,
        Err(error) => out
            .warnings
            .push(format!("Creator metadata analysis skipped: {error}")),
    }

    let page_content_refs = if deep {
        match collect_page_content_refs(document) {
            Ok(refs) => refs,
            Err(error) => {
                out.warnings
                    .push(format!("page-content role analysis skipped: {error}"));
                HashSet::new()
            }
        }
    } else {
        HashSet::new()
    };
    let mut risks: BTreeMap<RiskKind, (usize, String)> = BTreeMap::new();
    let mut stream_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut stream_payload_members: HashMap<[u8; 32], Vec<CowObjectHandle>> = HashMap::new();
    let mut metadata_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut image_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut form_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_raw_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut to_unicode_raw_payloads: HashMap<[u8; 32], (usize, usize)> = HashMap::new();
    let mut font_dictionary_fingerprints = HashMap::<[u8; 32], usize>::new();
    let mut font_dictionary_fingerprint_inventory_complete = true;
    let mut font_glyph_scope_inventory_complete = true;
    let (mut font_refs, mut to_unicode_refs) = objects.as_deref().map_or_else(
        || (HashSet::new(), HashSet::new()),
        collect_stream_dedup_refs,
    );
    let mut processed_font_refs = HashSet::new();
    let mut processed_to_unicode_refs = HashSet::new();
    out.preservation_key_inventory_complete = true;
    out.font_rendering_table_inventory_complete = true;
    out.font_raw_payload_inventory_version = 1;
    out.to_unicode_raw_payload_inventory_version = 1;
    out.targeted_dedup_key_inventory_version = 3;

    let mut analyze_object = |handle: CowObjectHandle,
                              object: &OwnedObject,
                              font_refs: &HashSet<CowObjectHandle>,
                              to_unicode_refs: &HashSet<CowObjectHandle>,
                              processed_font_refs: &mut HashSet<CowObjectHandle>,
                              processed_to_unicode_refs: &mut HashSet<CowObjectHandle>|
     -> Result<()> {
        // These keys may legally sit inside direct dictionaries/arrays nested
        // under any indirect object. Inventory them during the preflight's
        // existing source-object walk so later passes can skip redundant
        // whole-reachable-graph scans only when absence is proven.
        let mut keys = DirectKeyInventory {
            font_glyph_scope_inventory_complete: true,
            ..DirectKeyInventory::default()
        };
        count_direct_preflight_keys(document, object, &mut keys, true);
        out.piece_info_key_count = out.piece_info_key_count.saturating_add(keys.piece_info);
        out.authoring_metadata_key_count = out
            .authoring_metadata_key_count
            .saturating_add(keys.authoring_metadata);
        out.to_unicode_key_count = out.to_unicode_key_count.saturating_add(keys.to_unicode);
        out.ext_gstate_key_count = out.ext_gstate_key_count.saturating_add(keys.ext_gstate);
        out.char_procs_key_count = out.char_procs_key_count.saturating_add(keys.char_procs);
        out.appearance_key_count = out.appearance_key_count.saturating_add(keys.appearance);
        out.icc_based_array_count = out
            .icc_based_array_count
            .saturating_add(keys.icc_based_arrays);
        out.type3_font_scope_count = out
            .type3_font_scope_count
            .saturating_add(keys.type3_font_scopes);
        out.tiling_pattern_scope_count = out
            .tiling_pattern_scope_count
            .saturating_add(keys.tiling_pattern_scopes);
        font_glyph_scope_inventory_complete &= keys.font_glyph_scope_inventory_complete;
        if let Some(dictionary) = object.as_dictionary()
            && matches!(
                dictionary.get(b"A".as_slice()),
                Some(OwnedObject::Reference(_))
            )
        {
            let may_be_struct_elem = match dictionary_name(document, dictionary, b"Type") {
                Ok(Some(name)) => name == b"StructElem",
                Ok(None) => false,
                Err(_) => true,
            };
            if may_be_struct_elem {
                out.structure_attribute_ref_count =
                    out.structure_attribute_ref_count.saturating_add(1);
            }
        }
        if keys.piece_info > 0 {
            push_risk(
                &mut risks,
                RiskKind::PieceInfo,
                1,
                "Adobe/private-piece metadata can contain authoring provenance and source paths",
            );
        }

        let Some(dictionary) = object.as_dictionary() else {
            return Ok(());
        };
        match dictionary_name(document, dictionary, b"Type") {
            Ok(Some(name)) if name == b"Font" => {
                match exact_dictionary_fingerprint(object, b"exact-font-dictionary") {
                    Ok(Some(fingerprint)) => {
                        *font_dictionary_fingerprints.entry(fingerprint).or_default() += 1;
                    }
                    Ok(None) => {}
                    Err(_) => font_dictionary_fingerprint_inventory_complete = false,
                }
            }
            Ok(_) => {}
            Err(_) => font_dictionary_fingerprint_inventory_complete = false,
        }
        let is_stream = matches!(object, OwnedObject::Stream { .. });
        if is_stream {
            let may_be_pattern = match dictionary_name(document, dictionary, b"Type") {
                Ok(Some(name)) => name == b"Pattern",
                Ok(None) => false,
                Err(_) => true,
            };
            if may_be_pattern {
                out.pattern_stream_count = out.pattern_stream_count.saturating_add(1);
            }
            out.stream_count += 1;
            let raw = raw_stream_bytes(document, object)?;
            out.stream_raw_bytes += raw.len();
            if deep {
                let hash = record_payload(&mut stream_payloads, raw.as_ref());
                stream_payload_members.entry(hash).or_default().push(handle);
            }
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

        if is_stream && to_unicode_refs.contains(&handle) {
            record_to_unicode_stream_payload(document, object, &mut to_unicode_raw_payloads)?;
            processed_to_unicode_refs.insert(handle);
        }

        if is_stream && font_refs.contains(&handle) {
            record_font_program_stream(
                document,
                object,
                deep,
                &mut out,
                &mut font_raw_payloads,
                &mut font_payloads,
            )?;
            processed_font_refs.insert(handle);
        }

        if !is_stream {
            return Ok(());
        }
        let filter = filter_name(document, dictionary)?;
        *out.filter_counts.entry(filter.clone()).or_default() += 1;

        let subtype = dictionary_name(document, dictionary, b"Subtype")?;
        let is_image = subtype.as_deref() == Some(b"Image");
        let is_form = subtype.as_deref() == Some(b"Form");
        if is_image {
            out.image_count += 1;
            let raw = raw_stream_bytes(document, object)?;
            out.image_raw_bytes += raw.len();
            if deep {
                record_payload(&mut image_payloads, raw.as_ref());
            }
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
            let raw = raw_stream_bytes(document, object)?;
            if deep {
                let duplicate_basis = document
                    .decoded_owned_stream_data(object)
                    .unwrap_or_else(|_| raw.as_ref().to_vec());
                record_payload(&mut form_payloads, &duplicate_basis);
            }
        }
        if dictionary_name(document, dictionary, b"Type")?.as_deref() == Some(b"Metadata") {
            let raw = raw_stream_bytes(document, object)?;
            out.metadata_stream_count += 1;
            out.metadata_stream_bytes += raw.len();
            if deep {
                record_payload(&mut metadata_payloads, raw.as_ref());
            }
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
            if deep {
                let raw = raw_stream_bytes(document, object)?;
                if let Ok(decoded) = document.decoded_owned_stream_data(object)
                    && let Ok(repacked) =
                        crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::SIZE)
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
        Ok(())
    };

    if let Some(objects) = objects.as_deref() {
        for (handle, object) in objects {
            analyze_object(
                *handle,
                object,
                &font_refs,
                &to_unicode_refs,
                &mut processed_font_refs,
                &mut processed_to_unicode_refs,
            )?;
        }
    } else {
        for id in document.source().object_ids() {
            let handle = CowObjectHandle::Existing(id);
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            collect_stream_dedup_refs_from_value(&object, &mut font_refs, &mut to_unicode_refs);
            analyze_object(
                handle,
                &object,
                &font_refs,
                &to_unicode_refs,
                &mut processed_font_refs,
                &mut processed_to_unicode_refs,
            )?;
        }
    }
    drop(analyze_object);

    if !deep {
        for handle in to_unicode_refs
            .difference(&processed_to_unicode_refs)
            .copied()
        {
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            record_to_unicode_stream_payload(document, &object, &mut to_unicode_raw_payloads)?;
        }
        for handle in font_refs.difference(&processed_font_refs).copied() {
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            record_font_program_stream(
                document,
                &object,
                false,
                &mut out,
                &mut font_raw_payloads,
                &mut font_payloads,
            )?;
        }
    }

    out.font_glyph_scope_inventory_version = u8::from(font_glyph_scope_inventory_complete);
    out.font_dictionary_fingerprint_inventory_version =
        u8::from(font_dictionary_fingerprint_inventory_complete);
    out.font_dictionary_duplicate_fingerprint_groups = font_dictionary_fingerprints
        .values()
        .filter(|&&count| count > 1)
        .count();
    (out.font_raw_duplicate_payload_groups, _) = duplicate_payload_stats(font_raw_payloads);
    (out.to_unicode_raw_duplicate_payload_groups, _) =
        duplicate_payload_stats(to_unicode_raw_payloads);

    if deep {
        let objects = objects.as_deref().ok_or_else(|| {
            crate::Error::Invalid("deep analysis object table is missing".to_owned())
        })?;
        let incoming_roles = collect_incoming_roles(objects);
        let icc_profile_refs = collect_icc_profile_refs(document, objects)?;
        let mut stream_roles = HashMap::new();
        for (handle, object) in objects {
            if !matches!(object, OwnedObject::Stream { .. }) {
                continue;
            }
            let role = stream_role(
                document,
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
                .or_default() += raw_stream_bytes(document, object)?.len();
            stream_roles.insert(*handle, role);
        }
        (
            out.duplicate_stream_role_groups,
            out.duplicate_stream_role_wasted_bytes,
        ) = duplicate_payload_role_stats(&stream_payloads, &stream_payload_members, &stream_roles);
        record_object_graph_inventory(document, objects, &stream_roles, &mut out)?;

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
    }

    if content_density {
        match analyze_content(document) {
            Ok(content) => {
                let (duplicate_groups, duplicate_wasted_bytes) =
                    duplicate_payload_stats(content.inline_payloads);
                out.inline_image_count = content.inline_count;
                out.inline_image_bytes = content.inline_bytes;
                out.duplicate_inline_image_payload_groups = duplicate_groups;
                out.duplicate_inline_image_payload_wasted_bytes = duplicate_wasted_bytes;
                out.page_content_instruction_count = content.page_instruction_count;
                out.page_content_max_instructions_per_page = content.page_max_instructions;
                out.page_content_path_construction_operator_count =
                    content.page_path_construction_count;
                out.page_content_max_path_construction_operators_per_page =
                    content.page_max_path_construction;
                out.page_content_paint_operator_count = content.page_paint_count;
                out.page_content_max_paint_operators_per_page = content.page_max_paints;
                out.page_content_xobject_paint_operator_count = content.page_xobject_paint_count;
                out.page_content_max_xobject_paints_per_page = content.page_max_xobject_paints;
                out.page_content_text_show_operator_count = content.page_text_show_count;
                out.page_content_max_text_show_operators_per_page = content.page_max_text_shows;
                out.page_content_incomplete_parse_pages = content.page_incomplete_parse_count;
                out.form_content_instruction_count = content.form_instruction_count;
                out.form_content_max_instructions_per_form = content.form_max_instructions;
                out.form_content_path_construction_operator_count =
                    content.form_path_construction_count;
                out.form_content_max_path_construction_operators_per_form =
                    content.form_max_path_construction;
                out.form_content_paint_operator_count = content.form_paint_count;
                out.form_content_max_paint_operators_per_form = content.form_max_paints;
                out.form_content_incomplete_parse_forms = content.form_incomplete_parse_count;
            }
            Err(error) => out
                .warnings
                .push(format!("content-stream analysis skipped: {error}")),
        }
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

    if deep {
        match analyze_hidden_text(document) {
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
    }

    out.risks = risks
        .into_iter()
        .map(|(kind, (count, note))| RiskFinding { kind, count, note })
        .collect();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ClassicPdfBuilder;

    fn content_density_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /F 5 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q /F Do Q 0 0 m 10 10 l S BT (x) Tj ET")?;
        pdf.stream(
            5,
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources <<>>",
            b"0 0 m 5 5 l S",
        )?;
        pdf.finish(1)
    }

    #[test]
    fn preflight_piece_info_risk_covers_nested_direct_dictionaries() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Private << /Nested << /PieceInfo 6 0 R >> >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(6, b"<< /Illustrator true >>")?;
        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert!(
            analysis
                .risks
                .iter()
                .any(|finding| { finding.kind == RiskKind::PieceInfo && finding.count == 1 })
        );
        assert!(analysis.preservation_key_inventory_complete);
        assert_eq!(analysis.piece_info_key_count, 1);
        Ok(())
    }

    #[test]
    fn preflight_piece_info_risk_covers_direct_dictionary_inside_indirect_array() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Private 5 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"[ << /PieceInfo 6 0 R /Keep true >> ]")?;
        pdf.object(6, b"<< /Illustrator true >>")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert!(
            analysis
                .risks
                .iter()
                .any(|finding| finding.kind == RiskKind::PieceInfo && finding.count == 1)
        );
        assert!(analysis.preservation_key_inventory_complete);
        assert_eq!(analysis.piece_info_key_count, 1);
        Ok(())
    }

    #[test]
    fn preflight_preservation_inventory_counts_metadata_keys() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Metadata 5 0 R /Private << /ToUnicode 5 0 R /AP << /N 4 0 R >> >> >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /LastModified (now) /Resources << /ExtGState <<>> >> /CharProcs <<>> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"/Type /Metadata /Subtype /XML", b"<xmp/>")?;
        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert!(analysis.preservation_key_inventory_complete);
        assert_eq!(analysis.piece_info_key_count, 0);
        assert_eq!(analysis.authoring_metadata_key_count, 2);
        assert_eq!(analysis.targeted_dedup_key_inventory_version, 3);
        assert_eq!(analysis.to_unicode_key_count, 1);
        assert_eq!(analysis.to_unicode_raw_payload_inventory_version, 1);
        assert_eq!(analysis.to_unicode_raw_duplicate_payload_groups, 0);
        assert_eq!(analysis.ext_gstate_key_count, 1);
        assert_eq!(analysis.char_procs_key_count, 1);
        assert_eq!(analysis.appearance_key_count, 1);
        assert_eq!(analysis.icc_based_array_count, 0);
        assert_eq!(analysis.structure_attribute_ref_count, 0);
        Ok(())
    }

    #[test]
    fn preflight_font_glyph_scope_inventory_matches_actual_scope_scanners() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Private [/ICCBased 6 0 R << /Subtype /Type3 >>] >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Pattern << /P 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(
            5,
            b"/PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 1 1] /XStep 1 /YStep 1 /Resources <<>>",
            b"",
        )?;
        pdf.stream(6, b"/N 3 /Alternate /DeviceRGB", b"profile")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.font_glyph_scope_inventory_version, 1);
        assert_eq!(analysis.type3_font_scope_count, 1);
        assert_eq!(analysis.tiling_pattern_scope_count, 1);
        // These older inventories are intentionally weaker proofs: the Type3
        // scope has no /CharProcs and the tiling Pattern has no /Type /Pattern.
        assert_eq!(analysis.char_procs_key_count, 0);
        assert_eq!(analysis.pattern_stream_count, 0);
        // The direct Type3 dictionary sits after an ICC profile reference; the
        // independent glyph-scope walk must continue where targeted-key counting stops.
        assert_eq!(analysis.icc_based_array_count, 1);
        Ok(())
    }

    #[test]
    fn preflight_font_dictionary_inventory_detects_exact_duplicate_fingerprints() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")?;
        pdf.object(6, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.font_dictionary_fingerprint_inventory_version, 1);
        assert_eq!(analysis.font_dictionary_duplicate_fingerprint_groups, 1);
        Ok(())
    }

    #[test]
    fn preflight_to_unicode_raw_payload_inventory_detects_duplicates() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Private << /A << /ToUnicode 5 0 R >> /B << /ToUnicode 6 0 R >> >> >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"/CIDInit /ProcSet findresource begin")?;
        pdf.stream(6, b"", b"/CIDInit /ProcSet findresource begin")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.to_unicode_key_count, 2);
        assert_eq!(analysis.to_unicode_raw_payload_inventory_version, 1);
        assert_eq!(analysis.to_unicode_raw_duplicate_payload_groups, 1);
        Ok(())
    }

    #[test]
    fn preflight_streaming_recovers_targets_seen_before_holders() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"same-font-program")?;
        pdf.stream(6, b"", b"same-font-program")?;
        pdf.stream(7, b"", b"same-to-unicode")?;
        pdf.stream(8, b"", b"same-to-unicode")?;
        // Both target pairs sort before the holder dictionaries in xref order.
        // Shallow analysis must recover them after the streaming object walk
        // instead of depending on holder-before-target numbering.
        pdf.object(9, b"<< /FontFile2 5 0 R /ToUnicode 7 0 R >>")?;
        pdf.object(10, b"<< /FontFile2 6 0 R /ToUnicode 8 0 R >>")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.font_program_count, 2);
        assert_eq!(analysis.font_program_bytes, b"same-font-program".len() * 2);
        assert_eq!(analysis.font_raw_duplicate_payload_groups, 1);
        assert_eq!(analysis.to_unicode_key_count, 2);
        assert_eq!(analysis.to_unicode_raw_duplicate_payload_groups, 1);
        Ok(())
    }

    #[test]
    fn preflight_inventory_counts_icc_and_structure_attribute_holders() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ColorSpace << /CS [/ICCBased 5 0 R] >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"/N 3 /Alternate /DeviceRGB", b"profile")?;
        pdf.object(6, b"<< /Type /StructElem /A 7 0 R >>")?;
        pdf.object(7, b"<< /O /Layout >>")?;
        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.targeted_dedup_key_inventory_version, 3);
        assert_eq!(analysis.icc_based_array_count, 1);
        assert_eq!(analysis.structure_attribute_ref_count, 1);
        Ok(())
    }

    #[test]
    fn preflight_inventory_counts_pattern_streams() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Pattern << /P 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"/Pattern cs /P scn 0 0 10 10 re f")?;
        pdf.stream(
            5,
            b"/Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 1 1] /XStep 1 /YStep 1 /Resources <<>>",
            b"0 0 1 1 re f",
        )?;
        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.targeted_dedup_key_inventory_version, 3);
        assert_eq!(analysis.pattern_stream_count, 1);
        Ok(())
    }

    fn font_table_inventory_fixture(removable: bool) -> Result<Vec<u8>> {
        use write_fonts::{FontBuilder, types::Tag};

        let mut builder = FontBuilder::new();
        builder.add_raw(Tag::new(b"head"), vec![0_u8; 54]);
        builder.add_raw(Tag::new(b"glyf"), b"glyphs".to_vec());
        if removable {
            builder.add_raw(Tag::new(b"GSUB"), b"substitution".to_vec());
        }
        let font = builder.build();
        let encoded =
            crate::stream_codec::encode_flate(&font, crate::FlateLevel::SIZE).map_err(|error| {
                crate::Error::Invalid(format!("fixture flate encode failed: {error}"))
            })?;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"BT /F1 10 Tf <0000> Tj ET")?;
        pdf.stream(5, b"/Filter /FlateDecode", &encoded)?;
        pdf.object(6, b"<< /Type /Font /Subtype /Type0 /BaseFont /Inventory /Encoding /Identity-H /DescendantFonts [7 0 R] >>")?;
        pdf.object(7, b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /Inventory /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 8 0 R >>")?;
        pdf.object(8, b"<< /Type /FontDescriptor /FontName /Inventory /Flags 4 /FontBBox [0 0 10 10] /ItalicAngle 0 /Ascent 10 /Descent 0 /CapHeight 10 /StemV 80 /FontFile2 5 0 R >>")?;
        pdf.finish(1)
    }

    #[test]
    fn preflight_font_table_inventory_is_complete_and_conservative() -> Result<()> {
        let clean = analyze_pdf_preflight(&font_table_inventory_fixture(false)?)?;
        assert!(clean.font_rendering_table_inventory_complete);
        assert_eq!(clean.font_rendering_table_candidate_count, 0);
        assert_eq!(clean.font_raw_payload_inventory_version, 1);
        assert_eq!(clean.font_raw_duplicate_payload_groups, 0);

        let removable = analyze_pdf_preflight(&font_table_inventory_fixture(true)?)?;
        assert!(removable.font_rendering_table_inventory_complete);
        assert_eq!(removable.font_rendering_table_candidate_count, 1);
        assert_eq!(removable.font_raw_payload_inventory_version, 1);
        assert_eq!(removable.font_raw_duplicate_payload_groups, 0);
        Ok(())
    }

    #[test]
    fn preflight_font_raw_payload_inventory_detects_duplicates() -> Result<()> {
        use write_fonts::{FontBuilder, types::Tag};

        let mut builder = FontBuilder::new();
        builder.add_raw(Tag::new(b"head"), vec![0_u8; 54]);
        builder.add_raw(Tag::new(b"glyf"), b"glyphs".to_vec());
        let font = builder.build();
        let encoded =
            crate::stream_codec::encode_flate(&font, crate::FlateLevel::SIZE).map_err(|error| {
                crate::Error::Invalid(format!("fixture flate encode failed: {error}"))
            })?;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 6 0 R /F2 10 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"BT /F1 10 Tf <0000> Tj /F2 10 Tf <0000> Tj ET")?;
        pdf.stream(5, b"/Filter /FlateDecode", &encoded)?;
        pdf.object(6, b"<< /Type /Font /Subtype /Type0 /BaseFont /InventoryA /Encoding /Identity-H /DescendantFonts [7 0 R] >>")?;
        pdf.object(7, b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /InventoryA /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 8 0 R >>")?;
        pdf.object(8, b"<< /Type /FontDescriptor /FontName /InventoryA /Flags 4 /FontBBox [0 0 10 10] /ItalicAngle 0 /Ascent 10 /Descent 0 /CapHeight 10 /StemV 80 /FontFile2 5 0 R >>")?;
        pdf.stream(9, b"/Filter /FlateDecode", &encoded)?;
        pdf.object(10, b"<< /Type /Font /Subtype /Type0 /BaseFont /InventoryB /Encoding /Identity-H /DescendantFonts [11 0 R] >>")?;
        pdf.object(11, b"<< /Type /Font /Subtype /CIDFontType2 /BaseFont /InventoryB /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 12 0 R >>")?;
        pdf.object(12, b"<< /Type /FontDescriptor /FontName /InventoryB /Flags 4 /FontBBox [0 0 10 10] /ItalicAngle 0 /Ascent 10 /Descent 0 /CapHeight 10 /StemV 80 /FontFile2 9 0 R >>")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.font_raw_payload_inventory_version, 1);
        assert_eq!(analysis.font_raw_duplicate_payload_groups, 1);
        Ok(())
    }

    #[test]
    fn preflight_analysis_reports_page_and_form_content_density() -> Result<()> {
        let input = content_density_fixture()?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert!(!analysis.analysis_complete);
        assert_eq!(analysis.page_content_instruction_count, 9);
        assert_eq!(analysis.page_content_max_instructions_per_page, 9);
        assert_eq!(analysis.page_content_path_construction_operator_count, 2);
        assert_eq!(analysis.page_content_paint_operator_count, 3);
        assert_eq!(analysis.page_content_xobject_paint_operator_count, 1);
        assert_eq!(analysis.page_content_text_show_operator_count, 1);
        assert_eq!(analysis.form_content_instruction_count, 3);
        assert_eq!(analysis.form_content_max_instructions_per_form, 3);
        assert_eq!(analysis.form_content_path_construction_operator_count, 2);
        assert_eq!(analysis.form_content_paint_operator_count, 1);
        assert_eq!(analysis.page_content_incomplete_parse_pages, 0);
        assert_eq!(analysis.form_content_incomplete_parse_forms, 0);
        Ok(())
    }
}
