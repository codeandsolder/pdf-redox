//! Shared Type3 shapes for painted Bézier and line outlines in any graphics scope.
//! Discovery is approximate, but canonical membership is checked across
//! every occurrence using the same local-point coordinate bound.
use super::*;

const SEED_EPS: f64 = 0.012;
const MAX_ERROR_PT: f64 = 0.02;

#[derive(Clone, Debug)]
struct ShapeOp {
    token: Vec<u8>,
    args: Vec<f64>,
}

#[derive(Clone, Debug)]
struct Shape {
    start: usize,
    end: usize,
    page_index: usize,
    paint: Vec<u8>,
    ops: Vec<ShapeOp>,
    skeleton: Vec<u8>,
    normalized: Vec<f64>,
    dimensions: Vec<f64>,
    bounds: [f64; 4],
    page_space_factor: f64,
}

const fn arity(op: &[u8]) -> Option<usize> {
    Some(match op {
        b"m" | b"l" => 2,
        b"c" => 6,
        b"v" | b"y" | b"re" => 4,
        b"h" => 0,
        _ => return None,
    })
}

fn pdf_num(x: f64) -> String {
    if x.abs() < 0.000_000_000_5 {
        return "0".into();
    }
    let mut out = format!("{x:.9}");
    while out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    out
}

// Linear CTM determines how a local-coordinate error magnifies in page space.
fn matrix_from_event(e: &Event) -> Option<[f64; 6]> {
    let matrix: [f64; 6] = e.numbers.as_slice().try_into().ok()?;
    matrix
        .iter()
        .all(|value| value.is_finite())
        .then_some(matrix)
}

fn compose_matrix(a: [f64; 6], b: [f64; 6]) -> [f64; 6] {
    [
        a[2].mul_add(b[1], a[0] * b[0]),
        a[3].mul_add(b[1], a[1] * b[0]),
        a[2].mul_add(b[3], a[0] * b[2]),
        a[3].mul_add(b[3], a[1] * b[2]),
        a[2].mul_add(b[5], a[0].mul_add(b[4], a[4])),
        a[3].mul_add(b[5], a[1].mul_add(b[4], a[5])),
    ]
}

fn normalize_control_points(
    ops: &[ShapeOp],
    bounds: [f64; 4],
    width: f64,
    height: f64,
) -> (Vec<f64>, Vec<f64>) {
    let mut normalized = Vec::new();
    let mut dimensions = Vec::new();
    for op in ops {
        for (index, value) in op.args.iter().enumerate() {
            let horizontal = index % 2 == 0;
            let dimension = if horizontal { width } else { height };
            let origin = if horizontal { bounds[0] } else { bounds[1] };
            let local = if op.token == b"re" && index >= 2 {
                *value
            } else {
                *value - origin
            };
            normalized.push(local / dimension);
            dimensions.push(dimension);
        }
    }
    (normalized, dimensions)
}

fn read_shape(input: &[u8], events: &[Event], i: usize, ctm: [f64; 6]) -> Option<(Shape, usize)> {
    // Only an unpainted path followed by a filling operator is replaced.
    // Its surrounding graphics state (including any cm, q/Q, clipping and
    // optional-content scopes) stays in the original content stream.
    let first = events.get(i)?;
    if !matches!(first.operator.as_slice(), b"m" | b"re") {
        return None;
    }
    let mut ops = Vec::new();
    let mut skeleton = Vec::new();
    let mut bounds = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    let mut cursor = i;
    while let Some(e) = events.get(cursor) {
        let Some(need) = arity(&e.operator) else {
            break;
        };
        if !e.all_operands_numeric
            || e.operand_count != need
            || (cursor > i && !adjacent(input, events.get(cursor - 1)?, e))
        {
            return None;
        }
        let args = e.numbers.iter().copied().collect::<Vec<_>>();
        if e.operator == b"re" {
            // PDF 're' operands are x, y, WIDTH, HEIGHT. Width and height
            // are not independent x/y control points.
            for (x, y) in [(args[0], args[1]), (args[0] + args[2], args[1] + args[3])] {
                bounds[0] = bounds[0].min(x);
                bounds[1] = bounds[1].min(y);
                bounds[2] = bounds[2].max(x);
                bounds[3] = bounds[3].max(y);
            }
        } else {
            for [x, y] in args.as_chunks::<2>().0 {
                bounds[0] = bounds[0].min(*x);
                bounds[1] = bounds[1].min(*y);
                bounds[2] = bounds[2].max(*x);
                bounds[3] = bounds[3].max(*y);
            }
        }
        skeleton.push(u8::try_from(need).ok()?);
        skeleton.extend_from_slice(&e.operator);
        skeleton.push(0);
        ops.push(ShapeOp {
            token: e.operator.clone(),
            args,
        });
        cursor += 1;
    }
    let paint = events.get(cursor)?;
    if ops.len() < 3
        || !matches!(paint.operator.as_slice(), b"f" | b"F" | b"f*")
        || paint.operand_count != 0
        || !adjacent(input, events.get(cursor - 1)?, paint)
    {
        return None;
    }
    let width = bounds[2] - bounds[0];
    let height = bounds[3] - bounds[1];
    if !(width > 0.0 && height > 0.0 && width.is_finite() && height.is_finite()) {
        return None;
    }
    let (normalized, dimensions) = normalize_control_points(&ops, bounds, width, height);
    skeleton.extend_from_slice(&paint.operator);
    let combined = ctm;
    let page_space_factor = (combined[0].abs() + combined[2].abs())
        .max(combined[1].abs() + combined[3].abs())
        .max(1.0);
    if !page_space_factor.is_finite() || normalized.iter().any(|n| !n.is_finite()) {
        return None;
    }
    Some((
        Shape {
            start: first.start,
            end: paint.end,
            page_index: 0,
            paint: paint.operator.clone(),
            ops,
            skeleton,
            normalized,
            dimensions,
            bounds,
            page_space_factor,
        },
        cursor + 1,
    ))
}

fn collect(input: &[u8]) -> Option<Vec<Shape>> {
    let events = events_for(input, "filled curve outlines")?;
    let mut shapes = Vec::new();
    let (mut cursor, mut text, mut path_open) = (0, false, false);
    let mut ctm = [1.0, 0.0, 0.0, 1.0, 0.0, 0.0];
    let mut ctm_stack = Vec::new();
    while cursor < events.len() {
        let e = &events[cursor];
        if e.operator == b"BT" {
            text = true;
        }
        if e.operator == b"ET" {
            text = false;
        }
        if !text
            && !path_open
            && matches!(e.operator.as_slice(), b"m" | b"re")
            && let Some((shape, next)) = read_shape(input, &events, cursor, ctm)
        {
            shapes.push(shape);
            cursor = next;
            continue;
        }
        match e.operator.as_slice() {
            b"q" => ctm_stack.push(ctm),
            b"Q" => ctm = ctm_stack.pop()?,
            b"cm" => ctm = compose_matrix(ctm, matrix_from_event(e)?),
            _ => {}
        }
        if arity(&e.operator).is_some() {
            path_open = true;
        }
        if matches!(
            e.operator.as_slice(),
            b"f" | b"F" | b"f*" | b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*" | b"n"
        ) {
            path_open = false;
        }
        cursor += 1;
    }
    Some(shapes)
}

#[derive(Debug)]
struct Group {
    members: Vec<usize>,
    seed: usize,
    low: Vec<f64>,
    high: Vec<f64>,
}

impl Group {
    fn first(i: usize, item: &Shape) -> Self {
        let (low, high) = item
            .normalized
            .iter()
            .zip(&item.dimensions)
            .map(|(x, d)| {
                (
                    x - MAX_ERROR_PT / (d * item.page_space_factor),
                    x + MAX_ERROR_PT / (d * item.page_space_factor),
                )
            })
            .unzip();
        Self {
            members: vec![i],
            seed: i,
            low,
            high,
        }
    }
    fn add(&mut self, i: usize, shapes: &[Shape]) -> bool {
        let item = &shapes[i];
        if item.normalized.len() != self.low.len()
            || item
                .normalized
                .iter()
                .zip(&shapes[self.seed].normalized)
                .any(|(a, b)| (a - b).abs() > SEED_EPS)
        {
            return false;
        }
        if self
            .low
            .iter()
            .zip(&self.high)
            .zip(&item.normalized)
            .zip(&item.dimensions)
            .any(|(((lo, hi), v), d)| {
                lo.max(v - MAX_ERROR_PT / (d * item.page_space_factor))
                    > hi.min(v + MAX_ERROR_PT / (d * item.page_space_factor)) + 1e-12
            })
        {
            return false;
        }
        for (((lo, hi), v), d) in self
            .low
            .iter_mut()
            .zip(&mut self.high)
            .zip(&item.normalized)
            .zip(&item.dimensions)
        {
            *lo = lo.max(v - MAX_ERROR_PT / (d * item.page_space_factor));
            *hi = hi.min(v + MAX_ERROR_PT / (d * item.page_space_factor));
        }
        self.members.push(i);
        true
    }
    fn charproc(&self, shapes: &[Shape]) -> Vec<u8> {
        let mut out = b"0 0 d0\n".to_vec();
        let mut idx = 0;
        for op in &shapes[self.seed].ops {
            for _ in &op.args {
                let v = f64::midpoint(self.low[idx], self.high[idx]);
                out.extend_from_slice(pdf_num(v).as_bytes());
                out.push(b' ');
                idx += 1;
            }
            out.extend_from_slice(&op.token);
            out.push(b'\n');
        }
        out.extend_from_slice(&shapes[self.seed].paint);
        out.push(b'\n');
        out
    }
}

fn group(shapes: &[Shape]) -> Vec<Group> {
    let mut candidates = BTreeMap::<Vec<u8>, Vec<usize>>::new();
    for (i, item) in shapes.iter().enumerate() {
        candidates.entry(item.skeleton.clone()).or_default().push(i);
    }
    let mut output = Vec::new();
    for members in candidates.into_values() {
        let mut current = Vec::<Group>::new();
        for i in members {
            if !current.iter_mut().any(|g| g.add(i, shapes)) {
                current.push(Group::first(i, &shapes[i]));
            }
        }
        output.extend(
            current
                .into_iter()
                .filter(|g| g.members.len() >= MIN_OUTLINED_GLYPH_OCCURRENCES),
        );
    }
    output
}

fn replacement(font: &[u8], code: usize, shape: &Shape) -> Option<Vec<u8>> {
    let code = u8::try_from(code).ok()?;
    if code == 0 {
        return None;
    }
    // Replace only the original path and fill operator. The surrounding
    // clipping, current colors, CTM, transparency and marked-content stay.
    // q/Q here ensure the new BT/ET state cannot escape its painting site.
    let mut out = b"q\n/Span << /ActualText () >> BDC\nBT 0 Tr 0 Ts 0 Tc 0 Tw 100 Tz /".to_vec();
    out.extend_from_slice(font);
    out.extend_from_slice(b" 1 Tf ");
    for v in [
        shape.bounds[2] - shape.bounds[0],
        0.,
        0.,
        shape.bounds[3] - shape.bounds[1],
        shape.bounds[0],
        shape.bounds[1],
    ] {
        out.extend_from_slice(pdf_num(v).as_bytes());
        out.push(b' ');
    }
    out.extend_from_slice(b"Tm <");
    out.extend_from_slice(format!("{code:02X}").as_bytes());
    out.extend_from_slice(b"> Tj ET\nEMC\nQ\n");
    Some(out)
}

const MIN_OCR_USEFUL_OCCURRENCES: usize = 64;

struct PageContents {
    page: ObjectHandle,
    decoded: Vec<u8>,
}

struct CurvedPlan {
    plans: Vec<GlyphFontPlan>,
    font_pages: Vec<BTreeSet<usize>>,
    edits: Vec<Vec<(usize, usize, Vec<u8>)>>,
    replacements: usize,
}

fn collect_sources(
    doc: &EditDocument,
    pages: &[ObjectHandle],
) -> Result<(Vec<PageContents>, Vec<Shape>)> {
    let mut sources = Vec::<PageContents>::new();
    let mut shapes = Vec::<Shape>::new();
    for &page in pages {
        let Some(obj) = doc.current_owned_object(page)? else {
            continue;
        };
        let Some(dict) = obj.as_dictionary() else {
            continue;
        };
        let Some(contents) = dict.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(doc, contents, &mut decoded)?;
        let Some(found) = collect(&decoded) else {
            continue;
        };
        if found.is_empty() {
            continue;
        }
        let page_index = sources.len();
        for mut shape in found {
            shape.page_index = page_index;
            shapes.push(shape);
        }
        sources.push(PageContents { page, decoded });
    }
    Ok((sources, shapes))
}

fn prepare_plans(
    doc: &EditDocument,
    sources: &[PageContents],
    shapes: &[Shape],
    groups: &[Group],
) -> Result<CurvedPlan> {
    // Avoid colliding with any existing page-local font resource, including
    // Type3 fonts installed by the earlier, exact polyline factorer.
    let mut occupied = BTreeSet::new();
    for source in sources {
        let resources = effective_page_resources(doc, source.page)?;
        if let Some(fonts) = resolved_dictionary(doc, resources.get(b"Font".as_slice()))? {
            occupied.extend(fonts.keys().cloned());
        }
    }

    let mut edits = vec![Vec::<(usize, usize, Vec<u8>)>::new(); sources.len()];
    let mut plans = Vec::<GlyphFontPlan>::new();
    let mut font_pages = Vec::<BTreeSet<usize>>::new();
    let mut replacements = 0_usize;
    for (font_index, chunk) in groups.chunks(MAX_TYPE3_GLYPHS_PER_FONT).enumerate() {
        let name = outlined_glyph_font_name(font_index, &mut occupied);
        let mut entries = Vec::new();
        let mut users = BTreeSet::new();
        for candidate in chunk {
            let code = entries.len() + 1;
            let mut planned = Vec::new();
            for &i in &candidate.members {
                let appearance = &shapes[i];
                let Some(data) = replacement(&name, code, appearance) else {
                    planned.clear();
                    break;
                };
                planned.push((
                    appearance.page_index,
                    appearance.start,
                    appearance.end,
                    data,
                ));
            }
            if planned.is_empty() {
                continue;
            }
            for (page_index, start, end, data) in planned {
                edits[page_index].push((start, end, data));
                users.insert(page_index);
                replacements += 1;
            }
            entries.push(GlyphShapePlan {
                canonical: GlyphComponent {
                    start: 0,
                    end: 0,
                    origin_x: 0,
                    origin_y: 0,
                    width: 100,
                    height: 100,
                    source_paints: 1,
                    signature: Vec::new(),
                },
                occurrences: Vec::new(),
                charproc: candidate.charproc(shapes),
            });
        }
        if !entries.is_empty() {
            plans.push(GlyphFontPlan {
                name,
                shapes: entries,
            });
            font_pages.push(users);
        }
    }
    Ok(CurvedPlan {
        plans,
        font_pages,
        edits,
        replacements,
    })
}

/// Share filled glyphs across the whole document. OCR can label each
/// canonical once even when appearances span different pages.
pub(super) fn factor(
    doc: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
) -> Result<OutlinedGlyphFactorStats> {
    let (sources, shapes) = collect_sources(doc, pages)?;
    let mut stats = OutlinedGlyphFactorStats::default();
    if shapes.is_empty() {
        return Ok(stats);
    }
    let mut groups = group(&shapes);
    groups.sort_unstable_by_key(|g| {
        std::cmp::Reverse(
            g.members
                .iter()
                .map(|&i| shapes[i].end - shapes[i].start)
                .sum::<usize>(),
        )
    });
    if groups.is_empty() {
        return Ok(stats);
    }

    let CurvedPlan {
        plans,
        font_pages,
        edits,
        replacements,
    } = prepare_plans(doc, &sources, &shapes, &groups)?;
    if replacements == 0 {
        return Ok(stats);
    }
    // Compare realistic per-page compressed streams, counting shared font
    // objects once, rather than pretending all page content is one stream.
    let mut rewritten = Vec::new();
    let (mut before, mut after) = (0_usize, 0_usize);
    let mut affected_pages = 0_usize;
    for (source, page_edits) in sources.iter().zip(&edits) {
        if page_edits.is_empty() {
            rewritten.push(None);
            continue;
        }
        let Some(content) = crate::content_stream::apply_replacements(&source.decoded, page_edits)
        else {
            return Ok(stats);
        };
        before += crate::stream_codec::flate_encoded_len(&source.decoded, flate_level)?;
        after += crate::stream_codec::flate_encoded_len(&content, flate_level)?;
        rewritten.push(Some(content));
        affected_pages += 1;
    }
    let fonts_overhead = outlined_glyph_estimated_after_flate(b"", &plans, flate_level)?
        .saturating_sub(crate::stream_codec::flate_encoded_len(b"", flate_level)?);
    after = after.saturating_add(fonts_overhead);

    // Processing values OCR-ready reusable outlines, not only compressed size.
    // Cap the extra storage cost rather than outright requiring a Flate win.
    if replacements < MIN_OCR_USEFUL_OCCURRENCES || after > before.saturating_add(before / 20) {
        return Ok(stats);
    }

    for (plan, users) in plans.iter().zip(&font_pages) {
        let font = outlined_glyph_font_dictionary(doc, plan);
        for &index in users {
            install_page_resource(doc, sources[index].page, b"Font", plan.name.clone(), font)?;
        }
    }
    for (source, content) in sources.iter().zip(rewritten) {
        if let Some(content) = content {
            stats.decoded_bytes_removed += source.decoded.len().saturating_sub(content.len());
            replace_page_content(doc, source.page, content)?;
        }
    }
    stats.pages_rewritten = affected_pages;
    stats.fonts_created = plans.len();
    stats.glyphs_created = plans.iter().map(|p| p.shapes.len()).sum();
    stats.occurrences_replaced = replacements;
    stats.source_paints_replaced = replacements;
    stats.estimated_flate_bytes_saved = before.saturating_sub(after);
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shape(width: f64, bulge: f64) -> Vec<u8> {
        format!("q 1 0 0 1 20 30 cm 0 0 m {width} 0 l {width} 1 l {width} 1.5 {width} {bulge} {width} 2 c {width} 3 l {width} 4 l 0 4 l 0 3 l 0 2 l h f Q\n").into_bytes()
    }
    #[test]
    fn groups_squashed_bezier_and_produces_type3_body() {
        let mut b = shape(4., 2.);
        b.extend(shape(5., 2.));
        let samples = collect(&b).expect("parse");
        assert_eq!(samples.len(), 2);
        let g = group(&samples);
        assert_eq!(g.len(), 1);
        assert!(g[0].charproc(&samples).starts_with(b"0 0 d0\n"));
        let text = replacement(b"Test", 1, &samples[1]).expect("placement");
        assert!(text.windows(4).any(|x| x == b"BT 0"));
        assert!(text.ends_with(b"ET\nEMC\nQ\n"));
        assert!(text.starts_with(b"q\n/Span"));
        assert!(
            text.windows(b"/ActualText ()".len())
                .any(|w| w == b"/ActualText ()")
        );
    }
    #[test]
    fn refuses_different_shapes_and_interleaved_state() {
        let mut bytes = shape(4., 2.);
        bytes.extend(shape(4., 3.));
        let appearances = collect(&bytes).expect("parse");
        assert_eq!(appearances.len(), 2);
        assert!(group(&appearances).is_empty());
        bytes.extend(b"q 1 0 0 1 0 0 cm 0 0 m 3 0 l 3 4 l 0 4 l 1 3 l 2 2 l 2 1 l 0 1 l 0.5 g f Q");
        assert_eq!(collect(&bytes).expect("parse").len(), 2);
    }

    #[test]
    fn recognizes_fills_inside_existing_graphics_and_marked_content_scopes() {
        let data = b"/OC /Glyphs BDC q 2 0 0 1 0 0 cm /GS1 gs \
          9 9 2 3 re 9 9 m 11 12 l f \
          13 9 2 3 re 13 9 m 15 12 l f Q EMC";
        let found = collect(data).expect("source content");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].bounds, [9.0, 9.0, 11.0, 12.0]);
        assert_eq!(found[1].bounds, [13.0, 9.0, 15.0, 12.0]);
        assert_eq!(found[0].page_space_factor, 2.0);
        let groups = group(&found);
        assert_eq!(groups.len(), 1);
        let rendered = replacement(b"Test", 1, &found[1]).expect("glyph rendering");
        assert!(rendered.starts_with(b"q\n/Span"));
        assert!(!rendered.windows(5).any(|part| part == b" cm\n"));
        assert!(rendered.ends_with(b"EMC\nQ\n"));
    }

    #[test]
    fn preserves_clipping_and_excludes_incomplete_paths() {
        let data = b"q 0 0 100 100 re W n \
            1 1 m 2 1 l 2 2 l f \
            3 3 m 4 3 l 4 4 l W n \
            7 7 m 8 7 l 8 8 l f Q";
        let found = collect(data).expect("source content");
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].bounds, [1.0, 1.0, 2.0, 2.0]);
        assert_eq!(found[1].bounds, [7.0, 7.0, 8.0, 8.0]);
    }

    #[test]
    fn nested_transforms_constrain_canonical_error_in_page_space() {
        let a = b"q 20 0 0 20 1 2 cm \
            0 0 m 4 0 l 4 4 l 0 4 l 0 0 l f Q";
        let b = b"q 1 0 0 1 1 2 cm \
            0 0 m 4 0 l 4 4 l 0.04 4 l 0 0 l f Q";
        let found_a = collect(a).expect("source a");
        let found_b = collect(b).expect("source b");
        assert_eq!(found_a[0].page_space_factor, 20.0);
        let mut both = found_a;
        both.extend(found_b);
        // Dimensions differ; a single canonical would violate the 0.02pt
        // transformed error budget on the 20x-scaled member.
        assert!(group(&both).is_empty());
    }

    /// Optional corpus probe: `PDF_REDOX_CORPUS_PDF=/path/to/file.pdf`.
    /// Deliberately ignored in CI because no fixture PDF is checked in.
    #[test]
    #[ignore = "requires an external source PDF supplied via an environment variable"]
    fn corpus_outline_coverage_diagnostic() -> Result<()> {
        let Ok(path) = std::env::var("PDF_REDOX_CORPUS_PDF") else {
            return Ok(());
        };
        let input = std::fs::read(&path)?;
        let doc = EditDocument::from_bytes(input)?;
        let pages = doc.page_handles()?;
        let (sources, shapes) = collect_sources(&doc, &pages)?;
        let groups = group(&shapes);
        let all_members: usize = groups.iter().map(|g| g.members.len()).sum();
        let unique_pages: BTreeSet<_> = shapes.iter().map(|s| s.page_index).collect();
        eprintln!(
            "CORPUS_OUTLINE_PROBE pdf={path} pages={} content_pages={} shapes={} \
             groups={} grouped_occurrences={} candidate_pages={}",
            pages.len(),
            sources.len(),
            shapes.len(),
            groups.len(),
            all_members,
            unique_pages.len()
        );
        let mut operators = BTreeMap::<usize, usize>::new();
        let mut sizes = BTreeMap::<usize, usize>::new();
        for shape in &shapes {
            *operators.entry(shape.ops.len()).or_default() += 1;
            let dim = (shape.bounds[2] - shape.bounds[0]).max(shape.bounds[3] - shape.bounds[1]);
            let bin = if dim < 2.0 {
                0
            } else if dim < 8.0 {
                1
            } else if dim < 16.0 {
                2
            } else if dim < 64.0 {
                3
            } else {
                4
            };
            *sizes.entry(bin).or_default() += 1;
        }
        let shortest: usize = operators
            .iter()
            .filter(|(n, _)| **n < 8)
            .map(|(_, v)| v)
            .sum();
        eprintln!(
            "CORPUS_OUTLINE_PROBE short_ops={} size_bins={} most_common_ops={:?}",
            shortest,
            format_args!("{sizes:?}"),
            operators.iter().rev().take(12).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[test]
    fn emits_real_type3_fonts_from_repeated_curved_paths() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        let mut content = Vec::new();
        for i in 0_u32..5000 {
            // Distinct but near-identical control coordinates model the PDF
            // corpus more accurately than a trivially Flate-repeated path.
            let jitter = |k: u32| -> f64 {
                f64::from(((i * 97_531 + k * 1_000_003) ^ ((i + 3 * k) * 77_311)) % 100_003)
                    / 10_000_000.0
            };
            let w = 4.0 + jitter(0);
            let x = i % 40;
            let y = i / 40;
            content.extend_from_slice(
                format!(
                    "q 1 0 0 1 {x} {y} cm {} {} m {w} {} l {} {} l {} {} {} {} {} {} c {} {} l {} {} l {} {} l {} {} l {} {} l h f Q\n",
                    jitter(1), jitter(2), jitter(3),
                    w - jitter(4), 1.0 + jitter(5),
                    w - jitter(6), 1.5 + jitter(7),
                    w + jitter(8), 2.0 + jitter(9),
                    w + jitter(18), 2.4 + jitter(19),
                    w, 3.0 + jitter(10),
                    w, 4.0 + jitter(11),
                    jitter(12), 4.0 + jitter(13),
                    jitter(14), 3.0 + jitter(15),
                    jitter(16), 2.0 + jitter(17),
                ).as_bytes(),
            );
        }
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 150] /Resources << >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", &content)?;
        let mut doc = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = doc.page_handles()?;
        let stats = factor(&mut doc, &pages, crate::FlateLevel::PROCESSING)?;
        assert_eq!(stats.pages_rewritten, 1, "{stats:?}");
        assert!(stats.fonts_created >= 1);
        assert!(stats.occurrences_replaced >= 4000, "{stats:?}");
        let object = doc.current_owned_object(pages[0])?.expect("page");
        let contents = object
            .as_dictionary()
            .expect("dictionary")
            .get(b"Contents".as_slice())
            .expect("contents");
        let mut rendered = Vec::new();
        decoded_content_value(&doc, contents, &mut rendered)?;
        assert!(rendered.windows(b" Tj ET".len()).any(|w| w == b" Tj ET"));
        assert!(!rendered.windows(b" 1.5 ".len()).any(|w| w == b" 1.5 "));
        Ok(())
    }

    #[test]
    fn refuses_open_path_or_text_context() {
        let mut data = b"0 0 m ".to_vec();
        data.extend(shape(4., 2.));
        assert!(collect(&data).expect("parse").is_empty());
        let mut data = b"BT ".to_vec();
        data.extend(shape(4., 2.));
        data.extend(b" ET");
        assert!(collect(&data).expect("parse").is_empty());
    }
}
