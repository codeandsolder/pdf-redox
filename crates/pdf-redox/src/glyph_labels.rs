//! Apply externally verified Unicode labels to existing empty Type3 `ActualText`
//! spans without changing the vector outlines or their resources.

use crate::{
    EditDocument, Error, GlyphCatalog, GlyphLabelReport, GlyphLabels, Result,
    content::{decoded_content_value, replace_page_content_flate},
    digest::sha256_hex,
    geometry::compose_affine_matrix,
};
use std::collections::{BTreeMap, BTreeSet};

type GlyphKey = (String, u8);
type PageKey = (usize, String, u8);
type OverrideKey = (String, usize, usize);

type VerifiedMaps<'a, 'b> = (
    BTreeMap<GlyphKey, &'a str>,
    BTreeMap<PageKey, usize>,
    BTreeMap<OverrideKey, &'b str>,
);

#[derive(Default)]
struct Pending {
    slot: Option<(usize, usize)>,
    font: Option<String>,
    code: Option<u8>,
    ambiguous: bool,
    page_quad: Option<[[f64; 2]; 4]>,
}

fn locate_empty_actual_text(
    input: &[u8],
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
) -> Option<(usize, usize)> {
    if &instruction.operator[..] != b"BDC" {
        return None;
    }
    let operands = instruction
        .operands()
        .zip(instruction.operand_spans())
        .collect::<Vec<_>>();
    if operands.len() != 2
        || crate::content_stream::operand_name(operands[0].0) != Some(b"Span".as_slice())
    {
        return None;
    }
    let dict_span = operands[1].1.clone();
    let bytes = input.get(dict_span.clone())?;
    let key = b"/ActualText";
    for (start, _) in bytes
        .windows(key.len())
        .enumerate()
        .filter(|(_, part)| *part == key)
    {
        let mut next = start + key.len();
        while matches!(
            bytes.get(next),
            Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\0')
        ) {
            next += 1;
        }
        if bytes.get(next..next + 2) == Some(b"()") {
            return Some((dict_span.start + next, dict_span.start + next + 2));
        }
    }
    None
}

fn text_hex(text: &str) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut out = String::from("<FEFF");
    for unit in text.encode_utf16() {
        let _ = write!(&mut out, "{unit:04X}");
    }
    out.push('>');
    out.into_bytes()
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn matrix_operands(
    decoded: &[u8],
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
) -> Option<[f64; 6]> {
    let operands = crate::content_stream::instruction_operands(decoded, instruction);
    let values: Vec<f64> = operands
        .into_iter()
        .map(|x| x.number)
        .collect::<Option<_>>()?;
    values.try_into().ok()
}

fn unit_quad(matrix: [f64; 6]) -> [[f64; 2]; 4] {
    let [xx, yx, xy, yy, tx, ty] = matrix;
    [
        [tx, ty],
        [tx + xx, ty + yx],
        [tx + xx + xy, ty + yx + yy],
        [tx + xy, ty + yy],
    ]
}

fn validate<'a, 'b>(
    catalog: &'a GlyphCatalog,
    labels: &'b GlyphLabels,
) -> Result<VerifiedMaps<'a, 'b>> {
    if catalog.schema_version != 2 || labels.schema_version != 1 {
        return Err(invalid(
            "unsupported glyph-catalog/label-set schema version",
        ));
    }
    let mut codes = BTreeMap::new();
    let mut expected = BTreeMap::new();
    let mut ids = BTreeSet::new();
    let mut allowed = BTreeSet::new();
    for g in &catalog.glyphs {
        if !ids.insert(g.id.as_str()) {
            return Err(invalid("duplicate glyph catalog ID"));
        }
        if codes
            .insert((g.font_resource.clone(), g.code), g.id.as_str())
            .is_some()
        {
            return Err(invalid("duplicate Type3 glyph resource/code"));
        }
        let mut ordinal_by_page = BTreeMap::<usize, usize>::new();
        for occ in &g.occurrences {
            let n = ordinal_by_page.entry(occ.page).or_default();
            if *n != occ.index_on_page || occ.page == 0 {
                return Err(invalid("invalid catalog occurrence ordering"));
            }
            allowed.insert((g.id.as_str(), occ.page, occ.index_on_page));
            *n += 1;
        }
        for (page, count) in ordinal_by_page {
            expected.insert((page, g.font_resource.clone(), g.code), count);
        }
    }
    for id in labels.labels.keys() {
        if !ids.contains(id.as_str()) {
            return Err(invalid(format!("unrecognized template label: {id}")));
        }
    }
    let mut overrides = BTreeMap::new();
    for item in &labels.overrides {
        if !allowed.contains(&(item.glyph_id.as_str(), item.page, item.index_on_page)) {
            return Err(invalid(format!(
                "unknown occurrence override: {}",
                item.glyph_id
            )));
        }
        let key = (item.glyph_id.clone(), item.page, item.index_on_page);
        if overrides.insert(key, item.text.as_str()).is_some() {
            return Err(invalid("duplicate occurrence override"));
        }
    }
    Ok((codes, expected, overrides))
}

struct AnnotationContext<'a> {
    codes: &'a BTreeMap<GlyphKey, &'a str>,
    labels: &'a GlyphLabels,
    overrides: &'a BTreeMap<OverrideKey, &'a str>,
    expected_quads: &'a BTreeMap<PageKey, Vec<[[f64; 2]; 4]>>,
}

#[expect(
    clippy::too_many_lines,
    reason = "one scoped PDF instruction state machine"
)]
fn amend_page(
    decoded: &[u8],
    page: usize,
    context: &AnnotationContext<'_>,
    seen: &mut BTreeMap<PageKey, usize>,
    report: &mut GlyphLabelReport,
) -> Result<Option<Vec<u8>>> {
    let AnnotationContext {
        codes,
        labels,
        overrides,
        expected_quads,
    } = context;
    let mut pending = Vec::<Option<Pending>>::new();
    let mut patches = Vec::new();
    let mut ctm = [1., 0., 0., 1., 0., 0.];
    let mut transform_stack = Vec::new();
    let incomplete = crate::content_stream::visit_instructions(decoded, |instruction| {
        match &instruction.operator[..] {
            b"q" => transform_stack.push(ctm),
            b"Q" => {
                ctm = transform_stack
                    .pop()
                    .ok_or_else(|| invalid("unbalanced graphics stack"))?;
            }
            b"cm" => {
                let matrix = matrix_operands(decoded, instruction)
                    .ok_or_else(|| invalid("invalid page-space graphics transform"))?;
                ctm = compose_affine_matrix(ctm, matrix);
            }
            b"Tm" => {
                if let Some(mark) = pending.iter_mut().rev().find_map(Option::as_mut) {
                    if mark.page_quad.is_some() {
                        mark.ambiguous = true;
                    }
                    let matrix = matrix_operands(decoded, instruction)
                        .ok_or_else(|| invalid("invalid Type3 text transform"))?;
                    mark.page_quad = Some(unit_quad(compose_affine_matrix(ctm, matrix)));
                }
            }
            b"BDC" => {
                pending.push(
                    locate_empty_actual_text(decoded, instruction).map(|slot| Pending {
                        slot: Some(slot),
                        ..Pending::default()
                    }),
                );
            }
            b"BMC" => pending.push(None),
            b"Tf" => {
                if let Some(mark) = pending.iter_mut().rev().find_map(Option::as_mut) {
                    if mark.font.is_some() {
                        mark.ambiguous = true;
                    }
                    let name = instruction
                        .operands()
                        .next()
                        .and_then(crate::content_stream::operand_name);
                    mark.font = name.map(|bytes| String::from_utf8_lossy(bytes).into_owned());
                }
            }
            b"Tj" => {
                if let Some(mark) = pending.iter_mut().rev().find_map(Option::as_mut) {
                    if mark.code.is_some() {
                        mark.ambiguous = true;
                    }
                    let bytes = instruction
                        .operands()
                        .next()
                        .and_then(crate::content_stream::operand_string);
                    mark.code = bytes.and_then(|data| (data.len() == 1).then_some(data[0]));
                }
            }
            b"EMC" => {
                let popped = pending.pop().ok_or_else(|| invalid("unbalanced EMC"))?;
                if let Some(mark) = popped {
                    if mark.ambiguous {
                        return Err(invalid("ambiguous generated Type3 marked text"));
                    }
                    if let (Some(font), Some(code), Some((start, end))) =
                        (mark.font, mark.code, mark.slot)
                        && let Some(id) = codes.get(&(font.clone(), code))
                    {
                        let key = (page, font, code);
                        let ordinal = seen.entry(key.clone()).or_default();
                        let actual = mark.page_quad.ok_or_else(|| invalid("missing Type3 Tm"))?;
                        let expected = expected_quads
                            .get(&key)
                            .and_then(|items| items.get(*ordinal))
                            .ok_or_else(|| invalid("unknown glyph page placement"))?;
                        if actual
                            .iter()
                            .zip(expected)
                            .flat_map(|(a, b)| a.iter().zip(b))
                            .any(|(a, b)| !a.is_finite() || (*a - *b).abs() > 0.03)
                        {
                            return Err(invalid(format!(
                                "glyph placement does not match catalog on page {page}"
                            )));
                        }
                        let replacement = overrides
                            .get(&(id.to_string(), page, *ordinal))
                            .copied()
                            .or_else(|| labels.labels.get(*id).map(String::as_str));
                        *ordinal += 1;
                        report.matched_occurrences += 1;
                        if let Some(text) = replacement.filter(|text| !text.is_empty()) {
                            patches.push((start, end, text_hex(text)));
                            report.labeled_occurrences += 1;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    if incomplete || !pending.is_empty() || !transform_stack.is_empty() {
        return Err(invalid("unsupported or malformed PDF content stream"));
    }
    if patches.is_empty() {
        return Ok(None);
    }
    crate::content_stream::apply_replacements_owned(decoded, patches)
        .ok_or_else(|| invalid("invalid ActualText replacement ranges"))
        .map(Some)
}

/// Annotate an already optimized PDF using verified semantic glyph labels.
///
/// Only the empty `ActualText` value of selected Type3 appearances is changed.
/// The catalog must match the exact input bytes, and every template occurrence
/// is checked against the PDF before a rewritten document is returned.
///
/// # Errors
///
/// Rejects stale catalogs, unrecognized IDs/occurrences, malformed marked
/// content and any mismatch between recorded and actual Type3 occurrences.
pub fn apply_glyph_labels(
    input: &[u8],
    catalog: &GlyphCatalog,
    labels: &GlyphLabels,
) -> Result<(Vec<u8>, GlyphLabelReport)> {
    if catalog.optimized_pdf_sha256 != sha256_hex(input) {
        return Err(invalid("catalog does not match the exact optimized PDF"));
    }
    let (codes, expected, overrides) = validate(catalog, labels)?;
    let mut expected_quads = BTreeMap::<PageKey, Vec<[[f64; 2]; 4]>>::new();
    for glyph in &catalog.glyphs {
        for appearance in &glyph.occurrences {
            expected_quads
                .entry((appearance.page, glyph.font_resource.clone(), glyph.code))
                .or_default()
                .push(appearance.page_quad);
        }
    }
    let context = AnnotationContext {
        codes: &codes,
        labels,
        overrides: &overrides,
        expected_quads: &expected_quads,
    };
    let mut doc = EditDocument::from_bytes(input.to_vec())?;
    let mut report = GlyphLabelReport::default();
    let mut seen = BTreeMap::new();
    let pages = doc.page_handles()?;
    for (page_index, handle) in pages.iter().copied().enumerate() {
        let Some(page_obj) = doc.current_owned_object(handle)? else {
            continue;
        };
        let Some(dict) = page_obj.as_dictionary() else {
            continue;
        };
        let Some(contents) = dict.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(&doc, contents, &mut decoded)?;
        if let Some(rewritten) =
            amend_page(&decoded, page_index + 1, &context, &mut seen, &mut report)?
        {
            replace_page_content_flate(
                &mut doc,
                handle,
                &rewritten,
                crate::FlateLevel::PROCESSING,
            )?;
            report.rewritten_pages += 1;
        }
    }
    if seen != expected {
        return Err(invalid(
            "Type3 glyph occurrence counts differ from the catalog",
        ));
    }
    let output = crate::writer::write_pdf_with_object_streams(&doc, crate::FlateLevel::PROCESSING)?;
    Ok((output, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encodes_unicode_actual_text_utf16_be() {
        assert_eq!(text_hex("A"), b"<FEFF0041>");
        assert_eq!(text_hex("中😀"), b"<FEFF4E2DD83DDE00>");
        assert_eq!(text_hex(""), b"<FEFF>");
    }

    #[test]
    fn empty_actual_text_is_the_only_supported_slot() -> Result<()> {
        let text = b"/Span << /ActualText () >> BDC\nBT /PdfRedoxGlyph0 1 Tf <01> Tj ET\nEMC";
        let mut hits = Vec::new();
        let unfinished = crate::content_stream::visit_instructions(text, |instruction| {
            if let Some(span) = locate_empty_actual_text(text, instruction) {
                hits.push(span);
            }
            Ok(())
        })?;
        assert!(!unfinished);
        assert_eq!(hits.len(), 1);
        assert_eq!(&text[hits[0].0..hits[0].1], b"()");
        Ok(())
    }
}
