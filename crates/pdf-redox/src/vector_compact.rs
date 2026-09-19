use crate::{
    EditDocument, ObjectHandle, OptimizationGoal, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{decoded_content_value, replace_page_content, resolved_dictionary},
};
use flate2::{Compression, write::ZlibEncoder};
use flpdf::content_stream::ContentScalar;
use flpdf::{Matrix, ObjectHandle as FlObjectHandle, ObjectHandleParserCallbacks, ParseControl};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::Write as _,
    sync::LazyLock,
};

static DEBUG_VECTOR: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("PDF_REDOX_DEBUG_VECTOR").is_some());

const RECT_SNAP_PAGE_GAP_PT: f64 = 0.001;
// Keep independently painted thin rectangles separate. Some PDF renderers
// snap subpixel fills to device pixels; absorbing such a member into a wider
// rectangle can change low-resolution preview rasterization even when the
// mathematical union is identical. Six points keeps every merged member at
// least one pixel wide at the 12 DPI corpus validation floor.
const MIN_MERGE_MEMBER_PAGE_EXTENT_PT: f64 = 6.0;
// A contained repaint is only redundant at the raster level when its own
// device-snapped edge cannot reach the antialiased boundary of the enclosing
// rectangle. Six page points is one pixel at the 12 DPI corpus gate.
const MIN_CONTAINED_REPAINT_PAGE_MARGIN_PT: f64 = 6.0;
// Large exact path blocks can defeat Deflate when repeated across page streams.
// Keep the scanner floor high so ordinary outlined text/glyph paths do not
// turn vector compaction into an expensive combinatorial search.
const MIN_PATH_FORM_BYTES: usize = 4 * 1024;
const MIN_PROCESSING_PATH_FORM_BYTES: usize = 96;
const MIN_PROCESSING_PATH_FORM_OPERATORS: usize = 12;
const MIN_PATH_FORM_OCCURRENCES: usize = 2;
// The cache-only no-path-candidate proof is meant for cheap no-op rejection,
// not for path-heavy pages where the real factoring pass will likely do useful
// work. Above this count, fall back immediately; this affects only runtime.
const MAX_CACHED_PATH_PROOF_BLOCKS: usize = 4 * 1024;
const PATH_FORM_BBOX_MARGIN: f64 = 1.0;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct VectorCompactionStats {
    pub pages_compacted: usize,
    pub fill_groups_batched: usize,
    pub covered_fills_pruned: usize,
    pub fill_paints_eliminated: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
    pub path_forms_created: usize,
    pub path_form_pages_rewritten: usize,
    pub path_form_occurrences_replaced: usize,
    pub path_form_decoded_bytes_factored: usize,
    pub path_form_estimated_flate_bytes_saved: usize,
    pub transformed_forms_created: usize,
    pub transformed_form_pages_rewritten: usize,
    pub transformed_form_occurrences_replaced: usize,
    pub transformed_form_operators_eliminated: usize,
    pub transformed_form_estimated_flate_bytes_saved: usize,
    pub generated_page_xobjects: BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
}

#[derive(Debug, Clone)]
enum OperandValue {
    Scalar(ContentScalar),
    Handle(FlObjectHandle),
}

impl OperandValue {
    fn number(&self) -> Option<f64> {
        match self {
            Self::Scalar(value) => value
                .as_integer()
                .map(|value| value as f64)
                .or_else(|| value.as_real()),
            Self::Handle(value) => value
                .as_integer()
                .map(|value| value as f64)
                .or_else(|| value.as_real()),
        }
    }

    fn name(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Scalar(value) => value.as_name().map(Cow::Borrowed),
            Self::Handle(value) => value.as_name().map(Cow::Owned),
        }
    }
}

#[derive(Debug, Clone)]
struct Operand {
    value: OperandValue,
    offset: usize,
}

fn operand_numbers(operands: &[Operand]) -> Option<SmallVec<[f64; 6]>> {
    let mut values = SmallVec::new();
    for operand in operands {
        values.push(operand.value.number()?);
    }
    Some(values)
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Rect {
    fn from_xywh(x: f64, y: f64, width: f64, height: f64) -> Option<Self> {
        if ![x, y, width, height].into_iter().all(f64::is_finite)
            || width.abs() <= f64::EPSILON
            || height.abs() <= f64::EPSILON
        {
            return None;
        }
        let x1 = x + width;
        let y1 = y + height;
        Some(Self {
            x0: x.min(x1),
            y0: y.min(y1),
            x1: x.max(x1),
            y1: y.max(y1),
        })
    }
}

#[derive(Debug, Clone)]
struct FillPaint {
    epoch: usize,
    path_start: usize,
    end: usize,
    operator: Vec<u8>,
    rect: Rect,
    ctm: Matrix,
    idempotent: bool,
}

#[derive(Debug, Clone, Copy)]
struct FillSafety {
    fill_alpha_opaque: bool,
    normal_blend: bool,
    no_soft_mask: bool,
    fill_overprint_disabled: bool,
    simple_fill_color: bool,
}

impl Default for FillSafety {
    fn default() -> Self {
        Self {
            fill_alpha_opaque: true,
            normal_blend: true,
            no_soft_mask: true,
            fill_overprint_disabled: true,
            simple_fill_color: true,
        }
    }
}

impl FillSafety {
    fn idempotent(self) -> bool {
        self.fill_alpha_opaque
            && self.normal_blend
            && self.no_soft_mask
            && self.fill_overprint_disabled
            && self.simple_fill_color
    }

    fn invalidate_transparency(&mut self) {
        self.fill_alpha_opaque = false;
        self.normal_blend = false;
        self.no_soft_mask = false;
        self.fill_overprint_disabled = false;
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ExtGStatePatch {
    fill_alpha_opaque: Option<bool>,
    normal_blend: Option<bool>,
    no_soft_mask: Option<bool>,
    fill_overprint_disabled: Option<bool>,
}

impl FillSafety {
    fn apply_ext_gstate(&mut self, patch: ExtGStatePatch) {
        if let Some(value) = patch.fill_alpha_opaque {
            self.fill_alpha_opaque = value;
        }
        if let Some(value) = patch.normal_blend {
            self.normal_blend = value;
        }
        if let Some(value) = patch.no_soft_mask {
            self.no_soft_mask = value;
        }
        if let Some(value) = patch.fill_overprint_disabled {
            self.fill_overprint_disabled = value;
        }
    }
}

struct FillScanner {
    operands: Vec<Operand>,
    current_rect: Option<Rect>,
    current_path_start: Option<usize>,
    path_is_single_rect: bool,
    epoch: usize,
    fills: Vec<FillPaint>,
    ctm: Matrix,
    fill_safety: FillSafety,
    ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>,
    state_stack: Vec<(Matrix, FillSafety)>,
}

impl FillScanner {
    fn new(ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>) -> Self {
        Self {
            operands: Vec::new(),
            current_rect: None,
            current_path_start: None,
            path_is_single_rect: true,
            epoch: 0,
            fills: Vec::new(),
            ctm: Matrix::default(),
            fill_safety: FillSafety::default(),
            ext_gstates,
            state_stack: Vec::new(),
        }
    }

    fn clear_path(&mut self) {
        self.current_rect = None;
        self.current_path_start = None;
        self.path_is_single_rect = true;
    }

    fn barrier(&mut self) {
        self.operands.clear();
        self.clear_path();
        self.epoch = self.epoch.saturating_add(1);
    }

    fn rectangle(&mut self) {
        if !self.path_is_single_rect || self.current_rect.is_some() || self.operands.len() != 4 {
            self.path_is_single_rect = false;
            self.operands.clear();
            return;
        }
        let values = operand_numbers(&self.operands);
        let Some(values) = values else {
            self.path_is_single_rect = false;
            self.operands.clear();
            return;
        };
        let Some(rect) = Rect::from_xywh(values[0], values[1], values[2], values[3]) else {
            self.path_is_single_rect = false;
            self.operands.clear();
            return;
        };
        self.current_path_start = self.operands.first().map(|operand| operand.offset);
        self.current_rect = Some(rect);
        self.operands.clear();
    }

    fn fill(&mut self, operator: &[u8], end: usize) {
        if self.operands.is_empty()
            && self.path_is_single_rect
            && let (Some(path_start), Some(rect)) = (self.current_path_start, self.current_rect)
        {
            self.fills.push(FillPaint {
                epoch: self.epoch,
                path_start,
                end,
                operator: operator.to_vec(),
                rect,
                ctm: self.ctm,
                idempotent: self.fill_safety.idempotent(),
            });
        } else {
            // An unsupported path still paints. It must split compaction runs so
            // a rewrite can never consume geometry that was omitted from `fills`.
            self.epoch = self.epoch.saturating_add(1);
        }
        self.operands.clear();
        self.clear_path();
    }

    fn simple_fill_color(&mut self, components: usize) {
        self.fill_safety.simple_fill_color = self.operands.len() == components
            && self
                .operands
                .iter()
                .all(|operand| operand.value.number().is_some_and(f64::is_finite));
        self.barrier();
    }

    fn unknown_fill_color(&mut self) {
        self.fill_safety.simple_fill_color = false;
        self.barrier();
    }

    fn apply_ext_gstate(&mut self) {
        let patch = (self.operands.len() == 1)
            .then(|| self.operands[0].value.name())
            .flatten()
            .and_then(|name| self.ext_gstates.get(name.as_ref()).copied());
        if let Some(patch) = patch {
            self.fill_safety.apply_ext_gstate(patch);
        } else {
            self.fill_safety.invalidate_transparency();
        }
        self.barrier();
    }

    fn concat_ctm(&mut self) {
        let values = operand_numbers(&self.operands);
        if let Some(values) = values
            && values.len() == 6
        {
            self.ctm.concat(Matrix::new(
                values[0], values[1], values[2], values[3], values[4], values[5],
            ));
        }
        self.barrier();
    }

    fn process_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        let end = offset.saturating_add(length);
        match operator {
            b"re" => self.rectangle(),
            b"f" | b"F" | b"f*" => self.fill(operator, end),
            b"q" => {
                self.state_stack.push((self.ctm, self.fill_safety));
                self.barrier();
            }
            b"Q" => {
                if let Some((ctm, fill_safety)) = self.state_stack.pop() {
                    self.ctm = ctm;
                    self.fill_safety = fill_safety;
                } else {
                    self.fill_safety = FillSafety {
                        fill_alpha_opaque: false,
                        normal_blend: false,
                        no_soft_mask: false,
                        fill_overprint_disabled: false,
                        simple_fill_color: false,
                    };
                }
                self.barrier();
            }
            b"cm" => self.concat_ctm(),
            b"g" => self.simple_fill_color(1),
            b"rg" => self.simple_fill_color(3),
            b"k" => self.simple_fill_color(4),
            b"cs" | b"sc" | b"scn" => self.unknown_fill_color(),
            b"gs" => self.apply_ext_gstate(),
            _ => self.barrier(),
        }
    }
}

impl ObjectHandleParserCallbacks for FillScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = scalar.as_operator() {
            return self.handle_operator(operator, offset, length);
        } else {
            self.operands.push(Operand {
                value: OperandValue::Scalar(scalar),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        self.process_operator(operator, offset, length);
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.process_operator(&operator, offset, length);
        } else if object.as_inline_image().is_some() {
            self.barrier();
        } else {
            self.operands.push(Operand {
                value: OperandValue::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1.0e-8 * a.abs().max(b.abs()).max(1.0)
}

pub(crate) fn merge_rect_fill_pair(a: [f64; 4], b: [f64; 4], ctm: Matrix) -> Option<[f64; 4]> {
    let merged = merge_collinear_rects(
        Rect {
            x0: a[0],
            y0: a[1],
            x1: a[2],
            y1: a[3],
        },
        Rect {
            x0: b[0],
            y0: b[1],
            x1: b[2],
            y1: b[3],
        },
        ctm,
    )?;
    Some([merged.x0, merged.y0, merged.x1, merged.y1])
}

pub(crate) fn compacted_rect_fill_len(rect: [f64; 4], operator: &[u8]) -> usize {
    format!(
        "{} {} {} {} re {}",
        pdf_number(rect[0]),
        pdf_number(rect[1]),
        pdf_number(rect[2] - rect[0]),
        pdf_number(rect[3] - rect[1]),
        String::from_utf8_lossy(operator)
    )
    .len()
}

fn merge_collinear_rects(a: Rect, b: Rect, ctm: Matrix) -> Option<Rect> {
    if close(a.y0, b.y0) && close(a.y1, b.y1) {
        let x_scale = ctm.a.hypot(ctm.b);
        let a_extent = (a.x1 - a.x0).abs() * x_scale;
        let b_extent = (b.x1 - b.x0).abs() * x_scale;
        if a_extent >= MIN_MERGE_MEMBER_PAGE_EXTENT_PT
            && b_extent >= MIN_MERGE_MEMBER_PAGE_EXTENT_PT
        {
            let gap = if a.x1 <= b.x0 {
                b.x0 - a.x1
            } else if b.x1 <= a.x0 {
                a.x0 - b.x1
            } else {
                return None;
            };
            if gap * x_scale <= RECT_SNAP_PAGE_GAP_PT {
                return Some(Rect {
                    x0: a.x0.min(b.x0),
                    y0: a.y0,
                    x1: a.x1.max(b.x1),
                    y1: a.y1,
                });
            }
        }
    }
    if close(a.x0, b.x0) && close(a.x1, b.x1) {
        let y_scale = ctm.c.hypot(ctm.d);
        let a_extent = (a.y1 - a.y0).abs() * y_scale;
        let b_extent = (b.y1 - b.y0).abs() * y_scale;
        if a_extent >= MIN_MERGE_MEMBER_PAGE_EXTENT_PT
            && b_extent >= MIN_MERGE_MEMBER_PAGE_EXTENT_PT
        {
            let gap = if a.y1 <= b.y0 {
                b.y0 - a.y1
            } else if b.y1 <= a.y0 {
                a.y0 - b.y1
            } else {
                return None;
            };
            if gap * y_scale <= RECT_SNAP_PAGE_GAP_PT {
                return Some(Rect {
                    x0: a.x0,
                    y0: a.y0.min(b.y0),
                    x1: a.x1,
                    y1: a.y1.max(b.y1),
                });
            }
        }
    }
    None
}

fn pdf_number(value: f64) -> String {
    let value = if value.abs() < 5.0e-11 { 0.0 } else { value };
    let mut out = format!("{value:.10}");
    while out.contains('.') && out.ends_with('0') {
        out.pop();
    }
    if out.ends_with('.') {
        out.pop();
    }
    if out == "-0" { "0".to_owned() } else { out }
}

#[derive(Debug)]
struct Rewrite {
    start: usize,
    end: usize,
    replacement: Vec<u8>,
    paint_count: usize,
}

fn contains_rect(outer: Rect, inner: Rect) -> bool {
    outer.x0 <= inner.x0 && outer.y0 <= inner.y0 && outer.x1 >= inner.x1 && outer.y1 >= inner.y1
}

fn contained_rect_is_raster_safe(outer: Rect, inner: Rect, ctm: Matrix) -> bool {
    if !contains_rect(outer, inner) {
        return false;
    }

    // Under the shared CTM, x/y-aligned rectangle edges become two pairs of
    // parallel lines. The perpendicular page-space distance produced by a
    // user-space x displacement is |det(CTM)| / |y basis|, and vice versa.
    let determinant = (ctm.a * ctm.d - ctm.b * ctm.c).abs();
    let x_basis = ctm.a.hypot(ctm.b);
    let y_basis = ctm.c.hypot(ctm.d);
    if !determinant.is_finite()
        || !x_basis.is_finite()
        || !y_basis.is_finite()
        || determinant <= 1.0e-12
        || x_basis <= 1.0e-12
        || y_basis <= 1.0e-12
    {
        return false;
    }
    let x_margin_scale = determinant / y_basis;
    let y_margin_scale = determinant / x_basis;
    let margin = MIN_CONTAINED_REPAINT_PAGE_MARGIN_PT;

    (inner.x0 - outer.x0) * x_margin_scale >= margin
        && (outer.x1 - inner.x1) * x_margin_scale >= margin
        && (inner.y0 - outer.y0) * y_margin_scale >= margin
        && (outer.y1 - inner.y1) * y_margin_scale >= margin
}

pub(crate) fn rect_contains_rect(outer: [f64; 4], inner: [f64; 4]) -> bool {
    contains_rect(
        Rect {
            x0: outer[0],
            y0: outer[1],
            x1: outer[2],
            y1: outer[3],
        },
        Rect {
            x0: inner[0],
            y0: inner[1],
            x1: inner[2],
            y1: inner[3],
        },
    )
}

fn covered_fill_ranges(fills: &[FillPaint]) -> BTreeSet<(usize, usize)> {
    let mut ranges = BTreeSet::new();
    for pair in fills.windows(2) {
        let earlier = &pair[0];
        let later = &pair[1];
        if earlier.epoch != later.epoch
            || earlier.operator != later.operator
            || !earlier.idempotent
            || !later.idempotent
        {
            continue;
        }

        if contained_rect_is_raster_safe(later.rect, earlier.rect, earlier.ctm) {
            ranges.insert((earlier.path_start, earlier.end));
        } else if contained_rect_is_raster_safe(earlier.rect, later.rect, earlier.ctm) {
            ranges.insert((later.path_start, later.end));
        }
    }
    ranges
}

fn apply_covered_fill_ranges(
    input: &[u8],
    ranges: &BTreeSet<(usize, usize)>,
) -> (Vec<u8>, VectorCompactionStats) {
    if ranges.is_empty() {
        return (input.to_vec(), VectorCompactionStats::default());
    }

    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut stats = VectorCompactionStats::default();
    for &(start, end) in ranges {
        if start < cursor || end > input.len() {
            return (input.to_vec(), VectorCompactionStats::default());
        }
        output.extend_from_slice(&input[cursor..start]);
        output.push(b' ');
        stats.covered_fills_pruned += 1;
        stats.fill_paints_eliminated += 1;
        stats.decoded_bytes_removed += end.saturating_sub(start).saturating_sub(1);
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    (output, stats)
}

fn build_rewrites(input: &[u8], fills: &[FillPaint]) -> Vec<Rewrite> {
    let mut rewrites = Vec::new();
    let mut i = 0usize;
    while i < fills.len() {
        let first = &fills[i];
        let mut merged = first.rect;
        let mut j = i + 1;
        while j < fills.len() {
            let next = &fills[j];
            if next.epoch != first.epoch || next.operator != first.operator {
                break;
            }
            let Some(next_merged) = merge_collinear_rects(merged, next.rect, first.ctm) else {
                break;
            };
            merged = next_merged;
            j += 1;
        }
        if j - i >= 2 {
            let last = &fills[j - 1];
            let replacement = format!(
                "{} {} {} {} re {}",
                pdf_number(merged.x0),
                pdf_number(merged.y0),
                pdf_number(merged.x1 - merged.x0),
                pdf_number(merged.y1 - merged.y0),
                String::from_utf8_lossy(&first.operator)
            )
            .into_bytes();
            if last.end <= input.len()
                && replacement.len() < last.end.saturating_sub(first.path_start)
            {
                rewrites.push(Rewrite {
                    start: first.path_start,
                    end: last.end,
                    replacement,
                    paint_count: j - i,
                });
                i = j;
                continue;
            }
        }
        i += 1;
    }
    rewrites
}

fn compact_content_from_fill_scan(
    input: &[u8],
    ext_gstates: &BTreeMap<Vec<u8>, ExtGStatePatch>,
    initial_fills: &[FillPaint],
) -> (Vec<u8>, VectorCompactionStats) {
    let covered_ranges = covered_fill_ranges(initial_fills);
    let (covered_pruned, mut stats) = apply_covered_fill_ranges(input, &covered_ranges);
    let rescanned_fills;
    let fills = if covered_ranges.is_empty() {
        initial_fills
    } else {
        let mut scanner = FillScanner::new(ext_gstates.clone());
        if flpdf::parse_detached_content_stream(
            &covered_pruned,
            "vector rectangle compaction after covered-fill pruning",
            &mut scanner,
        )
        .is_err()
        {
            return (input.to_vec(), VectorCompactionStats::default());
        }
        rescanned_fills = scanner.fills;
        &rescanned_fills
    };
    let rewrites = build_rewrites(&covered_pruned, fills);
    if *DEBUG_VECTOR {
        eprintln!(
            "vector-actual content={} fills={} covered_pruned={} rewrites={}",
            input.len(),
            fills.len(),
            stats.covered_fills_pruned,
            rewrites.len()
        );
    }
    if rewrites.is_empty() {
        return (covered_pruned, stats);
    }
    let mut output = Vec::with_capacity(covered_pruned.len());
    let mut cursor = 0usize;
    for rewrite in rewrites {
        if rewrite.start < cursor || rewrite.end > covered_pruned.len() {
            return (input.to_vec(), VectorCompactionStats::default());
        }
        output.extend_from_slice(&covered_pruned[cursor..rewrite.start]);
        output.extend_from_slice(&rewrite.replacement);
        stats.fill_groups_batched += 1;
        stats.fill_paints_eliminated += rewrite.paint_count.saturating_sub(1);
        stats.decoded_bytes_removed += rewrite
            .end
            .saturating_sub(rewrite.start)
            .saturating_sub(rewrite.replacement.len());
        cursor = rewrite.end;
    }
    output.extend_from_slice(&covered_pruned[cursor..]);
    (output, stats)
}

fn compact_content_with_ext_gstates(
    input: &[u8],
    ext_gstates: &BTreeMap<Vec<u8>, ExtGStatePatch>,
) -> (Vec<u8>, VectorCompactionStats) {
    let mut scanner = FillScanner::new(ext_gstates.clone());
    if flpdf::parse_detached_content_stream(input, "vector rectangle compaction", &mut scanner)
        .is_err()
    {
        return (input.to_vec(), VectorCompactionStats::default());
    }
    compact_content_from_fill_scan(input, ext_gstates, &scanner.fills)
}

#[cfg(test)]
fn compact_content(input: &[u8]) -> (Vec<u8>, VectorCompactionStats) {
    compact_content_with_ext_gstates(input, &BTreeMap::new())
}

fn current_number(document: &EditDocument, value: &OwnedObject) -> Result<Option<f64>> {
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => Some(value as f64),
        Some(OwnedObject::Real(value)) => Some(value),
        _ => None,
    })
}

fn current_bool(document: &EditDocument, value: &OwnedObject) -> Result<Option<bool>> {
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Boolean(value)) => Some(value),
        _ => None,
    })
}

fn ext_gstate_patches(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ExtGStatePatch>> {
    let Some(states) = resolved_dictionary(document, resources.get(b"ExtGState".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, value) in states {
        let Some(state) = resolved_dictionary(document, Some(&value))? else {
            continue;
        };
        let fill_alpha_opaque = match state.get(b"ca".as_slice()) {
            None => None,
            Some(value) => Some(
                current_number(document, value)?
                    .is_some_and(|value| value.is_finite() && (value - 1.0).abs() <= 1.0e-12),
            ),
        };
        let normal_blend = match state.get(b"BM".as_slice()) {
            None => None,
            Some(value) => Some(matches!(
                document.resolve_owned_value(value)?,
                Some(OwnedObject::Name(name)) if name == b"Normal"
            )),
        };
        let no_soft_mask = match state.get(b"SMask".as_slice()) {
            None => None,
            Some(value) => Some(matches!(
                document.resolve_owned_value(value)?,
                Some(OwnedObject::Name(name)) if name == b"None"
            )),
        };
        // PDF `op` controls non-stroking overprint. When it is absent, `OP`
        // supplies the value, so either key can make a fill non-idempotent.
        let fill_overprint_disabled = if let Some(value) = state.get(b"op".as_slice()) {
            Some(current_bool(document, value)? == Some(false))
        } else if let Some(value) = state.get(b"OP".as_slice()) {
            Some(current_bool(document, value)? == Some(false))
        } else {
            None
        };
        out.insert(
            name,
            ExtGStatePatch {
                fill_alpha_opaque,
                normal_blend,
                no_soft_mask,
                fill_overprint_disabled,
            },
        );
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct PathBlock {
    start: usize,
    end: usize,
    bounds: Rect,
    semantic_key: Vec<u8>,
    operator_count: usize,
}

struct PathBlockScanner {
    operands: Vec<Operand>,
    path_start: Option<usize>,
    path_valid: bool,
    bounds: Option<Rect>,
    semantic_key: Vec<u8>,
    operator_count: usize,
    blocks: Vec<PathBlock>,
    inside_text: bool,
}

impl PathBlockScanner {
    fn new() -> Self {
        Self {
            operands: Vec::new(),
            path_start: None,
            path_valid: true,
            bounds: None,
            semantic_key: Vec::new(),
            operator_count: 0,
            blocks: Vec::new(),
            inside_text: false,
        }
    }

    fn reset_path_state(&mut self) {
        self.path_start = None;
        self.path_valid = true;
        self.bounds = None;
        self.semantic_key.clear();
        self.operator_count = 0;
    }

    fn include_point(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            self.path_valid = false;
            return;
        }
        self.bounds = Some(match self.bounds {
            None => Rect {
                x0: x,
                y0: y,
                x1: x,
                y1: y,
            },
            Some(bounds) => Rect {
                x0: bounds.x0.min(x),
                y0: bounds.y0.min(y),
                x1: bounds.x1.max(x),
                y1: bounds.y1.max(y),
            },
        });
    }

    fn path_operator_with_operands(
        &mut self,
        operator: &[u8],
        operator_offset: usize,
        operands: &[Operand],
    ) {
        if self.inside_text {
            self.path_valid = false;
        }
        if self.path_start.is_none() {
            if operator == b"h" {
                self.path_valid = false;
                return;
            }
            self.path_start = operands
                .first()
                .map_or(Some(operator_offset), |operand| Some(operand.offset));
        }
        let values = operand_numbers(operands);
        let expected = match operator {
            b"m" | b"l" => 2,
            b"c" => 6,
            b"v" | b"y" => 4,
            b"re" => 4,
            b"h" => 0,
            _ => 0,
        };
        let Some(values) = values.filter(|values| values.len() == expected) else {
            self.path_valid = false;
            return;
        };
        self.semantic_key
            .push(u8::try_from(operator.len()).unwrap_or(u8::MAX));
        self.semantic_key.extend_from_slice(operator);
        self.operator_count = self.operator_count.saturating_add(1);
        for value in &values {
            let normalized = if *value == 0.0 { 0.0 } else { *value };
            self.semantic_key
                .extend_from_slice(&normalized.to_bits().to_be_bytes());
        }
        if operator == b"re" {
            let x = values[0];
            let y = values[1];
            let x1 = x + values[2];
            let y1 = y + values[3];
            self.include_point(x, y);
            self.include_point(x1, y1);
        } else {
            for pair in values.as_chunks::<2>().0 {
                self.include_point(pair[0], pair[1]);
            }
        }
    }

    fn finish_fill_with_operands(&mut self, operator: &[u8], end: usize, operands: &[Operand]) {
        let block_len = self.path_start.map_or(0, |start| end.saturating_sub(start));
        if operands.is_empty()
            && self.path_valid
            && let (Some(start), Some(bounds)) = (self.path_start, self.bounds)
            && block_len >= MIN_PROCESSING_PATH_FORM_BYTES
            && (block_len >= MIN_PATH_FORM_BYTES
                || self.operator_count >= MIN_PROCESSING_PATH_FORM_OPERATORS)
        {
            let mut semantic_key = self.semantic_key.clone();
            semantic_key.push(u8::try_from(operator.len()).unwrap_or(u8::MAX));
            semantic_key.extend_from_slice(operator);
            self.blocks.push(PathBlock {
                start,
                end,
                bounds,
                semantic_key,
                operator_count: self.operator_count.saturating_add(1),
            });
        }
        self.reset_path_state();
    }

    fn process_operator_with_operands(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
        operands: &[Operand],
    ) {
        let end = offset.saturating_add(length);
        match operator {
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => {
                self.path_operator_with_operands(operator, offset, operands);
            }
            b"f" | b"F" | b"f*" => self.finish_fill_with_operands(operator, end, operands),
            b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                self.unsupported_operator_state(true);
            }
            b"BT" => {
                self.unsupported_operator_state(false);
                self.inside_text = true;
            }
            b"ET" => {
                self.unsupported_operator_state(false);
                self.inside_text = false;
            }
            _ => self.unsupported_operator_state(false),
        }
    }

    fn process_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        let operands = std::mem::take(&mut self.operands);
        self.process_operator_with_operands(operator, offset, length, &operands);
        self.operands = operands;
        self.operands.clear();
    }

    fn unsupported_operator_state(&mut self, clears_path: bool) {
        if clears_path {
            self.reset_path_state();
        } else if self.path_start.is_some() {
            self.path_valid = false;
        }
    }

    fn unsupported_operator(&mut self, clears_path: bool) {
        self.unsupported_operator_state(clears_path);
        self.operands.clear();
    }
}

impl ObjectHandleParserCallbacks for PathBlockScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = scalar.as_operator() {
            return self.handle_operator(operator, offset, length);
        } else {
            self.operands.push(Operand {
                value: OperandValue::Scalar(scalar),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        self.process_operator(operator, offset, length);
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.process_operator(&operator, offset, length);
        } else if object.as_inline_image().is_some() {
            self.unsupported_operator(false);
        } else {
            self.operands.push(Operand {
                value: OperandValue::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransformedBlockStage {
    ExpectPlacement,
    Body,
}

#[derive(Debug, Clone)]
struct TransformedBlockFrame {
    stage: TransformedBlockStage,
    valid: bool,
    body_start: usize,
    semantic_key: Vec<u8>,
    operator_count: usize,
    path_bounds: Option<Rect>,
    paint_bounds: Option<Rect>,
    path_started: bool,
}

impl TransformedBlockFrame {
    fn new(path_empty: bool) -> Self {
        Self {
            stage: TransformedBlockStage::ExpectPlacement,
            valid: path_empty,
            body_start: 0,
            semantic_key: Vec::new(),
            operator_count: 0,
            path_bounds: None,
            paint_bounds: None,
            path_started: false,
        }
    }

    fn include_point(&mut self, x: f64, y: f64) {
        if !x.is_finite() || !y.is_finite() {
            self.valid = false;
            return;
        }
        self.path_bounds = Some(match self.path_bounds {
            None => Rect {
                x0: x,
                y0: y,
                x1: x,
                y1: y,
            },
            Some(bounds) => Rect {
                x0: bounds.x0.min(x),
                y0: bounds.y0.min(y),
                x1: bounds.x1.max(x),
                y1: bounds.y1.max(y),
            },
        });
    }

    fn include_painted_bounds(&mut self, bounds: Rect) {
        self.paint_bounds = Some(match self.paint_bounds {
            None => bounds,
            Some(current) => Rect {
                x0: current.x0.min(bounds.x0),
                y0: current.y0.min(bounds.y0),
                x1: current.x1.max(bounds.x1),
                y1: current.y1.max(bounds.y1),
            },
        });
    }

    fn append_semantic(&mut self, operator: &[u8], values: &[f64]) {
        self.semantic_key
            .push(u8::try_from(operator.len()).unwrap_or(u8::MAX));
        self.semantic_key.extend_from_slice(operator);
        for value in values {
            let normalized = if *value == 0.0 { 0.0 } else { *value };
            self.semantic_key
                .extend_from_slice(&normalized.to_bits().to_be_bytes());
        }
        self.operator_count = self.operator_count.saturating_add(1);
    }
}

#[derive(Debug, Clone)]
struct TransformedBlock {
    body_start: usize,
    body_end: usize,
    bounds: Rect,
    semantic_key: Vec<u8>,
    operator_count: usize,
}

struct TransformedBlockScanner {
    operands: Vec<Operand>,
    frames: Vec<TransformedBlockFrame>,
    blocks: Vec<TransformedBlock>,
    current_path_nonempty: bool,
}

impl TransformedBlockScanner {
    fn new() -> Self {
        Self {
            operands: Vec::new(),
            frames: Vec::new(),
            blocks: Vec::new(),
            current_path_nonempty: false,
        }
    }

    fn numeric_operands_from(operands: &[Operand], expected: usize) -> Option<SmallVec<[f64; 6]>> {
        let values = operand_numbers(operands)?;
        (values.len() == expected).then_some(values)
    }

    fn invalidate_parents_for_nested_q(&mut self) {
        for frame in &mut self.frames {
            frame.valid = false;
        }
    }

    fn body_operator(&mut self, operator: &[u8], operands: &[Operand]) {
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        if !frame.valid || frame.stage != TransformedBlockStage::Body {
            return;
        }
        let expected = match operator {
            b"m" | b"l" => Some(2),
            b"c" => Some(6),
            b"v" | b"y" | b"re" | b"k" => Some(4),
            b"rg" => Some(3),
            b"g" => Some(1),
            b"h" | b"f" | b"F" | b"f*" => Some(0),
            _ => None,
        };
        let Some(expected) = expected else {
            frame.valid = false;
            return;
        };
        let values = operand_numbers(operands);
        let Some(values) = values.filter(|values| values.len() == expected) else {
            frame.valid = false;
            return;
        };
        frame.append_semantic(operator, &values);
        match operator {
            b"m" => {
                frame.path_started = true;
                frame.include_point(values[0], values[1]);
            }
            b"l" | b"c" | b"v" | b"y" => {
                if !frame.path_started {
                    frame.valid = false;
                    return;
                }
                for pair in values.as_chunks::<2>().0 {
                    frame.include_point(pair[0], pair[1]);
                }
            }
            b"re" => {
                frame.path_started = true;
                let x = values[0];
                let y = values[1];
                frame.include_point(x, y);
                frame.include_point(x + values[2], y + values[3]);
            }
            b"h" => {
                if !frame.path_started {
                    frame.valid = false;
                }
            }
            b"f" | b"F" | b"f*" => {
                if !frame.path_started {
                    frame.valid = false;
                    return;
                }
                if let Some(bounds) = frame.path_bounds.take() {
                    frame.include_painted_bounds(bounds);
                } else {
                    frame.valid = false;
                }
                frame.path_started = false;
            }
            b"g" | b"rg" | b"k" => {}
            _ => unreachable!(),
        }
    }

    fn process_operator_with_operands(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
        operands: &[Operand],
    ) {
        let end = offset.saturating_add(length);
        match operator {
            b"q" => {
                self.invalidate_parents_for_nested_q();
                self.frames
                    .push(TransformedBlockFrame::new(!self.current_path_nonempty));
            }
            b"Q" => self.close_frame(offset),
            b"cm" => {
                let valid_placement = Self::numeric_operands_from(operands, 6).is_some();
                if let Some(frame) = self.frames.last_mut() {
                    if frame.stage == TransformedBlockStage::ExpectPlacement {
                        if valid_placement {
                            frame.stage = TransformedBlockStage::Body;
                            frame.body_start = end;
                        } else {
                            frame.valid = false;
                        }
                    } else {
                        frame.valid = false;
                    }
                }
            }
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => {
                self.current_path_nonempty = true;
                self.body_operator(operator, operands);
            }
            b"f" | b"F" | b"f*" => {
                self.body_operator(operator, operands);
                self.current_path_nonempty = false;
            }
            b"n" | b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*" => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.valid = false;
                }
                self.current_path_nonempty = false;
            }
            b"g" | b"rg" | b"k" => self.body_operator(operator, operands),
            _ => {
                if let Some(frame) = self.frames.last_mut() {
                    frame.valid = false;
                }
            }
        }
    }

    fn process_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        let operands = std::mem::take(&mut self.operands);
        self.process_operator_with_operands(operator, offset, length, &operands);
        self.operands = operands;
        self.operands.clear();
    }

    fn close_frame(&mut self, q_offset: usize) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        if frame.valid
            && frame.stage == TransformedBlockStage::Body
            && !frame.path_started
            && let Some(bounds) = frame.paint_bounds
            && q_offset > frame.body_start
        {
            self.blocks.push(TransformedBlock {
                body_start: frame.body_start,
                body_end: q_offset,
                bounds,
                semantic_key: frame.semantic_key,
                operator_count: frame.operator_count,
            });
        }
    }
}

impl ObjectHandleParserCallbacks for TransformedBlockScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = scalar.as_operator() {
            return self.handle_operator(operator, offset, length);
        } else {
            self.operands.push(Operand {
                value: OperandValue::Scalar(scalar),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        self.process_operator(operator, offset, length);
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.process_operator(&operator, offset, length);
        } else if object.as_inline_image().is_some() {
            if let Some(frame) = self.frames.last_mut() {
                frame.valid = false;
            }
            self.operands.clear();
        } else {
            self.operands.push(Operand {
                value: OperandValue::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn factorable_transformed_blocks(input: &[u8]) -> Vec<TransformedBlock> {
    let mut scanner = TransformedBlockScanner::new();
    if flpdf::parse_detached_content_stream(input, "transformed block form factoring", &mut scanner)
        .is_err()
    {
        return Vec::new();
    }
    scanner.blocks
}

#[cfg(test)]
fn factorable_path_blocks(input: &[u8]) -> Vec<PathBlock> {
    let mut scanner = PathBlockScanner::new();
    if flpdf::parse_detached_content_stream(input, "repeated path form factoring", &mut scanner)
        .is_err()
    {
        return Vec::new();
    }
    scanner.blocks
}

struct ProcessingFactorScanner {
    operands: Vec<Operand>,
    path: PathBlockScanner,
    transformed: TransformedBlockScanner,
}

impl ProcessingFactorScanner {
    fn new() -> Self {
        Self {
            operands: Vec::new(),
            path: PathBlockScanner::new(),
            transformed: TransformedBlockScanner::new(),
        }
    }

    fn process_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        self.path
            .process_operator_with_operands(operator, offset, length, &self.operands);
        self.transformed
            .process_operator_with_operands(operator, offset, length, &self.operands);
        self.operands.clear();
    }

    fn inline_image(&mut self) {
        self.path.unsupported_operator_state(false);
        if let Some(frame) = self.transformed.frames.last_mut() {
            frame.valid = false;
        }
        self.operands.clear();
    }
}

impl ObjectHandleParserCallbacks for ProcessingFactorScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn content_size(&mut self, size: usize) -> flpdf::Result<()> {
        self.path.content_size(size)?;
        self.transformed.content_size(size)
    }

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = scalar.as_operator() {
            self.process_operator(operator, offset, length);
        } else {
            self.operands.push(Operand {
                value: OperandValue::Scalar(scalar),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        self.process_operator(operator, offset, length);
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.process_operator(&operator, offset, length);
        } else if object.as_inline_image().is_some() {
            self.inline_image();
        } else {
            self.operands.push(Operand {
                value: OperandValue::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn factorable_processing_blocks(input: &[u8]) -> (Vec<PathBlock>, Vec<TransformedBlock>) {
    let mut scanner = ProcessingFactorScanner::new();
    if flpdf::parse_detached_content_stream(input, "processing form factoring", &mut scanner)
        .is_err()
    {
        return (Vec::new(), Vec::new());
    }
    (scanner.path.blocks, scanner.transformed.blocks)
}

pub(crate) struct ProcessingVectorAnalysis {
    fills: Vec<FillPaint>,
    path_blocks: Vec<PathBlock>,
    transformed_blocks: Vec<TransformedBlock>,
}

fn processing_transformed_factor_candidate(
    body_len: usize,
    operator_count: usize,
    occurrence_count: usize,
) -> bool {
    if body_len < MIN_PROCESSING_PATH_FORM_BYTES || occurrence_count < MIN_PATH_FORM_OCCURRENCES {
        return false;
    }
    operator_count.saturating_mul(occurrence_count.saturating_sub(1)) >= 8
}

pub(crate) fn processing_factor_candidate(
    cache: &BTreeMap<ObjectHandle, ProcessingVectorAnalysis>,
) -> bool {
    let mut path_keys = HashSet::new();
    let mut repeated_path = false;
    let mut transformed = HashMap::<&[u8], (usize, usize, usize)>::new();
    for analysis in cache.values() {
        for block in &analysis.path_blocks {
            if !path_keys.insert(block.semantic_key.as_slice()) {
                repeated_path = true;
            }
        }
        for block in &analysis.transformed_blocks {
            let body_len = block.body_end.saturating_sub(block.body_start);
            let entry = transformed
                .entry(block.semantic_key.as_slice())
                .or_insert((0, 0, 0));
            entry.0 = entry.0.saturating_add(1);
            // Equivalent semantic bodies can still differ lexically. Maxima are
            // deliberately conservative: they can cause a false positive (run
            // the full stage) but never prove away a potentially factorable group.
            entry.1 = entry.1.max(body_len);
            entry.2 = entry.2.max(block.operator_count);
        }
    }
    repeated_path
        || transformed.values().any(|(count, body_len, operators)| {
            processing_transformed_factor_candidate(*body_len, *operators, *count)
        })
}

pub(crate) struct ProcessingPageScanner {
    fill: FillScanner,
    factor: ProcessingFactorScanner,
}

impl ProcessingPageScanner {
    fn new(ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>) -> Self {
        Self {
            fill: FillScanner::new(ext_gstates),
            factor: ProcessingFactorScanner::new(),
        }
    }

    pub(crate) fn for_resources(
        document: &EditDocument,
        resources: &OwnedDictionary,
    ) -> Result<Self> {
        Ok(Self::new(ext_gstate_patches(document, resources)?))
    }

    pub(crate) fn finish(self) -> ProcessingVectorAnalysis {
        ProcessingVectorAnalysis {
            fills: self.fill.fills,
            path_blocks: self.factor.path.blocks,
            transformed_blocks: self.factor.transformed.blocks,
        }
    }
}

impl ObjectHandleParserCallbacks for ProcessingPageScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn content_size(&mut self, size: usize) -> flpdf::Result<()> {
        self.factor.content_size(size)
    }

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        let fill = self.fill.handle_scalar(scalar.clone(), offset, length)?;
        let factor = self.factor.handle_scalar(scalar, offset, length)?;
        if matches!(fill, ParseControl::Stop) || matches!(factor, ParseControl::Stop) {
            Ok(ParseControl::Stop)
        } else {
            Ok(ParseControl::Continue)
        }
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        let fill = self.fill.handle_operator(operator, offset, length)?;
        let factor = self.factor.handle_operator(operator, offset, length)?;
        if matches!(fill, ParseControl::Stop) || matches!(factor, ParseControl::Stop) {
            Ok(ParseControl::Stop)
        } else {
            Ok(ParseControl::Continue)
        }
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        let fill = self.fill.handle_object(object.clone(), offset, length)?;
        let factor = self.factor.handle_object(object, offset, length)?;
        Ok(
            if matches!(fill, ParseControl::Stop) || matches!(factor, ParseControl::Stop) {
                ParseControl::Stop
            } else {
                ParseControl::Continue
            },
        )
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        self.fill.handle_eof()?;
        self.factor.handle_eof()
    }
}

fn scan_processing_page(
    input: &[u8],
    ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>,
) -> Option<ProcessingVectorAnalysis> {
    let mut scanner = ProcessingPageScanner::new(ext_gstates);
    flpdf::parse_detached_content_stream(input, "processing vector analysis", &mut scanner).ok()?;
    Some(scanner.finish())
}

#[derive(Debug, Clone)]
struct PathOccurrence {
    page_index: usize,
    start: usize,
    end: usize,
    bounds: Rect,
}

#[derive(Debug)]
struct PagePathData {
    page: ObjectHandle,
    decoded: Vec<u8>,
}

fn page_effective_resources(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<OwnedDictionary> {
    Ok(match document.inherited_page_value(page, b"Resources")? {
        Some(value) => resolved_dictionary(document, Some(&value))?.unwrap_or_default(),
        None => OwnedDictionary::default(),
    })
}

fn install_page_xobject(
    document: &mut EditDocument,
    page: ObjectHandle,
    name: Vec<u8>,
    target: ObjectHandle,
) -> Result<()> {
    let mut resources = page_effective_resources(document, page)?;
    let mut xobjects =
        resolved_dictionary(document, resources.get(b"XObject".as_slice()))?.unwrap_or_default();
    xobjects.insert(name, OwnedObject::Reference(target));
    resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));
    let object = match page {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| crate::Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
}

fn path_form_name(index: usize, occupied: &mut BTreeSet<Vec<u8>>) -> Vec<u8> {
    let mut serial = index;
    loop {
        let name = format!("PdfRedoxPath{serial}").into_bytes();
        if occupied.insert(name.clone()) {
            return name;
        }
        serial = serial.saturating_add(1);
    }
}

fn path_form_dictionary(bounds: Rect) -> OwnedDictionary {
    let mut dictionary = OwnedDictionary::new();
    dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec()));
    dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec()));
    dictionary.insert(b"FormType".to_vec(), OwnedObject::Integer(1));
    dictionary.insert(
        b"BBox".to_vec(),
        OwnedObject::Array(vec![
            OwnedObject::Real(bounds.x0 - PATH_FORM_BBOX_MARGIN),
            OwnedObject::Real(bounds.y0 - PATH_FORM_BBOX_MARGIN),
            OwnedObject::Real(bounds.x1 + PATH_FORM_BBOX_MARGIN),
            OwnedObject::Real(bounds.y1 + PATH_FORM_BBOX_MARGIN),
        ]),
    );
    dictionary.insert(
        b"Resources".to_vec(),
        OwnedObject::Dictionary(OwnedDictionary::new()),
    );
    dictionary
}

fn apply_span_replacements(
    input: &[u8],
    replacements: &[(usize, usize, Vec<u8>)],
) -> Option<Vec<u8>> {
    let mut replacements = replacements.to_vec();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        if start < cursor || end > input.len() || start > end {
            return None;
        }
        output.extend_from_slice(&input[cursor..start]);
        output.extend_from_slice(&replacement);
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    Some(output)
}

struct PathFactoringOutcome {
    stats: VectorCompactionStats,
    cached_transformed: BTreeMap<ObjectHandle, Vec<TransformedBlock>>,
    rewritten_pages: BTreeSet<ObjectHandle>,
}

fn cached_path_factoring_is_impossible_after_transformed_preference(
    document: &EditDocument,
    pre_scanned: &BTreeMap<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>,
    pages: &[ObjectHandle],
) -> Result<bool> {
    // Mirror the path-factor pass's transformed-body preference using only
    // cached block metadata. This is a proof-only fast path: any missing cache
    // or surviving repeated path key falls back to the full implementation.
    // Refuse the proof before touching the document on path-heavy inputs.
    let cached_path_blocks = pre_scanned
        .values()
        .map(|(path_blocks, _)| path_blocks.len())
        .try_fold(0usize, |total, count| {
            let total = total.saturating_add(count);
            (total <= MAX_CACHED_PATH_PROOF_BLOCKS).then_some(total)
        });
    if cached_path_blocks.is_none() {
        return Ok(false);
    }

    let mut transformed_counts = HashMap::<&[u8], (usize, usize, usize)>::new();
    for &page in pages {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        if !dictionary.contains_key(b"Contents".as_slice()) {
            continue;
        }
        let Some((_, transformed_blocks)) = pre_scanned.get(&page) else {
            return Ok(false);
        };
        for block in transformed_blocks {
            let body_len = block.body_end.saturating_sub(block.body_start);
            let entry = transformed_counts
                .entry(block.semantic_key.as_slice())
                .or_insert((0, block.operator_count, body_len));
            entry.0 = entry.0.saturating_add(1);
        }
    }
    let preferred_transformed = transformed_counts
        .iter()
        .filter(|(_, (count, operators, body_len))| {
            processing_transformed_factor_candidate(*body_len, *operators, *count)
        })
        .map(|(key, _)| *key)
        .collect::<HashSet<_>>();

    let mut path_keys = HashSet::<&[u8]>::new();
    for &page in pages {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        if !dictionary.contains_key(b"Contents".as_slice()) {
            continue;
        }
        let Some((path_blocks, transformed_blocks)) = pre_scanned.get(&page) else {
            return Ok(false);
        };
        for block in path_blocks {
            let nested_in_preferred = transformed_blocks.iter().any(|outer| {
                preferred_transformed.contains(outer.semantic_key.as_slice())
                    && outer.body_start <= block.start
                    && block.end <= outer.body_end
            });
            if nested_in_preferred {
                continue;
            }
            if !path_keys.insert(block.semantic_key.as_slice()) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn factor_repeated_path_forms_hayro(
    document: &mut EditDocument,
    flate_level: i32,
    mut pre_scanned: BTreeMap<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>,
    pages: &[ObjectHandle],
) -> Result<PathFactoringOutcome> {
    if cached_path_factoring_is_impossible_after_transformed_preference(
        document,
        &pre_scanned,
        pages,
    )? {
        if *DEBUG_VECTOR {
            eprintln!("path-forms cached-proof=skip");
        }
        let cached_transformed = pre_scanned
            .into_iter()
            .map(|(page, (_, transformed))| (page, transformed))
            .collect();
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            cached_transformed,
            rewritten_pages: BTreeSet::new(),
        });
    }

    let mut page_data = Vec::with_capacity(pages.len());
    let mut page_blocks =
        Vec::<(Vec<PathBlock>, Vec<TransformedBlock>)>::with_capacity(pages.len());
    let mut groups = BTreeMap::<Vec<u8>, (Vec<u8>, usize, Vec<PathOccurrence>)>::new();
    let mut occupied_names = BTreeSet::new();
    let mut scanned_blocks = 0usize;
    let mut cached_transformed = BTreeMap::new();

    for &page in pages {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(document, contents, &mut decoded)?;
        let blocks = pre_scanned
            .remove(&page)
            .unwrap_or_else(|| factorable_processing_blocks(&decoded));
        let resources = page_effective_resources(document, page)?;
        if let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
        {
            occupied_names.extend(xobjects.keys().cloned());
        }
        page_data.push(PagePathData { page, decoded });
        page_blocks.push(blocks);
    }

    // Prefer a larger repeated transformed body over path candidates nested
    // inside it. Rewriting an inner path to a Form call makes that outer body
    // resource-dependent and intentionally ineligible for transformed-form
    // factoring later.
    let mut transformed_counts = HashMap::<&[u8], (usize, usize, usize)>::new();
    for (_, transformed_blocks) in &page_blocks {
        for block in transformed_blocks {
            let body_len = block.body_end.saturating_sub(block.body_start);
            let entry = transformed_counts
                .entry(block.semantic_key.as_slice())
                .or_insert((0, block.operator_count, body_len));
            entry.0 = entry.0.saturating_add(1);
        }
    }
    let preferred_transformed = transformed_counts
        .into_iter()
        .filter(|(_, (count, operators, body_len))| {
            processing_transformed_factor_candidate(*body_len, *operators, *count)
        })
        .map(|(key, _)| key.to_vec())
        .collect::<HashSet<_>>();

    for (page_index, (path_blocks, transformed_blocks)) in page_blocks.into_iter().enumerate() {
        let page = page_data[page_index].page;
        for block in path_blocks {
            scanned_blocks = scanned_blocks.saturating_add(1);
            let nested_in_preferred = transformed_blocks.iter().any(|outer| {
                preferred_transformed.contains(outer.semantic_key.as_slice())
                    && outer.body_start <= block.start
                    && block.end <= outer.body_end
            });
            if nested_in_preferred {
                continue;
            }
            let bytes = page_data[page_index].decoded[block.start..block.end].to_vec();
            let entry = groups
                .entry(block.semantic_key)
                .or_insert_with(|| (bytes, block.operator_count, Vec::new()));
            entry.2.push(PathOccurrence {
                page_index,
                start: block.start,
                end: block.end,
                bounds: block.bounds,
            });
        }
        cached_transformed.insert(page, transformed_blocks);
    }

    let semantic_groups = groups.len();
    let mut candidates = groups
        .into_values()
        .filter(|(_, _, occurrences)| occurrences.len() >= MIN_PATH_FORM_OCCURRENCES)
        .collect::<Vec<_>>();
    if *DEBUG_VECTOR {
        eprintln!(
            "path-forms scanned_blocks={} semantic_groups={} repeated_candidates={}",
            scanned_blocks,
            semantic_groups,
            candidates.len()
        );
    }
    if candidates.is_empty() {
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            cached_transformed,
            rewritten_pages: BTreeSet::new(),
        });
    }
    candidates.sort_unstable_by_key(|(bytes, _, _)| std::cmp::Reverse(bytes.len()));

    let mut replacements_by_page = vec![Vec::<(usize, usize, Vec<u8>)>::new(); page_data.len()];
    let mut selected = Vec::<(Vec<u8>, Vec<u8>, Rect, Vec<PathOccurrence>)>::new();
    let mut form_index = 0usize;
    for (bytes, _operator_count, occurrences) in candidates {
        let name = path_form_name(form_index, &mut occupied_names);
        form_index = form_index.saturating_add(1);
        let replacement = [b" /".as_slice(), name.as_slice(), b" Do ".as_slice()].concat();
        let bounds = occurrences[0].bounds;
        for occurrence in &occurrences {
            replacements_by_page[occurrence.page_index].push((
                occurrence.start,
                occurrence.end,
                replacement.clone(),
            ));
        }
        selected.push((bytes, name, bounds, occurrences));
    }
    if *DEBUG_VECTOR {
        eprintln!("path-forms overhead-selected={}", selected.len());
    }
    if selected.is_empty() {
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            cached_transformed,
            rewritten_pages: BTreeSet::new(),
        });
    }

    let affected_pages = selected
        .iter()
        .flat_map(|(_, _, _, occurrences)| {
            occurrences.iter().map(|occurrence| occurrence.page_index)
        })
        .collect::<BTreeSet<_>>();
    let mut before_flate = 0usize;
    let mut after_flate = 0usize;
    let mut rewritten_pages = BTreeMap::new();
    for &page_index in &affected_pages {
        let Some(page) = page_data.get(page_index) else {
            continue;
        };
        before_flate = before_flate.saturating_add(compressed_len(&page.decoded, flate_level)?);
        let Some(rewritten) =
            apply_span_replacements(&page.decoded, &replacements_by_page[page_index])
        else {
            return Ok(PathFactoringOutcome {
                stats: VectorCompactionStats::default(),
                cached_transformed,
                rewritten_pages: BTreeSet::new(),
            });
        };
        after_flate = after_flate.saturating_add(compressed_len(&rewritten, flate_level)?);
        rewritten_pages.insert(page_index, rewritten);
    }
    let resource_refs = selected
        .iter()
        .map(|(_, _, _, occurrences)| {
            occurrences
                .iter()
                .map(|occurrence| occurrence.page_index)
                .collect::<BTreeSet<_>>()
                .len()
        })
        .sum::<usize>();
    let fixed_overhead = selected
        .len()
        .saturating_mul(256)
        .saturating_add(resource_refs.saturating_mul(32));
    after_flate = after_flate.saturating_add(fixed_overhead);
    // Compressed Form bodies are non-negative. If rewritten page streams plus
    // unavoidable Form/resource overhead already lose, reject before spending
    // CPU compressing every candidate Form body.
    if after_flate >= before_flate {
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            cached_transformed,
            rewritten_pages: BTreeSet::new(),
        });
    }
    for (bytes, _, _, _) in &selected {
        after_flate = after_flate.saturating_add(compressed_len(bytes, flate_level)?);
    }
    if *DEBUG_VECTOR {
        eprintln!(
            "path-forms flate before={} after={}",
            before_flate, after_flate
        );
    }
    // As with transformed Forms, everything above is speculative. Refuse a
    // factoring rewrite that does not win after compressed stream bytes and
    // conservative Form/resource overhead are charged.
    if after_flate >= before_flate {
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            cached_transformed,
            rewritten_pages: BTreeSet::new(),
        });
    }
    let mut form_handles = Vec::with_capacity(selected.len());
    for (bytes, _, bounds, _) in &selected {
        let handle = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
            dictionary: path_form_dictionary(*bounds),
            data: StreamData::Owned(bytes.clone()),
        }));
        form_handles.push(handle);
    }
    let mut rewritten_page_handles = BTreeSet::new();
    for (page_index, rewritten) in rewritten_pages {
        let Some(page) = page_data.get(page_index) else {
            continue;
        };
        if rewritten != page.decoded {
            replace_page_content(document, page.page, rewritten)?;
            rewritten_page_handles.insert(page.page);
        }
    }
    for ((_, name, _, occurrences), handle) in selected.iter().zip(form_handles) {
        let pages = occurrences
            .iter()
            .map(|occurrence| occurrence.page_index)
            .collect::<BTreeSet<_>>();
        for page_index in pages {
            install_page_xobject(document, page_data[page_index].page, name.clone(), handle)?;
        }
    }

    let occurrences = selected
        .iter()
        .map(|(_, _, _, occurrences)| occurrences.len())
        .sum::<usize>();
    let factored = selected
        .iter()
        .map(|(bytes, _, _, occurrences)| {
            bytes
                .len()
                .saturating_mul(occurrences.len().saturating_sub(1))
        })
        .sum::<usize>();
    let mut generated_page_xobjects = BTreeMap::<ObjectHandle, BTreeSet<Vec<u8>>>::new();
    for (_, name, _, occurrences) in &selected {
        for occurrence in occurrences {
            generated_page_xobjects
                .entry(page_data[occurrence.page_index].page)
                .or_default()
                .insert(name.clone());
        }
    }
    Ok(PathFactoringOutcome {
        stats: VectorCompactionStats {
            path_form_pages_rewritten: replacements_by_page
                .iter()
                .filter(|r| !r.is_empty())
                .count(),
            path_forms_created: selected.len(),
            path_form_occurrences_replaced: occurrences,
            path_form_decoded_bytes_factored: factored,
            path_form_estimated_flate_bytes_saved: before_flate.saturating_sub(after_flate),
            estimated_flate_bytes_saved: before_flate.saturating_sub(after_flate),
            decoded_bytes_removed: factored,
            generated_page_xobjects,
            ..VectorCompactionStats::default()
        },
        cached_transformed,
        rewritten_pages: rewritten_page_handles,
    })
}

fn factor_repeated_transformed_blocks_hayro(
    document: &mut EditDocument,
    flate_level: i32,
    mut cached_transformed: BTreeMap<ObjectHandle, Vec<TransformedBlock>>,
    rewritten_pages: &BTreeSet<ObjectHandle>,
    pages: &[ObjectHandle],
) -> Result<VectorCompactionStats> {
    let mut page_data = Vec::with_capacity(pages.len());
    let mut groups = BTreeMap::<Vec<u8>, (Vec<u8>, usize, Vec<PathOccurrence>)>::new();
    let mut occupied_names = BTreeSet::new();
    let mut scanned_blocks = 0usize;

    for &page in pages {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(document, contents, &mut decoded)?;
        let blocks = if rewritten_pages.contains(&page) {
            factorable_transformed_blocks(&decoded)
        } else {
            cached_transformed
                .remove(&page)
                .unwrap_or_else(|| factorable_transformed_blocks(&decoded))
        };
        for block in blocks {
            scanned_blocks = scanned_blocks.saturating_add(1);
            let bytes = decoded[block.body_start..block.body_end].to_vec();
            let entry = groups
                .entry(block.semantic_key)
                .or_insert_with(|| (bytes, block.operator_count, Vec::new()));
            entry.2.push(PathOccurrence {
                page_index: page_data.len(),
                start: block.body_start,
                end: block.body_end,
                bounds: block.bounds,
            });
        }
        let resources = page_effective_resources(document, page)?;
        if let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
        {
            occupied_names.extend(xobjects.keys().cloned());
        }
        page_data.push(PagePathData { page, decoded });
    }

    let semantic_groups = groups.len();
    let mut candidates = groups
        .into_values()
        .filter(|(_, _, occurrences)| occurrences.len() >= MIN_PATH_FORM_OCCURRENCES)
        .collect::<Vec<_>>();
    if *DEBUG_VECTOR {
        eprintln!(
            "transform-forms scanned_blocks={} semantic_groups={} repeated_candidates={}",
            scanned_blocks,
            semantic_groups,
            candidates.len()
        );
    }
    if candidates.is_empty() {
        return Ok(VectorCompactionStats::default());
    }
    candidates.sort_unstable_by_key(|(bytes, operators, occurrences)| {
        std::cmp::Reverse(
            bytes
                .len()
                .saturating_mul(occurrences.len())
                .saturating_add(operators.saturating_mul(occurrences.len())),
        )
    });

    let mut replacements_by_page = vec![Vec::<(usize, usize, Vec<u8>)>::new(); page_data.len()];
    let mut selected = Vec::<(Vec<u8>, Vec<u8>, Rect, usize, Vec<PathOccurrence>)>::new();
    let mut form_index = 0usize;
    for (bytes, operator_count, occurrences) in candidates {
        if !processing_transformed_factor_candidate(bytes.len(), operator_count, occurrences.len())
        {
            continue;
        }
        let name = path_form_name(form_index.saturating_add(10_000), &mut occupied_names);
        form_index = form_index.saturating_add(1);
        let replacement = [b" /".as_slice(), name.as_slice(), b" Do ".as_slice()].concat();
        let bounds = occurrences[0].bounds;
        for occurrence in &occurrences {
            replacements_by_page[occurrence.page_index].push((
                occurrence.start,
                occurrence.end,
                replacement.clone(),
            ));
        }
        selected.push((bytes, name, bounds, operator_count, occurrences));
    }
    if *DEBUG_VECTOR {
        eprintln!("transform-forms selected={}", selected.len());
    }
    if selected.is_empty() {
        return Ok(VectorCompactionStats::default());
    }

    let affected_pages = selected
        .iter()
        .flat_map(|(_, _, _, _, occurrences)| {
            occurrences.iter().map(|occurrence| occurrence.page_index)
        })
        .collect::<BTreeSet<_>>();
    let mut before_flate = 0usize;
    let mut after_flate = 0usize;
    let mut rewritten_pages = BTreeMap::new();
    for &page_index in &affected_pages {
        let Some(page) = page_data.get(page_index) else {
            continue;
        };
        before_flate = before_flate.saturating_add(compressed_len(&page.decoded, flate_level)?);
        let Some(rewritten) =
            apply_span_replacements(&page.decoded, &replacements_by_page[page_index])
        else {
            return Ok(VectorCompactionStats::default());
        };
        after_flate = after_flate.saturating_add(compressed_len(&rewritten, flate_level)?);
        rewritten_pages.insert(page_index, rewritten);
    }
    let resource_refs = selected
        .iter()
        .map(|(_, _, _, _, occurrences)| {
            occurrences
                .iter()
                .map(|occurrence| occurrence.page_index)
                .collect::<BTreeSet<_>>()
                .len()
        })
        .sum::<usize>();
    let fixed_overhead = selected
        .len()
        .saturating_mul(256)
        .saturating_add(resource_refs.saturating_mul(32));
    after_flate = after_flate.saturating_add(fixed_overhead);
    // Compressed Form bodies are non-negative. If rewritten page streams plus
    // unavoidable Form/resource overhead already lose, reject before spending
    // CPU compressing every candidate Form body.
    if after_flate >= before_flate {
        return Ok(VectorCompactionStats::default());
    }
    for (bytes, _, _, _, _) in &selected {
        after_flate = after_flate.saturating_add(compressed_len(bytes, flate_level)?);
    }
    if *DEBUG_VECTOR {
        eprintln!(
            "transform-forms flate before={} after={}",
            before_flate, after_flate
        );
    }
    // All work above is still speculative. Do not materialize Forms or rewrite
    // pages unless factoring wins after charging both compressed stream bytes
    // and conservative per-Form/resource-reference overhead.
    if after_flate >= before_flate {
        return Ok(VectorCompactionStats::default());
    }
    let mut form_handles = Vec::with_capacity(selected.len());
    for (bytes, _, bounds, _, _) in &selected {
        let handle = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
            dictionary: path_form_dictionary(*bounds),
            data: StreamData::Owned(bytes.clone()),
        }));
        form_handles.push(handle);
    }
    for (page_index, rewritten) in rewritten_pages {
        let Some(page) = page_data.get(page_index) else {
            continue;
        };
        if rewritten != page.decoded {
            replace_page_content(document, page.page, rewritten)?;
        }
    }
    for ((_, name, _, _, occurrences), handle) in selected.iter().zip(form_handles) {
        let pages = occurrences
            .iter()
            .map(|occurrence| occurrence.page_index)
            .collect::<BTreeSet<_>>();
        for page_index in pages {
            install_page_xobject(document, page_data[page_index].page, name.clone(), handle)?;
        }
    }

    let occurrences = selected
        .iter()
        .map(|(_, _, _, _, occurrences)| occurrences.len())
        .sum::<usize>();
    let operators_factored = selected
        .iter()
        .map(|(_, _, _, operator_count, occurrences)| {
            operator_count.saturating_mul(occurrences.len().saturating_sub(1))
        })
        .sum::<usize>();
    let pages_rewritten = replacements_by_page
        .iter()
        .filter(|r| !r.is_empty())
        .count();
    let mut generated_page_xobjects = BTreeMap::<ObjectHandle, BTreeSet<Vec<u8>>>::new();
    for (_, name, _, _, occurrences) in &selected {
        for occurrence in occurrences {
            generated_page_xobjects
                .entry(page_data[occurrence.page_index].page)
                .or_default()
                .insert(name.clone());
        }
    }
    Ok(VectorCompactionStats {
        pages_compacted: pages_rewritten,
        transformed_forms_created: selected.len(),
        transformed_form_pages_rewritten: pages_rewritten,
        transformed_form_occurrences_replaced: occurrences,
        transformed_form_operators_eliminated: operators_factored,
        transformed_form_estimated_flate_bytes_saved: before_flate.saturating_sub(after_flate),
        estimated_flate_bytes_saved: before_flate.saturating_sub(after_flate),
        generated_page_xobjects,
        ..VectorCompactionStats::default()
    })
}

fn compressed_len(bytes: &[u8], flate_level: i32) -> Result<usize> {
    let level = u32::try_from(flate_level.clamp(0, 9)).unwrap_or(9);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?.len())
}

pub(crate) fn compact_vector_paths_hayro(
    document: &mut EditDocument,
    flate_level: i32,
    goal: OptimizationGoal,
    mut raster_pre_scanned: BTreeMap<ObjectHandle, ProcessingVectorAnalysis>,
) -> Result<VectorCompactionStats> {
    let mut total = VectorCompactionStats::default();
    let mut pre_scanned_processing =
        BTreeMap::<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>::new();
    let pages = document.page_handles()?;
    for &page in &pages {
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(document, contents, &mut decoded)?;
        let resources = match document.inherited_page_value(page, b"Resources")? {
            Some(value) => resolved_dictionary(document, Some(&value))?.unwrap_or_default(),
            None => OwnedDictionary::default(),
        };
        let ext_gstates = ext_gstate_patches(document, &resources)?;
        let ((compacted, stats), factor_cache) = if goal == OptimizationGoal::Processing {
            let analysis = raster_pre_scanned
                .remove(&page)
                .or_else(|| scan_processing_page(&decoded, ext_gstates.clone()));
            if let Some(analysis) = analysis {
                (
                    compact_content_from_fill_scan(&decoded, &ext_gstates, &analysis.fills),
                    Some((analysis.path_blocks, analysis.transformed_blocks)),
                )
            } else {
                (
                    compact_content_with_ext_gstates(&decoded, &ext_gstates),
                    None,
                )
            }
        } else {
            (
                compact_content_with_ext_gstates(&decoded, &ext_gstates),
                None,
            )
        };
        if compacted == decoded {
            if let Some(factor_cache) = factor_cache {
                pre_scanned_processing.insert(page, factor_cache);
            }
            continue;
        }
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = compressed_len(&compacted, flate_level)?;
        if goal == OptimizationGoal::Size && after_flate >= before_flate {
            continue;
        }
        replace_page_content(document, page, compacted)?;
        total.pages_compacted += 1;
        total.fill_groups_batched += stats.fill_groups_batched;
        total.covered_fills_pruned += stats.covered_fills_pruned;
        total.fill_paints_eliminated += stats.fill_paints_eliminated;
        total.decoded_bytes_removed += stats.decoded_bytes_removed;
        total.estimated_flate_bytes_saved += before_flate.saturating_sub(after_flate);
    }
    if goal == OptimizationGoal::Processing {
        let factored = factor_repeated_path_forms_hayro(
            document,
            flate_level,
            pre_scanned_processing,
            &pages,
        )?;
        total.path_forms_created += factored.stats.path_forms_created;
        total.path_form_pages_rewritten += factored.stats.path_form_pages_rewritten;
        total.path_form_occurrences_replaced += factored.stats.path_form_occurrences_replaced;
        total.path_form_decoded_bytes_factored += factored.stats.path_form_decoded_bytes_factored;
        total.path_form_estimated_flate_bytes_saved +=
            factored.stats.path_form_estimated_flate_bytes_saved;
        total.decoded_bytes_removed += factored.stats.decoded_bytes_removed;
        total.estimated_flate_bytes_saved += factored.stats.estimated_flate_bytes_saved;
        for (page, names) in &factored.stats.generated_page_xobjects {
            total
                .generated_page_xobjects
                .entry(*page)
                .or_default()
                .extend(names.iter().cloned());
        }

        let transformed = factor_repeated_transformed_blocks_hayro(
            document,
            flate_level,
            factored.cached_transformed,
            &factored.rewritten_pages,
            &pages,
        )?;
        total.pages_compacted += transformed.pages_compacted;
        total.transformed_forms_created += transformed.transformed_forms_created;
        total.transformed_form_pages_rewritten += transformed.transformed_form_pages_rewritten;
        total.transformed_form_occurrences_replaced +=
            transformed.transformed_form_occurrences_replaced;
        total.transformed_form_operators_eliminated +=
            transformed.transformed_form_operators_eliminated;
        total.transformed_form_estimated_flate_bytes_saved +=
            transformed.transformed_form_estimated_flate_bytes_saved;
        total.estimated_flate_bytes_saved += transformed.estimated_flate_bytes_saved;
        for (page, names) in &transformed.generated_page_xobjects {
            total
                .generated_page_xobjects
                .entry(*page)
                .or_default()
                .extend(names.iter().cloned());
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectId;

    fn processing_analysis(
        path_keys: &[&[u8]],
        transformed_keys: &[&[u8]],
    ) -> ProcessingVectorAnalysis {
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        };
        ProcessingVectorAnalysis {
            fills: Vec::new(),
            path_blocks: path_keys
                .iter()
                .map(|key| PathBlock {
                    start: 0,
                    end: 1,
                    bounds,
                    semantic_key: (*key).to_vec(),
                    operator_count: 1,
                })
                .collect(),
            transformed_blocks: transformed_keys
                .iter()
                .map(|key| TransformedBlock {
                    body_start: 0,
                    body_end: MIN_PROCESSING_PATH_FORM_BYTES,
                    bounds,
                    semantic_key: (*key).to_vec(),
                    operator_count: 8,
                })
                .collect(),
        }
    }

    #[test]
    fn processing_factor_candidate_requires_a_repeated_semantic_key() {
        let first = ObjectHandle::Existing(ObjectId::new(1, 0));
        let second = ObjectHandle::Existing(ObjectId::new(2, 0));

        let unique = BTreeMap::from([
            (first, processing_analysis(&[b"path-a"], &[b"transform-a"])),
            (second, processing_analysis(&[b"path-b"], &[b"transform-b"])),
        ]);
        assert!(!processing_factor_candidate(&unique));

        let repeated_path = BTreeMap::from([
            (first, processing_analysis(&[b"path-a"], &[])),
            (second, processing_analysis(&[b"path-a"], &[])),
        ]);
        assert!(processing_factor_candidate(&repeated_path));

        let repeated_transform = BTreeMap::from([
            (first, processing_analysis(&[], &[b"transform-a"])),
            (second, processing_analysis(&[], &[b"transform-a"])),
        ]);
        assert!(processing_factor_candidate(&repeated_transform));
    }

    #[test]
    fn processing_factor_candidate_rejects_repeated_unfactorable_transforms() {
        let first = ObjectHandle::Existing(ObjectId::new(1, 0));
        let second = ObjectHandle::Existing(ObjectId::new(2, 0));
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 1.0,
            y1: 1.0,
        };
        let tiny = |key: &[u8]| ProcessingVectorAnalysis {
            fills: Vec::new(),
            path_blocks: Vec::new(),
            transformed_blocks: vec![TransformedBlock {
                body_start: 0,
                body_end: 39,
                bounds,
                semantic_key: key.to_vec(),
                operator_count: 32,
            }],
        };
        let cache = BTreeMap::from([
            (first, tiny(b"same-transform")),
            (second, tiny(b"same-transform")),
        ]);
        assert!(!processing_factor_candidate(&cache));
    }

    #[test]
    fn processing_transformed_factor_candidate_rejects_tiny_bodies_even_when_frequent() {
        assert!(!processing_transformed_factor_candidate(39, 4, 100_000));
        assert!(!processing_transformed_factor_candidate(96, 4, 2));
        assert!(processing_transformed_factor_candidate(96, 8, 2));
        assert!(processing_transformed_factor_candidate(96, 1, 9));
    }

    #[test]
    fn shared_processing_factor_operands_match_standalone_scanners() {
        let mut input = b"q 1 0 0 1 10 20 cm 1 0 0 rg 0 0 10 10 re f Q ".to_vec();
        input.extend_from_slice(b"0 0 m ");
        for index in 0..700 {
            input.extend_from_slice(format!("{} {} l ", index + 1, index + 2).as_bytes());
        }
        input.extend_from_slice(b"f");

        let (shared_paths, shared_transformed) = factorable_processing_blocks(&input);
        let standalone_paths = factorable_path_blocks(&input);
        let standalone_transformed = factorable_transformed_blocks(&input);

        assert_eq!(shared_paths.len(), standalone_paths.len());
        for (shared, standalone) in shared_paths.iter().zip(&standalone_paths) {
            assert_eq!(shared.start, standalone.start);
            assert_eq!(shared.end, standalone.end);
            assert_eq!(shared.semantic_key, standalone.semantic_key);
            assert_eq!(shared.operator_count, standalone.operator_count);
            assert_eq!(shared.bounds.x0, standalone.bounds.x0);
            assert_eq!(shared.bounds.y0, standalone.bounds.y0);
            assert_eq!(shared.bounds.x1, standalone.bounds.x1);
            assert_eq!(shared.bounds.y1, standalone.bounds.y1);
        }

        assert_eq!(shared_transformed.len(), standalone_transformed.len());
        for (shared, standalone) in shared_transformed.iter().zip(&standalone_transformed) {
            assert_eq!(shared.body_start, standalone.body_start);
            assert_eq!(shared.body_end, standalone.body_end);
            assert_eq!(shared.semantic_key, standalone.semantic_key);
            assert_eq!(shared.operator_count, standalone.operator_count);
            assert_eq!(shared.bounds.x0, standalone.bounds.x0);
            assert_eq!(shared.bounds.y0, standalone.bounds.y0);
            assert_eq!(shared.bounds.x1, standalone.bounds.x1);
            assert_eq!(shared.bounds.y1, standalone.bounds.y1);
        }
    }

    #[test]
    fn transformed_block_factoring_matches_body_across_placement_matrices() {
        let input = b"q 1 0 0 1 10 20 cm 1 0 0 rg 0 0 10 10 re f Q \
                      q 2 0 0 2 30 40 cm 1 0 0 rg 0 0 10 10 re f Q";
        let blocks = factorable_transformed_blocks(input);
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].semantic_key, blocks[1].semantic_key);
        assert_eq!(blocks[0].operator_count, 3);
        assert_eq!(blocks[0].bounds.x0, 0.0);
        assert_eq!(blocks[0].bounds.y0, 0.0);
        assert_eq!(blocks[0].bounds.x1, 10.0);
        assert_eq!(blocks[0].bounds.y1, 10.0);
    }

    #[test]
    fn transformed_block_factoring_rejects_resource_dependent_body() {
        let input = b"q 1 0 0 1 10 20 cm /Im0 Do Q";
        assert!(factorable_transformed_blocks(input).is_empty());
    }

    #[test]
    fn transformed_block_factoring_rejects_open_caller_path() {
        let input = b"0 0 m q 1 0 0 1 10 20 cm 0 0 10 10 re f Q";
        assert!(factorable_transformed_blocks(input).is_empty());
    }

    #[test]
    fn transformed_block_factoring_rejects_nested_graphics_state() {
        let input = b"q 1 0 0 1 10 20 cm q 0 0 10 10 re f Q Q";
        assert!(factorable_transformed_blocks(input).is_empty());
    }

    #[test]
    fn finds_large_fill_path_for_form_factoring() {
        let mut input = b"0 0 m ".to_vec();
        for index in 0..700 {
            input.extend_from_slice(format!("{} {} l ", index + 1, index + 2).as_bytes());
        }
        input.extend_from_slice(b"f");
        let blocks = factorable_path_blocks(&input);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].start, 0);
        assert_eq!(blocks[0].end, input.len());
    }

    #[test]
    fn path_form_factoring_rejects_state_change_inside_path() {
        let mut input = b"0 0 m ".to_vec();
        for index in 0..350 {
            input.extend_from_slice(format!("{} {} l ", index + 1, index + 2).as_bytes());
        }
        input.extend_from_slice(b"1 0 0 rg ");
        for index in 350..700 {
            input.extend_from_slice(format!("{} {} l ", index + 1, index + 2).as_bytes());
        }
        input.extend_from_slice(b"f");
        assert!(factorable_path_blocks(&input).is_empty());
    }

    #[test]
    fn path_form_factoring_rejects_path_inside_text_object() {
        let mut input = b"BT 0 0 m ".to_vec();
        for index in 0..700 {
            input.extend_from_slice(format!("{} {} l ", index + 1, index + 2).as_bytes());
        }
        input.extend_from_slice(b"f ET");
        assert!(factorable_path_blocks(&input).is_empty());
    }

    #[test]
    fn merges_physically_tiny_rounding_gap() {
        let input = b"0.1 0 0 0.1 0 0 cm 0 0 100 100 re f 100.005 0 80 100 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 1);
        assert_eq!(stats.fill_paints_eliminated, 1);
        assert!(String::from_utf8_lossy(&output).contains("0 0 180.005 100 re f"));
    }

    #[test]
    fn does_not_merge_page_space_thin_member() {
        let input = b"636.43 216.22 63 15.72 re f* 699.43 216.22 4.68 15.72 re f*";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn does_not_merge_visible_gap() {
        let input = b"0 0 10 10 re f 10.01 0 5 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn does_not_merge_overlapping_rectangles() {
        let input = b"0 0 10 10 re f 9 0 5 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn prunes_immediately_covered_opaque_fill() {
        let input = b"7 7 1 1 re f 0 0 20 20 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 1);
        assert_eq!(stats.fill_paints_eliminated, 1);
        assert!(!String::from_utf8_lossy(&output).contains("7 7 1 1 re f"));
        assert!(String::from_utf8_lossy(&output).contains("0 0 20 20 re f"));
    }

    #[test]
    fn prunes_immediately_redundant_contained_repaint() {
        let input = b"0 0 20 20 re f 7 7 1 1 re f";
        let (output, stats) = compact_content(input);
        let output = String::from_utf8_lossy(&output);
        assert_eq!(stats.covered_fills_pruned, 1);
        assert_eq!(stats.fill_paints_eliminated, 1);
        assert!(output.contains("0 0 20 20 re f"));
        assert!(!output.contains("7 7 1 1 re f"));
    }

    #[test]
    fn does_not_prune_contained_repaint_near_enclosing_edge() {
        let input = b"0 0 10 10 re f 2 2 1 1 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn does_not_prune_identical_antialiased_fills() {
        let input = b"0 0 10 10 re f 0 0 10 10 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn does_not_prune_forward_containment_near_distinct_enclosing_edge() {
        let input = b"0.2 2 1 1 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn does_not_prune_shared_antialiased_containment_edge() {
        let input = b"0 7 1 1 re f 0 0 20 20 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn does_not_prune_covered_fill_after_unknown_ext_gstate() {
        let input = b"/GS0 gs 0 0 1 1 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn q_restores_known_opaque_fill_state() {
        let input = b"q /GS0 gs 7 7 1 1 re f Q 7 7 1 1 re f 0 0 20 20 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 1);
        assert!(String::from_utf8_lossy(&output).contains("/GS0 gs 7 7 1 1 re f"));
    }

    #[test]
    fn does_not_prune_pattern_fill() {
        let input = b"/Pattern cs /P0 scn 0 0 1 1 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn state_change_breaks_merge() {
        let input = b"0 0 10 10 re f 1 0 0 rg 10 0 5 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output, input);
    }
    #[test]
    fn resolved_opaque_ext_gstate_allows_covered_fill_pruning() {
        let mut states = BTreeMap::new();
        states.insert(
            b"GS0".to_vec(),
            ExtGStatePatch {
                fill_alpha_opaque: Some(true),
                normal_blend: Some(true),
                no_soft_mask: Some(true),
                fill_overprint_disabled: Some(true),
            },
        );
        let input = b"/GS0 gs 7 7 2 2 re f 0 0 20 20 re f";
        let (output, stats) = compact_content_with_ext_gstates(input, &states);
        assert_eq!(stats.covered_fills_pruned, 1);
        assert!(!String::from_utf8_lossy(&output).contains("0 0 2 2 re"));
    }

    #[test]
    fn ext_gstate_partial_update_does_not_reset_unsafe_alpha() {
        let mut states = BTreeMap::new();
        states.insert(
            b"Half".to_vec(),
            ExtGStatePatch {
                fill_alpha_opaque: Some(false),
                ..Default::default()
            },
        );
        states.insert(
            b"Normal".to_vec(),
            ExtGStatePatch {
                normal_blend: Some(true),
                ..Default::default()
            },
        );
        let input = b"/Half gs /Normal gs 0 0 2 2 re f 0 0 10 10 re f";
        let (output, stats) = compact_content_with_ext_gstates(input, &states);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output, input);
    }
}
