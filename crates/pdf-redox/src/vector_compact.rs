use crate::content::page_content_if_present;
use crate::content_stream::trivia_only;
use crate::ext_gstate::ext_gstate_semantics;
use crate::geometry::{Matrix, Rect};
use crate::{
    EditDocument, ObjectHandle, OptimizationGoal, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{
        decoded_content_value, effective_page_resources, install_page_resource_bindings,
        page_box_numbers_matching, page_user_unit, replace_page_content,
        replace_page_content_with_handle, resolved_dictionary,
    },
    content_stream::{operand_name, operand_number},
    flate::FlateReuseCache,
};
use num_traits::ToPrimitive;
use sha2::{Digest as _, Sha256};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

// Only the encoded length is retained: no unbounded copies of source streams
// or compressed payloads. A fixed FIFO makes worst-case retention O(1) per
// input and prevents duplicate- or adversarial-document memory growth.
// Key by the *full decoded bytes' SHA-256* and the exact Flate level, never
// page identity: the same content can reappear on different pages and passes.
const MAX_VECTOR_FLATE_LENGTHS: usize = 8_192;
type VectorFlateKey = (i32, [u8; 32]);

#[derive(Default)]
struct VectorFlateLengthMemo {
    enabled: bool,
    lengths: HashMap<VectorFlateKey, usize>,
    insertion_order: VecDeque<VectorFlateKey>,
    #[cfg(test)]
    hits: usize,
    #[cfg(test)]
    misses: usize,
    #[cfg(test)]
    evictions: usize,
}

impl VectorFlateLengthMemo {
    fn processing() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    fn encoded_len(&mut self, decoded: &[u8], level: crate::FlateLevel) -> Result<usize> {
        self.capped_len(decoded, level, usize::MAX)?
            .ok_or_else(|| crate::Error::Compression("Flate output exceeded its bound".to_owned()))
    }

    fn capped_len(
        &mut self,
        decoded: &[u8],
        level: crate::FlateLevel,
        limit: usize,
    ) -> Result<Option<usize>> {
        if !self.enabled {
            return crate::stream_codec::flate_capped_encoded_len(decoded, limit, level);
        }
        Ok(self
            .measure(decoded, level, limit)?
            .map(|measurement| measurement.length))
    }

    fn measure(
        &mut self,
        decoded: &[u8],
        level: crate::FlateLevel,
        limit: usize,
    ) -> Result<Option<FlateMeasurement>> {
        let digest = Sha256::digest(decoded).into();
        let key = (level.value(), digest);
        if self.enabled {
            if let Some(&length) = self.lengths.get(&key) {
                #[cfg(test)]
                {
                    self.hits = self.hits.saturating_add(1);
                }
                return Ok((length <= limit).then_some(FlateMeasurement {
                    length,
                    encoding: None,
                    digest,
                }));
            }
            #[cfg(test)]
            {
                self.misses = self.misses.saturating_add(1);
            }
        }
        let Some(encoded) = crate::stream_codec::encode_flate_capped(decoded, limit, level)? else {
            return Ok(None);
        };
        let length = encoded.len();
        self.remember_encoded_len(digest, level, length);
        Ok(Some(FlateMeasurement {
            length,
            encoding: Some(encoded),
            digest,
        }))
    }

    fn remember_encoded_len(&mut self, digest: [u8; 32], level: crate::FlateLevel, length: usize) {
        if self.enabled {
            self.insert_length((level.value(), digest), length);
        }
    }

    fn insert_length(&mut self, key: VectorFlateKey, length: usize) {
        if self.lengths.contains_key(&key) {
            return;
        }
        if self.lengths.len() == MAX_VECTOR_FLATE_LENGTHS
            && let Some(evicted) = self.insertion_order.pop_front()
        {
            self.lengths.remove(&evicted);
            #[cfg(test)]
            {
                self.evictions = self.evictions.saturating_add(1);
            }
        }
        self.insertion_order.push_back(key);
        self.lengths.insert(key, length);
    }
}

/// An exact measured length and, on a cache miss, its reusable encoding.
/// Failed capped measurements are never entered into the exact-length memo.
struct FlateMeasurement {
    length: usize,
    encoding: Option<Vec<u8>>,
    digest: [u8; 32],
}

impl FlateMeasurement {
    fn retain(self, cache: &mut FlateReuseCache, handle: ObjectHandle, level: crate::FlateLevel) {
        if let Some(encoded) = self.encoding {
            cache.insert_with_digest(handle, level, self.digest, encoded);
        }
    }
}

// Related caches form one unit during page mutations: compressed bytes
// benefit the final Flate phase, while encoded lengths benefit form factoring.
struct VectorCompactionFlateState<'a> {
    reuse: &'a mut FlateReuseCache,
    lengths: VectorFlateLengthMemo,
}

impl<'a> VectorCompactionFlateState<'a> {
    fn new(reuse: &'a mut FlateReuseCache, goal: OptimizationGoal) -> Self {
        Self {
            reuse,
            lengths: match goal {
                OptimizationGoal::Processing => VectorFlateLengthMemo::processing(),
                OptimizationGoal::Size => VectorFlateLengthMemo::default(),
            },
        }
    }
}

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
pub struct VectorCompactionStats {
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
    pub shared_run_forms_created: usize,
    pub shared_run_pages_rewritten: usize,
    pub shared_run_blocks_replaced: usize,
    pub shared_run_decoded_bytes_factored: usize,
    pub shared_run_estimated_flate_bytes_saved: usize,
    pub path_coordinates_canonicalized: usize,
    pub path_coordinate_pages_rewritten: usize,
    pub path_coordinate_decoded_bytes_removed: usize,
    pub path_coordinate_estimated_flate_bytes_saved: usize,
    pub generated_page_xobjects: BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
}

impl VectorCompactionStats {
    pub fn absorb(&mut self, delta: Self) {
        self.pages_compacted += delta.pages_compacted;
        self.fill_groups_batched += delta.fill_groups_batched;
        self.covered_fills_pruned += delta.covered_fills_pruned;
        self.fill_paints_eliminated += delta.fill_paints_eliminated;
        self.decoded_bytes_removed += delta.decoded_bytes_removed;
        self.estimated_flate_bytes_saved += delta.estimated_flate_bytes_saved;
        self.path_forms_created += delta.path_forms_created;
        self.path_form_pages_rewritten += delta.path_form_pages_rewritten;
        self.path_form_occurrences_replaced += delta.path_form_occurrences_replaced;
        self.path_form_decoded_bytes_factored += delta.path_form_decoded_bytes_factored;
        self.path_form_estimated_flate_bytes_saved += delta.path_form_estimated_flate_bytes_saved;
        self.transformed_forms_created += delta.transformed_forms_created;
        self.transformed_form_pages_rewritten += delta.transformed_form_pages_rewritten;
        self.transformed_form_occurrences_replaced += delta.transformed_form_occurrences_replaced;
        self.transformed_form_operators_eliminated += delta.transformed_form_operators_eliminated;
        self.transformed_form_estimated_flate_bytes_saved +=
            delta.transformed_form_estimated_flate_bytes_saved;
        self.shared_run_forms_created += delta.shared_run_forms_created;
        self.shared_run_pages_rewritten += delta.shared_run_pages_rewritten;
        self.shared_run_blocks_replaced += delta.shared_run_blocks_replaced;
        self.shared_run_decoded_bytes_factored += delta.shared_run_decoded_bytes_factored;
        self.shared_run_estimated_flate_bytes_saved += delta.shared_run_estimated_flate_bytes_saved;
        self.path_coordinates_canonicalized += delta.path_coordinates_canonicalized;
        self.path_coordinate_pages_rewritten += delta.path_coordinate_pages_rewritten;
        self.path_coordinate_decoded_bytes_removed += delta.path_coordinate_decoded_bytes_removed;
        self.path_coordinate_estimated_flate_bytes_saved +=
            delta.path_coordinate_estimated_flate_bytes_saved;
        for (page, names) in delta.generated_page_xobjects {
            self.generated_page_xobjects
                .entry(page)
                .or_default()
                .extend(names);
        }
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "named safety predicates make fill-compaction preconditions directly auditable"
)]
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
    const fn idempotent(self) -> bool {
        self.fill_alpha_opaque
            && self.normal_blend
            && self.no_soft_mask
            && self.fill_overprint_disabled
            && self.simple_fill_color
    }

    const fn invalidate_transparency(&mut self) {
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
    const fn apply_ext_gstate(&mut self, patch: ExtGStatePatch) {
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

/// The fill, path-factor, transformed-factor and coordinate passes consume
/// the same bounded numeric operands. Project them once, and keep resource
/// names borrowed. Operators that cannot use numbers never decode them.
struct VectorOperands<'a> {
    count: usize,
    first_offset: Option<usize>,
    numbers: Option<SmallVec<[f64; 6]>>,
    name: Option<&'a [u8]>,
}

impl<'a> VectorOperands<'a> {
    fn project(instruction: &'a hayro_syntax::content::Instruction<'_, '_>) -> Self {
        let mut spans = instruction.operand_spans();
        let count = spans.len();
        let first_offset = spans.next().map(|span| span.start);
        let operator = &instruction.operator[..];
        let expected = match operator {
            b"m" | b"l" => Some(2),
            b"c" | b"cm" => Some(6),
            b"v" | b"y" | b"re" | b"k" => Some(4),
            b"rg" => Some(3),
            b"g" => Some(1),
            b"h" | b"f" | b"F" | b"f*" => Some(0),
            _ => None,
        };
        let numbers = if expected == Some(count) {
            instruction.operands().map(operand_number).collect()
        } else {
            None
        };
        let name = if operator == b"gs" && count == 1 {
            instruction.operands().next().and_then(operand_name)
        } else {
            None
        };
        Self {
            count,
            first_offset,
            numbers,
            name,
        }
    }

    fn numbers(&self) -> Option<&[f64]> {
        self.numbers.as_deref()
    }

    const fn is_empty(&self) -> bool {
        self.count == 0
    }
}

trait VectorInstructionScanner {
    fn process_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
        operands: &VectorOperands<'_>,
    );
    fn inline_image(&mut self);

    fn instruction(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        self.projected_instruction(instruction, &VectorOperands::project(instruction));
    }

    fn projected_instruction(
        &mut self,
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        operands: &VectorOperands<'_>,
    ) {
        if &instruction.operator[..] == b"BI" {
            self.inline_image();
            return;
        }
        let span = instruction.operator_span();
        self.process_operator(&instruction.operator[..], span.start, span.len(), operands);
    }

    fn scan(&mut self, input: &[u8]) -> crate::Result<bool> {
        crate::content_stream::visit_instructions(input, |instruction| {
            self.instruction(instruction);
            Ok(())
        })
    }
}

struct FillScanner {
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

    const fn clear_path(&mut self) {
        self.current_rect = None;
        self.current_path_start = None;
        self.path_is_single_rect = true;
    }

    const fn barrier(&mut self) {
        self.clear_path();
        self.epoch = self.epoch.saturating_add(1);
    }

    fn rectangle(&mut self, operands: &VectorOperands<'_>) {
        if !self.path_is_single_rect || self.current_rect.is_some() || operands.count != 4 {
            self.path_is_single_rect = false;
            return;
        }
        let values = operands.numbers();
        let Some(values) = values else {
            self.path_is_single_rect = false;
            return;
        };
        let Some(rect) = Rect::from_xywh(values[0], values[1], values[2], values[3]) else {
            self.path_is_single_rect = false;
            return;
        };
        self.current_path_start = operands.first_offset;
        self.current_rect = Some(rect);
    }

    fn fill(&mut self, operator: &[u8], end: usize, operands: &VectorOperands<'_>) {
        if operands.is_empty()
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
        self.clear_path();
    }

    fn simple_fill_color(&mut self, operands: &VectorOperands<'_>) {
        self.fill_safety.simple_fill_color = operands
            .numbers()
            .is_some_and(|values| values.iter().all(|value| value.is_finite()));
        self.barrier();
    }

    const fn unknown_fill_color(&mut self) {
        self.fill_safety.simple_fill_color = false;
        self.barrier();
    }

    fn apply_ext_gstate(&mut self, operands: &VectorOperands<'_>) {
        let patch = operands
            .name
            .and_then(|name| self.ext_gstates.get(name).copied());
        if let Some(patch) = patch {
            self.fill_safety.apply_ext_gstate(patch);
        } else {
            self.fill_safety.invalidate_transparency();
        }
        self.barrier();
    }

    fn concat_ctm(&mut self, operands: &VectorOperands<'_>) {
        let values = operands.numbers();
        if let Some(values) = values
            && values.len() == 6
        {
            self.ctm.concat(Matrix::new(
                values[0], values[1], values[2], values[3], values[4], values[5],
            ));
        }
        self.barrier();
    }
}

impl VectorInstructionScanner for FillScanner {
    fn process_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
        operands: &VectorOperands<'_>,
    ) {
        let end = offset.saturating_add(length);
        match operator {
            b"re" => self.rectangle(operands),
            b"f" | b"F" | b"f*" => self.fill(operator, end, operands),
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
            b"cm" => self.concat_ctm(operands),
            b"g" | b"rg" | b"k" => self.simple_fill_color(operands),
            b"cs" | b"sc" | b"scn" => self.unknown_fill_color(),
            b"gs" => self.apply_ext_gstate(operands),
            _ => self.barrier(),
        }
    }

    fn inline_image(&mut self) {
        self.barrier();
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1.0e-8 * a.abs().max(b.abs()).max(1.0)
}

pub fn merge_rect_fill_pair(a: [f64; 4], b: [f64; 4], ctm: Matrix) -> Option<[f64; 4]> {
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

pub fn compacted_rect_fill_len(rect: [f64; 4], operator: &[u8]) -> usize {
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

// Keep the inclusive 0.001 pt snap policy stable when decimal PDF operands land
// microscopically above the boundary after binary floating-point subtraction.
fn snap_gap_with_roundoff(gap: f64, scale: f64) -> bool {
    let page_gap = gap * scale;
    page_gap <= RECT_SNAP_PAGE_GAP_PT || close(page_gap, RECT_SNAP_PAGE_GAP_PT)
}

// A previous merge can leave an endpoint a few ulps beyond the next source edge.
// Forgive only overlap that is effectively zero in *page space*: using the
// coordinate-relative epsilon directly would become too permissive for very
// large user-space coordinates or magnifying CTMs.
fn edge_gap_or_roundoff_overlap(a0: f64, a1: f64, b0: f64, b1: f64, scale: f64) -> Option<f64> {
    if a1 <= b0 {
        Some(b0 - a1)
    } else if b1 <= a0 {
        Some(a0 - b1)
    } else {
        let overlap = if a0 <= b0 { a1 - b0 } else { b1 - a0 };
        close(overlap * scale, 0.0).then_some(0.0)
    }
}

fn merge_collinear_rects(a: Rect, b: Rect, ctm: Matrix) -> Option<Rect> {
    if close(a.y0, b.y0) && close(a.y1, b.y1) {
        let x_scale = ctm.a.hypot(ctm.b);
        let a_extent = (a.x1 - a.x0).abs() * x_scale;
        let b_extent = (b.x1 - b.x0).abs() * x_scale;
        if a_extent >= MIN_MERGE_MEMBER_PAGE_EXTENT_PT
            && b_extent >= MIN_MERGE_MEMBER_PAGE_EXTENT_PT
        {
            let gap = edge_gap_or_roundoff_overlap(a.x0, a.x1, b.x0, b.x1, x_scale)?;
            if snap_gap_with_roundoff(gap, x_scale) {
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
            let gap = edge_gap_or_roundoff_overlap(a.y0, a.y1, b.y0, b.y1, y_scale)?;
            if snap_gap_with_roundoff(gap, y_scale) {
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
    let determinant = ctm.b.mul_add(-ctm.c, ctm.a * ctm.d).abs();
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

pub fn rect_contains_rect(outer: [f64; 4], inner: [f64; 4]) -> bool {
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

fn apply_covered_fill_ranges<'a>(
    input: &'a [u8],
    ranges: &BTreeSet<(usize, usize)>,
) -> (Cow<'a, [u8]>, VectorCompactionStats) {
    if ranges.is_empty() {
        return (Cow::Borrowed(input), VectorCompactionStats::default());
    }

    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    let mut stats = VectorCompactionStats::default();
    for &(start, end) in ranges {
        if start < cursor || end > input.len() {
            return (Cow::Borrowed(input), VectorCompactionStats::default());
        }
        output.extend_from_slice(&input[cursor..start]);
        output.push(b' ');
        stats.covered_fills_pruned += 1;
        stats.fill_paints_eliminated += 1;
        stats.decoded_bytes_removed += end.saturating_sub(start).saturating_sub(1);
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    (Cow::Owned(output), stats)
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

fn compact_content_from_fill_scan<'a>(
    input: &'a [u8],
    ext_gstates: &BTreeMap<Vec<u8>, ExtGStatePatch>,
    initial_fills: &[FillPaint],
) -> (Cow<'a, [u8]>, VectorCompactionStats) {
    let covered_ranges = covered_fill_ranges(initial_fills);
    let (covered_pruned, mut stats) = apply_covered_fill_ranges(input, &covered_ranges);
    let rescanned_fills;
    let fills = if covered_ranges.is_empty() {
        initial_fills
    } else {
        let mut scanner = FillScanner::new(ext_gstates.clone());
        if !matches!(scanner.scan(&covered_pruned), Ok(false)) {
            return (Cow::Borrowed(input), VectorCompactionStats::default());
        }
        rescanned_fills = scanner.fills;
        &rescanned_fills
    };
    let rewrites = build_rewrites(&covered_pruned, fills);
    if rewrites.is_empty() {
        return (covered_pruned, stats);
    }
    let mut output = Vec::with_capacity(covered_pruned.len());
    let mut cursor = 0usize;
    for rewrite in rewrites {
        if rewrite.start < cursor || rewrite.end > covered_pruned.len() {
            return (Cow::Borrowed(input), VectorCompactionStats::default());
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
    (Cow::Owned(output), stats)
}

fn compact_content_with_ext_gstates<'a>(
    input: &'a [u8],
    ext_gstates: &BTreeMap<Vec<u8>, ExtGStatePatch>,
) -> (Cow<'a, [u8]>, VectorCompactionStats) {
    let mut scanner = FillScanner::new(ext_gstates.clone());
    if !matches!(scanner.scan(input), Ok(false)) {
        return (Cow::Borrowed(input), VectorCompactionStats::default());
    }
    compact_content_from_fill_scan(input, ext_gstates, &scanner.fills)
}

#[cfg(test)]
fn compact_content(input: &[u8]) -> (Cow<'_, [u8]>, VectorCompactionStats) {
    compact_content_with_ext_gstates(input, &BTreeMap::new())
}

fn ext_gstate_patches(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ExtGStatePatch>> {
    Ok(ext_gstate_semantics(document, resources)?
        .into_iter()
        .map(|(name, state)| {
            (
                name,
                ExtGStatePatch {
                    fill_alpha_opaque: state.fill_alpha_opaque,
                    normal_blend: state.normal_blend,
                    no_soft_mask: state.no_soft_mask,
                    // PDF `op` controls non-stroking overprint. When absent, `OP`
                    // supplies the value, so preserve that fallback here.
                    fill_overprint_disabled: state
                        .fill_overprint_disabled
                        .or(state.stroke_overprint_disabled),
                },
            )
        })
        .collect())
}

type SemanticKey = [u8; 32];

#[cfg(test)]
fn semantic_fingerprint(bytes: &[u8]) -> SemanticKey {
    Sha256::digest(bytes).into()
}

fn update_semantic_hasher(hasher: &mut Sha256, operator: &[u8], values: &[f64]) {
    hasher.update([u8::try_from(operator.len()).unwrap_or(u8::MAX)]);
    hasher.update(operator);
    for value in values {
        let normalized = if *value == 0.0 { 0.0 } else { *value };
        hasher.update(normalized.to_bits().to_be_bytes());
    }
}

#[derive(Debug, Clone)]
struct PathBlock {
    start: usize,
    end: usize,
    bounds: Rect,
    semantic_key: SemanticKey,
    operator_count: usize,
}

struct PathBlockScanner {
    path_start: Option<usize>,
    path_valid: bool,
    bounds: Option<Rect>,
    semantic_hasher: Sha256,
    operator_count: usize,
    blocks: Vec<PathBlock>,
    inside_text: bool,
}

impl PathBlockScanner {
    fn new() -> Self {
        Self {
            path_start: None,
            path_valid: true,
            bounds: None,
            semantic_hasher: Sha256::new(),
            operator_count: 0,
            blocks: Vec::new(),
            inside_text: false,
        }
    }

    fn reset_path_state(&mut self) {
        self.path_start = None;
        self.path_valid = true;
        self.bounds = None;
        self.semantic_hasher = Sha256::new();
        self.operator_count = 0;
    }

    const fn include_point(&mut self, x: f64, y: f64) {
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
        operands: &VectorOperands<'_>,
    ) {
        if self.inside_text {
            self.path_valid = false;
        }
        if self.path_start.is_none() {
            if operator == b"h" {
                self.path_valid = false;
                return;
            }
            self.path_start = Some(operands.first_offset.unwrap_or(operator_offset));
        }
        let Some(values) = operands.numbers() else {
            self.path_valid = false;
            return;
        };
        update_semantic_hasher(&mut self.semantic_hasher, operator, values);
        self.operator_count = self.operator_count.saturating_add(1);
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

    fn finish_fill_with_operands(
        &mut self,
        operator: &[u8],
        end: usize,
        operands: &VectorOperands<'_>,
    ) {
        let block_len = self.path_start.map_or(0, |start| end.saturating_sub(start));
        if operands.is_empty()
            && self.path_valid
            && let (Some(start), Some(bounds)) = (self.path_start, self.bounds)
            && block_len >= MIN_PROCESSING_PATH_FORM_BYTES
            && (block_len >= MIN_PATH_FORM_BYTES
                || self.operator_count >= MIN_PROCESSING_PATH_FORM_OPERATORS)
        {
            let mut hasher = self.semantic_hasher.clone();
            update_semantic_hasher(&mut hasher, operator, &[]);
            let semantic_key = hasher.finalize().into();
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
        operands: &VectorOperands<'_>,
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

    fn unsupported_operator_state(&mut self, clears_path: bool) {
        if clears_path {
            self.reset_path_state();
        } else if self.path_start.is_some() {
            self.path_valid = false;
        }
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
    semantic_hasher: Sha256,
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
            semantic_hasher: Sha256::new(),
            operator_count: 0,
            path_bounds: None,
            paint_bounds: None,
            path_started: false,
        }
    }

    const fn include_point(&mut self, x: f64, y: f64) {
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

    const fn include_painted_bounds(&mut self, bounds: Rect) {
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
        update_semantic_hasher(&mut self.semantic_hasher, operator, values);
        self.operator_count = self.operator_count.saturating_add(1);
    }
}

#[derive(Debug, Clone)]
struct TransformedBlock {
    body_start: usize,
    body_end: usize,
    bounds: Rect,
    semantic_key: SemanticKey,
    operator_count: usize,
}

struct TransformedBlockScanner {
    frames: Vec<TransformedBlockFrame>,
    blocks: Vec<TransformedBlock>,
    current_path_nonempty: bool,
}

impl TransformedBlockScanner {
    const fn new() -> Self {
        Self {
            frames: Vec::new(),
            blocks: Vec::new(),
            current_path_nonempty: false,
        }
    }

    fn invalidate_parents_for_nested_q(&mut self) {
        for frame in &mut self.frames {
            frame.valid = false;
        }
    }

    fn body_operator(&mut self, operator: &[u8], operands: &VectorOperands<'_>) {
        let Some(frame) = self.frames.last_mut() else {
            return;
        };
        if !frame.valid || frame.stage != TransformedBlockStage::Body {
            return;
        }
        let Some(values) = operands.numbers() else {
            frame.valid = false;
            return;
        };
        frame.append_semantic(operator, values);
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
        operands: &VectorOperands<'_>,
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
                let valid_placement = operands.numbers().is_some();
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
                semantic_key: frame.semantic_hasher.finalize().into(),
                operator_count: frame.operator_count,
            });
        }
    }
}

impl VectorInstructionScanner for TransformedBlockScanner {
    fn process_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
        operands: &VectorOperands<'_>,
    ) {
        self.process_operator_with_operands(operator, offset, length, operands);
    }

    fn inline_image(&mut self) {
        if let Some(frame) = self.frames.last_mut() {
            frame.valid = false;
        }
    }
}

fn factorable_transformed_blocks(input: &[u8]) -> Vec<TransformedBlock> {
    let mut scanner = TransformedBlockScanner::new();
    if !matches!(scanner.scan(input), Ok(false)) {
        return Vec::new();
    }
    scanner.blocks
}

#[cfg(test)]
fn factorable_path_blocks(input: &[u8]) -> Vec<PathBlock> {
    let mut scanner = PathBlockScanner::new();
    let parsed = crate::content_stream::visit_instructions(input, |instruction| {
        if &instruction.operator[..] == b"BI" {
            scanner.unsupported_operator_state(false);
        } else {
            let operands = VectorOperands::project(instruction);
            let span = instruction.operator_span();
            scanner.process_operator_with_operands(
                &instruction.operator[..],
                span.start,
                span.len(),
                &operands,
            );
        }
        Ok(())
    });
    if !matches!(parsed, Ok(false)) {
        return Vec::new();
    }
    scanner.blocks
}

struct ProcessingFactorScanner {
    path: PathBlockScanner,
    transformed: TransformedBlockScanner,
}

impl ProcessingFactorScanner {
    fn new() -> Self {
        Self {
            path: PathBlockScanner::new(),
            transformed: TransformedBlockScanner::new(),
        }
    }
}

impl VectorInstructionScanner for ProcessingFactorScanner {
    fn process_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
        operands: &VectorOperands<'_>,
    ) {
        self.path
            .process_operator_with_operands(operator, offset, length, operands);
        self.transformed
            .process_operator_with_operands(operator, offset, length, operands);
    }

    fn inline_image(&mut self) {
        self.path.unsupported_operator_state(false);
        self.transformed.inline_image();
    }
}

fn factorable_processing_blocks(input: &[u8]) -> (Vec<PathBlock>, Vec<TransformedBlock>) {
    let mut scanner = ProcessingFactorScanner::new();
    if !matches!(scanner.scan(input), Ok(false)) {
        return (Vec::new(), Vec::new());
    }
    (scanner.path.blocks, scanner.transformed.blocks)
}

pub struct ProcessingVectorAnalysis {
    fills: Vec<FillPaint>,
    path_blocks: Vec<PathBlock>,
    transformed_blocks: Vec<TransformedBlock>,
    path_coordinate_plan: Option<PathCoordinatePlan>,
}

const fn processing_transformed_factor_candidate(
    body_len: usize,
    operator_count: usize,
    occurrence_count: usize,
) -> bool {
    if body_len < MIN_PROCESSING_PATH_FORM_BYTES || occurrence_count < MIN_PATH_FORM_OCCURRENCES {
        return false;
    }
    operator_count.saturating_mul(occurrence_count.saturating_sub(1)) >= 8
}

pub fn processing_factor_candidate(
    cache: &BTreeMap<ObjectHandle, ProcessingVectorAnalysis>,
) -> bool {
    let mut path_keys = HashSet::new();
    let mut repeated_path = false;
    let mut transformed = HashMap::<SemanticKey, (usize, usize, usize)>::new();
    for analysis in cache.values() {
        for block in &analysis.path_blocks {
            if !path_keys.insert(block.semantic_key) {
                repeated_path = true;
            }
        }
        for block in &analysis.transformed_blocks {
            let body_len = block.body_end.saturating_sub(block.body_start);
            let entry = transformed.entry(block.semantic_key).or_insert((0, 0, 0));
            entry.0 = entry.0.saturating_add(1);
            // Equivalent semantic bodies can still differ lexically. Maxima are
            // deliberately conservative: they can cause a false positive (run
            // the full stage) but never prove away a potentially factorable group.
            entry.1 = entry.1.max(body_len);
            entry.2 = entry.2.max(block.operator_count);
        }
    }
    repeated_path
        || cache
            .values()
            .any(|analysis| analysis.path_coordinate_plan.is_some())
        || transformed.values().any(|(count, body_len, operators)| {
            processing_transformed_factor_candidate(*body_len, *operators, *count)
        })
}

pub struct ProcessingPageScanner {
    fill: FillScanner,
    factor: ProcessingFactorScanner,
    path_coordinates: Option<PathCoordinateScanner>,
}

impl ProcessingPageScanner {
    fn new(ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>, user_unit: Option<f64>) -> Self {
        Self {
            fill: FillScanner::new(ext_gstates),
            factor: ProcessingFactorScanner::new(),
            path_coordinates: user_unit.map(PathCoordinateScanner::new),
        }
    }

    pub(crate) fn for_resources(
        document: &EditDocument,
        resources: &OwnedDictionary,
        user_unit: Option<f64>,
    ) -> Result<Self> {
        Ok(Self::new(
            ext_gstate_patches(document, resources)?,
            user_unit,
        ))
    }

    pub(crate) fn finish(self, input: &[u8]) -> ProcessingVectorAnalysis {
        ProcessingVectorAnalysis {
            fills: self.fill.fills,
            path_blocks: self.factor.path.blocks,
            transformed_blocks: self.factor.transformed.blocks,
            path_coordinate_plan: self
                .path_coordinates
                .and_then(|scanner| scanner.finish(input)),
        }
    }

    pub(crate) fn instruction(
        &mut self,
        input: &[u8],
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
    ) {
        let operands = VectorOperands::project(instruction);
        if let Some(path_coordinates) = &mut self.path_coordinates {
            path_coordinates.projected_instruction(input, instruction, &operands);
        }
        self.fill.projected_instruction(instruction, &operands);
        self.factor.projected_instruction(instruction, &operands);
    }

    pub(crate) fn scan(&mut self, input: &[u8]) -> crate::Result<bool> {
        crate::content_stream::visit_instructions(input, |instruction| {
            self.instruction(input, instruction);
            Ok(())
        })
    }
}

fn scan_processing_page(
    input: &[u8],
    ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>,
    user_unit: Option<f64>,
) -> Option<ProcessingVectorAnalysis> {
    let mut scanner = ProcessingPageScanner::new(ext_gstates, user_unit);
    matches!(scanner.scan(input), Ok(false)).then(|| scanner.finish(input))
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
    decoded: std::sync::Arc<Vec<u8>>,
}

// Candidate bodies borrow the first semantic representative until the
// complete document-level compressed-size admission succeeds.
type FormGroups = BTreeMap<SemanticKey, (usize, Vec<PathOccurrence>)>;
type FormCandidate<'a> = (Cow<'a, [u8]>, usize, Vec<PathOccurrence>);

fn repeated_form_candidates(groups: FormGroups, pages: &[PagePathData]) -> Vec<FormCandidate<'_>> {
    groups
        .into_values()
        .filter(|(_, occurrences)| occurrences.len() >= MIN_PATH_FORM_OCCURRENCES)
        .map(|(operator_count, occurrences)| {
            let first = &occurrences[0];
            let body = &pages[first.page_index].decoded[first.start..first.end];
            (Cow::Borrowed(body), operator_count, occurrences)
        })
        .collect()
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

fn apply_span_replacements_from_arena(
    input: &[u8],
    mut replacements: Vec<CoordinateReplacement>,
    bytes: &[u8],
) -> Option<Vec<u8>> {
    if !replacements.is_sorted_by_key(|replacement| (replacement.start, replacement.end)) {
        replacements.sort_by_key(|replacement| (replacement.start, replacement.end));
    }
    crate::content_stream::apply_ordered_replacements(
        input,
        replacements.iter().map(|replacement| {
            (
                replacement.start,
                replacement.end,
                replacement
                    .byte_start
                    .checked_add(replacement.byte_length)
                    .and_then(|end| bytes.get(replacement.byte_start..end)),
            )
        }),
    )
}

type ProcessingFactorBlocks = BTreeMap<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>;

struct PathFactoringOutcome {
    stats: VectorCompactionStats,
    reusable_blocks: ProcessingFactorBlocks,
    rewritten_pages: BTreeSet<ObjectHandle>,
}

type PageReplacement<'a> = (usize, usize, &'a [u8]);

struct MeasuredFormRewrites {
    pages: Vec<(usize, Vec<u8>)>,
    savings: usize,
}

/// Use a single decreasing compressed byte budget for the whole speculative
/// factoring transaction. Every accepted size is exact; a cap failure proves
/// that the remaining page streams and Form bodies cannot make it profitable.
fn measure_form_rewrites<'a>(
    pages: impl Iterator<Item = (usize, &'a [u8], &'a [PageReplacement<'a>])> + Clone,
    bodies: impl Iterator<Item = &'a [u8]>,
    fixed_overhead: usize,
    level: crate::FlateLevel,
    flate_lengths: &mut VectorFlateLengthMemo,
) -> Result<Option<MeasuredFormRewrites>> {
    let before = pages.clone().try_fold(0_usize, |total, (_, input, _)| {
        flate_lengths
            .encoded_len(input, level)
            .map(|size| total.saturating_add(size))
    })?;
    let Some(mut remaining) = before
        .checked_sub(fixed_overhead)
        .and_then(|budget| budget.checked_sub(1))
    else {
        return Ok(None);
    };
    let mut rewritten_pages = Vec::new();
    for (index, input, replacements) in pages {
        let Some(rewritten) = crate::content_stream::apply_replacements(input, replacements) else {
            return Ok(None);
        };
        let Some(length) = flate_lengths.capped_len(&rewritten, level, remaining)? else {
            return Ok(None);
        };
        remaining -= length;
        rewritten_pages.push((index, rewritten));
    }
    for body in bodies {
        let Some(length) = flate_lengths.capped_len(body, level, remaining)? else {
            return Ok(None);
        };
        remaining -= length;
    }
    Ok(Some(MeasuredFormRewrites {
        pages: rewritten_pages,
        savings: remaining + 1,
    }))
}

struct SelectedPathForm<'a> {
    body: Cow<'a, [u8]>,
    name: Vec<u8>,
    replacement: Vec<u8>,
    bounds: Rect,
    operator_count: usize,
    occurrences: Vec<PathOccurrence>,
    page_indices: Vec<usize>,
}

fn select_path_forms<'a>(
    candidates: impl Iterator<Item = FormCandidate<'a>>,
    occupied: &mut BTreeSet<Vec<u8>>,
    name_start: usize,
) -> Vec<SelectedPathForm<'a>> {
    candidates
        .enumerate()
        .map(|(index, (body, operator_count, occurrences))| {
            let name = path_form_name(name_start.saturating_add(index), occupied);
            let replacement = [b" /".as_slice(), name.as_slice(), b" Do ".as_slice()].concat();
            let bounds = occurrences[0].bounds;
            // Discovery visits pages in order. Collapse repeated page indices
            // without retaining a separate entry for every occurrence.
            let mut page_indices = Vec::new();
            for occurrence in &occurrences {
                if page_indices.last() != Some(&occurrence.page_index) {
                    page_indices.push(occurrence.page_index);
                }
            }
            if !page_indices.is_sorted() {
                page_indices.sort_unstable();
                page_indices.dedup();
            }
            SelectedPathForm {
                body,
                name,
                replacement,
                bounds,
                operator_count,
                occurrences,
                page_indices,
            }
        })
        .collect()
}

struct PathFormCommit {
    pages_rewritten: usize,
    rewritten_page_handles: BTreeSet<ObjectHandle>,
    generated_page_xobjects: BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
    savings: usize,
}

/// Both path and transformed-body factoring share the same admission and
/// installation mechanics. Only their discovery and reported metrics differ.
fn apply_path_form_plan(
    document: &mut EditDocument,
    page_data: &[PagePathData],
    selected: &[SelectedPathForm<'_>],
    level: crate::FlateLevel,
    flate_lengths: &mut VectorFlateLengthMemo,
) -> Result<Option<PathFormCommit>> {
    if selected.is_empty() {
        return Ok(None);
    }
    let mut replacements_by_page = vec![Vec::<PageReplacement<'_>>::new(); page_data.len()];
    for form in selected {
        for occurrence in &form.occurrences {
            replacements_by_page[occurrence.page_index].push((
                occurrence.start,
                occurrence.end,
                form.replacement.as_slice(),
            ));
        }
    }
    let resource_refs = selected
        .iter()
        .map(|form| form.page_indices.len())
        .sum::<usize>();
    let fixed_overhead = selected
        .len()
        .saturating_mul(256)
        .saturating_add(resource_refs.saturating_mul(32));
    let pages = page_data
        .iter()
        .zip(&replacements_by_page)
        .enumerate()
        .filter_map(|(index, (page, replacements))| {
            (!replacements.is_empty()).then_some((
                index,
                page.decoded.as_slice(),
                replacements.as_slice(),
            ))
        });
    let Some(measured) = measure_form_rewrites(
        pages,
        selected.iter().map(|form| form.body.as_ref()),
        fixed_overhead,
        level,
        flate_lengths,
    )?
    else {
        return Ok(None);
    };
    let form_handles = selected
        .iter()
        .map(|form| {
            ObjectHandle::New(document.add_object(OwnedObject::Stream {
                dictionary: path_form_dictionary(form.bounds),
                data: StreamData::Owned(form.body.as_ref().to_vec().into()),
            }))
        })
        .collect::<Vec<_>>();
    let pages_rewritten = measured.pages.len();
    let mut rewritten_page_handles = BTreeSet::new();
    for (index, rewritten) in measured.pages {
        let page = &page_data[index];
        if rewritten.as_slice() != page.decoded.as_slice() {
            replace_page_content(document, page.page, rewritten)?;
            rewritten_page_handles.insert(page.page);
        }
    }
    install_page_resource_bindings(
        document,
        b"XObject",
        selected
            .iter()
            .zip(form_handles)
            .flat_map(|(form, handle)| {
                form.page_indices
                    .iter()
                    .map(move |&index| (page_data[index].page, form.name.as_slice(), handle))
            }),
    )?;
    let mut generated_page_xobjects = BTreeMap::<ObjectHandle, BTreeSet<Vec<u8>>>::new();
    for form in selected {
        for &index in &form.page_indices {
            generated_page_xobjects
                .entry(page_data[index].page)
                .or_default()
                .insert(form.name.clone());
        }
    }
    Ok(Some(PathFormCommit {
        pages_rewritten,
        rewritten_page_handles,
        generated_page_xobjects,
        savings: measured.savings,
    }))
}

/// Preference uses the first semantic representative, not the largest
/// lexical body among equivalent paths.
fn preferred_transformed_keys<'a>(
    blocks: impl Iterator<Item = &'a TransformedBlock>,
) -> HashSet<SemanticKey> {
    let mut counts = HashMap::<SemanticKey, (usize, usize, usize)>::new();
    for block in blocks {
        let entry = counts.entry(block.semantic_key).or_insert_with(|| {
            (
                0,
                block.operator_count,
                block.body_end.saturating_sub(block.body_start),
            )
        });
        entry.0 = entry.0.saturating_add(1);
    }
    counts
        .into_iter()
        .filter_map(|(key, (count, operators, body_len))| {
            processing_transformed_factor_candidate(body_len, operators, count).then_some(key)
        })
        .collect()
}

/// Prefix maxima of sorted interval endpoints answer containment even
/// when the original intervals are nested or overlapping.
struct PreferredBodyIndex {
    intervals: Vec<(usize, usize)>,
}

impl PreferredBodyIndex {
    fn new(blocks: &[TransformedBlock], preferred: &HashSet<SemanticKey>) -> Self {
        let mut intervals = blocks
            .iter()
            .filter(|block| preferred.contains(&block.semantic_key))
            .map(|block| (block.body_start, block.body_end))
            .collect::<Vec<_>>();
        intervals.sort_unstable_by_key(|&(start, _)| start);
        let mut max_end = 0;
        for (_, end) in &mut intervals {
            max_end = max_end.max(*end);
            *end = max_end;
        }
        Self { intervals }
    }

    fn contains(&self, start: usize, end: usize) -> bool {
        let count = self
            .intervals
            .partition_point(|&(outer_start, _)| outer_start <= start);
        count
            .checked_sub(1)
            .is_some_and(|index| self.intervals[index].1 >= end)
    }
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

    let mut scanned = Vec::with_capacity(pages.len());
    for &page in pages {
        let Some(object) = document.current_value(page)? else {
            continue;
        };
        if object.dictionary_entry(b"Contents").is_none() {
            continue;
        }
        let Some(blocks) = pre_scanned.get(&page) else {
            return Ok(false);
        };
        scanned.push(blocks);
    }
    let preferred_transformed =
        preferred_transformed_keys(scanned.iter().flat_map(|(_, transformed)| transformed));
    let mut path_keys = HashSet::<SemanticKey>::new();
    for (path_blocks, transformed_blocks) in scanned {
        let preferred = PreferredBodyIndex::new(transformed_blocks, &preferred_transformed);
        for block in path_blocks {
            if preferred.contains(block.start, block.end) {
                continue;
            }
            if !path_keys.insert(block.semantic_key) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn factor_repeated_path_forms(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    mut pre_scanned: BTreeMap<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>,
    pages: &[ObjectHandle],
    flate_lengths: &mut VectorFlateLengthMemo,
) -> Result<PathFactoringOutcome> {
    if cached_path_factoring_is_impossible_after_transformed_preference(
        document,
        &pre_scanned,
        pages,
    )? {
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            reusable_blocks: pre_scanned,
            rewritten_pages: BTreeSet::new(),
        });
    }

    let mut page_data = Vec::with_capacity(pages.len());
    let mut reusable_blocks = ProcessingFactorBlocks::new();
    let mut groups = FormGroups::new();
    let mut occupied_names = BTreeSet::new();

    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        let blocks = pre_scanned
            .remove(&page)
            .unwrap_or_else(|| factorable_processing_blocks(&decoded));
        let resources = effective_page_resources(document, page)?;
        if let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
        {
            occupied_names.extend(xobjects.keys().cloned());
        }
        page_data.push(PagePathData { page, decoded });
        reusable_blocks.insert(page, blocks);
    }

    // Prefer a larger repeated transformed body over path candidates nested
    // inside it. Rewriting an inner path to a Form call makes that outer body
    // resource-dependent and intentionally ineligible for transformed-form
    // factoring later.
    let preferred_transformed = preferred_transformed_keys(
        page_data
            .iter()
            .flat_map(|page| &reusable_blocks[&page.page].1),
    );
    for (page_index, page) in page_data.iter().enumerate() {
        let (path_blocks, transformed_blocks) = &reusable_blocks[&page.page];
        let preferred = PreferredBodyIndex::new(transformed_blocks, &preferred_transformed);
        for block in path_blocks {
            if preferred.contains(block.start, block.end) {
                continue;
            }
            let entry = groups
                .entry(block.semantic_key)
                .or_insert_with(|| (block.operator_count, Vec::new()));
            entry.1.push(PathOccurrence {
                page_index,
                start: block.start,
                end: block.end,
                bounds: block.bounds,
            });
        }
    }

    let mut candidates = repeated_form_candidates(groups, &page_data);
    candidates.sort_unstable_by_key(|(bytes, _, _)| std::cmp::Reverse(bytes.len()));
    let selected = select_path_forms(candidates.into_iter(), &mut occupied_names, 0);
    let Some(commit) =
        apply_path_form_plan(document, &page_data, &selected, flate_level, flate_lengths)?
    else {
        return Ok(PathFactoringOutcome {
            stats: VectorCompactionStats::default(),
            reusable_blocks,
            rewritten_pages: BTreeSet::new(),
        });
    };
    let occurrences = selected
        .iter()
        .map(|form| form.occurrences.len())
        .sum::<usize>();
    let factored = selected
        .iter()
        .map(|form| {
            form.body
                .len()
                .saturating_mul(form.occurrences.len().saturating_sub(1))
        })
        .sum::<usize>();
    Ok(PathFactoringOutcome {
        stats: VectorCompactionStats {
            path_form_pages_rewritten: commit.pages_rewritten,
            path_forms_created: selected.len(),
            path_form_occurrences_replaced: occurrences,
            path_form_decoded_bytes_factored: factored,
            path_form_estimated_flate_bytes_saved: commit.savings,
            estimated_flate_bytes_saved: commit.savings,
            decoded_bytes_removed: factored,
            generated_page_xobjects: commit.generated_page_xobjects,
            ..VectorCompactionStats::default()
        },
        reusable_blocks,
        rewritten_pages: commit.rewritten_page_handles,
    })
}

fn factor_repeated_transformed_blocks(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    reusable_blocks: &ProcessingFactorBlocks,
    rewritten_pages: &BTreeSet<ObjectHandle>,
    pages: &[ObjectHandle],
    flate_lengths: &mut VectorFlateLengthMemo,
) -> Result<VectorCompactionStats> {
    let mut page_data = Vec::with_capacity(pages.len());
    let mut groups = FormGroups::new();
    let mut occupied_names = BTreeSet::new();

    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        let scanned;
        let blocks = if let Some((_, transformed)) = reusable_blocks.get(&page)
            && !rewritten_pages.contains(&page)
        {
            transformed.as_slice()
        } else {
            scanned = factorable_transformed_blocks(&decoded);
            &scanned
        };
        for block in blocks {
            let entry = groups
                .entry(block.semantic_key)
                .or_insert_with(|| (block.operator_count, Vec::new()));
            entry.1.push(PathOccurrence {
                page_index: page_data.len(),
                start: block.body_start,
                end: block.body_end,
                bounds: block.bounds,
            });
        }
        let resources = effective_page_resources(document, page)?;
        if let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
        {
            occupied_names.extend(xobjects.keys().cloned());
        }
        page_data.push(PagePathData { page, decoded });
    }

    let mut candidates = repeated_form_candidates(groups, &page_data);
    candidates.sort_unstable_by_key(|(bytes, operators, occurrences)| {
        std::cmp::Reverse(
            bytes
                .len()
                .saturating_mul(occurrences.len())
                .saturating_add(operators.saturating_mul(occurrences.len())),
        )
    });
    let selected = select_path_forms(
        candidates
            .into_iter()
            .filter(|(bytes, operators, occurrences)| {
                processing_transformed_factor_candidate(bytes.len(), *operators, occurrences.len())
            }),
        &mut occupied_names,
        10_000,
    );
    let Some(commit) =
        apply_path_form_plan(document, &page_data, &selected, flate_level, flate_lengths)?
    else {
        return Ok(VectorCompactionStats::default());
    };
    let occurrences = selected
        .iter()
        .map(|form| form.occurrences.len())
        .sum::<usize>();
    let operators_factored = selected
        .iter()
        .map(|form| {
            form.operator_count
                .saturating_mul(form.occurrences.len().saturating_sub(1))
        })
        .sum::<usize>();
    Ok(VectorCompactionStats {
        pages_compacted: commit.pages_rewritten,
        transformed_forms_created: selected.len(),
        transformed_form_pages_rewritten: commit.pages_rewritten,
        transformed_form_occurrences_replaced: occurrences,
        transformed_form_operators_eliminated: operators_factored,
        transformed_form_estimated_flate_bytes_saved: commit.savings,
        estimated_flate_bytes_saved: commit.savings,
        generated_page_xobjects: commit.generated_page_xobjects,
        ..VectorCompactionStats::default()
    })
}

const MIN_SHARED_RUN_DECODED_BYTES: usize = 64 * 1024;
const MIN_SHARED_RUN_ESTIMATED_SAVINGS: usize = 4 * 1024;

const fn shared_q_page_can_reach_minimum(decoded_len: usize) -> bool {
    decoded_len >= MIN_SHARED_RUN_DECODED_BYTES
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum SharedResourceKind {
    ColorSpace,
    ExtGState,
    XObject,
}

impl SharedResourceKind {
    const fn dictionary_key(self) -> &'static [u8] {
        match self {
            Self::ColorSpace => b"ColorSpace",
            Self::ExtGState => b"ExtGState",
            Self::XObject => b"XObject",
        }
    }

    const fn name_prefix(self) -> &'static str {
        match self {
            Self::ColorSpace => "CS",
            Self::ExtGState => "GS",
            Self::XObject => "XO",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct SharedResourceRef {
    kind: SharedResourceKind,
    target: ObjectHandle,
}

fn shared_resource_name(resource: SharedResourceRef) -> Vec<u8> {
    let suffix = match resource.target {
        ObjectHandle::Existing(id) => format!("O{}G{}", id.number(), id.generation()),
        ObjectHandle::New(id) => format!("N{}", id.index()),
    };
    format!("PdfRedoxShared{}{suffix}", resource.kind.name_prefix()).into_bytes()
}

type ResourceTargets = BTreeMap<Vec<u8>, Option<ObjectHandle>>;

#[derive(Debug, Clone, Default)]
struct SharedResourceTargets {
    color_spaces: ResourceTargets,
    ext_gstates: ResourceTargets,
    xobjects: ResourceTargets,
}

impl SharedResourceTargets {
    fn target(&self, kind: SharedResourceKind, name: &[u8]) -> Option<ObjectHandle> {
        let map = match kind {
            SharedResourceKind::ColorSpace => &self.color_spaces,
            SharedResourceKind::ExtGState => &self.ext_gstates,
            SharedResourceKind::XObject => &self.xobjects,
        };
        map.get(name).copied().flatten()
    }
}

/// Read only selected resource names and direct reference identities.
/// Non-reference entries are retained as occupied names, never resolved as
/// factorable resource targets.
fn resource_target_projection(
    document: &EditDocument,
    resources: &crate::source::CurrentValue<'_>,
    key: &[u8],
) -> Result<ResourceTargets> {
    let Some(value) = resources.resolved_entry(document, key)? else {
        return Ok(BTreeMap::new());
    };
    let Some(entries) = value.dictionary_entries() else {
        return Ok(BTreeMap::new());
    };
    Ok(entries
        .map(|(name, value)| (name.as_ref().to_vec(), value.reference()))
        .collect())
}

fn shared_resource_targets(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<SharedResourceTargets> {
    let Some(resources) = document.inherited_page_resources(page)? else {
        return Ok(SharedResourceTargets::default());
    };
    Ok(SharedResourceTargets {
        color_spaces: resource_target_projection(document, &resources, b"ColorSpace")?,
        ext_gstates: resource_target_projection(document, &resources, b"ExtGState")?,
        xobjects: resource_target_projection(document, &resources, b"XObject")?,
    })
}

#[derive(Debug, Clone, Copy)]
struct SharedResourceUse {
    start: usize,
    end: usize,
    resource: SharedResourceRef,
}

#[derive(Debug, Clone)]
struct SharedQBlock {
    start: usize,
    end: usize,
    digest: [u8; 32],
    resource_uses: Vec<SharedResourceUse>,
    contiguous_from_previous: bool,
}

/// Stream the canonical byte representation through one sink. Resource uses
/// are recorded in parser order, so no copied/sorted span table is necessary.
fn visit_canonical_shared_span(
    input: &[u8],
    start: usize,
    end: usize,
    uses: &[SharedResourceUse],
    mut visit: impl FnMut(&[u8]) -> Option<()>,
) -> Option<()> {
    let mut cursor = start;
    for use_ in uses {
        if use_.start < cursor || use_.end < use_.start || use_.end > end {
            return None;
        }
        visit(input.get(cursor..use_.start)?)?;
        visit(b"/")?;
        visit(&shared_resource_name(use_.resource))?;
        cursor = use_.end;
    }
    visit(input.get(cursor..end)?)
}

#[cfg(test)]
fn canonicalized_shared_span(
    input: &[u8],
    start: usize,
    end: usize,
    uses: &[SharedResourceUse],
) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    visit_canonical_shared_span(input, start, end, uses, |bytes| {
        out.extend_from_slice(bytes);
        Some(())
    })?;
    Some(out)
}

struct SharedQBlockScanner<'a> {
    input: &'a [u8],
    targets: &'a SharedResourceTargets,
    depth: usize,
    block_start: Option<usize>,
    block_valid: bool,
    resource_uses: Vec<SharedResourceUse>,
    blocks: Vec<SharedQBlock>,
}

impl<'a> SharedQBlockScanner<'a> {
    const fn new(input: &'a [u8], targets: &'a SharedResourceTargets) -> Self {
        Self {
            input,
            targets,
            depth: 0,
            block_start: None,
            block_valid: true,
            resource_uses: Vec::new(),
            blocks: Vec::new(),
        }
    }

    fn record_resource(
        &mut self,
        kind: SharedResourceKind,
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
    ) {
        let mut spans = instruction.operand_spans();
        if spans.len() != 1 {
            self.block_valid = false;
            return;
        }
        let Some(span) = spans.next() else {
            self.block_valid = false;
            return;
        };
        let Some(target) = instruction
            .operands()
            .next()
            .and_then(crate::content_stream::operand_name)
            .and_then(|name| self.targets.target(kind, name))
        else {
            self.block_valid = false;
            return;
        };
        self.resource_uses.push(SharedResourceUse {
            start: span.start,
            end: span.end,
            resource: SharedResourceRef { kind, target },
        });
    }

    fn close_outer_block(&mut self, end: usize) {
        let Some(start) = self.block_start.take() else {
            self.resource_uses.clear();
            self.block_valid = true;
            return;
        };
        let mut hasher = Sha256::new();
        if self.block_valid
            && end > start
            && visit_canonical_shared_span(self.input, start, end, &self.resource_uses, |bytes| {
                hasher.update(bytes);
                Some(())
            })
            .is_some()
        {
            let contiguous_from_previous = self.blocks.last().is_some_and(|previous| {
                previous.end <= start
                    && self.input.get(previous.end..start).is_some_and(trivia_only)
            });
            self.blocks.push(SharedQBlock {
                contiguous_from_previous,
                start,
                end,
                digest: hasher.finalize().into(),
                resource_uses: std::mem::take(&mut self.resource_uses),
            });
        } else {
            self.resource_uses.clear();
        }
        self.block_valid = true;
    }

    fn instruction(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let operator = &instruction.operator[..];
        let span = instruction.operator_span();
        if self.depth == 0 {
            if operator == b"q" && instruction.operand_spans().len() == 0 {
                self.depth = 1;
                self.block_start = Some(span.start);
                self.block_valid = true;
                self.resource_uses.clear();
            }
            return;
        }
        if !self.block_valid && !matches!(operator, b"q" | b"Q") {
            return;
        }
        match operator {
            b"q" => {
                if instruction.operand_spans().len() != 0 {
                    self.block_valid = false;
                }
                self.depth = self.depth.saturating_add(1);
            }
            b"Q" => {
                if instruction.operand_spans().len() != 0 {
                    self.block_valid = false;
                }
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.close_outer_block(span.end);
                }
            }
            b"cs" | b"CS" => self.record_resource(SharedResourceKind::ColorSpace, instruction),
            b"gs" => self.record_resource(SharedResourceKind::ExtGState, instruction),
            b"Do" => self.record_resource(SharedResourceKind::XObject, instruction),
            b"scn" | b"SCN" => {
                if instruction
                    .operands()
                    .any(|operand| crate::content_stream::operand_name(operand).is_some())
                {
                    self.block_valid = false;
                }
            }
            b"cm" | b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" | b"W" | b"W*" | b"n"
            | b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"w" | b"J"
            | b"j" | b"M" | b"d" | b"G" | b"g" | b"RG" | b"rg" | b"K" | b"k" | b"ri" | b"i" => {}
            _ => self.block_valid = false,
        }
    }

    fn scan(&mut self) -> crate::Result<bool> {
        crate::content_stream::visit_instructions(self.input, |instruction| {
            self.instruction(instruction);
            Ok(())
        })
    }
}

fn factorable_shared_q_blocks(input: &[u8], targets: &SharedResourceTargets) -> Vec<SharedQBlock> {
    let mut scanner = SharedQBlockScanner::new(input, targets);
    if !matches!(scanner.scan(), Ok(false)) {
        return Vec::new();
    }
    scanner.blocks
}

fn shared_page_visible_box(document: &EditDocument, page: ObjectHandle) -> Result<Option<Rect>> {
    let Some(numbers) = page_box_numbers_matching(document, page, |numbers| {
        numbers.into_iter().all(f64::is_finite)
            && numbers[0].min(numbers[2]) < numbers[0].max(numbers[2])
            && numbers[1].min(numbers[3]) < numbers[1].max(numbers[3])
    })?
    else {
        return Ok(None);
    };
    Ok(Some(Rect {
        x0: numbers[0].min(numbers[2]),
        y0: numbers[1].min(numbers[3]),
        x1: numbers[0].max(numbers[2]),
        y1: numbers[1].max(numbers[3]),
    }))
}

const fn same_rect(left: Rect, right: Rect) -> bool {
    left.x0.to_bits() == right.x0.to_bits()
        && left.y0.to_bits() == right.y0.to_bits()
        && left.x1.to_bits() == right.x1.to_bits()
        && left.y1.to_bits() == right.y1.to_bits()
}

struct SharedPageData {
    page: ObjectHandle,
    decoded: std::sync::Arc<Vec<u8>>,
    blocks: Vec<SharedQBlock>,
    bounds: Rect,
    occupied_names: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
struct SharedRunCandidate {
    page_indices: Vec<usize>,
    start_block: usize,
    end_block: usize,
    bounds: Rect,
}

/// Explicit depth-first work stack. A shared prefix can split at every
/// nontrivia gap, so recursion depth is not bounded by the page count.
fn enqueue_shared_groups(
    page_data: &[&SharedPageData],
    pages: &[usize],
    cursor: usize,
    pending: &mut Vec<(Vec<usize>, usize)>,
) {
    let mut groups = BTreeMap::<[u8; 32], Vec<usize>>::new();
    for &page_index in pages {
        if let Some(block) = page_data[page_index].blocks.get(cursor) {
            groups.entry(block.digest).or_default().push(page_index);
        }
    }
    // Preserve the original sorted depth-first candidate order.
    pending.extend(
        groups
            .into_values()
            .rev()
            .filter(|group| group.len() >= 2)
            .map(|group| (group, cursor)),
    );
}

fn collect_shared_run_candidates(page_data: &[&SharedPageData]) -> Vec<SharedRunCandidate> {
    let mut out = Vec::new();
    let all_pages = (0..page_data.len()).collect::<Vec<_>>();
    let mut pending = Vec::new();
    enqueue_shared_groups(page_data, &all_pages, 0, &mut pending);
    while let Some((group, cursor)) = pending.pop() {
        let reference = group[0];
        let mut end = cursor + 1;
        while let Some(reference_block) = page_data[reference].blocks.get(end) {
            if !reference_block.contiguous_from_previous {
                break;
            }
            if !group.iter().all(|&page_index| {
                page_data[page_index].blocks.get(end).is_some_and(|block| {
                    block.contiguous_from_previous && block.digest == reference_block.digest
                })
            }) {
                break;
            }
            end += 1;
        }
        let bounds = page_data[reference].bounds;
        enqueue_shared_groups(page_data, &group, end, &mut pending);
        if group
            .iter()
            .all(|&page_index| same_rect(page_data[page_index].bounds, bounds))
        {
            out.push(SharedRunCandidate {
                page_indices: group,
                start_block: cursor,
                end_block: end,
                bounds,
            });
        }
    }
    out
}

fn visit_shared_run(
    page: &SharedPageData,
    candidate: &SharedRunCandidate,
    mut visit: impl FnMut(&[u8]) -> Option<()>,
) -> Option<BTreeSet<SharedResourceRef>> {
    let mut resources = BTreeSet::new();
    for block in page
        .blocks
        .get(candidate.start_block..candidate.end_block)?
    {
        visit_canonical_shared_span(
            &page.decoded,
            block.start,
            block.end,
            &block.resource_uses,
            &mut visit,
        )?;
        visit(b"\n")?;
        resources.extend(block.resource_uses.iter().map(|use_| use_.resource));
    }
    Some(resources)
}

fn shared_run_body(
    page: &SharedPageData,
    candidate: &SharedRunCandidate,
) -> Option<(Vec<u8>, BTreeSet<SharedResourceRef>)> {
    let mut body = Vec::new();
    let resources = visit_shared_run(page, candidate, |bytes| {
        body.extend_from_slice(bytes);
        Some(())
    })?;
    Some((body, resources))
}

fn shared_run_matches(
    page: &SharedPageData,
    candidate: &SharedRunCandidate,
    body: &[u8],
    resources: &BTreeSet<SharedResourceRef>,
) -> bool {
    let mut remaining = body;
    let actual = visit_shared_run(page, candidate, |bytes| {
        remaining = remaining.strip_prefix(bytes)?;
        Some(())
    });
    remaining.is_empty() && actual.as_ref() == Some(resources)
}

fn shared_form_resources(resources: &BTreeSet<SharedResourceRef>) -> OwnedDictionary {
    let mut by_kind = BTreeMap::<SharedResourceKind, OwnedDictionary>::new();
    for &resource in resources {
        by_kind.entry(resource.kind).or_default().insert(
            shared_resource_name(resource),
            OwnedObject::Reference(resource.target),
        );
    }
    let mut outer = OwnedDictionary::new();
    for (kind, dictionary) in by_kind {
        outer.insert(
            kind.dictionary_key().to_vec(),
            OwnedObject::Dictionary(dictionary),
        );
    }
    outer
}

fn shared_form_dictionary(
    bounds: Rect,
    resources: &BTreeSet<SharedResourceRef>,
) -> OwnedDictionary {
    let mut dictionary = path_form_dictionary(bounds);
    dictionary.insert(
        b"Resources".to_vec(),
        OwnedObject::Dictionary(shared_form_resources(resources)),
    );
    dictionary
}

struct SelectedSharedRun {
    candidate: SharedRunCandidate,
    name: Vec<u8>,
    replacement: Vec<u8>,
    body: Vec<u8>,
    resources: BTreeSet<SharedResourceRef>,
    compressed_body_bytes: usize,
}

/// Preserve positive and negative exact scan evidence across the structural
/// fixed point. Every retained entry belongs to an unchanged page and its
/// unchanged effective resource bindings.
#[derive(Default)]
struct SharedRunInventory {
    entries: BTreeMap<ObjectHandle, Option<SharedPageData>>,
}

impl SharedRunInventory {
    fn invalidate(&mut self, pages: &BTreeSet<ObjectHandle>) {
        self.entries.retain(|page, _| !pages.contains(page));
    }

    fn scan(
        &mut self,
        document: &EditDocument,
        pages: &[ObjectHandle],
    ) -> Result<Vec<&SharedPageData>> {
        let mut pending = Vec::new();
        let mut cached_count = 0usize;
        for &page in pages {
            if let Some(known) = self.entries.get(&page) {
                cached_count += usize::from(known.is_some());
                continue;
            }
            let Some(decoded) = page_content_if_present(document, page)? else {
                self.entries.insert(page, None);
                continue;
            };
            if shared_q_page_can_reach_minimum(decoded.len()) {
                pending.push((page, decoded));
            } else {
                self.entries.insert(page, None);
            }
        }
        if cached_count.saturating_add(pending.len()) < 2 {
            // Do not resolve resource targets or parse a lone potential page.
            return Ok(Vec::new());
        }
        for (page, decoded) in pending {
            let Some(bounds) = shared_page_visible_box(document, page)? else {
                self.entries.insert(page, None);
                continue;
            };
            let targets = shared_resource_targets(document, page)?;
            let blocks = factorable_shared_q_blocks(&decoded, &targets);
            let value = (!blocks.is_empty()).then(|| SharedPageData {
                page,
                decoded,
                blocks,
                bounds,
                occupied_names: targets.xobjects.into_keys().collect(),
            });
            self.entries.insert(page, value);
        }
        Ok(pages
            .iter()
            .filter_map(|page| self.entries.get(page)?.as_ref())
            .collect())
    }
}

/// Exact cross-page q-run factoring for Size, independent of rectangle/fill
/// compaction. A fill-only raster preflight cannot prove these Forms absent.
/// The underlying scanner checks page-box/resource identity and only admits
/// profitable, exact, balanced graphics-state scopes.
pub fn factor_size_shared_forms(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    pages: &[ObjectHandle],
) -> Result<VectorCompactionStats> {
    if pages.len() < 2 {
        return Ok(VectorCompactionStats::default());
    }
    factor_shared_q_prefix_runs(
        document,
        flate_level,
        pages,
        &mut VectorFlateLengthMemo::default(),
        &mut SharedRunInventory::default(),
    )
}

#[expect(
    clippy::too_many_lines,
    reason = "shared-run factoring scans, proves resource identity, models encoded savings, then commits one fail-closed transaction"
)]
fn factor_shared_q_prefix_runs(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    pages: &[ObjectHandle],
    flate_lengths: &mut VectorFlateLengthMemo,
    inventory: &mut SharedRunInventory,
) -> Result<VectorCompactionStats> {
    let page_data = inventory.scan(document, pages)?;
    if page_data.len() < 2 {
        return Ok(VectorCompactionStats::default());
    }
    let mut occupied_names = page_data
        .iter()
        .flat_map(|page| page.occupied_names.iter().cloned())
        .collect::<BTreeSet<_>>();

    let candidates = collect_shared_run_candidates(&page_data);
    let mut selected = Vec::new();
    let mut form_index = 0usize;
    for candidate in candidates {
        let representative = candidate.page_indices[0];
        let first = &page_data[representative].blocks[candidate.start_block];
        let last = &page_data[representative].blocks[candidate.end_block - 1];
        let decoded_span = last.end.saturating_sub(first.start);
        if decoded_span < MIN_SHARED_RUN_DECODED_BYTES {
            continue;
        }
        let Some((body, resources)) = shared_run_body(page_data[representative], &candidate) else {
            continue;
        };
        // Fingerprints only identify candidates. Compare the canonical bytes
        // and exact resource identities before replacing any page content.
        if !candidate.page_indices.iter().skip(1).all(|&page_index| {
            shared_run_matches(page_data[page_index], &candidate, &body, &resources)
        }) {
            continue;
        }
        let compressed_body_bytes = flate_lengths.encoded_len(&body, flate_level)?;
        let conservative_overhead = 384usize
            .saturating_add(resources.len().saturating_mul(96))
            .saturating_add(candidate.page_indices.len().saturating_mul(32));
        let repeated_body_savings =
            compressed_body_bytes.saturating_mul(candidate.page_indices.len().saturating_sub(1));
        if repeated_body_savings
            < conservative_overhead.saturating_add(MIN_SHARED_RUN_ESTIMATED_SAVINGS)
        {
            continue;
        }
        let name = path_form_name(form_index.saturating_add(20_000), &mut occupied_names);
        form_index = form_index.saturating_add(1);
        let replacement = [b" /".as_slice(), name.as_slice(), b" Do ".as_slice()].concat();
        selected.push(SelectedSharedRun {
            candidate,
            name,
            replacement,
            body,
            resources,
            compressed_body_bytes,
        });
    }
    if selected.is_empty() {
        return Ok(VectorCompactionStats::default());
    }

    let mut replacements_by_page = vec![Vec::<PageReplacement<'_>>::new(); page_data.len()];
    for run in &selected {
        for &page_index in &run.candidate.page_indices {
            let first = &page_data[page_index].blocks[run.candidate.start_block];
            let last = &page_data[page_index].blocks[run.candidate.end_block - 1];
            replacements_by_page[page_index].push((
                first.start,
                last.end,
                run.replacement.as_slice(),
            ));
        }
    }
    let resource_refs = selected
        .iter()
        .map(|run| run.resources.len())
        .sum::<usize>();
    let page_refs = selected
        .iter()
        .map(|run| run.candidate.page_indices.len())
        .sum::<usize>();
    let fixed_overhead = selected
        .iter()
        .map(|run| run.compressed_body_bytes)
        .sum::<usize>()
        .saturating_add(selected.len().saturating_mul(384))
        .saturating_add(resource_refs.saturating_mul(96))
        .saturating_add(page_refs.saturating_mul(32));
    let pages = page_data
        .iter()
        .zip(&replacements_by_page)
        .enumerate()
        .filter_map(|(index, (page, replacements))| {
            (!replacements.is_empty()).then_some((
                index,
                page.decoded.as_slice(),
                replacements.as_slice(),
            ))
        });
    let Some(measured) = measure_form_rewrites(
        pages,
        std::iter::empty(),
        fixed_overhead,
        flate_level,
        flate_lengths,
    )?
    else {
        return Ok(VectorCompactionStats::default());
    };
    let pages_rewritten = measured.pages.len();

    let mut form_handles = Vec::with_capacity(selected.len());
    for run in &selected {
        form_handles.push(ObjectHandle::New(document.add_object(
            OwnedObject::Stream {
                dictionary: shared_form_dictionary(run.candidate.bounds, &run.resources),
                data: StreamData::Owned(run.body.clone().into()),
            },
        )));
    }
    for (page_index, rewritten) in measured.pages {
        replace_page_content(document, page_data[page_index].page, rewritten)?;
    }
    let page_data = &page_data;
    install_page_resource_bindings(
        document,
        b"XObject",
        selected.iter().zip(form_handles).flat_map(|(run, handle)| {
            run.candidate
                .page_indices
                .iter()
                .map(move |&page_index| (page_data[page_index].page, run.name.as_slice(), handle))
        }),
    )?;

    let mut generated_page_xobjects = BTreeMap::<ObjectHandle, BTreeSet<Vec<u8>>>::new();
    let mut blocks_replaced = 0usize;
    let mut decoded_factored = 0usize;
    for run in &selected {
        let block_count = run
            .candidate
            .end_block
            .saturating_sub(run.candidate.start_block);
        blocks_replaced = blocks_replaced
            .saturating_add(block_count.saturating_mul(run.candidate.page_indices.len()));
        let mut occurrence_bytes = 0usize;
        for &page_index in &run.candidate.page_indices {
            let first = &page_data[page_index].blocks[run.candidate.start_block];
            let last = &page_data[page_index].blocks[run.candidate.end_block - 1];
            occurrence_bytes =
                occurrence_bytes.saturating_add(last.end.saturating_sub(first.start));
            generated_page_xobjects
                .entry(page_data[page_index].page)
                .or_default()
                .insert(run.name.clone());
        }
        decoded_factored =
            decoded_factored.saturating_add(occurrence_bytes.saturating_sub(run.body.len()));
    }

    Ok(VectorCompactionStats {
        pages_compacted: pages_rewritten,
        decoded_bytes_removed: decoded_factored,
        estimated_flate_bytes_saved: measured.savings,
        shared_run_forms_created: selected.len(),
        shared_run_pages_rewritten: pages_rewritten,
        shared_run_blocks_replaced: blocks_replaced,
        shared_run_decoded_bytes_factored: decoded_factored,
        shared_run_estimated_flate_bytes_saved: measured.savings,
        generated_page_xobjects,
        ..VectorCompactionStats::default()
    })
}

const PATH_COORDINATE_SCALE: f64 = 100.0;
const MAX_PATH_COORDINATE_PAGE_ERROR_PT: f64 = 0.0071;
const MIN_PATH_COORDINATE_FLATE_SAVINGS: usize = 4 * 1024;
const PATH_COORDINATE_MIN_RELATIVE_SAVINGS_DIVISOR: usize = 200;

#[derive(Debug, Clone)]
struct PathCoordinatePlan {
    canonicalized: Vec<u8>,
    coordinates_canonicalized: usize,
}

struct CoordinateReplacement {
    start: usize,
    end: usize,
    byte_start: usize,
    byte_length: usize,
}

struct PathCoordinateScanner {
    ctm: Option<Matrix>,
    state_stack: Vec<Option<Matrix>>,
    user_unit: f64,
    replacements: Vec<CoordinateReplacement>,
    replacement_bytes: Vec<u8>,
    coordinates_canonicalized: usize,
}

impl PathCoordinateScanner {
    fn new(user_unit: f64) -> Self {
        Self {
            ctm: Some(Matrix::default()),
            state_stack: Vec::new(),
            user_unit,
            replacements: Vec::new(),
            replacement_bytes: Vec::new(),
            coordinates_canonicalized: 0,
        }
    }

    fn rounded_coordinate(value: f64) -> Option<f64> {
        if !value.is_finite() {
            return None;
        }
        let scaled = value * PATH_COORDINATE_SCALE;
        if !scaled.is_finite() {
            return None;
        }
        Some(scaled.round() / PATH_COORDINATE_SCALE)
    }

    fn rounded_coordinate_bytes(value: f64) -> Option<SmallVec<[u8; 32]>> {
        let rounded = Self::rounded_coordinate(value)?;
        if rounded == 0.0 {
            return Some(SmallVec::from_slice(b"0"));
        }

        // A cent-valued f64 is unambiguous when its representable spacing
        // is smaller than half a cent and converting the integer cent count
        // back to f64 reconstructs the exact rounded value. Outside that
        // domain, retain Rust's full-precision fixed-decimal formatter.
        let scaled = rounded * PATH_COORDINATE_SCALE;
        let cents = scaled.round();
        let half_cent = 0.5 / PATH_COORDINATE_SCALE;
        if (f64::EPSILON * rounded.abs()) < half_cent
            && cents.abs() < 9_007_199_254_740_992.0
            && (cents / PATH_COORDINATE_SCALE).to_bits() == rounded.to_bits()
            && let Some(cents) = cents.to_i64()
        {
            let mut bytes = [0_u8; 32];
            let mut cursor = bytes.len();
            let unsigned = cents.unsigned_abs();
            let fraction = unsigned % 100;
            if fraction > 0 {
                if !fraction.is_multiple_of(10) {
                    cursor -= 1;
                    bytes[cursor] = b'0' + (fraction % 10) as u8;
                }
                cursor -= 1;
                bytes[cursor] = b'0' + (fraction / 10) as u8;
                cursor -= 1;
                bytes[cursor] = b'.';
            }
            let mut whole = unsigned / 100;
            loop {
                cursor -= 1;
                bytes[cursor] = b'0' + (whole % 10) as u8;
                whole /= 10;
                if whole == 0 {
                    break;
                }
            }
            if cents < 0 {
                cursor -= 1;
                bytes[cursor] = b'-';
            }
            return Some(SmallVec::from_slice(&bytes[cursor..]));
        }

        let mut out = format!("{rounded:.2}");
        while out.contains('.') && out.ends_with('0') {
            out.pop();
        }
        if out.ends_with('.') {
            out.pop();
        }
        if out == "-0" {
            "0".clone_into(&mut out);
        }
        Some(SmallVec::from_slice(out.as_bytes()))
    }

    fn point_error(&self, old: (f64, f64), new: (f64, f64)) -> Option<f64> {
        let ctm = self.ctm?;
        let old = ctm.transform(old.0, old.1);
        let new = ctm.transform(new.0, new.1);
        Some((old.0 - new.0).hypot(old.1 - new.1) * self.user_unit)
    }

    fn path_geometry_is_within_tolerance(&self, operator: &[u8], old: &[f64], new: &[f64]) -> bool {
        let point_ok = |this: &Self, old: (f64, f64), new: (f64, f64)| {
            this.point_error(old, new)
                .is_some_and(|error| error <= MAX_PATH_COORDINATE_PAGE_ERROR_PT)
        };
        match operator {
            b"m" | b"l" if old.len() == 2 && new.len() == 2 => {
                point_ok(self, (old[0], old[1]), (new[0], new[1]))
            }
            b"c" if old.len() == 6 && new.len() == 6 => (0..3).all(|index| {
                let offset = index * 2;
                point_ok(
                    self,
                    (old[offset], old[offset + 1]),
                    (new[offset], new[offset + 1]),
                )
            }),
            b"v" | b"y" if old.len() == 4 && new.len() == 4 => (0..2).all(|index| {
                let offset = index * 2;
                point_ok(
                    self,
                    (old[offset], old[offset + 1]),
                    (new[offset], new[offset + 1]),
                )
            }),
            b"re" if old.len() == 4 && new.len() == 4 => {
                let old_points = [
                    (old[0], old[1]),
                    (old[0] + old[2], old[1]),
                    (old[0], old[1] + old[3]),
                    (old[0] + old[2], old[1] + old[3]),
                ];
                let new_points = [
                    (new[0], new[1]),
                    (new[0] + new[2], new[1]),
                    (new[0], new[1] + new[3]),
                    (new[0] + new[2], new[1] + new[3]),
                ];
                old_points
                    .into_iter()
                    .zip(new_points)
                    .all(|(old, new)| point_ok(self, old, new))
            }
            _ => false,
        }
    }

    fn canonicalize_path_operands(
        &mut self,
        input: &[u8],
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        old: &[f64],
    ) {
        if self.ctm.is_none() {
            return;
        }
        let operator = &instruction.operator[..];
        let Some(new) = old
            .iter()
            .copied()
            .map(Self::rounded_coordinate)
            .collect::<Option<SmallVec<[f64; 6]>>>()
        else {
            return;
        };
        if !self.path_geometry_is_within_tolerance(operator, old, &new) {
            return;
        }

        // A path operator has at most six coordinate operands. Keep its
        // atomic replacement transaction inline until all spans validate.
        let mut pending = SmallVec::<[(usize, usize, SmallVec<[u8; 32]>); 6]>::new();
        for ((span, &original_value), &rounded) in instruction.operand_spans().zip(old).zip(&new) {
            if rounded.to_bits() == original_value.to_bits() {
                continue;
            }
            let Some(replacement) = Self::rounded_coordinate_bytes(rounded) else {
                return;
            };
            let end = span.end;
            let Some(original) = input.get(span.start..end) else {
                return;
            };
            if replacement.len() >= original.len() {
                continue;
            }
            pending.push((span.start, end, replacement));
        }
        self.coordinates_canonicalized =
            self.coordinates_canonicalized.saturating_add(pending.len());
        for (start, end, bytes) in pending {
            let byte_start = self.replacement_bytes.len();
            self.replacement_bytes.extend_from_slice(&bytes);
            self.replacements.push(CoordinateReplacement {
                start,
                end,
                byte_start,
                byte_length: bytes.len(),
            });
        }
    }

    fn concat_ctm(&mut self, values: Option<&[f64]>) {
        let matrix = values.map(|values| {
            Matrix::new(
                values[0], values[1], values[2], values[3], values[4], values[5],
            )
        });
        match (&mut self.ctm, matrix) {
            (Some(ctm), Some(matrix)) => ctm.concat(matrix),
            _ => self.ctm = None,
        }
    }

    fn projected_instruction(
        &mut self,
        input: &[u8],
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        operands: &VectorOperands<'_>,
    ) {
        match &instruction.operator[..] {
            b"q" => self.state_stack.push(self.ctm),
            b"Q" => self.ctm = self.state_stack.pop().unwrap_or(None),
            b"cm" => self.concat_ctm(operands.numbers()),
            b"m" | b"l" | b"c" | b"v" | b"y" | b"re" => {
                if let Some(values) = operands.numbers() {
                    self.canonicalize_path_operands(input, instruction, values);
                }
            }
            _ => {}
        }
    }

    fn scan(&mut self, input: &[u8]) -> crate::Result<bool> {
        crate::content_stream::visit_instructions(input, |instruction| {
            self.projected_instruction(input, instruction, &VectorOperands::project(instruction));
            Ok(())
        })
    }

    fn finish(self, input: &[u8]) -> Option<PathCoordinatePlan> {
        if !self.state_stack.is_empty() || self.replacements.is_empty() {
            return None;
        }
        Some(PathCoordinatePlan {
            canonicalized: apply_span_replacements_from_arena(
                input,
                self.replacements,
                &self.replacement_bytes,
            )?,
            coordinates_canonicalized: self.coordinates_canonicalized,
        })
    }
}

fn canonicalize_path_coordinates(input: &[u8], user_unit: f64) -> Option<(Vec<u8>, usize)> {
    if !user_unit.is_finite() || user_unit <= 0.0 {
        return None;
    }
    let mut scanner = PathCoordinateScanner::new(user_unit);
    let parsed = scanner.scan(input);
    if !matches!(parsed, Ok(false)) {
        return None;
    }
    let plan = scanner.finish(input)?;
    let coordinates = plan.coordinates_canonicalized;
    Some((plan.canonicalized, coordinates))
}

#[derive(Debug, Default)]
struct PathCoordinateOutcome {
    stats: VectorCompactionStats,
    rewritten_pages: BTreeSet<ObjectHandle>,
}

fn canonicalize_page_path_coordinates(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    pages: &[ObjectHandle],
    mut pre_scanned: BTreeMap<ObjectHandle, Option<PathCoordinatePlan>>,
    rewritten_pages: &BTreeSet<ObjectHandle>,
    flate_state: &mut VectorCompactionFlateState<'_>,
) -> Result<PathCoordinateOutcome> {
    let mut stats = VectorCompactionStats::default();
    let mut coordinate_rewritten_pages = BTreeSet::new();
    for &page in pages {
        let cached = (!rewritten_pages.contains(&page))
            .then(|| pre_scanned.remove(&page))
            .flatten();
        // A complete negative scan is sufficient: no content decoding or
        // dictionary traversal is needed when that page has not changed.
        if matches!(cached, Some(None)) {
            continue;
        }
        let Some(user_unit) = page_user_unit(document, page)? else {
            continue;
        };
        let Some(object) = document.current_value(page)? else {
            continue;
        };
        let Some(contents) = object.dictionary_entry(b"Contents") else {
            continue;
        };
        let contents_handle = contents.reference();
        let decoded = decoded_content_value(document, contents)?;
        let candidate = match cached {
            Some(Some(plan)) => Some((plan.canonicalized, plan.coordinates_canonicalized)),
            Some(None) => None,
            None => canonicalize_path_coordinates(&decoded, user_unit),
        };
        let Some((canonicalized, coordinates)) = candidate else {
            continue;
        };
        let Some(before) = flate_state
            .lengths
            .measure(&decoded, flate_level, usize::MAX)?
        else {
            return Err(crate::Error::Compression(
                "Flate output exceeded its bound".to_owned(),
            ));
        };
        let required = MIN_PATH_COORDINATE_FLATE_SAVINGS
            .max(before.length / PATH_COORDINATE_MIN_RELATIVE_SAVINGS_DIVISOR);
        let after = match before.length.checked_sub(required) {
            Some(limit) => flate_state
                .lengths
                .measure(&canonicalized, flate_level, limit)?,
            None => None,
        };
        let Some(after) = after else {
            if let Some(handle) = contents_handle {
                before.retain(flate_state.reuse, handle, flate_level);
            }
            continue;
        };
        let savings = before.length - after.length;
        let decoded_bytes_removed = decoded.len().saturating_sub(canonicalized.len());
        let replacement = replace_page_content_with_handle(document, page, canonicalized)?;
        after.retain(flate_state.reuse, replacement, flate_level);
        coordinate_rewritten_pages.insert(page);
        stats.pages_compacted = stats.pages_compacted.saturating_add(1);
        stats.path_coordinate_pages_rewritten =
            stats.path_coordinate_pages_rewritten.saturating_add(1);
        stats.path_coordinates_canonicalized = stats
            .path_coordinates_canonicalized
            .saturating_add(coordinates);
        stats.path_coordinate_decoded_bytes_removed = stats
            .path_coordinate_decoded_bytes_removed
            .saturating_add(decoded_bytes_removed);
        stats.path_coordinate_estimated_flate_bytes_saved = stats
            .path_coordinate_estimated_flate_bytes_saved
            .saturating_add(savings);
        stats.estimated_flate_bytes_saved =
            stats.estimated_flate_bytes_saved.saturating_add(savings);
    }
    Ok(PathCoordinateOutcome {
        stats,
        rewritten_pages: coordinate_rewritten_pages,
    })
}

fn compact_processing_fill_tail(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    pages: &BTreeSet<ObjectHandle>,
    flate_lengths: &mut VectorFlateLengthMemo,
) -> Result<VectorCompactionStats> {
    let mut total = VectorCompactionStats::default();
    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        let resources = effective_page_resources(document, page)?;
        let ext_gstates = ext_gstate_patches(document, &resources)?;
        let (compacted, stats) = compact_content_with_ext_gstates(&decoded, &ext_gstates);
        if matches!(compacted, Cow::Borrowed(_)) || compacted.as_ref() == decoded.as_slice() {
            continue;
        }
        let before_flate = flate_lengths.encoded_len(&decoded, flate_level)?;
        let after_flate = flate_lengths.encoded_len(&compacted, flate_level)?;
        replace_page_content(document, page, compacted.into_owned())?;
        total.pages_compacted = total.pages_compacted.saturating_add(1);
        total.fill_groups_batched = total
            .fill_groups_batched
            .saturating_add(stats.fill_groups_batched);
        total.covered_fills_pruned = total
            .covered_fills_pruned
            .saturating_add(stats.covered_fills_pruned);
        total.fill_paints_eliminated = total
            .fill_paints_eliminated
            .saturating_add(stats.fill_paints_eliminated);
        total.decoded_bytes_removed = total
            .decoded_bytes_removed
            .saturating_add(stats.decoded_bytes_removed);
        total.estimated_flate_bytes_saved = total
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(total)
}

// Exact, structure-only stages. These may be revisited after a content rewrite,
// but bounded-error coordinate canonicalization must remain a one-shot stage.
fn factor_processing_structural_forms(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    pages: &[ObjectHandle],
    pre_scanned: ProcessingFactorBlocks,
    flate_lengths: &mut VectorFlateLengthMemo,
    shared_inventory: &mut SharedRunInventory,
) -> Result<(
    VectorCompactionStats,
    BTreeSet<ObjectHandle>,
    ProcessingFactorBlocks,
)> {
    let PathFactoringOutcome {
        stats: path_stats,
        mut reusable_blocks,
        rewritten_pages: path_rewritten,
    } = factor_repeated_path_forms(document, flate_level, pre_scanned, pages, flate_lengths)?;
    let transformed = factor_repeated_transformed_blocks(
        document,
        flate_level,
        &reusable_blocks,
        &path_rewritten,
        pages,
        flate_lengths,
    )?;
    let mut rewritten = path_rewritten;
    rewritten.extend(transformed.generated_page_xobjects.keys().copied());
    shared_inventory.invalidate(&rewritten);
    let shared = factor_shared_q_prefix_runs(
        document,
        flate_level,
        pages,
        flate_lengths,
        shared_inventory,
    )?;
    let shared_rewritten = shared.generated_page_xobjects.keys().copied().collect();
    shared_inventory.invalidate(&shared_rewritten);
    rewritten.extend(shared_rewritten);
    // A rewritten stream invalidates its source offsets and block semantics.
    // All unchanged pages remain candidates for cross-page matches.
    for page in &rewritten {
        reusable_blocks.remove(page);
    }
    let mut stats = VectorCompactionStats::default();
    stats.absorb(path_stats);
    stats.absorb(transformed);
    stats.absorb(shared);
    Ok((stats, rewritten, reusable_blocks))
}

// Every accepted Form factor replaces a page span containing native operators
// (or q/Q scope delimiters) with one `Do`: the number of page-level non-`Do`
// instructions strictly decreases. Form streams are not searched by these
// page-only factor stages; exact fill compaction also only removes paint work.
// This finite, decreasing potential proves termination without a guessed pass
// limit. Neither bounded-error coordinate nor polyline rewriting runs here.
// Candidate discovery must consider all pages, including ones not just rewritten,
// because semantic matches can span pages.
fn close_processing_exact_factoring(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    pages: &[ObjectHandle],
    mut pre_scanned: ProcessingFactorBlocks,
    flate_lengths: &mut VectorFlateLengthMemo,
    shared_inventory: &mut SharedRunInventory,
) -> Result<VectorCompactionStats> {
    let mut total = VectorCompactionStats::default();
    loop {
        let (forms, rewritten, reusable_blocks) = factor_processing_structural_forms(
            document,
            flate_level,
            pages,
            pre_scanned,
            flate_lengths,
            shared_inventory,
        )?;
        let changed = forms.path_forms_created != 0
            || forms.transformed_forms_created != 0
            || forms.shared_run_forms_created != 0;
        if !changed {
            break;
        }
        // A page rewritten by any Form pass must be scanned using its new
        // content and resources; do not recycle pre-mutation raster/path proofs.
        let fills = compact_processing_fill_tail(document, flate_level, &rewritten, flate_lengths)?;
        // Fill cleanup rewrites only pages already present in rewritten;
        // every retained metadata entry is still tied to identical bytes.
        pre_scanned = reusable_blocks;
        total.absorb(forms);
        total.absorb(fills);
    }
    Ok(total)
}

fn finish_processing_vector_compaction(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    pre_scanned_processing: BTreeMap<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>,
    mut path_coordinate_plans: BTreeMap<ObjectHandle, Option<PathCoordinatePlan>>,
    mut vector_rewritten_pages: BTreeSet<ObjectHandle>,
    flate_state: &mut VectorCompactionFlateState<'_>,
) -> Result<VectorCompactionStats> {
    let mut total = VectorCompactionStats::default();
    let mut shared_inventory = SharedRunInventory::default();
    for page in &vector_rewritten_pages {
        path_coordinate_plans.remove(page);
    }
    let (factored, factored_pages, mut reusable_blocks) = factor_processing_structural_forms(
        document,
        flate_level,
        pages,
        pre_scanned_processing,
        &mut flate_state.lengths,
        &mut shared_inventory,
    )?;
    for page in &factored_pages {
        path_coordinate_plans.remove(page);
    }
    vector_rewritten_pages.extend(factored_pages.iter().copied());
    let mut late_rewritten_pages = factored_pages;
    total.absorb(factored);

    let path_coordinates = canonicalize_page_path_coordinates(
        document,
        flate_level,
        pages,
        path_coordinate_plans,
        &vector_rewritten_pages,
        flate_state,
    )?;
    vector_rewritten_pages.extend(path_coordinates.rewritten_pages.iter().copied());
    late_rewritten_pages.extend(path_coordinates.rewritten_pages.iter().copied());
    let path_coordinate_stats = path_coordinates.stats;
    total.pages_compacted += path_coordinate_stats.pages_compacted;
    total.decoded_bytes_removed += path_coordinate_stats.decoded_bytes_removed;
    total.estimated_flate_bytes_saved += path_coordinate_stats.estimated_flate_bytes_saved;
    total.path_coordinates_canonicalized += path_coordinate_stats.path_coordinates_canonicalized;
    total.path_coordinate_pages_rewritten += path_coordinate_stats.path_coordinate_pages_rewritten;
    total.path_coordinate_decoded_bytes_removed +=
        path_coordinate_stats.path_coordinate_decoded_bytes_removed;
    total.path_coordinate_estimated_flate_bytes_saved +=
        path_coordinate_stats.path_coordinate_estimated_flate_bytes_saved;

    // Processing-only coordinate/factoring rewrites can make exact rectangle
    // adjacency visible only after the initial fill scan. Close that local
    // dependency here instead of requiring another whole optimizer pass.
    let fill_tail = compact_processing_fill_tail(
        document,
        flate_level,
        &late_rewritten_pages,
        &mut flate_state.lengths,
    )?;
    total.absorb(fill_tail);
    // The earlier lossless factor passes had already run before bounded-error
    // coordinate rewriting and the fill tail. Close only the exact stages
    // that those mutations may expose, without rounding any path twice.
    if !late_rewritten_pages.is_empty() {
        for page in &late_rewritten_pages {
            reusable_blocks.remove(page);
        }
        shared_inventory.invalidate(&late_rewritten_pages);
        let closure = close_processing_exact_factoring(
            document,
            flate_level,
            pages,
            reusable_blocks,
            &mut flate_state.lengths,
            &mut shared_inventory,
        )?;
        total.absorb(closure);
    }
    Ok(total)
}

fn write_compacted_vector_page(
    document: &mut EditDocument,
    page: ObjectHandle,
    decoded: &[u8],
    compacted: Vec<u8>,
    flate_level: crate::FlateLevel,
    goal: OptimizationGoal,
    flate_state: &mut VectorCompactionFlateState<'_>,
) -> Result<Option<usize>> {
    let before_flate = flate_state.lengths.encoded_len(decoded, flate_level)?;
    // Processing also needs the exact encoded length for its savings report.
    // Retain those already-computed bytes for the final unfiltered-stream
    // compression phase instead of recompressing unchanged page content.
    let after_encoded = crate::stream_codec::encode_flate(&compacted, flate_level)?;
    let after_flate = after_encoded.len();
    if goal == OptimizationGoal::Size && after_flate >= before_flate {
        return Ok(None);
    }
    let decoded_sha256 = Sha256::digest(&compacted).into();
    flate_state
        .lengths
        .remember_encoded_len(decoded_sha256, flate_level, after_flate);
    let replacement = replace_page_content_with_handle(document, page, compacted)?;
    flate_state
        .reuse
        .insert_with_digest(replacement, flate_level, decoded_sha256, after_encoded);
    Ok(Some(before_flate.saturating_sub(after_flate)))
}

pub fn compact_vector_paths_with_flate_cache(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    goal: OptimizationGoal,
    mut raster_pre_scanned: BTreeMap<ObjectHandle, ProcessingVectorAnalysis>,
    flate_reuse_cache: &mut FlateReuseCache,
) -> Result<VectorCompactionStats> {
    let mut total = VectorCompactionStats::default();
    let mut flate_state = VectorCompactionFlateState::new(flate_reuse_cache, goal);
    let mut pre_scanned_processing =
        BTreeMap::<ObjectHandle, (Vec<PathBlock>, Vec<TransformedBlock>)>::new();
    let mut path_coordinate_plans = BTreeMap::<ObjectHandle, Option<PathCoordinatePlan>>::new();
    let mut vector_rewritten_pages = BTreeSet::<ObjectHandle>::new();
    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        let resources = effective_page_resources(document, page)?;
        let ext_gstates = ext_gstate_patches(document, &resources)?;
        let ((compacted, stats), factor_cache) = if goal == OptimizationGoal::Processing {
            let analysis = if let Some(analysis) = raster_pre_scanned.remove(&page) {
                Some(analysis)
            } else {
                scan_processing_page(
                    &decoded,
                    ext_gstates.clone(),
                    page_user_unit(document, page)?,
                )
            };
            analysis.map_or_else(
                || {
                    (
                        compact_content_with_ext_gstates(&decoded, &ext_gstates),
                        None,
                    )
                },
                |analysis| {
                    let ProcessingVectorAnalysis {
                        fills,
                        path_blocks,
                        transformed_blocks,
                        path_coordinate_plan,
                    } = analysis;
                    path_coordinate_plans.insert(page, path_coordinate_plan);
                    (
                        compact_content_from_fill_scan(&decoded, &ext_gstates, &fills),
                        Some((path_blocks, transformed_blocks)),
                    )
                },
            )
        } else {
            (
                compact_content_with_ext_gstates(&decoded, &ext_gstates),
                None,
            )
        };
        if matches!(compacted, Cow::Borrowed(_)) || compacted.as_ref() == decoded.as_slice() {
            if let Some(factor_cache) = factor_cache {
                pre_scanned_processing.insert(page, factor_cache);
            }
            continue;
        }
        let Some(saved_bytes) = write_compacted_vector_page(
            document,
            page,
            &decoded,
            compacted.into_owned(),
            flate_level,
            goal,
            &mut flate_state,
        )?
        else {
            continue;
        };
        vector_rewritten_pages.insert(page);
        total.pages_compacted += 1;
        total.fill_groups_batched += stats.fill_groups_batched;
        total.covered_fills_pruned += stats.covered_fills_pruned;
        total.fill_paints_eliminated += stats.fill_paints_eliminated;
        total.decoded_bytes_removed += stats.decoded_bytes_removed;
        total.estimated_flate_bytes_saved += saved_bytes;
    }
    if goal == OptimizationGoal::Processing {
        total.absorb(finish_processing_vector_compaction(
            document,
            pages,
            flate_level,
            pre_scanned_processing,
            path_coordinate_plans,
            vector_rewritten_pages,
            &mut flate_state,
        )?);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectId;

    #[test]
    fn preferred_body_index_matches_exhaustive_containment() {
        let ranges = [(30, 50), (0, 10), (10, 45), (20, 22), (60, 70), (60, 90)];
        let blocks = ranges.map(|(body_start, body_end)| TransformedBlock {
            body_start,
            body_end,
            semantic_key: [1; 32],
            operator_count: 10,
            bounds: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 1.0,
                y1: 1.0,
            },
        });
        let preferred = HashSet::from([[1; 32]]);
        let indexed = PreferredBodyIndex::new(&blocks, &preferred);
        for start in 0..100 {
            for end in start..100 {
                assert_eq!(
                    indexed.contains(start, end),
                    ranges.iter().any(|&(a, b)| a <= start && end <= b),
                    "{start}..{end}"
                );
            }
        }
    }

    #[test]
    fn repeated_form_candidates_materialize_only_repeated_first_occurrence() {
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let pages = vec![
            PagePathData {
                page: ObjectHandle::Existing(ObjectId::new(3, 0)),
                decoded: b"prefix 0 0 10 10 re f suffix".to_vec().into(),
            },
            PagePathData {
                page: ObjectHandle::Existing(ObjectId::new(4, 0)),
                decoded: b"prefix 0 0 10 10 re f suffix".to_vec().into(),
            },
        ];
        let occurrence = |page_index, start, end| PathOccurrence {
            page_index,
            start,
            end,
            bounds,
        };
        let groups = BTreeMap::from([
            // A unique key must never be materialized, even if its stale span
            // cannot be indexed. No Form will be created from that group.
            (
                semantic_fingerprint(b"unique"),
                (1, vec![occurrence(99, 0, 999)]),
            ),
            (
                semantic_fingerprint(b"repeated"),
                (4, vec![occurrence(0, 7, 21), occurrence(1, 7, 21)]),
            ),
        ]);
        let candidates = repeated_form_candidates(groups, &pages);
        assert_eq!(candidates.len(), 1);
        let (body, operators, occurrences) = &candidates[0];
        assert_eq!(body.as_ref(), b"0 0 10 10 re f");
        assert_eq!(*operators, 4);
        assert_eq!(occurrences.len(), 2);
        assert_eq!(occurrences[0].page_index, 0);
        assert_eq!(occurrences[1].page_index, 1);
    }

    #[test]
    fn form_budget_admission_preserves_exact_global_savings() -> Result<()> {
        let level = crate::FlateLevel::PROCESSING;
        let input = (0..=255u8)
            .flat_map(|byte| [byte, byte.rotate_left(3)])
            .cycle()
            .take(8192)
            .collect::<Vec<_>>();
        let edits = [(0, input.len(), b"q Q".as_slice())];
        let body = b"0 0 m 1 1 l S";
        let before = crate::stream_codec::flate_encoded_len(&input, level)?;
        let after = crate::stream_codec::flate_encoded_len(b"q Q", level)?
            + crate::stream_codec::flate_encoded_len(body, level)?;
        let pages = [(0, input.as_slice(), edits.as_slice())];
        for enabled in [false, true] {
            let mut memo = VectorFlateLengthMemo {
                enabled,
                ..VectorFlateLengthMemo::default()
            };
            let accepted = measure_form_rewrites(
                pages.into_iter(),
                std::iter::once(body.as_slice()),
                before - after - 1,
                level,
                &mut memo,
            )?
            .ok_or_else(|| crate::Error::Invalid("one-byte exact saving rejected".into()))?;
            assert_eq!(accepted.savings, 1);
            assert_eq!(accepted.pages, [(0, b"q Q".to_vec())]);
            assert!(
                measure_form_rewrites(
                    pages.into_iter(),
                    std::iter::once(body.as_slice()),
                    before - after,
                    level,
                    &mut memo
                )?
                .is_none()
            );
            let invalid = [(0, input.len() + 1, b"q Q".as_slice())];
            assert!(
                measure_form_rewrites(
                    [(0, input.as_slice(), invalid.as_slice())].into_iter(),
                    std::iter::once(body.as_slice()),
                    0,
                    level,
                    &mut memo
                )?
                .is_none()
            );
        }
        Ok(())
    }

    #[test]
    fn vector_flate_length_memo_preserves_exact_lengths_and_bounds_entries() -> Result<()> {
        let level = crate::FlateLevel::PROCESSING;
        let data = b"0 0 50 50 re f\n".repeat(64);
        let mut memo = VectorFlateLengthMemo {
            enabled: true,
            ..VectorFlateLengthMemo::default()
        };
        let expected = crate::stream_codec::flate_encoded_len(&data, level)?;
        assert_eq!(memo.encoded_len(&data, level)?, expected);
        assert_eq!(memo.encoded_len(&data, level)?, expected);
        assert_eq!(memo.hits, 1);
        assert_eq!(memo.misses, 1);
        let other_level = crate::FlateLevel::SIZE;
        assert_eq!(
            memo.encoded_len(&data, other_level)?,
            crate::stream_codec::flate_encoded_len(&data, other_level)?
        );
        assert_eq!(memo.misses, 2);
        for i in 0..=MAX_VECTOR_FLATE_LENGTHS {
            let unique = i.to_le_bytes();
            memo.remember_encoded_len(Sha256::digest(unique).into(), level, unique.len());
        }
        assert_eq!(memo.lengths.len(), MAX_VECTOR_FLATE_LENGTHS);
        assert_eq!(memo.insertion_order.len(), memo.lengths.len());
        assert!(memo.evictions > 0);
        assert_eq!(
            memo.encoded_len(&data, level)?,
            crate::stream_codec::flate_encoded_len(&data, level)?
        );
        Ok(())
    }

    #[test]
    fn capped_vector_measurement_rejects_without_caching_inexact_lengths() -> Result<()> {
        let level = crate::FlateLevel::PROCESSING;
        let data = b"0 0 30 30 re f\n".repeat(512);
        let expected = crate::stream_codec::encode_flate(&data, level)?;
        let mut memo = VectorFlateLengthMemo::processing();

        assert!(memo.measure(&data, level, expected.len() - 1)?.is_none());
        assert!(memo.lengths.is_empty());
        let measured = memo
            .measure(&data, level, expected.len())?
            .ok_or_else(|| crate::Error::Invalid("exact-cap encoding rejected".into()))?;
        assert_eq!(measured.length, expected.len());
        assert_eq!(measured.encoding.as_deref(), Some(expected.as_slice()));
        assert_eq!(memo.misses, 2);
        assert_eq!(memo.lengths.len(), 1);

        assert!(memo.measure(&data, level, expected.len() - 1)?.is_none());
        let cached = memo
            .measure(&data, level, usize::MAX)?
            .ok_or_else(|| crate::Error::Invalid("cached exact length rejected".into()))?;
        assert_eq!(cached.length, expected.len());
        assert!(cached.encoding.is_none());
        assert_eq!(memo.hits, 2);
        Ok(())
    }

    #[test]
    fn unchanged_fill_compaction_borrows_input() {
        let input = b"BT /F1 12 Tf (hello) Tj ET";
        let (result, stats) = compact_content(input);
        assert!(matches!(result, Cow::Borrowed(bytes) if bytes == input));
        assert_eq!(stats, VectorCompactionStats::default());
    }

    #[test]
    fn compacted_page_processing_reuses_exact_encoding() -> Result<()> {
        use crate::{FlateLevel, test_support::ClassicPdfBuilder};

        let original = b"0 0 30 30 re f\n".repeat(3_000);
        let compacted = b"0 0 30 30 re f\n".repeat(1_000);
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", &original)?;
        let source = pdf.finish(1)?;
        let page = ObjectHandle::Existing(ObjectId::new(3, 0));
        let batcher = crate::stream_codec::FlateBatchEncoder::new(std::num::NonZeroUsize::MIN)?;

        for (goal, level) in [
            (OptimizationGoal::Processing, FlateLevel::PROCESSING),
            (OptimizationGoal::Size, FlateLevel::SIZE),
        ] {
            let mut cached = EditDocument::from_bytes(source.clone())?;
            let mut uncached = EditDocument::from_bytes(source.clone())?;
            let mut cache = FlateReuseCache::default();
            let mut empty = FlateReuseCache::default();
            let mut cache_state = VectorCompactionFlateState::new(&mut cache, goal);
            let mut empty_state = VectorCompactionFlateState::new(&mut empty, goal);
            let estimated = write_compacted_vector_page(
                &mut cached,
                page,
                &original,
                compacted.clone(),
                level,
                goal,
                &mut cache_state,
            )?;
            let reference_estimated = write_compacted_vector_page(
                &mut uncached,
                page,
                &original,
                compacted.clone(),
                level,
                goal,
                &mut empty_state,
            )?;
            assert_eq!(estimated, reference_estimated);
            assert!(estimated.is_some());
            let expected_savings = crate::stream_codec::flate_encoded_len(&original, level)?
                .saturating_sub(crate::stream_codec::flate_encoded_len(&compacted, level)?);
            assert_eq!(estimated, Some(expected_savings));
            // The reference must use a fresh uncached encoding.
            drop(cache_state);
            drop(empty_state);
            empty = FlateReuseCache::default();

            let reused_stats = crate::flate::compress_unfiltered_streams(
                &mut cached,
                level,
                &mut cache,
                &batcher,
            )?;
            let reference_stats = crate::flate::compress_unfiltered_streams(
                &mut uncached,
                level,
                &mut empty,
                &batcher,
            )?;
            assert_eq!(reused_stats, reference_stats);
            assert_eq!(
                crate::writer::write_pdf(&cached)?,
                crate::writer::write_pdf(&uncached)?
            );
        }
        Ok(())
    }

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
                    semantic_key: semantic_fingerprint(key),
                    operator_count: 1,
                })
                .collect(),
            transformed_blocks: transformed_keys
                .iter()
                .map(|key| TransformedBlock {
                    body_start: 0,
                    body_end: MIN_PROCESSING_PATH_FORM_BYTES,
                    bounds,
                    semantic_key: semantic_fingerprint(key),
                    operator_count: 8,
                })
                .collect(),
            path_coordinate_plan: None,
        }
    }

    #[test]
    fn exact_closure_reuses_only_unchanged_page_blocks() -> Result<()> {
        use crate::{FlateLevel, test_support::ClassicPdfBuilder};

        // Pages 3 and 4 share a long transform-independent paint body.
        // Page 5 is intentionally different and is never rewritten. Compare
        // caching with a fresh full document scan after an accepted rewrite.
        let mut shared = Vec::new();
        let mut distinct = Vec::new();
        for index in 0..360 {
            let x = index * 13 + 17;
            let y = index * 19 + 29;
            shared.extend_from_slice(
                format!("{x} {y} {} {} re f ", index % 37 + 3, index % 23 + 5).as_bytes(),
            );
            distinct.extend_from_slice(
                format!(
                    "{} {} {} {} re f ",
                    x + 7,
                    y + 9,
                    index % 31 + 4,
                    index % 17 + 8
                )
                .as_bytes(),
            );
        }
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>")?;
        for (page, content) in [(3, 6), (4, 7), (5, 8)] {
            pdf.object(
                page,
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 9000 9000] /Resources << >> /Contents {content} 0 R >>"
                )
                .as_bytes(),
            )?;
        }
        let mut first = b"q 1 0 0 1 0 0 cm ".to_vec();
        first.extend_from_slice(&shared);
        first.extend_from_slice(b"Q");
        let mut second = b"q 2 0 0 2 20 30 cm ".to_vec();
        second.extend_from_slice(&shared);
        second.extend_from_slice(b"Q");
        let mut third = b"q 1 0 0 1 100 200 cm ".to_vec();
        third.extend_from_slice(&distinct);
        third.extend_from_slice(b"Q");
        pdf.stream(6, b"", &first)?;
        pdf.stream(7, b"", &second)?;
        pdf.stream(8, b"", &third)?;
        let bytes = pdf.finish(1)?;
        let mut warm = EditDocument::from_bytes(bytes.clone())?;
        let mut cold = EditDocument::from_bytes(bytes)?;
        let pages = [3, 4, 5].map(|number| ObjectHandle::Existing(ObjectId::new(number, 0)));

        let mut warm_shared = SharedRunInventory::default();
        let mut warm_lengths = VectorFlateLengthMemo::processing();
        let mut cold_lengths = VectorFlateLengthMemo::processing();
        let (warm_first, changed, reusable) = factor_processing_structural_forms(
            &mut warm,
            FlateLevel::PROCESSING,
            &pages,
            ProcessingFactorBlocks::new(),
            &mut warm_lengths,
            &mut warm_shared,
        )?;
        let (cold_first, cold_changed, _) = factor_processing_structural_forms(
            &mut cold,
            FlateLevel::PROCESSING,
            &pages,
            ProcessingFactorBlocks::new(),
            &mut cold_lengths,
            &mut SharedRunInventory::default(),
        )?;
        assert_eq!(warm_first, cold_first);
        assert_eq!(changed, cold_changed);
        assert!(warm_first.transformed_forms_created > 0);
        assert!(changed.contains(&pages[0]) && changed.contains(&pages[1]));
        assert!(!changed.contains(&pages[2]));
        assert!(reusable.contains_key(&pages[2]));
        assert!(!reusable.contains_key(&pages[0]));
        assert!(!reusable.contains_key(&pages[1]));

        let (warm_second, warm_changed, _) = factor_processing_structural_forms(
            &mut warm,
            FlateLevel::PROCESSING,
            &pages,
            reusable,
            &mut warm_lengths,
            &mut warm_shared,
        )?;
        let (cold_second, cold_changed, _) = factor_processing_structural_forms(
            &mut cold,
            FlateLevel::PROCESSING,
            &pages,
            ProcessingFactorBlocks::new(),
            &mut cold_lengths,
            &mut SharedRunInventory::default(),
        )?;
        assert_eq!(warm_second, cold_second);
        assert_eq!(warm_changed, cold_changed);
        for page in pages {
            let warm_content = page_content_if_present(&warm, page)?;
            assert!(warm_content.is_some());
            assert_eq!(warm_content, page_content_if_present(&cold, page)?);
        }
        Ok(())
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
    fn processing_factor_candidate_keeps_path_coordinate_noise() {
        let page = ObjectHandle::Existing(ObjectId::new(1, 0));
        let cache = BTreeMap::from([(
            page,
            ProcessingVectorAnalysis {
                fills: Vec::new(),
                path_blocks: Vec::new(),
                transformed_blocks: Vec::new(),
                path_coordinate_plan: Some(PathCoordinatePlan {
                    canonicalized: b"0".to_vec(),
                    coordinates_canonicalized: 1,
                }),
            },
        )]);
        assert!(processing_factor_candidate(&cache));
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
                semantic_key: semantic_fingerprint(key),
                operator_count: 32,
            }],
            path_coordinate_plan: None,
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
            assert!(close(shared.bounds.x0, standalone.bounds.x0));
            assert!(close(shared.bounds.y0, standalone.bounds.y0));
            assert!(close(shared.bounds.x1, standalone.bounds.x1));
            assert!(close(shared.bounds.y1, standalone.bounds.y1));
        }

        assert_eq!(shared_transformed.len(), standalone_transformed.len());
        for (shared, standalone) in shared_transformed.iter().zip(&standalone_transformed) {
            assert_eq!(shared.body_start, standalone.body_start);
            assert_eq!(shared.body_end, standalone.body_end);
            assert_eq!(shared.semantic_key, standalone.semantic_key);
            assert_eq!(shared.operator_count, standalone.operator_count);
            assert!(close(shared.bounds.x0, standalone.bounds.x0));
            assert!(close(shared.bounds.y0, standalone.bounds.y0));
            assert!(close(shared.bounds.x1, standalone.bounds.x1));
            assert!(close(shared.bounds.y1, standalone.bounds.y1));
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
        assert!(close(blocks[0].bounds.x0, 0.0));
        assert!(close(blocks[0].bounds.y0, 0.0));
        assert!(close(blocks[0].bounds.x1, 10.0));
        assert!(close(blocks[0].bounds.y1, 10.0));
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
    fn merges_decimal_roundoff_overlap_at_shared_edge() -> Result<()> {
        let first = merge_rect_fill_pair(
            [377.816, 0.0, 438.898, 10.0],
            [438.898, 0.0, 509.734_000_000_000_04, 10.0],
            Matrix::default(),
        )
        .ok_or_else(|| crate::Error::Invalid("first pair should merge".to_owned()))?;
        assert!(
            merge_rect_fill_pair(first, [509.734, 0.0, 563.924, 10.0], Matrix::default(),)
                .is_some()
        );
        Ok(())
    }

    #[test]
    fn merges_snap_limit_gap_despite_binary_roundoff() {
        assert!(
            merge_rect_fill_pair(
                [336.473, 0.0, 410.143, 10.0],
                [410.144, 0.0, 474.17, 10.0],
                Matrix::default(),
            )
            .is_some()
        );
    }

    #[test]
    fn does_not_merge_non_roundoff_overlap() {
        assert!(
            merge_rect_fill_pair(
                [0.0, 0.0, 500.0, 10.0],
                [499.9999, 0.0, 1000.0, 10.0],
                Matrix::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn does_not_treat_large_coordinate_overlap_as_roundoff_after_ctm() {
        let ctm = Matrix {
            a: 10_000.0,
            d: 1.0,
            ..Matrix::default()
        };
        assert!(
            merge_rect_fill_pair(
                [1_000_000.0, 0.0, 1_000_010.0, 10.0],
                [1_000_009.999_999, 0.0, 1_000_020.0, 10.0],
                ctm,
            )
            .is_none()
        );
    }

    #[test]
    fn does_not_merge_gap_beyond_snap_roundoff() {
        assert!(
            merge_rect_fill_pair(
                [0.0, 0.0, 10.0, 10.0],
                [10.00102, 0.0, 20.0, 10.0],
                Matrix::default(),
            )
            .is_none()
        );
    }

    #[test]
    fn does_not_merge_page_space_thin_member() {
        let input = b"636.43 216.22 63 15.72 re f* 699.43 216.22 4.68 15.72 re f*";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn does_not_merge_visible_gap() {
        let input = b"0 0 10 10 re f 10.01 0 5 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn does_not_merge_overlapping_rectangles() {
        let input = b"0 0 10 10 re f 9 0 5 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output.as_ref(), input);
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
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn does_not_prune_identical_antialiased_fills() {
        let input = b"0 0 10 10 re f 0 0 10 10 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn does_not_prune_forward_containment_near_distinct_enclosing_edge() {
        let input = b"0.2 2 1 1 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn does_not_prune_shared_antialiased_containment_edge() {
        let input = b"0 7 1 1 re f 0 0 20 20 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn does_not_prune_covered_fill_after_unknown_ext_gstate() {
        let input = b"/GS0 gs 0 0 1 1 re f 0 0 10 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output.as_ref(), input);
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
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn state_change_breaks_merge() {
        let input = b"0 0 10 10 re f 1 0 0 rg 10 0 5 10 re f";
        let (output, stats) = compact_content(input);
        assert_eq!(stats.fill_groups_batched, 0);
        assert_eq!(output.as_ref(), input);
    }
    #[test]
    fn resolved_opaque_ext_gstate_allows_covered_fill_pruning() {
        let mut ext_gstates = BTreeMap::new();
        ext_gstates.insert(
            b"GS0".to_vec(),
            ExtGStatePatch {
                fill_alpha_opaque: Some(true),
                normal_blend: Some(true),
                no_soft_mask: Some(true),
                fill_overprint_disabled: Some(true),
            },
        );
        let input = b"/GS0 gs 7 7 2 2 re f 0 0 20 20 re f";
        let (output, stats) = compact_content_with_ext_gstates(input, &ext_gstates);
        assert_eq!(stats.covered_fills_pruned, 1);
        assert!(!String::from_utf8_lossy(&output).contains("0 0 2 2 re"));
    }

    #[test]
    fn ext_gstate_partial_update_does_not_reset_unsafe_alpha() {
        let mut ext_gstates = BTreeMap::new();
        ext_gstates.insert(
            b"Half".to_vec(),
            ExtGStatePatch {
                fill_alpha_opaque: Some(false),
                ..Default::default()
            },
        );
        ext_gstates.insert(
            b"Normal".to_vec(),
            ExtGStatePatch {
                normal_blend: Some(true),
                ..Default::default()
            },
        );
        let input = b"/Half gs /Normal gs 0 0 2 2 re f 0 0 10 10 re f";
        let (output, stats) = compact_content_with_ext_gstates(input, &ext_gstates);
        assert_eq!(stats.covered_fills_pruned, 0);
        assert_eq!(output.as_ref(), input);
    }

    #[test]
    fn shared_q_page_length_gate_matches_run_floor() {
        assert!(!shared_q_page_can_reach_minimum(
            MIN_SHARED_RUN_DECODED_BYTES - 1
        ));
        assert!(shared_q_page_can_reach_minimum(
            MIN_SHARED_RUN_DECODED_BYTES
        ));
    }

    #[test]
    fn shared_q_block_digest_uses_resource_identity() {
        let color = ObjectHandle::Existing(ObjectId::new(3, 0));
        let state = ObjectHandle::Existing(ObjectId::new(4, 0));
        let image = ObjectHandle::Existing(ObjectId::new(5, 0));
        let first_targets = SharedResourceTargets {
            color_spaces: BTreeMap::from([(b"CSA".to_vec(), Some(color))]),
            ext_gstates: BTreeMap::from([(b"GSA".to_vec(), Some(state))]),
            xobjects: BTreeMap::from([(b"ImA".to_vec(), Some(image))]),
        };
        let second_targets = SharedResourceTargets {
            color_spaces: BTreeMap::from([(b"OtherCS".to_vec(), Some(color))]),
            ext_gstates: BTreeMap::from([(b"OtherGS".to_vec(), Some(state))]),
            xobjects: BTreeMap::from([(b"OtherImage".to_vec(), Some(image))]),
        };
        let first =
            b"q 0 0 m 10 0 l 10 10 l h W* n /CSA cs 1 1 1 scn /GSA gs q 1 0 0 1 2 3 cm /ImA Do Q Q";
        let second = b"q 0 0 m 10 0 l 10 10 l h W* n /OtherCS cs 1 1 1 scn /OtherGS gs q 1 0 0 1 2 3 cm /OtherImage Do Q Q";
        let first_blocks = factorable_shared_q_blocks(first, &first_targets);
        let second_blocks = factorable_shared_q_blocks(second, &second_targets);
        assert_eq!(first_blocks.len(), 1);
        assert_eq!(second_blocks.len(), 1);
        assert_eq!(first_blocks[0].digest, second_blocks[0].digest);

        let canonical = canonicalized_shared_span(
            first,
            first_blocks[0].start,
            first_blocks[0].end,
            &first_blocks[0].resource_uses,
        );
        assert!(canonical.is_some());
        let canonical = canonical.unwrap_or_default();
        assert_eq!(
            first_blocks[0].digest,
            <[u8; 32]>::from(Sha256::digest(&canonical))
        );
        assert!(
            canonical
                .windows(b"PdfRedoxSharedCSO3G0".len())
                .any(|window| { window == b"PdfRedoxSharedCSO3G0" })
        );
        assert!(
            canonical
                .windows(b"PdfRedoxSharedGSO4G0".len())
                .any(|window| { window == b"PdfRedoxSharedGSO4G0" })
        );
        assert!(
            canonical
                .windows(b"PdfRedoxSharedXOO5G0".len())
                .any(|window| { window == b"PdfRedoxSharedXOO5G0" })
        );
    }

    #[test]
    fn shared_run_inventory_reuses_only_unchanged_pages() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R 5 0 R] /Count 3 >>")?;
        for (page, stream) in [(3, 6), (4, 7), (5, 8)] {
            pdf.object(page, format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /Occupied 9 0 R >> >> /Contents {stream} 0 R >>").as_bytes())?;
        }
        let body = b"q 0 0 m 1 1 l S Q ".repeat(4000);
        pdf.stream(6, b"", &body)?;
        pdf.stream(7, b"", &body)?;
        pdf.stream(8, b"", b"q Q")?;
        pdf.stream(9, b"/Type /XObject /Subtype /Form /BBox [0 0 1 1]", b"q Q")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = document.page_handles()?;
        let mut inventory = SharedRunInventory::default();
        let first = inventory.scan(&document, &pages)?;
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].page, pages[0]);
        assert_eq!(first[1].page, pages[1]);
        assert_eq!(first[0].occupied_names, [b"Occupied".to_vec()]);
        let unchanged_payload = std::sync::Arc::clone(&first[1].decoded);
        let unchanged_blocks = first[1].blocks.as_ptr();
        assert!(
            inventory
                .entries
                .get(&pages[2])
                .is_some_and(Option::is_none)
        );
        replace_page_content(&mut document, pages[0], b"q Q".to_vec())?;
        replace_page_content(&mut document, pages[2], body)?;
        inventory.invalidate(&BTreeSet::from([pages[0], pages[2]]));
        let next = inventory.scan(&document, &pages)?;
        assert_eq!(next.len(), 2);
        assert_eq!(next[0].page, pages[1]);
        assert_eq!(next[1].page, pages[2]);
        assert!(std::sync::Arc::ptr_eq(&unchanged_payload, &next[0].decoded));
        assert_eq!(unchanged_blocks, next[0].blocks.as_ptr());
        Ok(())
    }

    #[test]
    fn shared_run_collision_proof_checks_exact_bytes_and_resource_identity() -> Result<()> {
        let targets = SharedResourceTargets::default();
        let first = b"q 0 0 m 1 1 l S Q";
        let second = b"q 0 0 m 2 2 l S Q";
        let make_page = |number, input: &[u8]| SharedPageData {
            page: ObjectHandle::Existing(ObjectId::new(number, 0)),
            decoded: input.to_vec().into(),
            blocks: factorable_shared_q_blocks(input, &targets),
            occupied_names: Vec::new(),
            bounds: Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
            },
        };
        let first = make_page(1, first);
        let mut second = make_page(2, second);
        second.blocks[0].digest = first.blocks[0].digest;
        let candidate = SharedRunCandidate {
            page_indices: vec![0, 1],
            start_block: 0,
            end_block: 1,
            bounds: first.bounds,
        };
        let (body, resources) = shared_run_body(&first, &candidate)
            .ok_or_else(|| crate::Error::Invalid("shared run fixture".into()))?;
        assert!(shared_run_matches(&first, &candidate, &body, &resources));
        assert!(!shared_run_matches(&second, &candidate, &body, &resources));
        Ok(())
    }

    #[test]
    fn shared_run_traversal_handles_long_nontrivia_gap_chains() {
        let input = b"q 0 0 m 1 1 l S Q 0 0 m n ".repeat(20_000);
        let targets = SharedResourceTargets::default();
        let bounds = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let pages = [1, 2].map(|number| SharedPageData {
            page: ObjectHandle::Existing(ObjectId::new(number, 0)),
            blocks: factorable_shared_q_blocks(&input, &targets),
            decoded: input.clone().into(),
            bounds,
            occupied_names: Vec::new(),
        });
        assert_eq!(pages[0].blocks.len(), 20_000);
        let candidates = collect_shared_run_candidates(&pages.iter().collect::<Vec<_>>());
        assert_eq!(candidates.len(), 20_000);
        for (i, candidate) in candidates.iter().enumerate() {
            assert_eq!(candidate.page_indices, [0, 1]);
            assert_eq!(candidate.start_block, i);
            assert_eq!(candidate.end_block, i + 1);
        }
    }

    #[test]
    fn shared_q_adjacency_and_invalid_scope_recovery() {
        let data = b"q 0 0 m 1 1 l S Q % trivia\n q 0 0 m 1 1 l S Q q BT /F1 10 Tf q /Missing Do Q ET Q q 0 0 m 1 1 l S Q";
        let blocks = factorable_shared_q_blocks(data, &SharedResourceTargets::default());
        assert_eq!(blocks.len(), 3);
        assert!(!blocks[0].contiguous_from_previous);
        assert!(blocks[1].contiguous_from_previous);
        assert!(!blocks[2].contiguous_from_previous);
        assert_eq!(blocks[0].digest, blocks[2].digest);
    }

    #[test]
    fn shared_resource_projection_reserves_direct_names_without_accepting_them() -> Result<()> {
        let mut pdf = crate::test_support::ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2,b"<< /Type /Pages /Kids [3 0 R] /Count 1 /Resources << /XObject << /Direct << /A 1 >> /Ref 4 0 R >> /ExtGState << /GS 5 0 R >> >> >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] >>")?;
        pdf.object(4, b"<< /Type /XObject /Subtype /Form >>")?;
        pdf.object(5, b"<< /Type /ExtGState >>")?;
        let document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let page = ObjectHandle::Existing(ObjectId::new(3, 0));
        let projected = shared_resource_targets(&document, page)?;
        assert!(projected.xobjects.contains_key(b"Direct".as_slice()));
        assert_eq!(
            projected.target(SharedResourceKind::XObject, b"Direct"),
            None
        );
        assert_eq!(
            projected.target(SharedResourceKind::XObject, b"Ref"),
            Some(ObjectHandle::Existing(ObjectId::new(4, 0)))
        );
        assert_eq!(
            projected.target(SharedResourceKind::ExtGState, b"GS"),
            Some(ObjectHandle::Existing(ObjectId::new(5, 0)))
        );
        Ok(())
    }

    #[test]
    fn shared_q_block_rejects_unsupported_font_semantics() {
        let targets = SharedResourceTargets::default();
        let input = b"q BT /F1 12 Tf (x) Tj ET Q";
        assert!(factorable_shared_q_blocks(input, &targets).is_empty());
    }

    #[test]
    fn shared_q_block_gap_accepts_only_pdf_trivia() {
        assert!(trivia_only(b" \n% comment\r\n\t"));
        assert!(!trivia_only(b" 0 0 m "));
    }

    #[test]
    fn path_coordinate_canonicalization_can_expose_exact_fill_adjacency() -> crate::Result<()> {
        let input = b"0 0 10.004 10 re f 10.003 0 9.997 10 re f";
        let (before, before_stats) = compact_content(input);
        assert_eq!(before.as_ref(), input);
        assert_eq!(before_stats.fill_groups_batched, 0);

        let (canonicalized, coordinates) =
            canonicalize_path_coordinates(input, 1.0).ok_or_else(|| {
                crate::Error::Invalid("expected path-coordinate canonicalization".to_owned())
            })?;
        assert!(coordinates > 0);

        let (after, after_stats) = compact_content(&canonicalized);
        assert_eq!(after_stats.fill_groups_batched, 1);
        assert_eq!(after_stats.fill_paints_eliminated, 1);
        assert!(String::from_utf8_lossy(&after).contains("0 0 20 10 re f"));
        Ok(())
    }

    #[test]
    fn owned_span_replacements_preserve_source_order_and_reject_overlaps() {
        // Owned small buffers and the longstanding borrowed Vec path must
        // produce byte-identical content, including out-of-order spans.
        let input = b"ABCDEF0123456789";
        let borrowed = vec![
            (6, 10, b"xy".to_vec()),
            (1, 3, b"z".to_vec()),
            (14, 16, b"Q".to_vec()),
        ];
        let owned = borrowed
            .iter()
            .map(|(start, end, bytes)| (*start, *end, SmallVec::<[u8; 32]>::from_slice(bytes)))
            .collect::<Vec<_>>();
        let borrowed_output = crate::content_stream::apply_replacements(input, &borrowed);
        let owned_output = crate::content_stream::apply_replacements(input, &owned);
        assert_eq!(borrowed_output, owned_output);
        assert_eq!(owned_output.as_deref(), Some(b"AzDEFxy4567Q".as_slice()));

        let overlapping = vec![
            (1, 4, SmallVec::<[u8; 32]>::from_slice(b"x")),
            (3, 5, SmallVec::<[u8; 32]>::from_slice(b"y")),
        ];
        assert!(crate::content_stream::apply_replacements(input, &overlapping).is_none());
        let invalid_end = vec![(14, 17, SmallVec::<[u8; 32]>::from_slice(b"x"))];
        assert!(crate::content_stream::apply_replacements(input, &invalid_end).is_none());
        // Coordinate assembler references a single contiguous arena instead
        // of owning separate payloads or cloning them at page finalization.
        let arena = b"xyzQ".to_vec();
        let indexed = vec![
            CoordinateReplacement {
                start: 14,
                end: 16,
                byte_start: 3,
                byte_length: 1,
            },
            CoordinateReplacement {
                start: 1,
                end: 3,
                byte_start: 2,
                byte_length: 1,
            },
            CoordinateReplacement {
                start: 6,
                end: 10,
                byte_start: 0,
                byte_length: 2,
            },
        ];
        assert_eq!(
            apply_span_replacements_from_arena(input, indexed, &arena),
            borrowed_output
        );
        let invalid_index = vec![CoordinateReplacement {
            start: 1,
            end: 3,
            byte_start: usize::MAX,
            byte_length: 2,
        }];
        assert!(apply_span_replacements_from_arena(input, invalid_index, &arena).is_none());
        let overlapping = vec![
            CoordinateReplacement {
                start: 1,
                end: 4,
                byte_start: 0,
                byte_length: 1,
            },
            CoordinateReplacement {
                start: 2,
                end: 5,
                byte_start: 1,
                byte_length: 1,
            },
        ];
        assert!(apply_span_replacements_from_arena(input, overlapping, &arena).is_none());

        // Extremely long replacements remain supported by SmallVec's heap
        // storage, without truncating the f64 formatter's unusual magnitudes.
        let long = vec![b'5'; 512];
        let owned = vec![(4, 6, SmallVec::<[u8; 32]>::from_slice(&long))];
        let result = crate::content_stream::apply_replacements(input, &owned);
        assert_eq!(
            result.as_ref().map(Vec::len),
            Some(input.len() - 2 + long.len())
        );
    }

    #[test]
    fn decimal_coordinate_encoder_matches_original_float_formatter() {
        // This is deliberately independent of the integer-cent encoder.
        // It also checks values outside its exact representability domain.
        let reference = |value| {
            let rounded = PathCoordinateScanner::rounded_coordinate(value)?;
            let mut text = format!("{rounded:.2}");
            while text.contains('.') && text.ends_with('0') {
                text.pop();
            }
            if text.ends_with('.') {
                text.pop();
            }
            if text == "-0" {
                text = "0".to_owned();
            }
            Some(text.into_bytes())
        };
        let edges = [
            0.0,
            -0.0,
            f64::EPSILON,
            -f64::EPSILON,
            f64::MIN_POSITIVE,
            f64::from_bits(1),
            -f64::from_bits(1),
            f64::MIN,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            0.005,
            -0.005,
            0.015,
            -0.015,
            0.105,
            -0.105,
            0.995,
            -0.995,
            1.005,
            -1.005,
            10.235,
            -10.235,
            999.995,
            -999.995,
            1.0e6,
            -1.0e6,
            1.0e12,
            -1.0e12,
            1.0e16,
            -1.0e16,
        ];
        for value in edges {
            assert_eq!(
                PathCoordinateScanner::rounded_coordinate_bytes(value).map(|b| b.to_vec()),
                reference(value),
                "edge {value:?}"
            );
        }

        // Neighboring floats around decimal ties and cent boundaries.
        for cents in -1000_i64..=1000 {
            for offset in [-0.005, 0.0, 0.005] {
                let value = cents.to_f64().unwrap_or_default() / PATH_COORDINATE_SCALE + offset;
                for bits in [
                    value.to_bits().wrapping_sub(1),
                    value.to_bits(),
                    value.to_bits().wrapping_add(1),
                ] {
                    let sample = f64::from_bits(bits);
                    assert_eq!(
                        PathCoordinateScanner::rounded_coordinate_bytes(sample).map(|b| b.to_vec()),
                        reference(sample),
                        "boundary {sample:?}"
                    );
                }
            }
        }
        // Deterministic full-range f64-bit sampling includes subnormals,
        // large exponents, negatives, and values near the fast-path boundary.
        let mut state = 0xD1B5_4A32_D192_ED03_u64;
        for _ in 0..180_000 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let sample = f64::from_bits(state);
            assert_eq!(
                PathCoordinateScanner::rounded_coordinate_bytes(sample).map(|b| b.to_vec()),
                reference(sample),
                "f64 bits 0x{state:016x}"
            );
        }
    }

    #[test]
    fn path_coordinate_canonicalization_rounds_only_path_operands() {
        let input = b"0.123456 rg 255.96000671 518.76000977 m 254.16000366 518.76000977 l S";
        let result = canonicalize_path_coordinates(input, 1.0);
        assert!(result.is_some(), "expected path coordinate rewrite");
        let Some((output, count)) = result else {
            return;
        };
        let output = String::from_utf8_lossy(&output);
        assert_eq!(count, 4);
        assert!(output.contains("0.123456 rg"));
        assert!(output.contains("255.96 518.76 m"));
        assert!(output.contains("254.16 518.76 l"));
    }

    #[test]
    fn shared_path_coordinate_plan_matches_standalone_scanner() {
        let input = b"q 100 0 0 100 0 0 cm 1.004 2 m S Q 5.003 6.003 m 7.004 8.004 l S";
        let standalone = canonicalize_path_coordinates(input, 1.0);
        assert!(standalone.is_some());

        let mut scanner = ProcessingPageScanner::new(BTreeMap::new(), Some(1.0));
        assert!(matches!(scanner.scan(input), Ok(false)));
        let analysis = scanner.finish(input);
        assert!(analysis.path_coordinate_plan.is_some());
        let Some(plan) = analysis.path_coordinate_plan else {
            return;
        };
        let cached = Some((plan.canonicalized, plan.coordinates_canonicalized));

        assert_eq!(cached, standalone);
    }

    #[test]
    fn path_coordinate_canonicalization_skips_unrelated_operators_without_losing_state()
    -> crate::Result<()> {
        let input = b"1 0 0 rg q 99 w 100 0 0 100 0 0 cm 1.004 2 m S Q BT /F1 12 Tf (keep 1.009) Tj ET 5.003 6.003 m S";
        let (output, count) = canonicalize_path_coordinates(input, 1.0)
            .ok_or_else(|| crate::Error::Invalid("missing outer path rewrite".to_owned()))?;
        assert_eq!(count, 2);
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("1 0 0 rg q 99 w"));
        assert!(output.contains("1.004 2 m S Q"));
        assert!(output.contains("BT /F1 12 Tf (keep 1.009) Tj ET"));
        assert!(output.contains("5 6 m S"));
        Ok(())
    }

    #[test]
    fn path_coordinate_canonicalization_respects_ctm_magnification() {
        let input = b"100 0 0 100 0 0 cm 1.004 2.004 m 3.004 4.004 l S";
        assert!(canonicalize_path_coordinates(input, 1.0).is_none());
    }

    #[test]
    fn path_coordinate_canonicalization_checks_rectangle_far_corner() {
        let input = b"1.004 2 3.004 4 re f";
        assert!(canonicalize_path_coordinates(input, 1.0).is_none());
    }

    #[test]
    fn path_coordinate_canonicalization_restores_ctm_across_q() {
        let input = b"q 100 0 0 100 0 0 cm 1.004 2 m S Q 5.003 6.003 m S";
        let result = canonicalize_path_coordinates(input, 1.0);
        assert!(result.is_some(), "expected outer path rewrite");
        let Some((output, count)) = result else {
            return;
        };
        let output = String::from_utf8_lossy(&output);
        assert_eq!(count, 2);
        assert!(output.contains("1.004 2 m"));
        assert!(output.contains("5 6 m"));
    }
}
