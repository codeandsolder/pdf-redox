//! Eliminate redundant opaque image paints under exactly equal placement.
//!
//! A later opaque Image `XObject` draw fully covers an earlier draw of the
//! same canonical image at the same CTM. This pass recognizes contiguous
//! sequences of independent, balanced image-paint blocks (also separated by
//! other eligible independent image-paint blocks) and removes earlier copies.
//! It never guesses about graphics-state transparency, clipping, marked
//! content, image masks, soft masks, or incomplete content streams.

use crate::{
    EditDocument, ObjectHandle, OwnedObject, Result,
    content::{
        PageRewriteResult, page_content, page_xobject_bindings, replace_page_content,
        resolved_dictionary, resolved_finite_number_array,
    },
    content_stream::{instruction_number_array, operand_name},
};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeadPaintStats {
    pub paints_removed: usize,
    pub pages_rewritten: usize,
    pub decoded_bytes_removed: usize,
}

const IDENTITY: [u64; 6] = [1.0_f64.to_bits(), 0, 0, 1.0_f64.to_bits(), 0, 0];

enum PaintToken {
    Save,
    Restore,
    Matrix([u64; 6]),
    Draw(Vec<u8>),
    Other,
}

struct PaintInstruction {
    start: usize,
    end: usize,
    token: PaintToken,
}

impl PaintInstruction {
    fn from_parsed(instruction: &hayro_syntax::content::Instruction<'_, '_>) -> Self {
        let count = instruction.operand_spans().len();
        let token = match &instruction.operator[..] {
            b"q" if count == 0 => PaintToken::Save,
            b"Q" if count == 0 => PaintToken::Restore,
            b"cm" => instruction_number_array::<6>(instruction)
                .filter(|values| values.iter().all(|value| value.is_finite()))
                .map_or(PaintToken::Other, |values| {
                    PaintToken::Matrix(values.map(f64::to_bits))
                }),
            b"Do" if count == 1 => instruction
                .operands()
                .next()
                .and_then(operand_name)
                .map_or(PaintToken::Other, |name| PaintToken::Draw(name.to_vec())),
            _ => PaintToken::Other,
        };
        let span = instruction.span();
        Self {
            start: span.start,
            end: span.end,
            token,
        }
    }
}

struct PaintCandidate {
    start: usize,
    end: usize,
    name: Vec<u8>,
    transform: [u64; 6],
    bare: bool,
    begins_run: bool,
}

#[derive(Default)]
struct PaintScanner {
    pending: VecDeque<PaintInstruction>,
    paints: Vec<PaintCandidate>,
    begins_run: bool,
    depth: usize,
    skipped: bool,
}

impl PaintScanner {
    fn advance(&mut self) {
        let Some(first) = self.pending.front() else {
            return;
        };
        let (draw_index, count, transform, bare) = match &first.token {
            PaintToken::Draw(_) => (0, 1, [0; 6], true),
            PaintToken::Save => match (
                self.pending.get(1).map(|instruction| &instruction.token),
                self.pending.get(2).map(|instruction| &instruction.token),
                self.pending.get(3).map(|instruction| &instruction.token),
            ) {
                (Some(PaintToken::Draw(_)), Some(PaintToken::Restore), _) => {
                    (1, 3, IDENTITY, false)
                }
                (
                    Some(PaintToken::Matrix(matrix)),
                    Some(PaintToken::Draw(_)),
                    Some(PaintToken::Restore),
                ) => (2, 4, *matrix, false),
                _ => {
                    self.pending.pop_front();
                    self.skipped = true;
                    self.begins_run = true;
                    return;
                }
            },
            _ => {
                self.pending.pop_front();
                self.skipped = true;
                self.begins_run = true;
                return;
            }
        };
        let start = first.start;
        let end = self.pending[count - 1].end;
        // The draw token is consumed rather than cloning its resource name.
        let PaintToken::Draw(name) =
            std::mem::replace(&mut self.pending[draw_index].token, PaintToken::Other)
        else {
            unreachable!("candidate grammar identifies a draw");
        };
        self.pending.drain(..count);
        self.paints.push(PaintCandidate {
            start,
            end,
            name,
            transform,
            bare,
            begins_run: self.begins_run,
        });
        self.begins_run = false;
    }

    fn scan(content: &[u8]) -> Option<Self> {
        let mut scanner = Self {
            begins_run: true,
            ..Self::default()
        };
        let mut iter = hayro_syntax::content::UntypedIter::new(content);
        while let Some(instruction) = iter.next() {
            match &instruction.operator[..] {
                // These operations invalidate the entire page's opacity or
                // structural proof. No further parsing or allocation is useful.
                b"gs" | b"W" | b"W*" | b"BMC" | b"BDC" | b"EMC" | b"MP" | b"DP" | b"BI" => {
                    return None;
                }
                b"q" => scanner.depth += 1,
                b"Q" => scanner.depth = scanner.depth.checked_sub(1)?,
                _ => {}
            }
            scanner
                .pending
                .push_back(PaintInstruction::from_parsed(&instruction));
            if scanner.pending.len() == 4 {
                scanner.advance();
            }
        }
        if !iter.is_at_end() || scanner.depth != 0 {
            return None;
        }
        while !scanner.pending.is_empty() {
            scanner.advance();
        }
        Some(scanner)
    }
}

/// Only ordinary fully opaque raster images qualify. In particular, JPX may
/// contain internal alpha even without an explicit PDF `/SMask`.
fn is_proven_opaque_image(document: &EditDocument, handle: ObjectHandle) -> Result<bool> {
    let Some(OwnedObject::Stream { dictionary, .. }) = document.current_owned_object(handle)?
    else {
        return Ok(false);
    };
    if !matches!(dictionary.get(b"Subtype".as_slice()), Some(OwnedObject::Name(v)) if v == b"Image")
    {
        return Ok(false);
    }
    for key in [
        b"Mask".as_slice(),
        b"SMask",
        b"SMaskInData",
        b"OC",
        b"Alternates",
        b"OPI",
        b"Matte",
    ] {
        if dictionary.contains_key(key) {
            return Ok(false);
        }
    }
    // Explicit /ImageMask false is semantically identical to its default.
    // A stencil ImageMask true inherits the current nonstroking color.
    if !matches!(
        dictionary.get(b"ImageMask".as_slice()),
        None | Some(OwnedObject::Boolean(false))
    ) {
        return Ok(false);
    }
    // A normal PDF Image XObject with *any* valid color space is opaque
    // unless it has a color-key/soft mask or JPX-internal alpha. Indexed,
    // ICCBased, Separation and DeviceN are not inherently transparent.
    if !matches!(
        dictionary.get(b"ColorSpace".as_slice()),
        Some(OwnedObject::Name(_) | OwnedObject::Array(_) | OwnedObject::Reference(_))
    ) {
        return Ok(false);
    }
    let ordinary_filter = |name: &[u8]| {
        matches!(
            name,
            b"FlateDecode"
                | b"DCTDecode"
                | b"LZWDecode"
                | b"RunLengthDecode"
                | b"ASCII85Decode"
                | b"ASCIIHexDecode"
                | b"CCITTFaxDecode"
                | b"JBIG2Decode"
        )
    };
    match dictionary.get(b"Filter".as_slice()) {
        None => {}
        Some(OwnedObject::Name(filter)) if ordinary_filter(filter) => {}
        Some(OwnedObject::Array(filters))
            if !filters.is_empty()
                && filters
                    .iter()
                    .all(|f| matches!(f, OwnedObject::Name(name) if ordinary_filter(name))) => {}
        _ => return Ok(false),
    }
    Ok(true)
}

/// Prove a Form `XObject` is nothing but an opaque image painted once.
/// General Forms with clipping, marked content or transparency remain untouched.
fn is_proven_opaque_drawable(
    document: &EditDocument,
    handle: ObjectHandle,
    active: &mut BTreeSet<ObjectHandle>,
    depth: usize,
) -> Result<bool> {
    if depth >= 24 || !active.insert(handle) {
        return Ok(false);
    }
    let proven = if is_proven_opaque_image(document, handle)? {
        true
    } else {
        form_contains_only_opaque_image(document, handle, active, depth)?
    };
    active.remove(&handle);
    Ok(proven)
}

fn form_contains_only_opaque_image(
    document: &EditDocument,
    handle: ObjectHandle,
    active: &mut BTreeSet<ObjectHandle>,
    depth: usize,
) -> Result<bool> {
    let Some(OwnedObject::Stream { dictionary, .. }) = document.current_owned_object(handle)?
    else {
        return Ok(false);
    };
    if !matches!(
        dictionary.get(b"Subtype".as_slice()),
        Some(OwnedObject::Name(name)) if name == b"Form"
    ) || [
        b"Group".as_slice(),
        b"OC",
        b"StructParent",
        b"StructParents",
        b"Ref",
        b"OPI",
    ]
    .iter()
    .any(|key| dictionary.contains_key(*key))
    {
        return Ok(false);
    }
    let Some(bounds) = dictionary.get(b"BBox".as_slice()) else {
        return Ok(false);
    };
    if !resolved_finite_number_array::<4>(document, crate::source::CurrentValue::Owned(bounds))?
        .is_some_and(|bbox| bbox[2] > bbox[0] && bbox[3] > bbox[1])
    {
        return Ok(false);
    }
    if let Some(matrix) = dictionary.get(b"Matrix".as_slice())
        && resolved_finite_number_array::<6>(document, crate::source::CurrentValue::Owned(matrix))?
            .is_none()
    {
        return Ok(false);
    }
    let Some(resources) = resolved_dictionary(document, dictionary.get(b"Resources".as_slice()))?
    else {
        return Ok(false);
    };
    let xobjects =
        resolved_dictionary(document, resources.get(b"XObject".as_slice()))?.unwrap_or_default();
    let content = document.decoded_content_stream_data(handle)?;
    let Some(draw) = single_image_form_draw(&content) else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(next)) = xobjects.get(&draw) else {
        return Ok(false);
    };
    is_proven_opaque_drawable(document, *next, active, depth + 1)
}

/// A Form with a single image/Form paint has no other drawing side effects.
fn single_image_form_draw(content: &[u8]) -> Option<Vec<u8>> {
    let scanner = PaintScanner::scan(content)?;
    if scanner.skipped || scanner.paints.len() != 1 {
        return None;
    }
    scanner.paints.into_iter().next().map(|paint| paint.name)
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct PaintKey {
    source: ObjectHandle,
    transform: [u64; 6],
    bare: bool,
}

fn select_dead_paints(
    document: &EditDocument,
    content: &[u8],
    resources: &BTreeMap<Vec<u8>, ObjectHandle>,
) -> Result<Vec<(usize, usize)>> {
    let Some(scanner) = PaintScanner::scan(content) else {
        return Ok(Vec::new());
    };
    // Complete scope admission precedes resource resolution. In particular a
    // later forbidden operator still suppresses all earlier opacity probes.
    let mut dead = Vec::new();
    let mut opaque = BTreeMap::new();
    let mut seen = BTreeMap::<PaintKey, (usize, usize)>::new();
    for paint in scanner.paints {
        if paint.begins_run {
            seen.clear();
        }
        let Some(&source) = resources.get(&paint.name) else {
            seen.clear();
            continue;
        };
        let supported = match opaque.entry(source) {
            std::collections::btree_map::Entry::Occupied(entry) => *entry.get(),
            std::collections::btree_map::Entry::Vacant(entry) => *entry.insert(
                is_proven_opaque_drawable(document, source, &mut BTreeSet::new(), 0)?,
            ),
        };
        if !supported {
            seen.clear();
            continue;
        }
        let key = PaintKey {
            source,
            transform: paint.transform,
            bare: paint.bare,
        };
        if let Some(previous) = seen.insert(key, (paint.start, paint.end)) {
            dead.push(previous);
        }
    }
    dead.sort_unstable();
    Ok(dead)
}

fn remove_ranges(input: &[u8], ranges: &[(usize, usize)]) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;
    for &(start, end) in ranges {
        if start < cursor || start > end || end > input.len() {
            return None;
        }
        output.extend_from_slice(input.get(cursor..start)?);
        cursor = end;
    }
    output.extend_from_slice(input.get(cursor..)?);
    Some(output)
}

/// Remove fully covered prior opaque image paints from page content.
///
/// Only one painted image resource and an exact six-component affine
/// transform are compared. The earlier image paint is removed only when
/// another eligible opaque draw of the same resolved image object occurs
/// later in an uninterrupted run of independent graphics-state blocks.
///
/// # Errors
///
/// Returns an error when PDF content or resources cannot be resolved.
pub fn eliminate_dead_image_paints(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
) -> Result<PageRewriteResult<DeadPaintStats>> {
    let mut result = PageRewriteResult::<DeadPaintStats>::default();
    for &page in pages {
        // Page transparency/knockout groups need group-level compositing
        // proof, not just the image's own /SMask or colorspace.
        let Some(page_object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(page_dict) = page_object.as_dictionary() else {
            continue;
        };
        if page_dict.contains_key(b"Group".as_slice()) {
            continue;
        }
        let resources = page_xobject_bindings(document, page)?.handles;
        if resources.is_empty() {
            continue;
        }
        let content = page_content(document, page)?;
        if !content.windows(2).any(|pair| pair == b"Do") {
            continue;
        }
        let dead = select_dead_paints(document, &content, &resources)?;
        if dead.is_empty() {
            continue;
        }
        if let Some(output) = remove_ranges(&content, &dead) {
            let removed_bytes = content.len() - output.len();
            replace_page_content(document, page, output)?;
            result.stats.paints_removed += dead.len();
            result.stats.decoded_bytes_removed += removed_bytes;
            result.stats.pages_rewritten += 1;
            result.rewritten_pages.insert(page);
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_stream::visit_instructions;
    use crate::{Config, optimize_pdf, test_support::ClassicPdfBuilder};

    const PAINT: &str = "q 80 0 0 40 20 30 cm /Im0 Do Q\n";
    const OTHER: &str = "q 80 0 0 40 120 30 cm /Im1 Do Q\n";

    fn fixture(
        content: &[u8],
        image_extra: &[u8],
        page_extra: &[u8],
        independent_duplicate_image: bool,
    ) -> Result<Vec<u8>> {
        let mut builder = ClassicPdfBuilder::new();
        builder.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        builder.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        let mut page = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Im0 5 0 R /Im1 ".to_vec();
        page.extend_from_slice(if independent_duplicate_image {
            b"6 0 R"
        } else {
            b"5 0 R"
        });
        page.extend_from_slice(b" >> >> /Contents 4 0 R ");
        page.extend_from_slice(page_extra);
        page.extend_from_slice(b" >>");
        builder.object(3, &page)?;
        builder.stream(4, b"", content)?;
        let mut extra = b"/Type /XObject /Subtype /Image /Width 2 /Height 2 /BitsPerComponent 8 /ColorSpace /DeviceRGB ".to_vec();
        extra.extend_from_slice(image_extra);
        let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 0];
        builder.stream(5, &extra, &pixels)?;
        if independent_duplicate_image {
            builder.stream(6, &extra, &pixels)?;
        }
        builder.finish(1)
    }

    fn check(content: &[u8], extra: &[u8], page_extra: &[u8]) -> Result<(DeadPaintStats, Vec<u8>)> {
        let pdf = fixture(content, extra, page_extra, false)?;
        let mut document = EditDocument::from_bytes(pdf)?;
        let pages = document.page_handles()?;
        let result = eliminate_dead_image_paints(&mut document, &pages)?;
        let output = page_content(&document, pages[0])?;
        Ok((result.stats, output))
    }

    fn draw_count(content: &[u8]) -> Result<usize> {
        let mut count = 0;
        let incomplete = visit_instructions(content, |instruction| {
            if &instruction.operator[..] == b"Do" {
                count += 1;
            }
            Ok(())
        })?;
        assert!(!incomplete);
        Ok(count)
    }

    #[test]
    fn streaming_paint_grammar_preserves_run_boundaries_and_form_admission() -> Result<()> {
        let content =
            b"q /Im0 Do Q /Im0 Do 0 0 m /Other Do q 1 0 0 1 0 0 cm /Im0 Do Q q /Im0 Do 0 0 l Q";
        let scanner = PaintScanner::scan(content)
            .ok_or_else(|| crate::Error::Invalid("paint grammar fixture did not parse".into()))?;
        let actual = scanner
            .paints
            .iter()
            .map(|paint| {
                (
                    &content[paint.start..paint.end],
                    paint.bare,
                    paint.begins_run,
                    paint.transform,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            actual,
            vec![
                (b"q /Im0 Do Q".as_slice(), false, true, IDENTITY),
                (b"/Im0 Do".as_slice(), true, false, [0; 6]),
                (b"/Other Do".as_slice(), true, true, [0; 6]),
                (
                    b"q 1 0 0 1 0 0 cm /Im0 Do Q".as_slice(),
                    false,
                    false,
                    IDENTITY
                ),
                (b"/Im0 Do".as_slice(), true, true, [0; 6]),
            ]
        );
        assert!(scanner.skipped);
        for input in [
            b"/Im0 Do".as_slice(),
            b"q /Im0 Do Q",
            b"q 1 0 0 1 0 0 cm /Im0 Do Q",
        ] {
            assert_eq!(
                single_image_form_draw(input).as_deref(),
                Some(b"Im0".as_slice())
            );
        }
        for input in [
            b"q /Im0 Do 0 0 m Q".as_slice(),
            b"/Im0 Do /Im0 Do",
            b"q /Im0 Do",
            b"q /Im0 Do 1 Q",
            b"q 1 0 0 1 0 cm /Im0 Do Q",
        ] {
            assert!(single_image_form_draw(input).is_none());
        }
        Ok(())
    }

    #[test]
    fn whole_scope_rejection_precedes_opacity_resolution() -> Result<()> {
        let pdf = fixture(b"q Q", b"", b"", false)?;
        let mut document = EditDocument::from_bytes(pdf)?;
        let id = crate::ObjectId::new(5, 0);
        document.overlay_mut().delete(id);
        let resources = BTreeMap::from([(b"Im0".to_vec(), ObjectHandle::Existing(id))]);
        assert!(select_dead_paints(&document, b"/Im0 Do /Im0 Do", &resources).is_err());
        for content in [
            b"/Im0 Do /Im0 Do /Unknown gs".as_slice(),
            b"/Im0 Do /Im0 Do 0 0 10 10 re W n",
            b"/Im0 Do /Im0 Do Q",
            b"/Im0 Do /Im0 Do q",
        ] {
            assert_eq!(
                select_dead_paints(&document, content, &resources)?,
                [] as [(usize, usize); 0]
            );
        }
        Ok(())
    }

    #[test]
    fn removes_two_of_three_exact_duplicate_paints() -> Result<()> {
        let content = PAINT.repeat(3);
        let (stats, output) = check(content.as_bytes(), b"", b"")?;
        assert_eq!(stats.paints_removed, 2);
        assert_eq!(stats.pages_rewritten, 1);
        assert!(stats.decoded_bytes_removed >= 60);
        assert_eq!(draw_count(&output)?, 1);
        assert!(output.ends_with(PAINT.as_bytes()));
        Ok(())
    }

    #[test]
    fn removes_first_paint_when_other_opaque_image_is_between() -> Result<()> {
        let content = format!("{PAINT}{OTHER}{PAINT}");
        let (stats, output) = check(content.as_bytes(), b"", b"")?;
        assert_eq!(stats.paints_removed, 1);
        assert_eq!(draw_count(&output)?, 2);
        assert!(!output.starts_with(PAINT.as_bytes()));
        assert!(output.ends_with(PAINT.as_bytes()));
        Ok(())
    }

    #[test]
    fn matches_resource_aliases_and_bare_do_runs() -> Result<()> {
        let alt = "q 80 0 0 40 20 30 cm /Im1 Do Q\n";
        let (stats, output) = check(format!("{PAINT}{alt}").as_bytes(), b"", b"")?;
        assert_eq!(stats.paints_removed, 1);
        assert_eq!(draw_count(&output)?, 1);

        let (stats, output) = check(b"/Im0 Do\n/Im1 Do\n", b"", b"")?;
        assert_eq!(stats.paints_removed, 1);
        assert_eq!(draw_count(&output)?, 1);
        Ok(())
    }

    #[test]
    fn identity_and_zero_ctm_never_alias() -> Result<()> {
        let identity = b"q /Im0 Do Q\n";
        let zero = b"q 0 0 0 0 0 0 cm /Im0 Do Q\n";
        let content = [identity.as_slice(), zero.as_slice()].concat();
        let (stats, original) = check(&content, b"", b"")?;
        assert_eq!(
            stats.paints_removed, 0,
            "zero CTM must not occlude the identity"
        );
        assert_eq!(draw_count(&original)?, 2);
        let explicit_identity = b"q 1 0 0 1 0 0 cm /Im0 Do Q\n";
        let content = [identity.as_slice(), explicit_identity.as_slice()].concat();
        let (stats, rewritten) = check(&content, b"", b"")?;
        assert_eq!(stats.paints_removed, 1, "identity CTMs are equivalent");
        assert_eq!(draw_count(&rewritten)?, 1);
        Ok(())
    }

    #[test]
    fn preserves_distinct_placements_and_state_barriers() -> Result<()> {
        let shifted = "q 80 0 0 40 21 30 cm /Im0 Do Q\n";
        for content in [
            format!("{PAINT}{shifted}"),
            format!("{PAINT}0 0 m 10 10 l S\n{PAINT}"),
            format!("{PAINT}q 1 0 0 rg Q\n{PAINT}"),
            format!("{PAINT}q 80 0 0 40 20 30 cm /Im0 Do Q\nQ"),
        ] {
            let (stats, _) = check(content.as_bytes(), b"", b"")?;
            assert_eq!(stats.paints_removed, 0, "{content}");
        }
        Ok(())
    }

    #[test]
    fn keeps_masked_images_and_nonopaque_encodings() -> Result<()> {
        for extra in [
            b"/SMask 7 0 R".as_slice(),
            b"/Mask [0 0]".as_slice(),
            b"/Alternates []".as_slice(),
            b"/Filter /JPXDecode".as_slice(),
            b"/OC 7 0 R".as_slice(),
            b"/Interpolate true /SMaskInData 0".as_slice(),
        ] {
            let (stats, output) = check(PAINT.repeat(2).as_bytes(), extra, b"")?;
            assert_eq!(stats.paints_removed, 0, "extra image key {extra:?}");
            assert_eq!(draw_count(&output)?, 2);
        }
        Ok(())
    }

    #[test]
    fn keeps_unknown_alpha_clip_tagging_and_page_groups() -> Result<()> {
        for prefix in [
            b"/GS0 gs\n".as_slice(),
            b"0 0 612 792 re W n\n".as_slice(),
            b"/Span << /MCID 0 >> BDC\n".as_slice(),
            b"/Marked MP\n".as_slice(),
        ] {
            let content = [prefix, PAINT.repeat(2).as_bytes()].concat();
            let (stats, _) = check(&content, b"", b"")?;
            assert_eq!(stats.paints_removed, 0, "prefix {prefix:?}");
        }
        let (stats, _) = check(
            PAINT.repeat(2).as_bytes(),
            b"",
            b"/Group << /S /Transparency /K true >>",
        )?;
        assert_eq!(stats.paints_removed, 0);
        Ok(())
    }

    #[test]
    fn distinct_opaque_image_objects_require_canonicalization() -> Result<()> {
        let bytes = fixture(
            format!("{PAINT}q 80 0 0 40 20 30 cm /Im1 Do Q\n").as_bytes(),
            b"",
            b"",
            true,
        )?;
        let mut document = EditDocument::from_bytes(bytes.clone())?;
        let pages = document.page_handles()?;
        let first = eliminate_dead_image_paints(&mut document, &pages)?;
        assert_eq!(first.stats.paints_removed, 0);
        // The full pipeline canonicalizes exact duplicate image streams before
        // attempting dead-paint elimination; also exercise the public report.
        let (optimized, report) = optimize_pdf(&bytes, &Config::optimize_only())?;
        assert_eq!(
            report.dead_image_paints_removed, 1,
            "{:?}",
            report.stage_timings_ms
        );
        let final_document = EditDocument::from_bytes(optimized)?;
        let final_page = final_document.page_handles()?[0];
        assert_eq!(draw_count(&page_content(&final_document, final_page)?)?, 1);
        Ok(())
    }

    /// A nested Form contains exactly one opaque image paint, and the same
    /// outer Form is drawn twice by self-contained page graphics blocks.
    fn form_fixture(nested: bool, group: bool, extra_content: bool) -> Result<Vec<u8>> {
        let mut builder = ClassicPdfBuilder::new();
        builder.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        builder.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        let child = if nested { 7 } else { 6 };
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /XObject << /Fm0 {child} 0 R >> >> /Contents 4 0 R >>"
        );
        builder.object(3, page.as_bytes())?;
        builder.stream(4, b"", b"q /Fm0 Do Q\nq /Fm0 Do Q\n")?;
        builder.stream(
            5,
            b"/Type /XObject /Subtype /Image /Width 2 /Height 2 /BitsPerComponent 8 /ColorSpace /DeviceRGB",
            &[0,0,0,255,255,255,255,0,0,0,255,0],
        )?;
        let form_suffix = if group {
            b" /Group << /S /Transparency /K true >>".as_slice()
        } else {
            b"".as_slice()
        };
        let mut form_info=b"/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /XObject << /Im0 5 0 R >> >>".to_vec();
        form_info.extend_from_slice(form_suffix);
        builder.stream(
            6,
            &form_info,
            if extra_content {
                b"q 80 0 0 40 20 30 cm /Im0 Do Q\n0 0 m 100 100 l S\n"
            } else {
                b"q 80 0 0 40 20 30 cm /Im0 Do Q\n"
            },
        )?;
        if nested {
            builder.stream(
                7,
                b"/Type /XObject /Subtype /Form /BBox [0 0 612 792] /Resources << /XObject << /Inner 6 0 R >> >>",
                b"/Inner Do\n",
            )?;
        }
        builder.finish(1)
    }

    #[test]
    fn removes_repeated_opaque_image_only_form_and_nested_form() -> Result<()> {
        for nested in [false, true] {
            let bytes = form_fixture(nested, false, false)?;
            let mut doc = EditDocument::from_bytes(bytes)?;
            let pages = doc.page_handles()?;
            let original = page_content(&doc, pages[0])?;
            assert_eq!(draw_count(&original)?, 2);
            let stats = eliminate_dead_image_paints(&mut doc, &pages)?.stats;
            assert_eq!(stats.paints_removed, 1, "nested={nested}");
            let result = page_content(&doc, pages[0])?;
            assert_eq!(draw_count(&result)?, 1);
            assert_eq!(result, b"\nq /Fm0 Do Q\n");
        }
        Ok(())
    }

    #[test]
    fn preserves_transparency_group_forms_and_mixed_content_forms() -> Result<()> {
        for (group, extra) in [(true, false), (false, true)] {
            let bytes = form_fixture(true, group, extra)?;
            let mut doc = EditDocument::from_bytes(bytes)?;
            let pages = doc.page_handles()?;
            let stats = eliminate_dead_image_paints(&mut doc, &pages)?.stats;
            assert_eq!(stats.paints_removed, 0, "group={group} extra={extra}");
        }
        Ok(())
    }

    #[test]
    fn second_pass_is_idempotent() -> Result<()> {
        let bytes = fixture(PAINT.repeat(3).as_bytes(), b"", b"", false)?;
        let mut doc = EditDocument::from_bytes(bytes)?;
        let pages = doc.page_handles()?;
        assert_eq!(
            eliminate_dead_image_paints(&mut doc, &pages)?
                .stats
                .paints_removed,
            2
        );
        assert_eq!(
            eliminate_dead_image_paints(&mut doc, &pages)?
                .stats
                .paints_removed,
            0
        );
        Ok(())
    }
}
