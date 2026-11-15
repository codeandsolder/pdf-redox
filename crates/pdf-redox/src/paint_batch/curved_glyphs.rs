//! Shared Type3 shapes for isolated, filled, Bézier or line outlines.
//! Discovery is approximate, but canonical membership is checked across
//! every occurrence using the same local-point coordinate bound.
use super::*;

const SEED_EPS: f64 = 0.012;
const MAX_ERROR_PT: f64 = 0.02;
const MAX_EXTENT: f64 = 16.0;

#[derive(Clone, Debug)]
struct ShapeOp {
    token: Vec<u8>,
    args: Vec<f64>,
}

#[derive(Clone, Debug)]
struct Shape {
    start: usize,
    end: usize,
    prefix: Vec<u8>,
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

fn read_shape(input: &[u8], events: &[Event], i: usize, outer: [f64; 6]) -> Option<(Shape, usize)> {
    let q = events.get(i)?;
    let cm = events.get(i + 1)?;
    if q.operator != b"q"
        || q.operand_count != 0
        || cm.operator != b"cm"
        || !cm.all_operands_numeric
        || cm.operand_count != 6
        || !adjacent(input, q, cm)
    {
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
    let mut cursor = i + 2;
    while let Some(e) = events.get(cursor) {
        let Some(need) = arity(&e.operator) else {
            break;
        };
        if !e.all_operands_numeric
            || e.operand_count != need
            || !adjacent(input, events.get(cursor - 1)?, e)
        {
            return None;
        }
        let args = e.numbers.iter().copied().collect::<Vec<_>>();
        for v in args.as_chunks::<2>().0 {
            bounds[0] = bounds[0].min(v[0]);
            bounds[1] = bounds[1].min(v[1]);
            bounds[2] = bounds[2].max(v[0]);
            bounds[3] = bounds[3].max(v[1]);
        }
        if e.operator == b"re" {
            bounds[0] = bounds[0].min(args[0] + args[2]);
            bounds[1] = bounds[1].min(args[1] + args[3]);
            bounds[2] = bounds[2].max(args[0] + args[2]);
            bounds[3] = bounds[3].max(args[1] + args[3]);
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
    let restore = events.get(cursor + 1)?;
    if ops.len() < 8
        || !matches!(ops.first()?.token.as_slice(), b"m" | b"re")
        || !matches!(paint.operator.as_slice(), b"f" | b"F" | b"f*")
        || paint.operand_count != 0
        || restore.operator != b"Q"
        || restore.operand_count != 0
        || !adjacent(input, events.get(cursor - 1)?, paint)
        || !adjacent(input, paint, restore)
    {
        return None;
    }
    let width = bounds[2] - bounds[0];
    let height = bounds[3] - bounds[1];
    if !(width > 1.5 && height > 2.0 && width <= MAX_EXTENT && height <= MAX_EXTENT) {
        return None;
    }
    let (normalized, dimensions) = normalize_control_points(&ops, bounds, width, height);
    skeleton.extend_from_slice(&paint.operator);
    let combined = compose_matrix(outer, matrix_from_event(cm)?);
    let page_space_factor = (combined[0].abs() + combined[2].abs())
        .max(combined[1].abs() + combined[3].abs())
        .max(1.0);
    if !page_space_factor.is_finite() {
        return None;
    }
    Some((
        Shape {
            start: q.start,
            end: restore.end,
            prefix: input.get(q.start..cm.end)?.to_vec(),
            paint: paint.operator.clone(),
            ops,
            skeleton,
            normalized,
            dimensions,
            bounds,
            page_space_factor,
        },
        cursor + 2,
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
            && e.operator == b"q"
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
    let mut out = shape.prefix.clone();
    // Explicitly set text parameters: the q/Q pair restores the caller state.
    out.extend_from_slice(b"\n/Span << /ActualText () >> BDC\nBT 0 Tr 0 Ts 0 Tc 0 Tw 100 Tz /");
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

pub(super) fn factor(
    doc: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
) -> Result<OutlinedGlyphFactorStats> {
    let mut stats = OutlinedGlyphFactorStats::default();
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
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let Some(shapes) = collect(&decoded) else {
            continue;
        };
        let mut groups = group(&shapes);
        if groups.is_empty() {
            continue;
        }
        groups.sort_unstable_by_key(|g| {
            std::cmp::Reverse(
                g.members
                    .iter()
                    .map(|&i| shapes[i].end - shapes[i].start)
                    .sum::<usize>(),
            )
        });
        let resources = effective_page_resources(doc, page)?;
        let mut occupied = resolved_dictionary(doc, resources.get(b"Font".as_slice()))?
            .map_or_default(|fonts| fonts.keys().cloned().collect::<BTreeSet<_>>());
        let mut plans = Vec::<GlyphFontPlan>::new();
        let mut edits = Vec::new();
        let mut count = 0;
        for (font_index, chunk) in groups.chunks(MAX_TYPE3_GLYPHS_PER_FONT).enumerate() {
            let name = outlined_glyph_font_name(font_index, &mut occupied);
            let mut entries = Vec::new();
            for g in chunk {
                let code = entries.len() + 1;
                let mut replacements = Vec::new();
                for &i in &g.members {
                    let Some(bytes) = replacement(&name, code, &shapes[i]) else {
                        replacements.clear();
                        break;
                    };
                    replacements.push((shapes[i].start, shapes[i].end, bytes));
                }
                if replacements.is_empty() {
                    continue;
                }
                count += replacements.len();
                edits.extend(replacements);
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
                    charproc: g.charproc(&shapes),
                });
            }
            if !entries.is_empty() {
                plans.push(GlyphFontPlan {
                    name,
                    shapes: entries,
                });
            }
        }
        let Some(rewritten) = crate::content_stream::apply_replacements(&decoded, &edits) else {
            continue;
        };
        let before = crate::stream_codec::flate_encoded_len(&decoded, flate_level)?;
        let after = outlined_glyph_estimated_after_flate(&rewritten, &plans, flate_level)?;
        if before <= after || before - after < MIN_OUTLINED_GLYPH_ESTIMATED_SAVINGS {
            continue;
        }
        for plan in &plans {
            let font = outlined_glyph_font_dictionary(doc, plan);
            install_page_resource(doc, page, b"Font", plan.name.clone(), font)?;
        }
        replace_page_content(doc, page, rewritten.clone())?;
        stats.pages_rewritten += 1;
        stats.fonts_created += plans.len();
        stats.glyphs_created += plans.iter().map(|p| p.shapes.len()).sum::<usize>();
        stats.occurrences_replaced += count;
        stats.source_paints_replaced += count;
        stats.decoded_bytes_removed += decoded.len().saturating_sub(rewritten.len());
        stats.estimated_flate_bytes_saved += before - after;
    }
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
