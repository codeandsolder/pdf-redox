//! Shared Type3 shapes for painted Bézier and line outlines in any graphics scope.
//! Discovery is approximate, but canonical membership is checked across
//! every occurrence using the same local-point coordinate bound.
use super::{
    BTreeMap, BTreeSet, EditDocument, Event, GlyphComponent, GlyphFontPlan, GlyphShapePlan,
    MAX_TYPE3_GLYPHS_PER_FONT, MIN_OUTLINED_GLYPH_OCCURRENCES, ObjectHandle,
    OutlinedGlyphFactorStats, Result, adjacent, effective_page_resources, events_for,
    install_page_resource, outlined_glyph_estimated_after_flate, outlined_glyph_font_dictionary,
    outlined_glyph_font_name, replace_page_content, resolved_dictionary,
};
use crate::content::page_content_if_present;
use crate::geometry::compose_affine_matrix;
use crate::glyph_catalog::{GlyphCatalog, GlyphOccurrence, GlyphTemplate};
use sha2::{Digest as _, Sha256};

const SEED_EPS: f64 = 0.012;
const MAX_ERROR_PT: f64 = 0.02;

#[derive(Clone, Debug)]
struct ShapeOp {
    token: Vec<u8>,
    args: Vec<f64>,
    start: usize,
    end: usize,
}

#[derive(Clone, Debug)]
struct Shape {
    start: usize,
    end: usize,
    page_index: usize,
    fill_index: usize,
    raw_subpaths: Vec<(usize, Vec<u8>)>,
    paint: Vec<u8>,
    ops: Vec<ShapeOp>,
    skeleton: Vec<u8>,
    normalized: Vec<f64>,
    dimensions: Vec<f64>,
    bounds: [f64; 4],
    page_space_factor: f64,
    min_page_scale: f64,
    effective_ctm: [f64; 6],
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
    let matrix: [f64; 6] = e.numbers().try_into().ok()?;
    matrix
        .iter()
        .all(|value| value.is_finite())
        .then_some(matrix)
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
        if !e.all_operands_numeric()
            || e.operand_count != need
            || (cursor > i && !adjacent(input, events.get(cursor - 1)?, e))
        {
            return None;
        }
        let args = e.numbers().to_vec();
        if e.operator.as_slice() == b"re" {
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
            token: e.operator.to_vec(),
            args,
            start: e.start,
            end: e.end,
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
    // Lower bound on the minimum singular value (|det| / Frobenius).
    // Combined with the separation gap, this ensures independently painted
    // components cannot touch under any affine orientation.
    let [xx, yx, xy, yy, _, _] = combined;
    let linear_norm = xx.hypot(yx).hypot(xy).hypot(yy);
    let min_page_scale = if linear_norm > 0. {
        xx.mul_add(yy, -yx * xy).abs() / linear_norm
    } else {
        0.
    };
    if !page_space_factor.is_finite() || normalized.iter().any(|n| !n.is_finite()) {
        return None;
    }
    Some((
        Shape {
            start: first.start,
            end: paint.end,
            page_index: 0,
            fill_index: 0,
            raw_subpaths: Vec::new(),
            paint: paint.operator.to_vec(),
            ops,
            skeleton,
            normalized,
            dimensions,
            bounds,
            page_space_factor,
            min_page_scale,
            effective_ctm: combined,
        },
        cursor + 1,
    ))
}

/// A paint transaction remains a single replacement span, even when its
/// disjoint contour components are shared as separate Type3 glyphs.
#[derive(Clone, Debug)]
struct CompoundFill {
    start: usize,
    end: usize,
    page_index: usize,
    components: Vec<usize>,
    paint: Vec<u8>,
}

#[derive(Clone)]
struct Subpath {
    ops: Vec<ShapeOp>,
    bounds: [f64; 4],
}

fn path_bounds(ops: &[ShapeOp]) -> Option<[f64; 4]> {
    let mut bbox = [
        f64::INFINITY,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NEG_INFINITY,
    ];
    for op in ops {
        if op.token == b"re" {
            let [x, y, w, h]: [f64; 4] = op.args.as_slice().try_into().ok()?;
            for (a, b) in [(x, y), (x + w, y + h)] {
                bbox[0] = bbox[0].min(a);
                bbox[1] = bbox[1].min(b);
                bbox[2] = bbox[2].max(a);
                bbox[3] = bbox[3].max(b);
            }
        } else {
            for [x, y] in op.args.as_chunks::<2>().0 {
                bbox[0] = bbox[0].min(*x);
                bbox[1] = bbox[1].min(*y);
                bbox[2] = bbox[2].max(*x);
                bbox[3] = bbox[3].max(*y);
            }
        }
    }
    bbox.iter().all(|v| v.is_finite()).then_some(bbox)
}

/// A small ink separation margin in page coordinates reduces opportunities
/// for antialias fringes to overlap after decomposing one original fill.
const COMPONENT_SEPARATION_PT: f64 = 0.15;

const fn root(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn independent_components(input: &[u8], original: &Shape) -> Option<Vec<Shape>> {
    // A singular or near-singular CTM cannot guarantee disjointness after
    // rasterization; leave its complete original fill untouched.
    let mut parts = Vec::<Subpath>::new();
    for op in &original.ops {
        if op.token == b"m" || op.token == b"re" {
            parts.push(Subpath {
                ops: Vec::new(),
                bounds: [0.; 4],
            });
        }
        parts.last_mut()?.ops.push(op.clone());
    }
    if parts.is_empty() {
        return None;
    }
    for p in &mut parts {
        p.bounds = path_bounds(&p.ops)?;
    }

    if !original.min_page_scale.is_finite() || original.min_page_scale < 1e-8 {
        return None;
    }
    // The margin in local coordinates is deliberately larger when a page
    // transform strongly compresses the shape along any direction.
    let gap = COMPONENT_SEPARATION_PT / original.min_page_scale;
    let n = parts.len();
    let mut parent = (0..n).collect::<Vec<_>>();
    let mut order = (0..n).collect::<Vec<_>>();
    order.sort_unstable_by(|&a, &b| parts[a].bounds[0].total_cmp(&parts[b].bounds[0]));
    // Sorted x sweep avoids comparing the whole path's thousands of contours.
    for i in 0..n {
        let a = order[i];
        let bx = parts[a].bounds;
        for &b in &order[i + 1..] {
            let by = parts[b].bounds;
            if by[0] - bx[2] > gap {
                break;
            }
            if by[1] - bx[3] > gap || bx[1] - by[3] > gap {
                continue;
            }
            let x = root(&mut parent, a);
            let y = root(&mut parent, b);
            if x != y {
                parent[y] = x;
            }
        }
    }
    let mut bins = BTreeMap::<usize, Vec<usize>>::new();
    for i in 0..n {
        bins.entry(root(&mut parent, i)).or_default().push(i);
    }
    let mut result = Vec::with_capacity(bins.len());
    for indexes in bins.into_values() {
        let mut ops = Vec::new();
        let mut raw_subpaths = Vec::new();
        let mut bounds = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        for i in indexes {
            let part = &parts[i];
            bounds[0] = bounds[0].min(part.bounds[0]);
            bounds[1] = bounds[1].min(part.bounds[1]);
            bounds[2] = bounds[2].max(part.bounds[2]);
            bounds[3] = bounds[3].max(part.bounds[3]);
            let first = part.ops.first()?.start;
            let last = part.ops.last()?.end;
            raw_subpaths.push((first, input.get(first..last)?.to_vec()));
            ops.extend(part.ops.iter().cloned());
        }
        let mut skeleton = Vec::new();
        for op in &ops {
            skeleton.push(u8::try_from(op.args.len()).ok()?);
            skeleton.extend_from_slice(&op.token);
            skeleton.push(0);
        }
        skeleton.extend_from_slice(&original.paint);
        let width = bounds[2] - bounds[0];
        let height = bounds[3] - bounds[1];
        let (normalized, dimensions) = if width > 0. && height > 0. {
            normalize_control_points(&ops, bounds, width, height)
        } else {
            // Empty-ink or degenerate components are kept as residual source
            // but cannot be pooled into a nonzero-dimensional glyph.
            (Vec::new(), Vec::new())
        };
        // Do not clone the entire parent paint for every child component:
        // a DEFOND fill can contain hundreds of subpaths and thousands of
        // Bézier control points.
        result.push(Shape {
            start: original.start,
            end: original.end,
            page_index: original.page_index,
            fill_index: original.fill_index,
            raw_subpaths,
            paint: original.paint.clone(),
            ops,
            skeleton,
            normalized,
            dimensions,
            bounds,
            page_space_factor: original.page_space_factor,
            min_page_scale: original.min_page_scale,
            effective_ctm: original.effective_ctm,
        });
    }
    Some(result)
}

fn collect_compound(input: &[u8]) -> Option<(Vec<Shape>, Vec<CompoundFill>)> {
    let events = events_for(input)?;
    let (mut cursor, mut text, mut path_open) = (0, false, false);
    let mut ctm = [1., 0., 0., 1., 0., 0.];
    let mut stack = Vec::new();
    let mut shapes = Vec::new();
    let mut fills = Vec::new();
    while cursor < events.len() {
        let e = &events[cursor];
        if e.operator.as_slice() == b"BT" {
            text = true;
        }
        if e.operator.as_slice() == b"ET" {
            text = false;
        }
        if !text
            && !path_open
            && matches!(e.operator.as_slice(), b"m" | b"re")
            && let Some((composite, next)) = read_shape(input, &events, cursor, ctm)
            && let Some(parts) = independent_components(input, &composite)
        {
            let index = fills.len();
            let mut component_indices = Vec::new();
            for mut part in parts {
                part.fill_index = index;
                component_indices.push(shapes.len());
                shapes.push(part);
            }
            fills.push(CompoundFill {
                start: composite.start,
                end: composite.end,
                page_index: 0,
                components: component_indices,
                paint: composite.paint,
            });
            cursor = next;
            continue;
        }
        match e.operator.as_slice() {
            b"q" => stack.push(ctm),
            b"Q" => ctm = stack.pop()?,
            b"cm" => ctm = compose_affine_matrix(ctm, matrix_from_event(e)?),
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
    Some((shapes, fills))
}

#[cfg(test)]
fn collect(input: &[u8]) -> Option<Vec<Shape>> {
    Some(collect_compound(input)?.0)
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
        if !item.normalized.is_empty()
            && !item.ops.is_empty()
            && item.normalized.iter().all(|v| v.is_finite())
        {
            candidates.entry(item.skeleton.clone()).or_default().push(i);
        }
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
    page_number: usize,
    decoded: Vec<u8>,
}

struct CurvedPlan {
    plans: Vec<GlyphFontPlan>,
    font_pages: Vec<BTreeSet<usize>>,
    edits: Vec<Vec<(usize, usize, Vec<u8>)>>,
    replacements: usize,
    paint_ops_replaced: usize,
}

fn collect_sources(
    doc: &EditDocument,
    pages: &[ObjectHandle],
) -> Result<(Vec<PageContents>, Vec<Shape>, Vec<CompoundFill>)> {
    let mut sources = Vec::<PageContents>::new();
    let mut shapes = Vec::<Shape>::new();
    let mut fills = Vec::<CompoundFill>::new();
    for (actual_page_index, &page) in pages.iter().enumerate() {
        let Some(decoded) = page_content_if_present(doc, page)? else {
            continue;
        };
        let Some((found, found_fills)) = collect_compound(&decoded) else {
            continue;
        };
        if found.is_empty() {
            continue;
        }
        let page_index = sources.len();
        let component_offset = shapes.len();
        let fill_offset = fills.len();
        for mut shape in found {
            shape.page_index = page_index;
            shape.fill_index += fill_offset;
            shapes.push(shape);
        }
        for mut fill in found_fills {
            fill.page_index = page_index;
            for idx in &mut fill.components {
                *idx += component_offset;
            }
            fills.push(fill);
        }
        sources.push(PageContents {
            page,
            page_number: actual_page_index + 1,
            decoded,
        });
    }
    Ok((sources, shapes, fills))
}

fn rewrite_compound_fill(
    fill: &CompoundFill,
    shapes: &[Shape],
    chosen: &[Option<(Vec<u8>, usize)>],
) -> Option<(Vec<u8>, usize)> {
    let selected = fill
        .components
        .iter()
        .filter(|&&i| chosen[i].is_some())
        .count();
    if selected == 0 {
        return None;
    }
    let mut output = Vec::new();
    let mut kept = Vec::<(usize, &[u8])>::new();
    for &i in &fill.components {
        if chosen[i].is_none() {
            kept.extend(
                shapes[i]
                    .raw_subpaths
                    .iter()
                    .map(|(order, data)| (*order, data.as_slice())),
            );
        }
    }
    // All unresolved components are painted *once*, using the original
    // winding/even-odd fill operator and unchanged coordinates.
    if !kept.is_empty() {
        kept.sort_unstable_by_key(|(offset, _)| *offset);
        for (_, raw) in kept {
            output.extend_from_slice(raw);
            output.push(b'\n');
        }
        output.extend_from_slice(&fill.paint);
        output.push(b'\n');
    }
    // Components moved to Type3 have AABBs separated from all residual
    // components, so the original source operation's simultaneous fill
    // cannot contribute compositing interactions between them.
    for &i in &fill.components {
        if let Some((name, code)) = &chosen[i] {
            let bytes = replacement(name, *code, &shapes[i])?;
            output.extend_from_slice(&bytes);
        }
    }
    Some((output, selected))
}

fn prepare_plans(
    doc: &EditDocument,
    sources: &[PageContents],
    shapes: &[Shape],
    groups: &[Group],
    fills: &[CompoundFill],
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
    let mut chosen = vec![None::<(Vec<u8>, usize)>; shapes.len()];
    for (font_index, chunk) in groups.chunks(MAX_TYPE3_GLYPHS_PER_FONT).enumerate() {
        let name = outlined_glyph_font_name(font_index, &mut occupied);
        let mut entries = Vec::new();
        let mut users = BTreeSet::new();
        for candidate in chunk {
            let code = entries.len() + 1;
            if u8::try_from(code).is_err() {
                continue;
            }
            for &i in &candidate.members {
                chosen[i] = Some((name.clone(), code));
                users.insert(shapes[i].page_index);
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

    let mut replacements = 0_usize;
    let mut paint_ops_replaced = 0_usize;
    for fill in fills {
        if let Some((output, selected)) = rewrite_compound_fill(fill, shapes, &chosen) {
            paint_ops_replaced += 1;
            replacements += selected;
            edits[fill.page_index].push((fill.start, fill.end, output));
        }
    }

    Ok(CurvedPlan {
        plans,
        font_pages,
        edits,
        replacements,
        paint_ops_replaced,
    })
}

const fn transformed_corner(matrix: [f64; 6], x: f64, y: f64) -> [f64; 2] {
    [
        matrix[0].mul_add(x, matrix[2].mul_add(y, matrix[4])),
        matrix[1].mul_add(x, matrix[3].mul_add(y, matrix[5])),
    ]
}

fn candidate_svg_path(group: &Group, shapes: &[Shape]) -> String {
    use std::fmt::Write as _;
    let canonical = group
        .low
        .iter()
        .zip(&group.high)
        .map(|(lo, hi)| f64::midpoint(*lo, *hi))
        .collect::<Vec<_>>();
    let mut path = String::new();
    let mut current = [0.0, 0.0];
    let mut subpath_start = current;
    let mut pos = 0_usize;
    for op in &shapes[group.seed].ops {
        let values = &canonical[pos..pos + op.args.len()];
        pos += op.args.len();
        match op.token.as_slice() {
            b"m" => {
                let _ = write!(&mut path, "M{} {} ", pdf_num(values[0]), pdf_num(values[1]));
                current = [values[0], values[1]];
                subpath_start = current;
            }
            b"l" => {
                let _ = write!(&mut path, "L{} {} ", pdf_num(values[0]), pdf_num(values[1]));
                current = [values[0], values[1]];
            }
            b"c" => {
                let _ = write!(
                    &mut path,
                    "C{} {} {} {} {} {} ",
                    pdf_num(values[0]),
                    pdf_num(values[1]),
                    pdf_num(values[2]),
                    pdf_num(values[3]),
                    pdf_num(values[4]),
                    pdf_num(values[5])
                );
                current = [values[4], values[5]];
            }
            b"v" => {
                let _ = write!(
                    &mut path,
                    "C{} {} {} {} {} {} ",
                    pdf_num(current[0]),
                    pdf_num(current[1]),
                    pdf_num(values[0]),
                    pdf_num(values[1]),
                    pdf_num(values[2]),
                    pdf_num(values[3])
                );
                current = [values[2], values[3]];
            }
            b"y" => {
                let _ = write!(
                    &mut path,
                    "C{} {} {} {} {} {} ",
                    pdf_num(values[0]),
                    pdf_num(values[1]),
                    pdf_num(values[2]),
                    pdf_num(values[3]),
                    pdf_num(values[2]),
                    pdf_num(values[3])
                );
                current = [values[2], values[3]];
            }
            b"re" => {
                let [x, y, w, h] = values else {
                    return String::new();
                };
                let (x, y, w, h) = (*x, *y, *w, *h);
                let _ = write!(
                    &mut path,
                    "M{} {} L{} {} L{} {} L{} {} Z ",
                    pdf_num(x),
                    pdf_num(y),
                    pdf_num(x + w),
                    pdf_num(y),
                    pdf_num(x + w),
                    pdf_num(y + h),
                    pdf_num(x),
                    pdf_num(y + h)
                );
                current = [x, y];
                subpath_start = current;
            }
            b"h" => {
                path.push_str("Z ");
                current = subpath_start;
            }
            _ => unreachable!("candidate ops already validated"),
        }
    }
    path.trim_end().to_owned()
}

fn collect_catalog(
    catalog: &mut GlyphCatalog,
    groups: &[Group],
    plans: &[GlyphFontPlan],
    shapes: &[Shape],
    sources: &[PageContents],
) {
    use std::fmt::Write as _;
    catalog.schema_version = 2;
    for (plan, chunk) in plans.iter().zip(groups.chunks(MAX_TYPE3_GLYPHS_PER_FONT)) {
        for (index, (entry, group)) in plan.shapes.iter().zip(chunk).enumerate() {
            let digest = Sha256::digest(&entry.charproc);
            let mut id = String::with_capacity(64);
            for byte in &digest {
                let _ = write!(&mut id, "{byte:02x}");
            }
            let Ok(code) = u8::try_from(index + 1) else {
                continue;
            };
            let mut occurrences = Vec::with_capacity(group.members.len());
            let mut ordinals = BTreeMap::<usize, usize>::new();
            for &member in &group.members {
                let shape = &shapes[member];
                let page = sources[shape.page_index].page_number;
                let ordinal = ordinals.entry(page).or_insert(0);
                let index_on_page = *ordinal;
                *ordinal += 1;
                let [x0, y0, x1, y1] = shape.bounds;
                occurrences.push(GlyphOccurrence {
                    page,
                    index_on_page,
                    local_bbox: shape.bounds,
                    ctm: shape.effective_ctm,
                    page_quad: [
                        transformed_corner(shape.effective_ctm, x0, y0),
                        transformed_corner(shape.effective_ctm, x1, y0),
                        transformed_corner(shape.effective_ctm, x1, y1),
                        transformed_corner(shape.effective_ctm, x0, y1),
                    ],
                });
            }
            catalog.glyphs.push(GlyphTemplate {
                id,
                font_resource: String::from_utf8_lossy(&plan.name).into_owned(),
                code,
                charproc: String::from_utf8_lossy(&entry.charproc).into_owned(),
                svg_path: candidate_svg_path(group, shapes),
                fill_rule: if shapes[group.seed].paint == b"f*" {
                    "evenodd"
                } else {
                    "nonzero"
                }
                .to_owned(),
                occurrences,
            });
        }
    }
}

/// Share filled glyphs across the whole document. OCR can label each
/// canonical once even when appearances span different pages.
pub(super) fn factor(
    doc: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    catalog: Option<&mut GlyphCatalog>,
) -> Result<OutlinedGlyphFactorStats> {
    let (sources, shapes, fills) = collect_sources(doc, pages)?;
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
        paint_ops_replaced,
    } = prepare_plans(doc, &sources, &shapes, &groups, &fills)?;
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
    stats.source_paints_replaced = paint_ops_replaced;
    stats.estimated_flate_bytes_saved = before.saturating_sub(after);
    if let Some(catalog) = catalog {
        collect_catalog(catalog, &groups, &plans, &shapes, &sources);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::{
        EditDocument, GlyphCatalog, Group, Result, candidate_svg_path, collect, collect_catalog,
        collect_compound, collect_sources, factor, group, prepare_plans, replacement,
    };
    fn shape(width: f64, bulge: f64) -> Vec<u8> {
        format!("q 1 0 0 1 20 30 cm 0 0 m {width} 0 l {width} 1 l {width} 1.5 {width} {bulge} {width} 2 c {width} 3 l {width} 4 l 0 4 l 0 3 l 0 2 l h f Q\n").into_bytes()
    }
    #[test]
    fn groups_squashed_bezier_and_produces_type3_body() -> Result<()> {
        let mut b = shape(4., 2.);
        b.extend(shape(5., 2.));
        let samples = collect(&b).ok_or_else(|| crate::Error::Invalid("parse".to_owned()))?;
        assert_eq!(samples.len(), 2);
        let g = group(&samples);
        assert_eq!(g.len(), 1);
        assert!(g[0].charproc(&samples).starts_with(b"0 0 d0\n"));
        let text = replacement(b"Test", 1, &samples[1])
            .ok_or_else(|| crate::Error::Invalid("placement".to_owned()))?;
        assert!(text.windows(4).any(|x| x == b"BT 0"));
        assert!(text.ends_with(b"ET\nEMC\nQ\n"));
        assert!(text.starts_with(b"q\n/Span"));
        assert!(
            text.windows(b"/ActualText ()".len())
                .any(|w| w == b"/ActualText ()")
        );
        Ok(())
    }
    #[test]
    fn refuses_different_shapes_and_interleaved_state() -> Result<()> {
        let mut bytes = shape(4., 2.);
        bytes.extend(shape(4., 3.));
        let appearances =
            collect(&bytes).ok_or_else(|| crate::Error::Invalid("parse".to_owned()))?;
        assert_eq!(appearances.len(), 2);
        assert!(group(&appearances).is_empty());
        bytes.extend(b"q 1 0 0 1 0 0 cm 0 0 m 3 0 l 3 4 l 0 4 l 1 3 l 2 2 l 2 1 l 0 1 l 0.5 g f Q");
        assert_eq!(
            collect(&bytes)
                .ok_or_else(|| crate::Error::Invalid("parse".to_owned()))?
                .len(),
            2
        );
        Ok(())
    }

    #[test]
    fn recognizes_fills_inside_existing_graphics_and_marked_content_scopes() -> Result<()> {
        let data = b"/OC /Glyphs BDC q 2 0 0 1 0 0 cm /GS1 gs \
          9 9 2 3 re 9 9 m 11 12 l f \
          13 9 2 3 re 13 9 m 15 12 l f Q EMC";
        let found =
            collect(data).ok_or_else(|| crate::Error::Invalid("source content".to_owned()))?;
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].bounds, [9.0, 9.0, 11.0, 12.0]);
        assert_eq!(found[1].bounds, [13.0, 9.0, 15.0, 12.0]);
        assert_eq!(found[0].page_space_factor, 2.0);
        let groups = group(&found);
        assert_eq!(groups.len(), 1);
        let rendered = replacement(b"Test", 1, &found[1])
            .ok_or_else(|| crate::Error::Invalid("glyph rendering".to_owned()))?;
        assert!(rendered.starts_with(b"q\n/Span"));
        assert!(!rendered.windows(5).any(|part| part == b" cm\n"));
        assert!(rendered.ends_with(b"EMC\nQ\n"));
        Ok(())
    }

    #[test]
    fn preserves_clipping_and_excludes_incomplete_paths() -> Result<()> {
        let data = b"q 0 0 100 100 re W n \
            1 1 m 2 1 l 2 2 l f \
            3 3 m 4 3 l 4 4 l W n \
            7 7 m 8 7 l 8 8 l f Q";
        let found =
            collect(data).ok_or_else(|| crate::Error::Invalid("source content".to_owned()))?;
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].bounds, [1.0, 1.0, 2.0, 2.0]);
        assert_eq!(found[1].bounds, [7.0, 7.0, 8.0, 8.0]);
        Ok(())
    }

    #[test]
    fn nested_transforms_constrain_canonical_error_in_page_space() -> Result<()> {
        let a = b"q 20 0 0 20 1 2 cm \
            0 0 m 4 0 l 4 4 l 0 4 l 0 0 l f Q";
        let b = b"q 1 0 0 1 1 2 cm \
            0 0 m 4 0 l 4 4 l 0.04 4 l 0 0 l f Q";
        let found_a = collect(a).ok_or_else(|| crate::Error::Invalid("source a".to_owned()))?;
        let found_b = collect(b).ok_or_else(|| crate::Error::Invalid("source b".to_owned()))?;
        assert_eq!(found_a[0].page_space_factor, 20.0);
        let mut both = found_a;
        both.extend(found_b);
        // Dimensions differ; a single canonical would violate the 0.02pt
        // transformed error budget on the 20x-scaled member.
        assert!(group(&both).is_empty());
        Ok(())
    }

    #[test]
    fn compound_fill_keeps_nested_holes_and_unrelated_contours_separate() -> Result<()> {
        // A rectangle with a nested even-odd hole must stay together, while
        // separated rectangles can each become an independent component.
        let stream = b"q \
            0 0 4 4 re 1 1 2 2 re \
            10 0 4 4 re \
            20 0 4 4 re f* Q";
        let (parts, fills) = collect_compound(stream)
            .ok_or_else(|| crate::Error::Invalid("compound fill".to_owned()))?;
        assert_eq!(fills.len(), 1);
        assert_eq!(parts.len(), 3);
        assert_eq!(
            parts.iter().map(|p| p.ops.len()).collect::<Vec<_>>(),
            [2, 1, 1]
        );
        assert_eq!(parts[0].bounds, [0., 0., 4., 4.]);
        assert_eq!(parts[0].paint, b"f*");
        assert_eq!(parts[0].raw_subpaths.len(), 2);
        assert_eq!(fills[0].components, vec![0, 1, 2]);
        assert_eq!(
            fills[0].start,
            stream
                .iter()
                .position(|&b| b == b'0')
                .ok_or_else(|| crate::Error::Invalid("start".to_owned()))?
        );
        Ok(())
    }

    #[test]
    fn compound_fill_preserves_residual_paths_with_one_atomic_rewrite() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        let mut content = Vec::new();
        for i in 0..4 {
            let origin = i * 10;
            content.extend_from_slice(
                format!(
                    "q 1 0 0 1 {origin} {origin} cm \
                  0 0 4 4 re 1 1 2 2 re \
                  10 0 3 4 re \
                  20 0 m 25 0 l 20 {} l 20 {} l h f* Q\n",
                    3 + i,
                    1 + i
                )
                .as_bytes(),
            );
        }
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3,b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", &content)?;
        let doc = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = doc.page_handles()?;
        let (sources, parts, fills) = collect_sources(&doc, &pages)?;
        assert_eq!(fills.len(), 4);
        assert_eq!(parts.len(), 12);
        // The two-rectangle even-odd glyph + the separate rectangular
        // glyph are repeated. The distinct triangle stays original.
        let groups = group(&parts);
        assert_eq!(groups.len(), 2);
        let plan = prepare_plans(&doc, &sources, &parts, &groups, &fills)?;
        assert_eq!(plan.replacements, 8);
        assert_eq!(plan.paint_ops_replaced, 4);
        assert_eq!(
            plan.edits[0].len(),
            4,
            "one edit per original compound fill"
        );
        let rewritten =
            crate::content_stream::apply_replacements(&sources[0].decoded, &plan.edits[0])
                .ok_or_else(|| crate::Error::Invalid("nonoverlapping edits".to_owned()))?;
        assert_eq!(
            rewritten
                .windows(b"/ActualText ()".len())
                .filter(|w| *w == b"/ActualText ()")
                .count(),
            8
        );
        assert_eq!(
            rewritten
                .windows(b" f*\n".len())
                .filter(|w| *w == b" f*\n")
                .count(),
            0
        );
        assert_eq!(
            rewritten
                .windows(b"\nf*\n".len())
                .filter(|w| *w == b"\nf*\n")
                .count(),
            4
        );
        // The unmatched triangle's original coordinates survive exactly.
        assert!(
            rewritten
                .windows(b"20 6 l 20 4 l h".len())
                .any(|w| w == b"20 6 l 20 4 l h")
        );
        Ok(())
    }

    #[test]
    fn nearly_touching_or_singular_compound_paths_are_not_split() -> Result<()> {
        let touching = collect(b"0 0 2 2 re 2 0 2 2 re 2 1 2 1 re f")
            .ok_or_else(|| crate::Error::Invalid("touching fill".to_owned()))?;
        assert_eq!(touching.len(), 1);
        let separated = collect(b"0 0 2 2 re 2.3 0 2 2 re 2.3 1 2 1 re f")
            .ok_or_else(|| crate::Error::Invalid("separated fill".to_owned()))?;
        assert_eq!(separated.len(), 2);
        let singular = collect(b"q 1 0 0 0 0 0 cm 0 0 2 2 re 10 0 2 2 re 20 0 2 2 re f Q")
            .ok_or_else(|| crate::Error::Invalid("singular graphics state".to_owned()))?;
        assert!(
            singular.is_empty(),
            "cannot prove separated ink under singular CTM"
        );
        Ok(())
    }

    #[test]
    fn export_svg_encodes_pdf_curve_shorthands_correctly() -> Result<()> {
        let data = b"0 0 m 1 1 2 2 v 2 4 3 3 y 4 4 l h \
            6 6 2 3 re f*";
        let shapes = collect(data).ok_or_else(|| crate::Error::Invalid("parse".to_owned()))?;
        assert_eq!(shapes.len(), 2);
        let g = Group::first(0, &shapes[0]);
        let path = candidate_svg_path(&g, &shapes);
        assert!(path.contains('C'), "{path}");
        assert!(path.contains('Z'), "{path}");
        assert_eq!(path.matches('M').count(), 1, "{path}");
        let rectangle = Group::first(1, &shapes[1]);
        let rectangle_path = candidate_svg_path(&rectangle, &shapes);
        assert!(rectangle_path.starts_with('M'));
        assert!(rectangle_path.ends_with('Z'));
        let proc = g.charproc(&shapes);
        assert!(proc.ends_with(b"f*\n"));
        Ok(())
    }

    #[test]
    fn captures_actual_one_based_page_indices_when_empty_pages_are_skipped() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        pdf.object(3,b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 5 0 R >>")?;
        pdf.object(4,b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 6 0 R >>")?;
        pdf.stream(5, b"", b"")?;
        let repeated = b"q 3 0 0 4 7 8 cm 1 1 m 3 1 l 3 5 l 1 5 l h f Q \
            q 3 0 0 4 7 8 cm 5 1 m 7 1 l 7 5 l 5 5 l h f Q";
        pdf.stream(6, b"", repeated)?;
        let doc = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = doc.page_handles()?;
        let (sources, shapes, _fills) = collect_sources(&doc, &pages)?;
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].page_number, 2);
        assert_eq!(shapes.len(), 2);
        let groups = group(&shapes);
        assert_eq!(groups.len(), 1);
        let mut catalog = GlyphCatalog::default();
        let plans = prepare_plans(
            &doc,
            &sources,
            &shapes,
            &groups,
            &collect_sources(&doc, &pages)?.2,
        )?;
        collect_catalog(&mut catalog, &groups, &plans.plans, &shapes, &sources);
        assert_eq!(catalog.glyphs.len(), 1);
        assert_eq!(catalog.glyphs[0].occurrences.len(), 2);
        assert!(catalog.glyphs[0].occurrences.iter().all(|o| o.page == 2));
        assert_eq!(
            catalog.glyphs[0].occurrences[0].ctm,
            [3., 0., 0., 4., 7., 8.]
        );
        assert!(catalog.glyphs[0].occurrences[0].page_quad[0][0] > 7.);
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
                    "q 1 0 0 1 {x} {y} cm {} {} m {w} {} l {} {} l {} {} {} {} {} {} c {w} {} l {w} {} l {} {} l {} {} l {} {} l h f Q\n",
                    jitter(1), jitter(2), jitter(3),
                    w - jitter(4), 1.0 + jitter(5),
                    w - jitter(6), 1.5 + jitter(7),
                    w + jitter(8), 2.0 + jitter(9),
                    w + jitter(18), 2.4 + jitter(19),
                    3.0 + jitter(10),
                    4.0 + jitter(11),
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
        let mut catalog = GlyphCatalog::default();
        let stats = factor(
            &mut doc,
            &pages,
            crate::FlateLevel::PROCESSING,
            Some(&mut catalog),
        )?;
        assert_eq!(stats.pages_rewritten, 1, "{stats:?}");
        assert_eq!(catalog.schema_version, 2);
        assert_eq!(catalog.glyphs.len(), stats.glyphs_created);
        assert_eq!(
            catalog
                .glyphs
                .iter()
                .map(|g| g.occurrences.len())
                .sum::<usize>(),
            stats.occurrences_replaced
        );
        assert!(catalog.glyphs.iter().all(|g| g.id.len() == 64));
        assert!(
            catalog
                .glyphs
                .iter()
                .all(|g| g.occurrences.iter().all(|o| o.page == 1))
        );
        assert!(catalog.glyphs.iter().any(|g| g.svg_path.starts_with('M')));

        assert!(stats.fonts_created >= 1);
        assert!(stats.occurrences_replaced >= 4000, "{stats:?}");
        let object = doc
            .current_owned_object(pages[0])?
            .ok_or_else(|| crate::Error::Invalid("page".to_owned()))?;
        let contents = object
            .as_dictionary()
            .ok_or_else(|| crate::Error::Invalid("dictionary".to_owned()))?
            .get(b"Contents".as_slice())
            .ok_or_else(|| crate::Error::Invalid("contents".to_owned()))?;
        let mut rendered = Vec::new();
        crate::content::decoded_content_value(
            &doc,
            crate::source::CurrentValue::Owned(contents),
            &mut rendered,
        )?;
        assert!(rendered.windows(b" Tj ET".len()).any(|w| w == b" Tj ET"));
        assert!(!rendered.windows(b" 1.5 ".len()).any(|w| w == b" 1.5 "));
        Ok(())
    }

    #[test]
    fn refuses_open_path_or_text_context() -> Result<()> {
        let mut data = b"0 0 m ".to_vec();
        data.extend(shape(4., 2.));
        assert!(
            collect(&data)
                .ok_or_else(|| crate::Error::Invalid("parse".to_owned()))?
                .is_empty()
        );
        let mut data = b"BT ".to_vec();
        data.extend(shape(4., 2.));
        data.extend(b" ET");
        assert!(
            collect(&data)
                .ok_or_else(|| crate::Error::Invalid("parse".to_owned()))?
                .is_empty()
        );
        Ok(())
    }
}
