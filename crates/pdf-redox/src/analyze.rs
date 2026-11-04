use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedObject, PdfAnalysis, Result, RiskFinding,
    RiskKind,
    content::{page_content, page_resources},
    dedup::exact_dictionary_fingerprint,
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

fn duplicate_payload_stats(map: &HashMap<[u8; 32], (usize, usize)>) -> (usize, usize) {
    map.values()
        .filter(|(count, _)| *count > 1)
        .fold((0, 0), |(groups, wasted), (count, bytes)| {
            (groups + 1, wasted + (count - 1) * bytes)
        })
}

// Compact, lossless role topology: reference handles, ordered dictionary keys
// and COS types are retained, while scalar payloads and stream bytes are not.
#[derive(Debug)]
enum RoleValue {
    Null,
    Boolean,
    Number,
    Name,
    String,
    Reference(CowObjectHandle),
    Array(Vec<Self>),
    Dictionary(Vec<(Box<[u8]>, Self)>),
    Stream(Vec<(Box<[u8]>, Self)>),
}

impl RoleValue {
    fn from_object(object: &OwnedObject) -> Self {
        match object {
            OwnedObject::Null => Self::Null,
            OwnedObject::Boolean(_) => Self::Boolean,
            OwnedObject::Integer(_) | OwnedObject::Real(_) => Self::Number,
            OwnedObject::Name(_) => Self::Name,
            OwnedObject::String(_) => Self::String,
            OwnedObject::Reference(handle) => Self::Reference(*handle),
            OwnedObject::Array(values) => {
                Self::Array(values.iter().map(Self::from_object).collect())
            }
            OwnedObject::Dictionary(dictionary) => Self::Dictionary(
                dictionary
                    .iter()
                    .map(|(key, value)| (key.clone().into_boxed_slice(), Self::from_object(value)))
                    .collect(),
            ),
            OwnedObject::Stream { dictionary, .. } => Self::Stream(
                dictionary
                    .iter()
                    .map(|(key, value)| (key.clone().into_boxed_slice(), Self::from_object(value)))
                    .collect(),
            ),
        }
    }

    fn as_dictionary(&self) -> Option<&[(Box<[u8]>, Self)]> {
        match self {
            Self::Dictionary(dictionary) | Self::Stream(dictionary) => Some(dictionary),
            _ => None,
        }
    }

    fn collect_references(&self, refs: &mut BTreeSet<CowObjectHandle>) {
        match self {
            Self::Reference(handle) => {
                refs.insert(*handle);
            }
            Self::Array(values) => {
                for value in values {
                    value.collect_references(refs);
                }
            }
            Self::Dictionary(dictionary) => {
                for (_, value) in dictionary {
                    value.collect_references(refs);
                }
            }
            Self::Stream(dictionary) => {
                // Match OwnedObject::references(): stream /Length is transport
                // metadata and must not contribute an indirect reference edge.
                for (key, value) in dictionary {
                    if key.as_ref() != b"Length" {
                        value.collect_references(refs);
                    }
                }
            }
            Self::Null | Self::Boolean | Self::Number | Self::Name | Self::String => {}
        }
    }

    fn references(&self) -> Vec<CowObjectHandle> {
        let mut refs = BTreeSet::new();
        self.collect_references(&mut refs);
        refs.into_iter().collect()
    }
}

#[derive(Default)]
struct PreflightRefs {
    font: HashSet<CowObjectHandle>,
    font_file2: HashSet<CowObjectHandle>,
    to_unicode: HashSet<CowObjectHandle>,
    cid_to_gid: HashSet<CowObjectHandle>,
    ext_gstate_maps: HashSet<CowObjectHandle>,
    ext_gstates: HashSet<CowObjectHandle>,
    icc_profiles: HashSet<CowObjectHandle>,
}

fn collect_ext_g_state_map_refs(value: &OwnedObject, refs: &mut PreflightRefs) {
    match value {
        OwnedObject::Reference(handle) => {
            refs.ext_gstate_maps.insert(*handle);
        }
        OwnedObject::Dictionary(dictionary) => {
            for child in dictionary.values() {
                if let OwnedObject::Reference(handle) = child {
                    refs.ext_gstates.insert(*handle);
                }
            }
        }
        _ => {}
    }
}

fn resolve_ext_g_state_map_refs(document: &EditDocument, refs: &mut PreflightRefs) -> Result<()> {
    let maps: Vec<_> = refs.ext_gstate_maps.iter().copied().collect();
    for handle in maps {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        for child in dictionary.values() {
            if let OwnedObject::Reference(target) = child {
                refs.ext_gstates.insert(*target);
            }
        }
    }
    Ok(())
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

fn role_descends_into_dictionary(role: &str) -> bool {
    role != "page-content"
}

fn collect_incoming_roles(
    objects: &[(CowObjectHandle, RoleValue)],
    page_handles: &HashSet<CowObjectHandle>,
) -> HashMap<CowObjectHandle, HashSet<&'static str>> {
    let objects_by_handle: HashMap<_, _> = objects
        .iter()
        .map(|(handle, object)| (*handle, object))
        .collect();
    let mut roles: HashMap<CowObjectHandle, HashSet<&'static str>> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut seen = HashSet::new();

    for (handle, object) in objects {
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        if page_handles.contains(handle)
            && let Some(contents) = dictionary
                .iter()
                .find(|(key, _)| key.as_ref() == b"Contents")
                .map(|(_, value)| value)
        {
            queue.push_back((contents, "page-content"));
        }
        for (key, value) in dictionary {
            if let Some(role) = incoming_role_for_key(key) {
                queue.push_back((value, role));
            }
        }
    }

    while let Some((value, role)) = queue.pop_front() {
        match value {
            RoleValue::Reference(handle) => {
                if !seen.insert((*handle, role)) {
                    continue;
                }
                let Some(target) = objects_by_handle.get(handle) else {
                    continue;
                };
                match *target {
                    RoleValue::Stream(_) => {
                        roles.entry(*handle).or_default().insert(role);
                    }
                    RoleValue::Array(values) => {
                        for value in values {
                            queue.push_back((value, role));
                        }
                    }
                    RoleValue::Dictionary(dictionary) if role_descends_into_dictionary(role) => {
                        for (key, child) in dictionary {
                            queue.push_back((child, incoming_role_for_key(key).unwrap_or(role)));
                        }
                    }
                    _ => {}
                }
            }
            RoleValue::Array(values) => {
                for value in values {
                    queue.push_back((value, role));
                }
            }
            RoleValue::Dictionary(dictionary) | RoleValue::Stream(dictionary)
                if role_descends_into_dictionary(role) =>
            {
                for (key, child) in dictionary {
                    queue.push_back((child, incoming_role_for_key(key).unwrap_or(role)));
                }
            }
            _ => {}
        }
    }

    roles
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

#[derive(Debug)]
struct DeepObjectFacts {
    kind: Option<Vec<u8>>,
    subtype: Option<Vec<u8>>,
    raw_stream_bytes: Option<usize>,
}

fn stream_role(
    object: &RoleValue,
    handle: CowObjectHandle,
    facts: &DeepObjectFacts,
    font_refs: &HashSet<CowObjectHandle>,
    icc_profile_refs: &HashSet<CowObjectHandle>,
    incoming_roles: &HashMap<CowObjectHandle, HashSet<&'static str>>,
) -> &'static str {
    let RoleValue::Stream(dictionary) = object else {
        return "other";
    };
    if let Some(kind) = facts.kind.as_deref() {
        match kind {
            b"Metadata" => return "metadata",
            b"EmbeddedFile" => return "embedded-file",
            b"ObjStm" => return "object-stream",
            b"XRef" => return "xref-stream",
            _ => {}
        }
    }
    if let Some(subtype) = facts.subtype.as_deref() {
        match subtype {
            b"Image" => return "image",
            b"Form" => return "form",
            _ => {}
        }
    }
    if font_refs.contains(&handle) {
        return "font-program";
    }
    let incoming = incoming_roles.get(&handle);
    if incoming.is_some_and(|roles| roles.contains("page-content")) {
        return "page-content";
    }
    if icc_profile_refs.contains(&handle) {
        return "icc-profile";
    }
    if let Some(roles) = incoming {
        if roles.len() == 1 {
            return roles.iter().next().copied().unwrap_or("other");
        }
        if roles.len() > 1 {
            return "multi-reference";
        }
    }
    if facts.kind.as_deref() == Some(b"CMap") {
        return "cmap";
    }
    // Several common stream classes are identified by their defining dictionary
    // keys rather than `/Type` or `/Subtype`. Keeping them out of the generic
    // `other` bucket makes corpus byte anatomy actionable.
    let contains = |needle: &[u8]| dictionary.iter().any(|(key, _)| key.as_ref() == needle);
    if contains(b"PatternType") {
        return "pattern";
    }
    if contains(b"ShadingType") {
        return "shading";
    }
    if contains(b"FunctionType") {
        return "function";
    }
    if contains(b"Width") && contains(b"Height") && contains(b"BitsPerComponent") {
        return "image-like";
    }
    "other"
}

fn structural_object_role(
    object: &RoleValue,
    stream_role_name: Option<&str>,
    facts: Option<&DeepObjectFacts>,
) -> Result<String> {
    if matches!(object, RoleValue::Stream(_)) {
        return Ok(format!("stream:{}", stream_role_name.unwrap_or("other")));
    }
    if object.as_dictionary().is_some() {
        let Some(facts) = facts else {
            return Err(crate::Error::Invalid(
                "deep analysis dictionary facts are missing".to_owned(),
            ));
        };
        return Ok(match (&facts.kind, &facts.subtype) {
            (Some(kind), Some(subtype)) => format!(
                "dict:{}/{}",
                String::from_utf8_lossy(kind),
                String::from_utf8_lossy(subtype)
            ),
            (Some(kind), None) => format!("dict:{}", String::from_utf8_lossy(kind)),
            (None, Some(subtype)) => {
                format!("dict:subtype:{}", String::from_utf8_lossy(subtype))
            }
            (None, None) => "dict:untyped".to_owned(),
        });
    }
    Ok(match object {
        RoleValue::Array(_) => "array".to_owned(),
        RoleValue::Name => "name".to_owned(),
        RoleValue::String => "string".to_owned(),
        RoleValue::Number => "number".to_owned(),
        RoleValue::Boolean => "boolean".to_owned(),
        RoleValue::Null => "null".to_owned(),
        RoleValue::Reference(_) => "reference".to_owned(),
        RoleValue::Dictionary(_) | RoleValue::Stream(_) => unreachable!(),
    })
}

fn record_role_inventory(
    objects: &[(CowObjectHandle, RoleValue)],
    deep_object_facts: &HashMap<CowObjectHandle, DeepObjectFacts>,
    font_refs: &HashSet<CowObjectHandle>,
    icc_profile_refs: &HashSet<CowObjectHandle>,
    incoming_roles: &HashMap<CowObjectHandle, HashSet<&'static str>>,
    out: &mut PdfAnalysis,
) -> Result<HashMap<CowObjectHandle, &'static str>> {
    let mut stream_roles = HashMap::new();
    let mut object_roles = HashMap::new();
    for (handle, object) in objects {
        let stream_role_name = if matches!(object, RoleValue::Stream(_)) {
            let Some(facts) = deep_object_facts.get(handle) else {
                return Err(crate::Error::Invalid(
                    "deep analysis stream facts are missing".to_owned(),
                ));
            };
            let role = stream_role(
                object,
                *handle,
                facts,
                font_refs,
                icc_profile_refs,
                incoming_roles,
            );
            let Some(raw_stream_bytes) = facts.raw_stream_bytes else {
                return Err(crate::Error::Invalid(
                    "deep analysis stream byte count is missing".to_owned(),
                ));
            };
            *out.stream_role_counts.entry(role.to_owned()).or_default() += 1;
            *out.stream_role_raw_bytes
                .entry(role.to_owned())
                .or_default() += raw_stream_bytes;
            stream_roles.insert(*handle, role);
            Some(role)
        } else {
            None
        };

        let object_role =
            structural_object_role(object, stream_role_name, deep_object_facts.get(handle))?;
        *out.object_role_counts
            .entry(object_role.clone())
            .or_default() += 1;
        object_roles.insert(*handle, object_role);
    }

    // Target roles must be complete before reference edges can be labelled, so
    // edge accounting remains a second pass over the immutable object snapshot.
    for (handle, object) in objects {
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let source_role = object_roles
            .get(handle)
            .cloned()
            .unwrap_or_else(|| "unknown".to_owned());
        for (key, value) in dictionary {
            let key_text = String::from_utf8_lossy(key);
            *out.object_key_counts
                .entry(format!("{source_role}/{key_text}"))
                .or_default() += 1;
            for target in value.references() {
                let target_role = object_roles.get(&target).map_or("missing", String::as_str);
                *out.object_reference_edge_counts
                    .entry(format!("{source_role}/{key_text}->{target_role}"))
                    .or_default() += 1;
            }
        }
    }
    Ok(stream_roles)
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
    representation: crate::page_representation::PageRepresentationSummary,
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
    mut classifier: Option<&mut crate::page_representation::PageClassifier<'_>>,
    resources: Option<&crate::OwnedDictionary>,
) -> Result<bool> {
    crate::content_stream::visit_instructions(content, |instruction| {
        let operator = &instruction.operator[..];
        if let (Some(classifier), Some(resources)) = (classifier.as_deref_mut(), resources) {
            let operands = crate::content_stream::instruction_operands(content, instruction);
            classifier.observe_operator(operator, &operands, resources)?;
        }
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

fn record_form_content_analysis(out: &mut ContentAnalysisStats, content: &[u8]) -> Result<()> {
    let mut form_stats = PageInstructionCounter::default();
    let incomplete = scan_content(
        content,
        &mut out.inline_count,
        &mut out.inline_bytes,
        &mut out.inline_payloads,
        Some(&mut form_stats),
        None,
        None,
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
    Ok(())
}

fn analyze_page_content(
    document: &EditDocument,
    pages: &[CowObjectHandle],
    out: &mut ContentAnalysisStats,
) -> Result<()> {
    for &page in pages {
        let content = page_content(document, page)?;
        let resources = page_resources(document, page)?.unwrap_or_default();
        let mut classifier = crate::page_representation::begin_page_classifier(document, page)?;
        let mut page_stats = PageInstructionCounter::default();
        let incomplete = scan_content(
            &content,
            &mut out.inline_count,
            &mut out.inline_bytes,
            &mut out.inline_payloads,
            Some(&mut page_stats),
            classifier.as_mut(),
            Some(&resources),
        )?;
        if let Some(classifier) = classifier {
            out.representation.record(classifier.finish(incomplete));
        } else {
            out.representation.record_unknown_page();
        }
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
    out.representation.finish_document();
    Ok(())
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
    raw: &[u8],
    raw_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
) {
    record_payload(raw_payloads, raw);
}

fn record_font_program_payload(
    raw: &[u8],
    decoded: Option<&[u8]>,
    deep: bool,
    out: &mut PdfAnalysis,
    raw_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
    decoded_payloads: &mut HashMap<[u8; 32], (usize, usize)>,
) {
    out.font_program_count += 1;
    out.font_program_bytes += raw.len();
    record_payload(raw_payloads, raw);
    if decoded.is_some_and(crate::font_subset::has_pdf_rendering_unused_tables) {
        out.font_rendering_table_candidate_count += 1;
    }
    if deep {
        record_payload(decoded_payloads, decoded.unwrap_or(raw));
    }
}

fn record_font_file2_opportunity(
    decoded: &[u8],
    sparse_skeletons: &mut HashMap<[u8; 32], usize>,
    material_cmap_bytes: &mut usize,
) {
    let unwrapped = crate::font_subset::unwrap_single_face_collection(decoded);
    let standalone = unwrapped.as_deref().unwrap_or(decoded);
    let stripped = crate::font_subset::has_pdf_rendering_unused_tables(standalone)
        .then(|| crate::font_subset::strip_pdf_unused_tables(standalone))
        .flatten();
    let rendering = stripped
        .as_ref()
        .map_or(standalone, |(bytes, _)| bytes.as_slice());
    if let Some(skeleton) = crate::font::sparse_cid_union_skeleton_hash(rendering) {
        *sparse_skeletons.entry(skeleton).or_default() += 1;
    }
    if let Some(cmap_bytes) = crate::font_subset::cidfont_type2_cmap_size(rendering)
        && cmap_bytes >= crate::font::MIN_CID_CMAP_DECODED_BYTES_PER_PROGRAM
    {
        *material_cmap_bytes = material_cmap_bytes.saturating_add(cmap_bytes);
    }
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

/// Internal optimizer analysis plus non-serialized source-handle hints.
pub struct OptimizationAnalysis {
    /// Public analysis/report data.
    pub report: PdfAnalysis,
    /// Indirect dictionaries referenced from source `/ExtGState` resource maps.
    pub ext_gstate_dictionary_handles: Vec<CowObjectHandle>,
}

pub fn analyze_document_for_optimization(
    input: &[u8],
    document: &EditDocument,
) -> Result<OptimizationAnalysis> {
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
    piece_info_roots: usize,
}

impl DirectKeyInventory {
    fn new() -> Self {
        Self {
            font_glyph_scope_inventory_complete: true,
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy)]
struct DirectPreflightMode {
    collect_refs: bool,
    collect_icc_profiles: bool,
    count_targeted_keys: bool,
}

struct DirectPreflightScanner<'a> {
    document: &'a EditDocument,
    refs: &'a mut PreflightRefs,
    inventory: &'a mut DirectKeyInventory,
}

impl DirectPreflightScanner<'_> {
    fn scan(&mut self, object: &OwnedObject, mode: DirectPreflightMode) -> Result<()> {
        match object {
            OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
                self.scan_dictionary(object, dictionary, mode)
            }
            OwnedObject::Array(values) => self.scan_array(values, mode),
            OwnedObject::Reference(_)
            | OwnedObject::Null
            | OwnedObject::Boolean(_)
            | OwnedObject::Integer(_)
            | OwnedObject::Real(_)
            | OwnedObject::Name(_)
            | OwnedObject::String(_) => Ok(()),
        }
    }

    fn scan_dictionary(
        &mut self,
        object: &OwnedObject,
        dictionary: &crate::OwnedDictionary,
        mode: DirectPreflightMode,
    ) -> Result<()> {
        if dictionary.contains_key(b"Subtype".as_slice()) {
            match dictionary_name(self.document, dictionary, b"Subtype") {
                Ok(Some(name)) if name == b"Type3" => {
                    self.inventory.type3_font_scopes =
                        self.inventory.type3_font_scopes.saturating_add(1);
                }
                Ok(_) => {}
                Err(_) => self.inventory.font_glyph_scope_inventory_complete = false,
            }
        }
        if matches!(object, OwnedObject::Stream { .. })
            && let Some(pattern_type) = dictionary.get(b"PatternType".as_slice())
        {
            match self.document.resolve_owned_value(pattern_type) {
                Ok(Some(OwnedObject::Integer(1))) => {
                    self.inventory.tiling_pattern_scopes =
                        self.inventory.tiling_pattern_scopes.saturating_add(1);
                }
                Ok(_) => {}
                Err(_) => self.inventory.font_glyph_scope_inventory_complete = false,
            }
        }

        if mode.count_targeted_keys {
            self.inventory.piece_info = self.inventory.piece_info.saturating_add(usize::from(
                dictionary.contains_key(b"PieceInfo".as_slice()),
            ));
            self.inventory.authoring_metadata = self
                .inventory
                .authoring_metadata
                .saturating_add(usize::from(dictionary.contains_key(b"Metadata".as_slice())));
            self.inventory.authoring_metadata =
                self.inventory
                    .authoring_metadata
                    .saturating_add(usize::from(
                        dictionary.contains_key(b"LastModified".as_slice()),
                    ));
            self.inventory.to_unicode = self.inventory.to_unicode.saturating_add(usize::from(
                dictionary.contains_key(b"ToUnicode".as_slice()),
            ));
            self.inventory.ext_gstate = self.inventory.ext_gstate.saturating_add(usize::from(
                dictionary.contains_key(b"ExtGState".as_slice()),
            ));
            self.inventory.char_procs = self.inventory.char_procs.saturating_add(usize::from(
                dictionary.contains_key(b"CharProcs".as_slice()),
            ));
            self.inventory.appearance = self
                .inventory
                .appearance
                .saturating_add(usize::from(dictionary.contains_key(b"AP".as_slice())));
        }

        for (key, value) in dictionary {
            let child_collect_refs = mode.collect_refs
                && if matches!(key.as_slice(), b"FontFile" | b"FontFile2" | b"FontFile3") {
                    if let OwnedObject::Reference(handle) = value {
                        self.refs.font.insert(*handle);
                        if key.as_slice() == b"FontFile2" {
                            self.refs.font_file2.insert(*handle);
                        }
                    }
                    false
                } else if key.as_slice() == b"ToUnicode" {
                    if let OwnedObject::Reference(handle) = value {
                        self.refs.to_unicode.insert(*handle);
                    }
                    false
                } else if key.as_slice() == b"CIDToGIDMap" {
                    if let OwnedObject::Reference(handle) = value {
                        self.refs.cid_to_gid.insert(*handle);
                    }
                    false
                } else if key.as_slice() == b"ExtGState" {
                    collect_ext_g_state_map_refs(value, self.refs);
                    false
                } else {
                    !matches!(value, OwnedObject::Reference(_))
                };
            if !matches!(value, OwnedObject::Reference(_)) {
                self.scan(
                    value,
                    DirectPreflightMode {
                        collect_refs: child_collect_refs,
                        ..mode
                    },
                )?;
            }
        }
        Ok(())
    }

    fn scan_array(&mut self, values: &[OwnedObject], mode: DirectPreflightMode) -> Result<()> {
        let mut child_collect_icc_profiles = mode.collect_icc_profiles;
        let mut resolved_icc_based = None;
        if mode.collect_icc_profiles && values.len() >= 2 {
            let is_icc_based = matches!(
                self.document.resolve_owned_value(&values[0])?,
                Some(OwnedObject::Name(name)) if name == b"ICCBased"
            );
            resolved_icc_based = Some(is_icc_based);
            if is_icc_based {
                if let Some(OwnedObject::Reference(handle)) = values.get(1) {
                    self.refs.icc_profiles.insert(*handle);
                }
                // ICCBased arrays are terminal for profile-reference discovery.
                child_collect_icc_profiles = false;
            }
        }

        let mut count_child_targeted_keys = mode.count_targeted_keys;
        if mode.count_targeted_keys {
            let icc_status = resolved_icc_based.or_else(|| {
                values
                    .first()
                    .map(|first| match self.document.resolve_owned_value(first) {
                        Ok(Some(OwnedObject::Name(name))) => name == b"ICCBased",
                        Ok(_) => false,
                        Err(_) => true,
                    })
            });
            if icc_status == Some(true) && matches!(values.get(1), Some(OwnedObject::Reference(_)))
            {
                self.inventory.icc_based_arrays = self.inventory.icc_based_arrays.saturating_add(1);
                // Preserve the targeted-key inventory's ICCBased terminal semantics,
                // but continue the physical walk for reference and glyph-scope proofs.
                count_child_targeted_keys = false;
            }
        }
        for value in values {
            if !matches!(value, OwnedObject::Reference(_)) {
                self.scan(
                    value,
                    DirectPreflightMode {
                        collect_icc_profiles: child_collect_icc_profiles,
                        count_targeted_keys: count_child_targeted_keys,
                        ..mode
                    },
                )?;
            }
        }
        Ok(())
    }
}

fn scan_direct_preflight_root(
    document: &EditDocument,
    object: &OwnedObject,
    refs: &mut PreflightRefs,
    inventory: &mut DirectKeyInventory,
    collect_icc_profiles: bool,
) -> Result<()> {
    let piece_info_before = inventory.piece_info;
    DirectPreflightScanner {
        document,
        refs,
        inventory,
    }
    .scan(
        object,
        DirectPreflightMode {
            collect_refs: true,
            collect_icc_profiles,
            count_targeted_keys: true,
        },
    )?;
    inventory.piece_info_roots = inventory
        .piece_info_roots
        .saturating_add(usize::from(inventory.piece_info > piece_info_before));
    Ok(())
}

fn record_content_analysis(out: &mut PdfAnalysis, content: &ContentAnalysisStats) {
    out.page_representation_inventory_version = 1;
    out.document_representation = content.representation.document;
    out.raster_page_count = content.representation.raster_pages;
    out.native_page_count = content.representation.native_pages;
    out.mixed_page_count = content.representation.mixed_pages;
    out.blank_page_count = content.representation.blank_pages;
    out.unknown_page_count = content.representation.unknown_pages;
    out.page_representation_unknown_reasons = content
        .representation
        .unknown_reasons
        .iter()
        .map(|(&reason, &count)| (reason.to_owned(), count))
        .collect();
    let (duplicate_groups, duplicate_wasted_bytes) =
        duplicate_payload_stats(&content.inline_payloads);
    out.inline_image_count = content.inline_count;
    out.inline_image_bytes = content.inline_bytes;
    out.duplicate_inline_image_payload_groups = duplicate_groups;
    out.duplicate_inline_image_payload_wasted_bytes = duplicate_wasted_bytes;
    out.page_content_instruction_count = content.page_instruction_count;
    out.page_content_max_instructions_per_page = content.page_max_instructions;
    out.page_content_path_construction_operator_count = content.page_path_construction_count;
    out.page_content_max_path_construction_operators_per_page = content.page_max_path_construction;
    out.page_content_paint_operator_count = content.page_paint_count;
    out.page_content_max_paint_operators_per_page = content.page_max_paints;
    out.page_content_xobject_paint_operator_count = content.page_xobject_paint_count;
    out.page_content_max_xobject_paints_per_page = content.page_max_xobject_paints;
    out.page_content_text_show_operator_count = content.page_text_show_count;
    out.page_content_max_text_show_operators_per_page = content.page_max_text_shows;
    out.page_content_incomplete_parse_pages = content.page_incomplete_parse_count;
    out.form_content_instruction_count = content.form_instruction_count;
    out.form_content_max_instructions_per_form = content.form_max_instructions;
    out.form_content_path_construction_operator_count = content.form_path_construction_count;
    out.form_content_max_path_construction_operators_per_form = content.form_max_path_construction;
    out.form_content_paint_operator_count = content.form_paint_count;
    out.form_content_max_paint_operators_per_form = content.form_max_paints;
    out.form_content_incomplete_parse_forms = content.form_incomplete_parse_count;
}

fn analyze_pdf_impl(input: &[u8], deep: bool, content_density: bool) -> Result<PdfAnalysis> {
    let document = EditDocument::from_bytes(input.to_vec())?;
    Ok(analyze_document_impl(input, &document, deep, content_density)?.report)
}

// Exact deep-analysis recompression estimates can run independently. Keep decoded
// buffers bounded, and use the same whole-stream encoder as PDF optimization.
struct FlateEstimate {
    decoded: Vec<u8>,
    raw_len: usize,
    non_image: bool,
}

#[derive(Default)]
struct DeepFlateEstimator {
    pending: Vec<FlateEstimate>,
    pending_bytes: usize,
    encoder: Option<crate::stream_codec::FlateBatchEncoder>,
    encoder_unavailable: bool,
}

impl DeepFlateEstimator {
    const MAX_BATCH_BYTES: usize = 32 * 1024 * 1024;
    // Allow two independent very large streams to share the existing worker
    // pool. The decoded working set remains bounded; a single stream exceeding
    // the limit is still processed by itself.
    const MAX_LARGE_BATCH_BYTES: usize = 576 * 1024 * 1024;
    // Do not retain a giant decoded buffer while the next stream is decoded:
    // its size is unknown until decompression completes, and two such buffers
    // can exceed the memory required by the original serial implementation.
    const MAX_STAGED_LARGE_BYTES: usize = 128 * 1024 * 1024;
    const MAX_BATCH_STREAMS: usize = 64;

    fn queue(&mut self, decoded: Vec<u8>, raw_len: usize, non_image: bool, out: &mut PdfAnalysis) {
        let incoming_large = decoded.len() >= Self::MAX_BATCH_BYTES;
        let pending_large = self
            .pending
            .iter()
            .any(|job| job.decoded.len() >= Self::MAX_BATCH_BYTES);
        let limit = if incoming_large || pending_large {
            Self::MAX_LARGE_BATCH_BYTES
        } else {
            Self::MAX_BATCH_BYTES
        };
        if self.pending_bytes.saturating_add(decoded.len()) > limit {
            self.flush(out);
        }
        self.pending_bytes = self.pending_bytes.saturating_add(decoded.len());
        self.pending.push(FlateEstimate {
            decoded,
            raw_len,
            non_image,
        });

        let mut large_count = 0;
        let mut largest_large_bytes = 0_usize;
        let mut small_bytes = 0_usize;
        for job in &self.pending {
            if job.decoded.len() >= Self::MAX_BATCH_BYTES {
                large_count += 1;
                largest_large_bytes = largest_large_bytes.max(job.decoded.len());
            } else {
                small_bytes = small_bytes.saturating_add(job.decoded.len());
            }
        }
        if self.pending.len() >= Self::MAX_BATCH_STREAMS
            || (large_count == 0 && self.pending_bytes >= Self::MAX_BATCH_BYTES)
            || (large_count > 0
                && (large_count >= 2
                    || largest_large_bytes > Self::MAX_STAGED_LARGE_BYTES
                    || small_bytes >= Self::MAX_BATCH_BYTES
                    || self.pending_bytes >= Self::MAX_LARGE_BATCH_BYTES))
        {
            self.flush(out);
        }
    }

    fn flush(&mut self, out: &mut PdfAnalysis) {
        if self.pending.is_empty() {
            return;
        }
        if self.pending.len() > 1 && self.encoder.is_none() && !self.encoder_unavailable {
            match crate::stream_codec::FlateBatchEncoder::new(
                crate::Config::default().flate_workers,
            ) {
                Ok(encoder) => self.encoder = Some(encoder),
                Err(_) => self.encoder_unavailable = true,
            }
        }

        // The executor returns one error if any job fails. Scalar fallback
        // retains the old independent, best-effort semantics for each stream.
        let inputs = self
            .pending
            .iter()
            .map(|job| job.decoded.as_slice())
            .collect::<Vec<_>>();
        // Only output sizes that meet both saving thresholds can change the
        // report. The cap is exact: saving >= max(1024, ceil(raw_len / 20)).
        // For highly compressible inputs this avoids allocating an output
        // buffer hundreds of megabytes larger than the source PDF stream.
        let maximum_lengths = self
            .pending
            .iter()
            .map(|job| {
                job.raw_len
                    .saturating_sub(1024.max(job.raw_len.div_ceil(20)))
            })
            .collect::<Vec<_>>();
        let lengths = self
            .encoder
            .as_ref()
            .and_then(|encoder| {
                encoder
                    .encode_many_capped_lengths(&inputs, &maximum_lengths, crate::FlateLevel::SIZE)
                    .ok()
            })
            .unwrap_or_else(|| {
                inputs
                    .iter()
                    .zip(&maximum_lengths)
                    .map(|(&decoded, &maximum_length)| {
                        crate::stream_codec::flate_capped_encoded_len(
                            decoded,
                            maximum_length,
                            crate::FlateLevel::SIZE,
                        )
                        .unwrap_or_else(|_| {
                            crate::stream_codec::encode_flate(decoded, crate::FlateLevel::SIZE)
                                .ok()
                                .map(|output| output.len())
                        })
                    })
                    .collect()
            });
        drop(inputs);
        for (job, encoded_len) in self.pending.drain(..).zip(lengths) {
            let Some(encoded_len) = encoded_len else {
                continue;
            };
            let saving = job.raw_len.saturating_sub(encoded_len);
            if saving >= 1024 && saving * 100 >= job.raw_len.saturating_mul(5) {
                out.flate_recompress_candidate_count += 1;
                out.flate_recompress_potential_saving_bytes += saving;
                if job.non_image {
                    out.non_image_flate_recompress_candidate_count += 1;
                    out.non_image_flate_recompress_potential_saving_bytes += saving;
                }
            }
        }
        self.pending_bytes = 0;
    }
}

struct ObjectAnalysisAccumulator {
    flate_estimates: DeepFlateEstimator,
    deep: bool,
    content_density_reachable_handles: HashSet<CowObjectHandle>,
    content_analysis: Option<ContentAnalysisStats>,
    form_content_analysis_error: Option<String>,
    risks: BTreeMap<RiskKind, (usize, String)>,
    stream_payloads: HashMap<[u8; 32], (usize, usize)>,
    stream_payload_members: HashMap<[u8; 32], Vec<CowObjectHandle>>,
    metadata_payloads: HashMap<[u8; 32], (usize, usize)>,
    image_payloads: HashMap<[u8; 32], (usize, usize)>,
    form_payloads: HashMap<[u8; 32], (usize, usize)>,
    font_payloads: HashMap<[u8; 32], (usize, usize)>,
    font_raw_payloads: HashMap<[u8; 32], (usize, usize)>,
    to_unicode_raw_payloads: HashMap<[u8; 32], (usize, usize)>,
    font_dictionary_fingerprints: HashMap<[u8; 32], usize>,
    font_dictionary_fingerprint_inventory_complete: bool,
    deep_object_facts: HashMap<CowObjectHandle, DeepObjectFacts>,
    processed_font_refs: HashSet<CowObjectHandle>,
    processed_font_file2_refs: HashSet<CowObjectHandle>,
    processed_to_unicode_refs: HashSet<CowObjectHandle>,
    font_sparse_skeletons: HashMap<[u8; 32], usize>,
    font_material_cmap_bytes: usize,
}

impl ObjectAnalysisAccumulator {
    fn new(
        deep: bool,
        content_density: bool,
        content_density_reachable_handles: HashSet<CowObjectHandle>,
    ) -> Self {
        Self {
            deep,
            flate_estimates: DeepFlateEstimator::default(),
            content_density_reachable_handles,
            content_analysis: content_density.then(ContentAnalysisStats::default),
            form_content_analysis_error: None,
            risks: BTreeMap::new(),
            stream_payloads: HashMap::new(),
            stream_payload_members: HashMap::new(),
            metadata_payloads: HashMap::new(),
            image_payloads: HashMap::new(),
            form_payloads: HashMap::new(),
            font_payloads: HashMap::new(),
            font_raw_payloads: HashMap::new(),
            to_unicode_raw_payloads: HashMap::new(),
            font_dictionary_fingerprints: HashMap::new(),
            font_dictionary_fingerprint_inventory_complete: true,
            deep_object_facts: HashMap::new(),
            processed_font_refs: HashSet::new(),
            processed_font_file2_refs: HashSet::new(),
            processed_to_unicode_refs: HashSet::new(),
            font_sparse_skeletons: HashMap::new(),
            font_material_cmap_bytes: 0,
        }
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one object is observed once and contributes several coupled analysis inventories"
    )]
    fn analyze_object(
        &mut self,
        document: &EditDocument,
        out: &mut PdfAnalysis,
        handle: CowObjectHandle,
        object: &OwnedObject,
        preflight_refs: &PreflightRefs,
    ) -> Result<()> {
        let Some(dictionary) = object.as_dictionary() else {
            return Ok(());
        };
        let type_name = dictionary_name(document, dictionary, b"Type");
        if matches!(
            dictionary.get(b"A".as_slice()),
            Some(OwnedObject::Reference(_))
        ) {
            let may_be_struct_elem = match type_name.as_ref() {
                Ok(Some(name)) => name.as_slice() == b"StructElem",
                Ok(None) => false,
                Err(_) => true,
            };
            if may_be_struct_elem {
                out.structure_attribute_ref_count =
                    out.structure_attribute_ref_count.saturating_add(1);
            }
        }
        match type_name.as_ref() {
            Ok(Some(name)) if name.as_slice() == b"Font" => {
                match exact_dictionary_fingerprint(object, b"exact-font-dictionary") {
                    Ok(Some(fingerprint)) => {
                        *self
                            .font_dictionary_fingerprints
                            .entry(fingerprint)
                            .or_default() += 1;
                    }
                    Ok(None) => {}
                    Err(_) => self.font_dictionary_fingerprint_inventory_complete = false,
                }
            }
            Ok(_) => {}
            Err(_) => self.font_dictionary_fingerprint_inventory_complete = false,
        }
        let is_stream = matches!(object, OwnedObject::Stream { .. });
        if dictionary.contains_key(b"Thumb".as_slice()) {
            push_risk(
                &mut self.risks,
                RiskKind::Thumbnail,
                1,
                "page thumbnail stream is separate hidden raster content",
            );
        }
        if dictionary.contains_key(b"AA".as_slice())
            || dictionary.contains_key(b"OpenAction".as_slice())
        {
            push_risk(
                &mut self.risks,
                RiskKind::AutomaticAction,
                1,
                "automatic PDF actions are executable/interactive hidden state",
            );
        }
        if dictionary.contains_key(b"OCProperties".as_slice()) {
            push_risk(
                &mut self.risks,
                RiskKind::HiddenLayer,
                1,
                "optional-content groups can hide content from the default view",
            );
        }
        if dictionary.contains_key(b"ActualText".as_slice()) {
            push_risk(
                &mut self.risks,
                RiskKind::SuspiciousHiddenText,
                1,
                "ActualText may differ from visible glyphs and should be retained but audited in privacy mode",
            );
        }
        if dictionary.contains_key(b"V".as_slice()) && dictionary.contains_key(b"FT".as_slice()) {
            push_risk(
                &mut self.risks,
                RiskKind::FormValue,
                1,
                "interactive form field contains a stored value",
            );
        }
        if !is_stream {
            if self.deep {
                self.deep_object_facts.insert(
                    handle,
                    DeepObjectFacts {
                        kind: type_name?,
                        subtype: dictionary_name(document, dictionary, b"Subtype")?,
                        raw_stream_bytes: None,
                    },
                );
            }
            return Ok(());
        }

        let may_be_pattern = match type_name.as_ref() {
            Ok(Some(name)) => name.as_slice() == b"Pattern",
            Ok(None) => false,
            Err(_) => true,
        };
        if may_be_pattern {
            out.pattern_stream_count = out.pattern_stream_count.saturating_add(1);
        }
        let raw = raw_stream_bytes(document, object)?;
        out.stream_count += 1;
        out.stream_raw_bytes += raw.len();
        if self.deep {
            let hash = record_payload(&mut self.stream_payloads, raw.as_ref());
            self.stream_payload_members
                .entry(hash)
                .or_default()
                .push(handle);
        }

        if preflight_refs.to_unicode.contains(&handle) {
            record_to_unicode_stream_payload(raw.as_ref(), &mut self.to_unicode_raw_payloads);
            self.processed_to_unicode_refs.insert(handle);
        }

        let filter = filter_name(document, dictionary)?;
        *out.filter_counts.entry(filter.clone()).or_default() += 1;

        let subtype = dictionary_name(document, dictionary, b"Subtype")?;
        let is_image = subtype.as_deref() == Some(b"Image");
        let is_form = subtype.as_deref() == Some(b"Form");
        let is_font = preflight_refs.font.contains(&handle);
        let is_content_density_form =
            is_form && self.content_density_reachable_handles.contains(&handle);
        let decoded = if is_font
            || (self.deep && (is_form || filter == "/FlateDecode"))
            || is_content_density_form
        {
            match document.decoded_owned_stream_data(object) {
                Ok(decoded) => Some(decoded),
                Err(error) => {
                    if is_content_density_form && self.form_content_analysis_error.is_none() {
                        self.form_content_analysis_error = Some(error.to_string());
                    }
                    None
                }
            }
        } else {
            None
        };
        if is_font {
            record_font_program_payload(
                raw.as_ref(),
                decoded.as_deref(),
                self.deep,
                out,
                &mut self.font_raw_payloads,
                &mut self.font_payloads,
            );
            self.processed_font_refs.insert(handle);
            if preflight_refs.font_file2.contains(&handle) {
                if let Some(decoded) = decoded.as_deref() {
                    record_font_file2_opportunity(
                        decoded,
                        &mut self.font_sparse_skeletons,
                        &mut self.font_material_cmap_bytes,
                    );
                }
                self.processed_font_file2_refs.insert(handle);
            }
        }
        if is_image {
            out.image_count += 1;
            out.image_raw_bytes += raw.len();
            if self.deep {
                record_payload(&mut self.image_payloads, raw.as_ref());
            }
            if filter.contains("DCTDecode") {
                let marker_hits = count_bytes(raw.as_ref(), &[0xff, 0xe1])
                    + count_bytes(raw.as_ref(), &[0xff, 0xed])
                    + count_bytes(raw.as_ref(), &[0xff, 0xfe]);
                push_risk(
                    &mut self.risks,
                    RiskKind::JpegMetadata,
                    marker_hits,
                    "embedded JPEG carries EXIF/XMP/IPTC/comment marker candidates",
                );
            }
        }
        if is_form {
            out.form_xobject_count += 1;
            if self.deep {
                record_payload(
                    &mut self.form_payloads,
                    decoded.as_deref().unwrap_or_else(|| raw.as_ref()),
                );
            }
            if is_content_density_form
                && self.form_content_analysis_error.is_none()
                && let Some(decoded) = decoded.as_deref()
                && let Some(content_analysis) = self.content_analysis.as_mut()
                && let Err(error) = record_form_content_analysis(content_analysis, decoded)
            {
                self.form_content_analysis_error = Some(error.to_string());
            }
        }
        let type_name = type_name?;
        if type_name.as_deref() == Some(b"Metadata") {
            out.metadata_stream_count += 1;
            out.metadata_stream_bytes += raw.len();
            if self.deep {
                record_payload(&mut self.metadata_payloads, raw.as_ref());
            }
            push_risk(
                &mut self.risks,
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
            // A source payload below 1,024 bytes cannot save 1,024 bytes,
            // regardless of the result of recompression.
            if self.deep
                && raw.len() >= 1024
                && let Some(decoded) = decoded
            {
                self.flate_estimates
                    .queue(decoded, raw.len(), !is_image, out);
            }
        }
        if self.deep {
            self.deep_object_facts.insert(
                handle,
                DeepObjectFacts {
                    kind: type_name,
                    subtype,
                    raw_stream_bytes: Some(raw.len()),
                },
            );
        }
        Ok(())
    }

    fn complete_preflight_targets(
        &mut self,
        document: &EditDocument,
        out: &mut PdfAnalysis,
        preflight_refs: &PreflightRefs,
    ) -> Result<()> {
        if self.deep {
            return Ok(());
        }

        for handle in preflight_refs
            .to_unicode
            .difference(&self.processed_to_unicode_refs)
            .copied()
        {
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            if !matches!(object, OwnedObject::Stream { .. }) {
                continue;
            }
            let raw = raw_stream_bytes(document, &object)?;
            record_to_unicode_stream_payload(raw.as_ref(), &mut self.to_unicode_raw_payloads);
        }
        for handle in preflight_refs
            .font
            .difference(&self.processed_font_refs)
            .copied()
        {
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            if !matches!(object, OwnedObject::Stream { .. }) {
                continue;
            }
            let raw = raw_stream_bytes(document, &object)?;
            let decoded = document.decoded_owned_stream_data(&object).ok();
            record_font_program_payload(
                raw.as_ref(),
                decoded.as_deref(),
                false,
                out,
                &mut self.font_raw_payloads,
                &mut self.font_payloads,
            );
            if preflight_refs.font_file2.contains(&handle) {
                if let Some(decoded) = decoded.as_deref() {
                    record_font_file2_opportunity(
                        decoded,
                        &mut self.font_sparse_skeletons,
                        &mut self.font_material_cmap_bytes,
                    );
                }
                self.processed_font_file2_refs.insert(handle);
            }
        }
        // A target can be discovered as a generic FontFile before a later holder
        // identifies the same stream specifically as FontFile2. Revisit only
        // those File2 targets whose opportunity inventory was not recorded yet.
        for handle in preflight_refs
            .font_file2
            .difference(&self.processed_font_file2_refs)
            .copied()
        {
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            if let Ok(decoded) = document.decoded_owned_stream_data(&object) {
                record_font_file2_opportunity(
                    &decoded,
                    &mut self.font_sparse_skeletons,
                    &mut self.font_material_cmap_bytes,
                );
            }
        }
        Ok(())
    }

    fn finalize_object_inventory(
        &mut self,
        document: &EditDocument,
        out: &mut PdfAnalysis,
        direct_key_inventory: &DirectKeyInventory,
        preflight_refs: &mut PreflightRefs,
        objects: Option<&[(CowObjectHandle, RoleValue)]>,
        page_content_role_pages: &HashSet<CowObjectHandle>,
    ) -> Result<Vec<CowObjectHandle>> {
        out.piece_info_key_count = direct_key_inventory.piece_info;
        out.authoring_metadata_key_count = direct_key_inventory.authoring_metadata;
        out.to_unicode_key_count = direct_key_inventory.to_unicode;
        out.ext_gstate_key_count = direct_key_inventory.ext_gstate;
        out.char_procs_key_count = direct_key_inventory.char_procs;
        out.appearance_key_count = direct_key_inventory.appearance;
        out.icc_based_array_count = direct_key_inventory.icc_based_arrays;
        out.type3_font_scope_count = direct_key_inventory.type3_font_scopes;
        out.tiling_pattern_scope_count = direct_key_inventory.tiling_pattern_scopes;
        push_risk(
            &mut self.risks,
            RiskKind::PieceInfo,
            direct_key_inventory.piece_info_roots,
            "Adobe/private-piece metadata can contain authoring provenance and source paths",
        );

        resolve_ext_g_state_map_refs(document, preflight_refs)?;
        let mut ext_gstate_dictionary_handles: Vec<_> =
            preflight_refs.ext_gstates.iter().copied().collect();
        ext_gstate_dictionary_handles.sort_unstable();

        self.complete_preflight_targets(document, out, preflight_refs)?;

        out.font_opportunity_inventory_version = 1;
        out.font_sparse_union_duplicate_skeleton_groups = self
            .font_sparse_skeletons
            .values()
            .filter(|&&count| count > 1)
            .count();
        out.font_material_cid_cmap_decoded_bytes = self.font_material_cmap_bytes;
        out.font_explicit_cid_to_gid_ref_count = preflight_refs.cid_to_gid.len();
        out.font_glyph_scope_inventory_version =
            u8::from(direct_key_inventory.font_glyph_scope_inventory_complete);
        out.font_dictionary_fingerprint_inventory_version =
            u8::from(self.font_dictionary_fingerprint_inventory_complete);
        out.font_dictionary_duplicate_fingerprint_groups = self
            .font_dictionary_fingerprints
            .values()
            .filter(|&&count| count > 1)
            .count();
        (out.font_raw_duplicate_payload_groups, _) =
            duplicate_payload_stats(&self.font_raw_payloads);
        (out.to_unicode_raw_duplicate_payload_groups, _) =
            duplicate_payload_stats(&self.to_unicode_raw_payloads);

        if self.deep {
            let objects = objects.ok_or_else(|| {
                crate::Error::Invalid("deep analysis object table is missing".to_owned())
            })?;
            let incoming_roles = collect_incoming_roles(objects, page_content_role_pages);
            let stream_roles = record_role_inventory(
                objects,
                &self.deep_object_facts,
                &preflight_refs.font,
                &preflight_refs.icc_profiles,
                &incoming_roles,
                out,
            )?;
            (
                out.duplicate_stream_role_groups,
                out.duplicate_stream_role_wasted_bytes,
            ) = duplicate_payload_role_stats(
                &self.stream_payloads,
                &self.stream_payload_members,
                &stream_roles,
            );
            (
                out.duplicate_metadata_payload_groups,
                out.duplicate_metadata_payload_wasted_bytes,
            ) = duplicate_payload_stats(&self.metadata_payloads);
            (
                out.duplicate_stream_payload_groups,
                out.duplicate_stream_payload_wasted_bytes,
            ) = duplicate_payload_stats(&self.stream_payloads);
            (
                out.duplicate_image_payload_groups,
                out.duplicate_image_payload_wasted_bytes,
            ) = duplicate_payload_stats(&self.image_payloads);
            (
                out.duplicate_form_payload_groups,
                out.duplicate_form_payload_wasted_bytes,
            ) = duplicate_payload_stats(&self.form_payloads);
            (
                out.duplicate_font_payload_groups,
                out.duplicate_font_payload_wasted_bytes,
            ) = duplicate_payload_stats(&self.font_payloads);
        }

        Ok(ext_gstate_dictionary_handles)
    }
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
) -> Result<OptimizationAnalysis> {
    let object_ids = deep.then(|| document.source().object_ids());
    let mut objects = object_ids
        .as_ref()
        .map(|ids| Vec::<(CowObjectHandle, RoleValue)>::with_capacity(ids.len()));
    let mut out = PdfAnalysis {
        input_bytes: input.len(),
        analysis_complete: deep,
        input_sha256: input_sha256(input),
        page_count: document.source().page_count(),
        object_count: document.source().object_count(),
        ..PdfAnalysis::default()
    };
    // Deep role analysis and content-density analysis are read-only and share
    // the same ordered page identity. Recover the page tree once when either
    // analysis needs it, while preserving each analysis' warning-on-failure behavior.
    let analysis_pages = (deep || content_density).then(|| document.page_handles());
    // Content-density Form metrics are output-graph metrics. Snapshot reachability
    // once, then let the authoritative object pass classify which live objects are Forms.
    let analysis_reachable_objects = content_density.then(|| document.reachable_output_objects());

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

    let page_content_role_pages = if deep {
        match analysis_pages.as_ref() {
            Some(Ok(pages)) => pages.iter().copied().collect(),
            Some(Err(error)) => {
                out.warnings
                    .push(format!("page-content role analysis skipped: {error}"));
                HashSet::new()
            }
            None => HashSet::new(),
        }
    } else {
        HashSet::new()
    };
    let content_density_reachable_handles: HashSet<_> =
        match (analysis_pages.as_ref(), analysis_reachable_objects.as_ref()) {
            (Some(Ok(_)), Some(Ok(handles))) => handles.iter().copied().collect(),
            _ => HashSet::new(),
        };
    let mut object_analysis =
        ObjectAnalysisAccumulator::new(deep, content_density, content_density_reachable_handles);
    let mut direct_key_inventory = DirectKeyInventory::new();
    let mut preflight_refs = PreflightRefs::default();
    if let Some(ids) = object_ids.as_deref() {
        let role_objects = objects
            .as_mut()
            .ok_or_else(|| crate::Error::Invalid("deep role graph missing".to_owned()))?;
        for &id in ids {
            let handle = CowObjectHandle::Existing(id);
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            scan_direct_preflight_root(
                document,
                &object,
                &mut preflight_refs,
                &mut direct_key_inventory,
                true,
            )?;
            role_objects.push((handle, RoleValue::from_object(&object)));
        }
        // The source xref inventory can change as lazy object-stream entries
        // are materialized. Preserve the original deep-analysis observation
        // point, after all indexed source objects have been inspected.
        out.object_count = document.source().object_count();
    }
    out.preservation_key_inventory_complete = true;
    out.font_rendering_table_inventory_complete = true;
    out.font_raw_payload_inventory_version = 1;
    out.to_unicode_raw_payload_inventory_version = 1;
    out.targeted_dedup_key_inventory_version = 3;

    if let Some(objects) = objects.as_deref() {
        for (handle, _) in objects {
            let Some(object) = document.current_owned_object(*handle)? else {
                continue;
            };
            object_analysis.analyze_object(
                document,
                &mut out,
                *handle,
                &object,
                &preflight_refs,
            )?;
        }
    } else {
        for id in document.source().object_ids() {
            let handle = CowObjectHandle::Existing(id);
            let Some(object) = document.current_owned_object(handle)? else {
                continue;
            };
            scan_direct_preflight_root(
                document,
                &object,
                &mut preflight_refs,
                &mut direct_key_inventory,
                false,
            )?;
            object_analysis.analyze_object(document, &mut out, handle, &object, &preflight_refs)?;
        }
    }
    object_analysis.flate_estimates.flush(&mut out);
    let ext_gstate_dictionary_handles = object_analysis.finalize_object_inventory(
        document,
        &mut out,
        &direct_key_inventory,
        &mut preflight_refs,
        objects.as_deref(),
        &page_content_role_pages,
    )?;

    if content_density {
        let page_content_analysis_error = match analysis_pages.as_ref() {
            Some(Ok(pages)) => object_analysis
                .content_analysis
                .as_mut()
                .and_then(|content| {
                    analyze_page_content(document, pages, content)
                        .err()
                        .map(|error| error.to_string())
                }),
            Some(Err(error)) => Some(error.to_string()),
            None => None,
        };
        let reachability_error = analysis_reachable_objects
            .as_ref()
            .and_then(|handles| handles.as_ref().err())
            .map(ToString::to_string);
        if let Some(error) = page_content_analysis_error
            .or(reachability_error)
            .or(object_analysis.form_content_analysis_error)
        {
            out.warnings
                .push(format!("content-stream analysis skipped: {error}"));
        } else if let Some(content) = object_analysis.content_analysis {
            record_content_analysis(&mut out, &content);
        }
    }

    let startxrefs = count_bytes(input, b"startxref");
    out.incremental_update_count = startxrefs.saturating_sub(1);
    push_risk(
        &mut object_analysis.risks,
        RiskKind::IncrementalHistory,
        out.incremental_update_count,
        "incremental revisions can retain previous object bodies, old metadata, or pre-redaction content; a fresh rewrite drops the /Prev chain",
    );
    push_risk(
        &mut object_analysis.risks,
        RiskKind::InfoDictionary,
        count_bytes(input, b"/Info"),
        "document Info dictionary commonly stores author, creator, producer, and timestamps",
    );
    push_risk(
        &mut object_analysis.risks,
        RiskKind::EmbeddedFile,
        count_bytes(input, b"/EmbeddedFiles") + count_bytes(input, b"/EmbeddedFile"),
        "PDF can carry arbitrary attached files invisibly to normal page rendering",
    );
    push_risk(
        &mut object_analysis.risks,
        RiskKind::Javascript,
        count_bytes(input, b"/JavaScript") + count_bytes(input, b"/JS"),
        "document or annotation JavaScript is active hidden content",
    );
    push_risk(
        &mut object_analysis.risks,
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
                    &mut object_analysis.risks,
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

    out.risks = object_analysis
        .risks
        .into_iter()
        .map(|(kind, (count, note))| RiskFinding { kind, count, note })
        .collect();
    Ok(OptimizationAnalysis {
        report: out,
        ext_gstate_dictionary_handles,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ClassicPdfBuilder;

    #[test]
    fn packed_role_topology_preserves_nested_references_and_incoming_roles() {
        let page = CowObjectHandle::Existing(crate::ObjectId::new(3, 0));
        let contents = CowObjectHandle::Existing(crate::ObjectId::new(4, 0));
        let metadata = CowObjectHandle::Existing(crate::ObjectId::new(5, 0));
        let page_object = OwnedObject::Dictionary(BTreeMap::from([
            (b"Contents".to_vec(), OwnedObject::Reference(contents)),
            (b"Metadata".to_vec(), OwnedObject::Reference(metadata)),
            (
                b"Private".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Name(b"private".to_vec()),
                    OwnedObject::String(vec![7_u8; 128]),
                    OwnedObject::Integer(42),
                    OwnedObject::Reference(contents),
                ]),
            ),
        ]));
        let metadata_object = OwnedObject::Dictionary(BTreeMap::from([(
            b"Nested".to_vec(),
            OwnedObject::Array(vec![OwnedObject::Reference(contents)]),
        )]));
        let content_object = OwnedObject::Stream {
            dictionary: BTreeMap::from([(b"Length".to_vec(), OwnedObject::Reference(metadata))]),
            data: crate::StreamData::Owned(vec![0_u8; 1024]),
        };
        let objects = [
            (page, &page_object),
            (contents, &content_object),
            (metadata, &metadata_object),
        ];
        let compact = objects
            .iter()
            .map(|(handle, object)| {
                let role = RoleValue::from_object(object);
                assert_eq!(role.references(), object.references());
                assert_eq!(
                    role.as_dictionary().map(<[_]>::len),
                    object.as_dictionary().map(BTreeMap::len)
                );
                (*handle, role)
            })
            .collect::<Vec<_>>();
        let incoming = collect_incoming_roles(&compact, &HashSet::from([page]));
        assert_eq!(
            incoming
                .get(&contents)
                .map(|roles| roles.contains("page-content")),
            Some(true)
        );
        assert_eq!(
            incoming
                .get(&contents)
                .map(|roles| roles.contains("metadata")),
            Some(true)
        );
    }

    #[test]
    fn deep_flate_estimator_overlaps_two_moderately_large_jobs_but_flushes_giant() {
        let mut estimator = DeepFlateEstimator::default();
        let mut report = PdfAnalysis::default();
        estimator.queue(vec![0_u8; 35 * 1024 * 1024], 1_000_000, true, &mut report);
        assert_eq!(estimator.pending.len(), 1);
        estimator.queue(vec![1_u8; 36 * 1024 * 1024], 1_000_000, false, &mut report);
        assert!(estimator.pending.is_empty());
        assert_eq!(report.flate_recompress_candidate_count, 2);
        assert_eq!(report.non_image_flate_recompress_candidate_count, 1);

        // A giant must not survive to the next stream's decode, where its
        // retained buffer could make a formerly successful PDF run out of RAM.
        estimator.queue(vec![2_u8; 129 * 1024 * 1024], 2_000_000, true, &mut report);
        assert!(estimator.pending.is_empty());
        assert_eq!(estimator.pending_bytes, 0);
        assert_eq!(report.flate_recompress_candidate_count, 3);
    }

    #[test]
    fn deep_flate_estimate_batches_preserve_scalar_thresholds_and_classification() -> Result<()> {
        let mut estimator = DeepFlateEstimator::default();
        let mut actual = PdfAnalysis::default();
        let mut expected = PdfAnalysis::default();

        // More than two complete stream-count batches, with both eligible and
        // ineligible savings and a mix of image/non-image Flate streams.
        for index in 0..145_usize {
            let size = 4_096 + index * 327;
            let decoded = match index % 3 {
                0 => vec![0_u8; size],
                1 => (0_u8..=255).cycle().take(size).collect::<Vec<_>>(),
                _ => b"q 1 0 0 1 0 0 cm Q"
                    .iter()
                    .copied()
                    .cycle()
                    .take(size)
                    .collect(),
            };
            let scalar_len =
                crate::stream_codec::encode_flate(&decoded, crate::FlateLevel::SIZE)?.len();
            let raw_len = scalar_len
                + match index % 5 {
                    0 => 100,
                    1 => 1_023,
                    2 => 1_024,
                    3 => 2_500,
                    _ => 5_000,
                };
            let non_image = index % 2 == 0;
            let saving = raw_len.saturating_sub(scalar_len);
            if saving >= 1024 && saving * 100 >= raw_len.saturating_mul(5) {
                expected.flate_recompress_candidate_count += 1;
                expected.flate_recompress_potential_saving_bytes += saving;
                if non_image {
                    expected.non_image_flate_recompress_candidate_count += 1;
                    expected.non_image_flate_recompress_potential_saving_bytes += saving;
                }
            }
            estimator.queue(decoded, raw_len, non_image, &mut actual);
        }
        estimator.flush(&mut actual);

        assert_eq!(
            actual.flate_recompress_candidate_count,
            expected.flate_recompress_candidate_count
        );
        assert_eq!(
            actual.flate_recompress_potential_saving_bytes,
            expected.flate_recompress_potential_saving_bytes
        );
        assert_eq!(
            actual.non_image_flate_recompress_candidate_count,
            expected.non_image_flate_recompress_candidate_count
        );
        assert_eq!(
            actual.non_image_flate_recompress_potential_saving_bytes,
            expected.non_image_flate_recompress_potential_saving_bytes
        );
        assert!(estimator.pending.is_empty());
        assert_eq!(estimator.pending_bytes, 0);
        Ok(())
    }

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
    fn preflight_extgstate_inventory_collects_only_indirect_resource_entries() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ExtGState << /GS1 5 0 R /GS2 6 0 R /Inline1 << /CA 0.25 >> /Inline2 << /CA 0.25 >> >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /CA 0.5 /ca 0.5 >>")?;
        pdf.object(6, b"<< /CA 0.5 /ca 0.5 >>")?;

        let input = pdf.finish(1)?;
        let document = EditDocument::from_bytes(input.clone())?;
        let analysis = analyze_document_for_optimization(&input, &document)?;
        assert_eq!(analysis.ext_gstate_dictionary_handles.len(), 2);
        Ok(())
    }

    #[test]
    fn preflight_extgstate_inventory_recovers_targets_before_indirect_resource_map() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ExtGState 9 0 R >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /BM /Multiply /CA 0.5 >>")?;
        pdf.object(6, b"<< /BM /Multiply /CA 0.5 >>")?;
        pdf.object(9, b"<< /GS1 5 0 R /GS2 6 0 R >>")?;

        let input = pdf.finish(1)?;
        let document = EditDocument::from_bytes(input.clone())?;
        let analysis = analyze_document_for_optimization(&input, &document)?;
        assert_eq!(analysis.ext_gstate_dictionary_handles.len(), 2);
        Ok(())
    }

    #[test]
    fn preflight_extgstate_inventory_ignores_duplicate_inline_entries() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ExtGState << /GS1 << /CA 0.5 >> /GS2 << /CA 0.5 >> >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;

        let input = pdf.finish(1)?;
        let document = EditDocument::from_bytes(input.clone())?;
        let analysis = analyze_document_for_optimization(&input, &document)?;
        assert_eq!(analysis.ext_gstate_dictionary_handles.len(), 0);
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
        builder.add_raw(Tag::new(b"maxp"), vec![0_u8; 6]);
        builder.add_raw(Tag::new(b"hhea"), vec![0_u8; 36]);
        builder.add_raw(Tag::new(b"hmtx"), vec![0_u8; 4]);
        builder.add_raw(Tag::new(b"loca"), vec![0_u8; 8]);
        builder.add_raw(Tag::new(b"glyf"), b"glyphs".to_vec());
        builder.add_raw(Tag::new(b"cmap"), vec![0_u8; 2 * 1024]);
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
        assert_eq!(analysis.font_opportunity_inventory_version, 1);
        assert_eq!(analysis.font_sparse_union_duplicate_skeleton_groups, 1);
        assert_eq!(analysis.font_material_cid_cmap_decoded_bytes, 4 * 1024);
        assert_eq!(analysis.font_explicit_cid_to_gid_ref_count, 0);
        Ok(())
    }

    #[test]
    fn preflight_font_opportunity_inventory_finds_explicit_cid_to_gid_refs() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Private << /CIDToGIDMap 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"map")?;

        let analysis = analyze_pdf_preflight(&pdf.finish(1)?)?;
        assert_eq!(analysis.font_opportunity_inventory_version, 1);
        assert_eq!(analysis.font_explicit_cid_to_gid_ref_count, 1);
        Ok(())
    }

    #[test]
    fn deep_analysis_seeds_page_content_role_from_page_objects() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Private << /Contents 8 0 R /ColorSpace [/ICCBased 4 0 R] >> >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 7 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(6, b"", b"q Q")?;
        pdf.object(7, b"[4 0 R 6 0 R]")?;
        pdf.stream(8, b"", b"q Q")?;

        let analysis = analyze_pdf(&pdf.finish(1)?)?;
        assert_eq!(analysis.stream_role_counts.get("page-content"), Some(&2));
        assert_eq!(analysis.stream_role_counts.get("icc-profile"), None);
        assert_eq!(analysis.stream_role_counts.get("other"), Some(&1));
        assert_eq!(analysis.stream_role_raw_bytes.get("page-content"), Some(&6));
        assert_eq!(analysis.stream_role_raw_bytes.get("other"), Some(&3));
        assert_eq!(
            analysis.object_role_counts.get("stream:page-content"),
            Some(&2)
        );
        assert_eq!(analysis.object_role_counts.get("stream:other"), Some(&1));
        assert_eq!(analysis.object_role_counts.get("dict:Page"), Some(&1));
        Ok(())
    }

    #[test]
    fn preflight_content_density_accepts_indirect_form_subtype() -> Result<()> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /XObject << /F 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q /F Do Q")?;
        pdf.stream(
            5,
            b"/Type /XObject /Subtype 6 0 R /BBox [0 0 10 10] /Resources <<>>",
            b"0 0 m 5 5 l S",
        )?;
        pdf.object(6, b"/Form")?;
        // This Form is indexed by xref but unreachable from the output graph. The
        // fused object pass must not let all-source structural analysis leak it into
        // live content-density metrics.
        pdf.stream(
            7,
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources <<>>",
            b"q 0 0 m 1 1 l S Q",
        )?;

        let input = pdf.finish(1)?;
        let preflight = analyze_pdf_preflight(&input)?;
        assert_eq!(preflight.form_content_instruction_count, 3);
        assert_eq!(preflight.form_content_path_construction_operator_count, 2);
        assert_eq!(preflight.form_content_paint_operator_count, 1);

        let deep = analyze_pdf(&input)?;
        assert_eq!(deep.form_content_instruction_count, 3);
        assert_eq!(deep.form_content_path_construction_operator_count, 2);
        assert_eq!(deep.form_content_paint_operator_count, 1);
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
