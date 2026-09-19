use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, RasterLayoutConfig, Result,
    StreamData,
    content::{form_content, form_resources, page_content, page_resources, resolved_dictionary},
    hidden_text::{
        HiddenTextSharedContext, hidden_text_shared_context_hayro,
        scan_physical_hidden_text_with_callback_hayro,
    },
    inline_images::{
        ContentTarget as InlineContentTarget, FragmentedInlineExternalizationStats,
        cleanup_fragmented_inline_staging_hayro, externalize_fragmented_inline_target_hayro,
    },
    vector_compact::{
        ProcessingPageScanner, ProcessingVectorAnalysis, compacted_rect_fill_len,
        merge_rect_fill_pair, rect_contains_rect,
    },
};
use flate2::{Compression, write::ZlibEncoder};
use flpdf::content_stream::ContentScalar;
use flpdf::{Matrix, ObjectHandle as FlObjectHandle, ObjectHandleParserCallbacks, ParseControl};
use sha2::{Digest, Sha256};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    io::Write,
    sync::{Arc, LazyLock},
    time::Instant,
};

static DEBUG_RASTER: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("PDF_REDOX_DEBUG_RASTER").is_some());
static DEBUG_VECTOR: LazyLock<bool> =
    LazyLock::new(|| std::env::var_os("PDF_REDOX_DEBUG_VECTOR").is_some());

const POINTS_PER_MM: f64 = 72.0 / 25.4;
const MATRIX_EPSILON: f64 = 1.0e-7;
const BASIS_REL_EPSILON: f64 = 3.0e-3;
const ALPHA_INVISIBLE: f64 = 0.001;
const ALPHA_OPAQUE: f64 = 0.995;
const SHARED_IMAGE_CACHE_MAX_BYTES: usize = 128 * 1024 * 1024;
const SHARED_IMAGE_CACHE_MAX_ENTRIES: usize = 1024;
const RECONSTRUCTED_JPEG_QUALITY: u8 = 85;
const MIN_TOTAL_CROP_MARGIN_PIXELS: u32 = 20;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RasterLayoutStats {
    pub inline: FragmentedInlineExternalizationStats,
    pub pixel_clusters_reconstructed: usize,
    pub pixel_paints_reconstructed: usize,
    pub native_fragment_groups_reconstructed: usize,
    pub native_fragment_paints_reconstructed: usize,
    pub stripe_groups_merged: usize,
    pub stripe_paints_merged: usize,
    pub masks_baked: usize,
    pub transparent_paints_pruned: usize,
    pub occluded_raster_paints_pruned: usize,
    pub transparent_margins_cropped: usize,
    pub background_margins_cropped: usize,
    pub cropped_pixels_removed: u64,
    pub binary_images_packed: usize,
    pub binary_masks_packed: usize,
    pub stencil_images_emitted: usize,
    pub relaxed_stencil_images_emitted: usize,
    pub binary_image_encoded_bytes_saved: u64,
    pub deferred_tile_candidates: usize,
    pub deferred_tile_paints_consumed: usize,
    pub staging_xobject_entries_removed: usize,
    pub inline_occurrences_remaining: usize,
    pub inline_inventory_complete: bool,
    pub page_rect_fill_paints_seen: usize,
    pub page_count: usize,
    pub page_vector_merge_candidate: bool,
    pub page_vector_inventory_complete: bool,
    pub page_physical_hidden_text_candidate: bool,
    pub page_hidden_text_candidates: BTreeSet<ObjectHandle>,
    pub page_hidden_text_shared_complete: BTreeSet<ObjectHandle>,
    pub shared_hidden_text_paints_pruned: usize,
    pub page_hidden_text_inventory_complete: bool,
    pub resource_inventory_complete: bool,
    pub page_resource_names: BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
    pub form_resource_names: BTreeMap<ObjectHandle, BTreeSet<Vec<u8>>>,
    pub page_resource_names_by_type: BTreeMap<ObjectHandle, BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>>,
    pub form_resource_names_by_type: BTreeMap<ObjectHandle, BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>>,
    pub inline_externalize_us: u64,
    pub target_scan_us: u64,
    pub hidden_prune_us: u64,
    pub hidden_state_us: u64,
    pub hidden_visibility_us: u64,
    pub hidden_coverage_us: u64,
    pub hidden_rewrite_us: u64,
    pub image_materialize_us: u64,
    pub native_plan_us: u64,
    pub stripe_plan_us: u64,
    pub pixel_plan_us: u64,
    pub apply_plans_us: u64,
    pub staging_cleanup_us: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ContentTarget {
    Page(ObjectHandle),
    Form(ObjectHandle),
}

#[derive(Debug, Clone)]
enum ParsedOperandValue {
    Scalar(ContentScalar),
    Handle(FlObjectHandle),
}

impl ParsedOperandValue {
    fn number(&self) -> Option<f64> {
        match self {
            Self::Scalar(value) => value
                .as_integer()
                .map(|value| value as f64)
                .or_else(|| value.as_real()),
            Self::Handle(value) => parsed_number(value),
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
struct ParsedOperand {
    value: ParsedOperandValue,
    offset: usize,
}

fn parsed_operand_numbers(operands: &[ParsedOperand]) -> Option<SmallVec<[f64; 6]>> {
    let mut values = SmallVec::new();
    for operand in operands {
        values.push(operand.value.number()?);
    }
    Some(values)
}

#[derive(Debug, Clone)]
struct RasterDraw {
    target: ObjectHandle,
    resource_name: Vec<u8>,
    ctm: Matrix,
    replace_ctm: Matrix,
    range_start: usize,
    range_end: usize,
    epoch: usize,
    paint_generation: usize,
    gs_name: Option<Vec<u8>>,
    rendering_intent: Option<Vec<u8>>,
    clip: Option<Rect>,
    clip_polygon: Option<Vec<(f64, f64)>>,
    clip_complex: bool,
}

#[derive(Debug, Clone)]
struct RectFillPaint {
    rect: Rect,
    gs_name: Option<Vec<u8>>,
    clip: Option<Rect>,
    clip_polygon: Option<Vec<(f64, f64)>>,
    clip_complex: bool,
}

#[derive(Debug, Clone, Copy)]
enum CoveragePaint {
    Image(usize),
    RectFill(usize),
}

#[derive(Debug)]
struct GraphicsFrame {
    start: usize,
    base_ctm: Matrix,
    image_draws: Vec<usize>,
    other_paint: bool,
    semantic_boundary: bool,
}

type GraphicsStateSnapshot = (
    Matrix,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Rect>,
    Option<Vec<(f64, f64)>>,
    bool,
);
type NativeFragmentEntry = (usize, bool, bool, (f64, f64));
type NativeFragmentGroupKey = (usize, Option<Vec<u8>>, Option<Vec<u8>>, [u8; 32], u8);
type PixelGroupKey = (usize, Option<Vec<u8>>, Option<Vec<u8>>);

#[derive(Debug, Clone, Copy)]
struct ImagePaintState {
    alpha: f64,
    normal_blend: bool,
}

impl Default for ImagePaintState {
    fn default() -> Self {
        Self {
            alpha: 1.0,
            normal_blend: true,
        }
    }
}

#[derive(Debug, Clone)]
struct VectorFillRun {
    epoch: usize,
    operator: Vec<u8>,
    rect: [f64; 4],
    ctm: Matrix,
    start: usize,
    end: usize,
}

#[derive(Debug)]
struct ResourceUsage {
    names: BTreeSet<Vec<u8>>,
    by_type: BTreeMap<Vec<u8>, BTreeSet<Vec<u8>>>,
}

#[derive(Debug)]
struct RasterScanner {
    xobjects: BTreeMap<Vec<u8>, ObjectHandle>,
    image_names: HashSet<Vec<u8>>,
    ctm: Matrix,
    stack: Vec<GraphicsStateSnapshot>,
    frames: Vec<GraphicsFrame>,
    operands: Vec<ParsedOperand>,
    draws: Vec<RasterDraw>,
    rect_fills: Vec<RectFillPaint>,
    coverage_paints: Vec<CoveragePaint>,
    epoch: usize,
    paint_generation: usize,
    gs_name: Option<Vec<u8>>,
    rendering_intent: Option<Vec<u8>>,
    clip: Option<Rect>,
    clip_polygon: Option<Vec<(f64, f64)>>,
    clip_complex: bool,
    path_rect: Option<Rect>,
    path_points: Option<SmallVec<[(f64, f64); 5]>>,
    path_closed: bool,
    path_complex: bool,
    clip_pending: bool,
    complete: bool,
    inline_occurrences: usize,
    text_paints: usize,
    text_seen: bool,
    text_with_ext_gstate: bool,
    covering_paint_after_text: bool,
    vector_rect: Option<[f64; 4]>,
    vector_path_start: Option<usize>,
    vector_path_is_single_rect: bool,
    vector_epoch: usize,
    vector_run: Option<VectorFillRun>,
    vector_merge_candidate: bool,
    resource_counts: BTreeMap<Vec<u8>, usize>,
    resource_counts_by_type: BTreeMap<Vec<u8>, BTreeMap<Vec<u8>, usize>>,
    resource_last_name: Option<Vec<u8>>,
    resource_pending_operands: bool,
    analysis_only_no_images: bool,
    analysis_q_depth: usize,
}

impl RasterScanner {
    fn new(xobjects: BTreeMap<Vec<u8>, ObjectHandle>, image_names: HashSet<Vec<u8>>) -> Self {
        Self {
            xobjects,
            image_names,
            ctm: Matrix::default(),
            stack: Vec::new(),
            frames: Vec::new(),
            operands: Vec::new(),
            draws: Vec::new(),
            rect_fills: Vec::new(),
            coverage_paints: Vec::new(),
            epoch: 0,
            paint_generation: 0,
            gs_name: None,
            rendering_intent: None,
            clip: None,
            clip_polygon: None,
            clip_complex: false,
            path_rect: None,
            path_points: None,
            path_closed: false,
            path_complex: false,
            clip_pending: false,
            complete: true,
            inline_occurrences: 0,
            text_paints: 0,
            text_seen: false,
            text_with_ext_gstate: false,
            covering_paint_after_text: false,
            vector_rect: None,
            vector_path_start: None,
            vector_path_is_single_rect: true,
            vector_epoch: 0,
            vector_run: None,
            vector_merge_candidate: false,
            resource_counts: BTreeMap::new(),
            resource_counts_by_type: BTreeMap::new(),
            resource_last_name: None,
            resource_pending_operands: false,
            analysis_only_no_images: false,
            analysis_q_depth: 0,
        }
    }

    fn resource_type_for_operator(operator: &[u8]) -> Option<&'static [u8]> {
        match operator {
            b"CS" | b"cs" => Some(b"ColorSpace"),
            b"gs" => Some(b"ExtGState"),
            b"Tf" => Some(b"Font"),
            b"SCN" | b"scn" => Some(b"Pattern"),
            b"BDC" | b"DP" => Some(b"Properties"),
            b"sh" => Some(b"Shading"),
            b"Do" => Some(b"XObject"),
            _ => None,
        }
    }

    fn record_resource_operator(&mut self, operator: &[u8]) {
        self.resource_pending_operands = false;
        let name = self.resource_last_name.take();
        if let Some(resource_type) = Self::resource_type_for_operator(operator)
            && let Some(name) = name
        {
            *self.resource_counts.entry(name.clone()).or_default() += 1;
            *self
                .resource_counts_by_type
                .entry(resource_type.to_vec())
                .or_default()
                .entry(name)
                .or_default() += 1;
        }
    }

    fn resource_usage_after_removing(&self, removed: &HashSet<usize>) -> ResourceUsage {
        let mut removed_xobjects = HashMap::<&[u8], usize>::new();
        for &index in removed {
            if let Some(draw) = self.draws.get(index) {
                *removed_xobjects
                    .entry(draw.resource_name.as_slice())
                    .or_default() += 1;
            }
        }

        let names = self
            .resource_counts
            .iter()
            .filter_map(|(name, &count)| {
                let removed = removed_xobjects
                    .get(name.as_slice())
                    .copied()
                    .unwrap_or_default();
                (count > removed).then(|| name.clone())
            })
            .collect();

        let names_by_type = self
            .resource_counts_by_type
            .iter()
            .filter_map(|(resource_type, names)| {
                let is_xobject = resource_type.as_slice() == b"XObject";
                let names = names
                    .iter()
                    .filter_map(|(name, &count)| {
                        let removed = if is_xobject {
                            removed_xobjects
                                .get(name.as_slice())
                                .copied()
                                .unwrap_or_default()
                        } else {
                            0
                        };
                        (count > removed).then(|| name.clone())
                    })
                    .collect::<BTreeSet<_>>();
                (!names.is_empty()).then(|| (resource_type.clone(), names))
            })
            .collect();

        ResourceUsage {
            names,
            by_type: names_by_type,
        }
    }

    fn vector_barrier(&mut self) {
        self.vector_rect = None;
        self.vector_path_start = None;
        self.vector_path_is_single_rect = true;
        self.vector_epoch = self.vector_epoch.saturating_add(1);
    }

    fn vector_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        match operator {
            b"re" => {
                if !self.vector_path_is_single_rect
                    || self.vector_rect.is_some()
                    || self.operands.len() != 4
                {
                    self.vector_path_is_single_rect = false;
                    return;
                }
                let Some(values) = parsed_operand_numbers(&self.operands) else {
                    self.vector_path_is_single_rect = false;
                    return;
                };
                let [x, y, width, height] = [values[0], values[1], values[2], values[3]];
                if ![x, y, width, height].into_iter().all(f64::is_finite)
                    || width.abs() <= f64::EPSILON
                    || height.abs() <= f64::EPSILON
                {
                    self.vector_path_is_single_rect = false;
                    return;
                }
                let x1 = x + width;
                let y1 = y + height;
                self.vector_rect = Some([x.min(x1), y.min(y1), x.max(x1), y.max(y1)]);
                self.vector_path_start = self.operands.first().map(|operand| operand.offset);
            }
            b"f" | b"F" | b"f*" => {
                let current = if self.operands.is_empty() && self.vector_path_is_single_rect {
                    self.vector_rect
                        .zip(self.vector_path_start)
                        .map(|(rect, start)| VectorFillRun {
                            epoch: self.vector_epoch,
                            operator: operator.to_vec(),
                            rect,
                            ctm: self.ctm,
                            start,
                            end: offset.saturating_add(length),
                        })
                } else {
                    None
                };
                if let Some(current) = current {
                    // This is only a skip hint for the later vector pass, so false
                    // positives are harmless. Flag geometric containment even when
                    // the current ExtGState may ultimately make pruning unsafe; the
                    // resource-aware vector scanner performs the destructive proof.
                    if let Some(run) = &self.vector_run
                        && run.epoch == current.epoch
                        && run.operator == current.operator
                        && (rect_contains_rect(current.rect, run.rect)
                            || rect_contains_rect(run.rect, current.rect))
                    {
                        self.vector_merge_candidate = true;
                    }
                    let mut extended = false;
                    if let Some(run) = &mut self.vector_run
                        && run.epoch == current.epoch
                        && run.operator == current.operator
                        && let Some(merged) = merge_rect_fill_pair(run.rect, current.rect, run.ctm)
                    {
                        run.rect = merged;
                        run.end = current.end;
                        if compacted_rect_fill_len(run.rect, &run.operator)
                            < run.end.saturating_sub(run.start)
                        {
                            self.vector_merge_candidate = true;
                        }
                        extended = true;
                    }
                    if !extended {
                        self.vector_run = Some(current);
                    }
                } else {
                    // Match FillScanner: an unsupported path still paints and
                    // must split runs rather than disappearing from the model.
                    self.vector_epoch = self.vector_epoch.saturating_add(1);
                    self.vector_run = None;
                }
                self.vector_rect = None;
                self.vector_path_start = None;
                self.vector_path_is_single_rect = true;
            }
            _ => self.vector_barrier(),
        }
    }

    fn barrier(&mut self) {
        self.epoch = self.epoch.saturating_add(1);
    }

    fn mark_other_paint(&mut self) {
        self.paint_generation = self.paint_generation.saturating_add(1);
        for frame in &mut self.frames {
            frame.other_paint = true;
        }
    }

    fn mark_semantic_boundary(&mut self) {
        for frame in &mut self.frames {
            frame.semantic_boundary = true;
        }
    }

    fn clear_path(&mut self) {
        self.path_rect = None;
        self.path_points = None;
        self.path_closed = false;
        self.path_complex = false;
        self.clip_pending = false;
    }

    fn path_as_axis_aligned_rect(&self) -> Option<Rect> {
        if let Some(rect) = self.path_rect {
            return Some(rect);
        }
        if self.path_complex {
            return None;
        }
        let points = self.path_points.as_ref()?;
        if points.len() < 4 {
            return None;
        }
        let mut vertices = points.as_slice();
        if vertices.len() >= 2 {
            let first = vertices[0];
            let last = *vertices.last()?;
            if (first.0 - last.0).abs() <= MATRIX_EPSILON
                && (first.1 - last.1).abs() <= MATRIX_EPSILON
            {
                vertices = &vertices[..vertices.len() - 1];
            }
        }
        if vertices.len() < 4 {
            return None;
        }
        let x0 = vertices.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
        let y0 = vertices.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let x1 = vertices
            .iter()
            .map(|p| p.0)
            .fold(f64::NEG_INFINITY, f64::max);
        let y1 = vertices
            .iter()
            .map(|p| p.1)
            .fold(f64::NEG_INFINITY, f64::max);
        if x1 - x0 <= MATRIX_EPSILON || y1 - y0 <= MATRIX_EPSILON {
            return None;
        }
        let on = |value: f64, edge: f64| (value - edge).abs() <= MATRIX_EPSILON;
        let corners = [(x0, y0), (x0, y1), (x1, y0), (x1, y1)];
        if !corners
            .iter()
            .all(|&(cx, cy)| vertices.iter().any(|&(x, y)| on(x, cx) && on(y, cy)))
        {
            return None;
        }
        for index in 0..vertices.len() {
            let a = vertices[index];
            let b = vertices[(index + 1) % vertices.len()];
            let horizontal = on(a.1, b.1) && (on(a.1, y0) || on(a.1, y1));
            let vertical = on(a.0, b.0) && (on(a.0, x0) || on(a.0, x1));
            if !horizontal && !vertical {
                return None;
            }
        }
        Some(Rect { x0, y0, x1, y1 })
    }

    fn begin_line_path(&mut self, x: f64, y: f64) {
        if self.path_rect.is_some() || self.path_points.is_some() {
            self.path_complex = true;
            self.path_rect = None;
            self.path_points = None;
            return;
        }
        let mut points = SmallVec::new();
        points.push(self.ctm.transform(x, y));
        self.path_points = Some(points);
        self.path_closed = false;
    }

    fn add_line_point(&mut self, x: f64, y: f64) {
        if self.path_complex || self.path_rect.is_some() {
            self.path_complex = true;
            self.path_points = None;
            return;
        }
        let Some(points) = self.path_points.as_mut() else {
            self.path_complex = true;
            return;
        };
        points.push(self.ctm.transform(x, y));
        if points.len() > 5 {
            self.path_complex = true;
            self.path_points = None;
        }
    }

    fn apply_pending_clip(&mut self) {
        if self.clip_pending {
            if let Some(path) = self.path_as_axis_aligned_rect() {
                if self.clip_polygon.is_some() {
                    self.clip_complex = true;
                } else {
                    self.clip = match self.clip {
                        Some(current) => current.intersection(path),
                        None => Some(path),
                    };
                }
            } else if !self.path_complex
                && self.clip.is_none()
                && self.clip_polygon.is_none()
                && let Some(points) = normalized_polygon(self.path_points.as_deref())
            {
                self.clip_polygon = Some(points);
            } else {
                self.clip_complex = true;
            }
        }
        self.clear_path();
    }

    fn add_rect_path(&mut self, x: f64, y: f64, width: f64, height: f64) {
        if self.ctm.b.abs() > MATRIX_EPSILON || self.ctm.c.abs() > MATRIX_EPSILON {
            self.path_complex = true;
            self.path_rect = None;
            return;
        }
        let points = [
            self.ctm.transform(x, y),
            self.ctm.transform(x + width, y),
            self.ctm.transform(x, y + height),
            self.ctm.transform(x + width, y + height),
        ];
        let rect = Rect {
            x0: points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
            y0: points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
            x1: points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
            y1: points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
        };
        if self.path_rect.is_some() || self.path_complex {
            self.path_complex = true;
            self.path_rect = None;
        } else {
            self.path_rect = Some(rect);
        }
    }

    fn analysis_only_operator(&mut self, operator: &[u8]) {
        match operator {
            b"q" => self.analysis_q_depth = self.analysis_q_depth.saturating_add(1),
            b"Q" => {
                if self.analysis_q_depth == 0 {
                    self.complete = false;
                } else {
                    self.analysis_q_depth -= 1;
                }
            }
            b"Do" => {
                if self.operands.len() != 1
                    || self.operands[0].value.name().is_none()
                    || self.operands[0]
                        .value
                        .name()
                        .is_some_and(|name| !self.xobjects.contains_key(name.as_ref()))
                {
                    self.complete = false;
                }
            }
            b"gs" => {
                if self.operands.len() == 1 {
                    self.gs_name = self.operands[0].value.name().map(Cow::into_owned);
                    if self.gs_name.is_none() {
                        self.complete = false;
                    }
                } else {
                    self.complete = false;
                }
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                self.text_paints = self.text_paints.saturating_add(1);
                self.text_seen = true;
                self.text_with_ext_gstate |= self.gs_name.is_some();
            }
            _ => {}
        }
    }

    fn operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        self.record_resource_operator(operator);
        if self.analysis_only_no_images {
            self.analysis_only_operator(operator);
            return;
        }
        self.vector_operator(operator, offset, length);
        match operator {
            b"q" => {
                self.stack.push((
                    self.ctm,
                    self.gs_name.clone(),
                    self.rendering_intent.clone(),
                    self.clip,
                    self.clip_polygon.clone(),
                    self.clip_complex,
                ));
                self.frames.push(GraphicsFrame {
                    start: offset,
                    base_ctm: self.ctm,
                    image_draws: Vec::new(),
                    other_paint: false,
                    semantic_boundary: false,
                });
            }
            b"Q" => {
                if let Some(frame) = self.frames.pop()
                    && !frame.other_paint
                    && !frame.semantic_boundary
                    && frame.image_draws.len() == 1
                    && let Some(draw) = self.draws.get_mut(frame.image_draws[0])
                {
                    draw.range_start = frame.start;
                    draw.range_end = offset.saturating_add(length);
                    draw.replace_ctm = frame.base_ctm;
                }
                if let Some((ctm, gs_name, rendering_intent, clip, clip_polygon, clip_complex)) =
                    self.stack.pop()
                {
                    self.ctm = ctm;
                    self.gs_name = gs_name;
                    self.rendering_intent = rendering_intent;
                    self.clip = clip;
                    self.clip_polygon = clip_polygon;
                    self.clip_complex = clip_complex;
                } else {
                    self.complete = false;
                }
            }
            b"cm" => {
                if self.operands.len() == 6 {
                    let values = parsed_operand_numbers(&self.operands);
                    if let Some(values) = values {
                        self.ctm.concat(Matrix::new(
                            values[0], values[1], values[2], values[3], values[4], values[5],
                        ));
                    } else {
                        self.complete = false;
                    }
                } else {
                    self.complete = false;
                }
            }
            b"Do" => {
                if self.operands.len() != 1 {
                    self.complete = false;
                    self.barrier();
                    return;
                }
                let Some(name) = self.operands[0].value.name() else {
                    self.complete = false;
                    self.barrier();
                    return;
                };
                let Some(target) = self.xobjects.get(name.as_ref()).copied() else {
                    self.complete = false;
                    self.barrier();
                    return;
                };
                if self.image_names.contains(name.as_ref()) {
                    let draw_index = self.draws.len();
                    self.draws.push(RasterDraw {
                        target,
                        resource_name: name.into_owned(),
                        ctm: self.ctm,
                        replace_ctm: self.ctm,
                        range_start: self.operands[0].offset,
                        range_end: offset.saturating_add(length),
                        epoch: self.epoch,
                        paint_generation: self.paint_generation,
                        gs_name: self.gs_name.clone(),
                        rendering_intent: self.rendering_intent.clone(),
                        clip: self.clip,
                        clip_polygon: self.clip_polygon.clone(),
                        clip_complex: self.clip_complex,
                    });
                    if self.text_seen {
                        self.covering_paint_after_text = true;
                    }
                    self.coverage_paints.push(CoveragePaint::Image(draw_index));
                    for frame in &mut self.frames {
                        frame.image_draws.push(draw_index);
                    }
                } else {
                    // A Form/unknown XObject is a real paint in this stacking order.
                    self.mark_other_paint();
                    self.barrier();
                }
            }
            // Only state/visibility operations that can change how subsequent image paints
            // themselves render split a raster-layout epoch. Ordinary text/vector painting is
            // deliberately *not* a barrier: old raster fragments are often interleaved with
            // freshly-added searchable text/vector overlays. Z-order safety is handled when a
            // merge plan is applied, not by requiring operator adjacency.
            b"gs" => {
                if self.operands.len() == 1 {
                    self.gs_name = self.operands[0].value.name().map(Cow::into_owned);
                    if self.gs_name.is_none() {
                        self.complete = false;
                    }
                } else {
                    self.complete = false;
                }
            }
            b"ri" => {
                if self.operands.len() == 1 {
                    self.rendering_intent = self.operands[0].value.name().map(Cow::into_owned);
                    if self.rendering_intent.is_none() {
                        self.complete = false;
                    }
                } else {
                    self.complete = false;
                }
            }
            b"re" => {
                if self.operands.len() == 4 {
                    let values = parsed_operand_numbers(&self.operands);
                    if let Some(values) = values {
                        self.add_rect_path(values[0], values[1], values[2], values[3]);
                    } else {
                        self.path_complex = true;
                    }
                } else {
                    self.path_complex = true;
                }
            }
            b"m" => {
                if self.operands.len() == 2 {
                    let values = parsed_operand_numbers(&self.operands);
                    if let Some(values) = values {
                        self.begin_line_path(values[0], values[1]);
                    } else {
                        self.path_complex = true;
                    }
                } else {
                    self.path_complex = true;
                }
            }
            b"l" => {
                if self.operands.len() == 2 {
                    let values = parsed_operand_numbers(&self.operands);
                    if let Some(values) = values {
                        self.add_line_point(values[0], values[1]);
                    } else {
                        self.path_complex = true;
                    }
                } else {
                    self.path_complex = true;
                }
            }
            b"h" => self.path_closed = true,
            b"c" | b"v" | b"y" => {
                self.path_complex = true;
                self.path_rect = None;
                self.path_points = None;
            }
            b"W" | b"W*" => self.clip_pending = true,
            b"n" => self.apply_pending_clip(),
            // Ordinary non-path graphics-state setup is safe inside a single-image wrapper.
            b"w" | b"J" | b"j" | b"M" | b"d" | b"g" | b"G" | b"rg" | b"RG" | b"k" | b"K"
            | b"cs" | b"CS" | b"sc" | b"SC" | b"scn" | b"SCN" | b"BX" | b"EX" => {}
            // These actually paint/consume the current path. Simple rectangular fills are
            // retained in paint order so the hidden-paint pass can use them as opaque cover.
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" => {
                let fills_path =
                    matches!(operator, b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*");
                if fills_path && let Some(rect) = self.path_as_axis_aligned_rect() {
                    let fill_index = self.rect_fills.len();
                    self.rect_fills.push(RectFillPaint {
                        rect,
                        gs_name: self.gs_name.clone(),
                        clip: self.clip,
                        clip_polygon: self.clip_polygon.clone(),
                        clip_complex: self.clip_complex,
                    });
                    if self.text_seen {
                        self.covering_paint_after_text = true;
                    }
                    self.coverage_paints
                        .push(CoveragePaint::RectFill(fill_index));
                }
                self.apply_pending_clip();
                self.mark_other_paint();
            }
            b"sh" => self.mark_other_paint(),
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                self.text_paints = self.text_paints.saturating_add(1);
                self.text_seen = true;
                self.text_with_ext_gstate |= self.gs_name.is_some();
                self.mark_other_paint();
            }
            // Marked-content boundaries are not visual barriers for clustering, but consuming
            // one as part of an image wrapper would change document structure semantics.
            b"BMC" | b"BDC" | b"EMC" | b"MP" | b"DP" => self.mark_semantic_boundary(),
            // Text-state operators are harmless unless followed by an actual text paint.
            b"BT" | b"ET" | b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Tf" | b"Tr" | b"Ts" | b"Td"
            | b"TD" | b"Tm" | b"T*" => {}
            _ => self.mark_semantic_boundary(),
        }
    }
}

impl ObjectHandleParserCallbacks for RasterScanner {
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
            self.resource_pending_operands = true;
            if let Some(name) = scalar.as_name() {
                self.resource_last_name = Some(name.to_vec());
            }
            self.operands.push(ParsedOperand {
                value: ParsedOperandValue::Scalar(scalar),
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
        self.operator(operator, offset, length);
        self.operands.clear();
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.operator(&operator, offset, length);
            self.operands.clear();
        } else if object.as_inline_image().is_some() {
            self.inline_occurrences = self.inline_occurrences.saturating_add(1);
            self.mark_other_paint();
            self.barrier();
        } else {
            self.resource_pending_operands = true;
            if let Some(name) = object.as_name() {
                self.resource_last_name = Some(name);
            }
            self.operands.push(ParsedOperand {
                value: ParsedOperandValue::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        if self.analysis_only_no_images {
            if self.analysis_q_depth != 0 {
                self.complete = false;
            }
        } else if !self.stack.is_empty() || !self.frames.is_empty() {
            self.complete = false;
        }
        Ok(())
    }
}

struct RasterVectorScanner {
    raster: RasterScanner,
    vector: ProcessingPageScanner,
}

impl RasterVectorScanner {
    fn new(document: &EditDocument, resources: &OwnedDictionary) -> Result<Self> {
        let mut raster = new_raster_scanner(document, resources)?;
        if raster.image_names.is_empty() {
            raster.analysis_only_no_images = true;
        }
        Ok(Self {
            raster,
            vector: ProcessingPageScanner::for_resources(document, resources)?,
        })
    }

    fn finish(self) -> (RasterScanner, ProcessingVectorAnalysis) {
        (self.raster, self.vector.finish())
    }
}

impl ObjectHandleParserCallbacks for RasterVectorScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn content_size(&mut self, size: usize) -> flpdf::Result<()> {
        self.vector.content_size(size)
    }

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        let raster = self.raster.handle_scalar(scalar.clone(), offset, length)?;
        let vector = self.vector.handle_scalar(scalar, offset, length)?;
        if matches!(raster, ParseControl::Stop) || matches!(vector, ParseControl::Stop) {
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
        let raster = self.raster.handle_operator(operator, offset, length)?;
        let vector = self.vector.handle_operator(operator, offset, length)?;
        if matches!(raster, ParseControl::Stop) || matches!(vector, ParseControl::Stop) {
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
        let raster = self.raster.handle_object(object.clone(), offset, length)?;
        let vector = self.vector.handle_object(object, offset, length)?;
        Ok(
            if matches!(raster, ParseControl::Stop) || matches!(vector, ParseControl::Stop) {
                ParseControl::Stop
            } else {
                ParseControl::Continue
            },
        )
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        self.raster.handle_eof()?;
        self.vector.handle_eof()
    }
}

fn normalized_polygon(points: Option<&[(f64, f64)]>) -> Option<Vec<(f64, f64)>> {
    let points = points?;
    if points.len() < 3 {
        return None;
    }
    let mut out = points.to_vec();
    if out.len() >= 2 {
        let first = out[0];
        let last = *out.last()?;
        if (first.0 - last.0).abs() <= MATRIX_EPSILON && (first.1 - last.1).abs() <= MATRIX_EPSILON
        {
            out.pop();
        }
    }
    (out.len() >= 3 && polygon_is_convex(&out)).then_some(out)
}

fn polygon_is_convex(points: &[(f64, f64)]) -> bool {
    if points.len() < 3 {
        return false;
    }
    let mut sign = 0.0_f64;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        let c = points[(index + 2) % points.len()];
        let cross = (b.0 - a.0) * (c.1 - b.1) - (b.1 - a.1) * (c.0 - b.0);
        if cross.abs() <= MATRIX_EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    sign != 0.0
}

fn convex_polygon_contains_point(points: &[(f64, f64)], point: (f64, f64)) -> bool {
    let mut sign = 0.0_f64;
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        let cross = (b.0 - a.0) * (point.1 - a.1) - (b.1 - a.1) * (point.0 - a.0);
        if cross.abs() <= MATRIX_EPSILON {
            continue;
        }
        if sign == 0.0 {
            sign = cross.signum();
        } else if cross.signum() != sign {
            return false;
        }
    }
    true
}

fn convex_polygon_contains_rect(points: &[(f64, f64)], rect: Rect) -> bool {
    [
        (rect.x0, rect.y0),
        (rect.x0, rect.y1),
        (rect.x1, rect.y0),
        (rect.x1, rect.y1),
    ]
    .into_iter()
    .all(|point| convex_polygon_contains_point(points, point))
}

fn point_in_rect(point: (f64, f64), rect: Rect) -> bool {
    point.0 >= rect.x0 - MATRIX_EPSILON
        && point.0 <= rect.x1 + MATRIX_EPSILON
        && point.1 >= rect.y0 - MATRIX_EPSILON
        && point.1 <= rect.y1 + MATRIX_EPSILON
}

fn orientation(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

fn segments_intersect(a0: (f64, f64), a1: (f64, f64), b0: (f64, f64), b1: (f64, f64)) -> bool {
    let a = orientation(a0, a1, b0);
    let b = orientation(a0, a1, b1);
    let c = orientation(b0, b1, a0);
    let d = orientation(b0, b1, a1);
    if a.abs() <= MATRIX_EPSILON
        || b.abs() <= MATRIX_EPSILON
        || c.abs() <= MATRIX_EPSILON
        || d.abs() <= MATRIX_EPSILON
    {
        let minmax_overlap = |x0: f64, x1: f64, y0: f64, y1: f64| {
            x0.min(x1) <= y0.max(y1) + MATRIX_EPSILON && y0.min(y1) <= x0.max(x1) + MATRIX_EPSILON
        };
        if minmax_overlap(a0.0, a1.0, b0.0, b1.0)
            && minmax_overlap(a0.1, a1.1, b0.1, b1.1)
            && (a.abs() <= MATRIX_EPSILON
                || b.abs() <= MATRIX_EPSILON
                || c.abs() <= MATRIX_EPSILON
                || d.abs() <= MATRIX_EPSILON)
        {
            return true;
        }
    }
    (a > MATRIX_EPSILON && b < -MATRIX_EPSILON || a < -MATRIX_EPSILON && b > MATRIX_EPSILON)
        && (c > MATRIX_EPSILON && d < -MATRIX_EPSILON || c < -MATRIX_EPSILON && d > MATRIX_EPSILON)
}

fn convex_polygon_intersects_rect(points: &[(f64, f64)], rect: Rect) -> bool {
    if points
        .iter()
        .copied()
        .any(|point| point_in_rect(point, rect))
    {
        return true;
    }
    let corners = [
        (rect.x0, rect.y0),
        (rect.x1, rect.y0),
        (rect.x1, rect.y1),
        (rect.x0, rect.y1),
    ];
    if corners
        .iter()
        .copied()
        .any(|point| convex_polygon_contains_point(points, point))
    {
        return true;
    }
    for index in 0..points.len() {
        let a = points[index];
        let b = points[(index + 1) % points.len()];
        for edge in 0..4 {
            if segments_intersect(a, b, corners[edge], corners[(edge + 1) % 4]) {
                return true;
            }
        }
    }
    false
}

fn parsed_number(object: &FlObjectHandle) -> Option<f64> {
    object
        .as_integer()
        .map(|value| value as f64)
        .or_else(|| object.as_real())
}

fn target_content(document: &EditDocument, target: ContentTarget) -> Result<Vec<u8>> {
    match target {
        ContentTarget::Page(page) => page_content(document, page),
        ContentTarget::Form(form) => form_content(document, form),
    }
}

fn target_resources(
    document: &EditDocument,
    target: ContentTarget,
) -> Result<Option<OwnedDictionary>> {
    match target {
        ContentTarget::Page(page) => page_resources(document, page),
        ContentTarget::Form(form) => form_resources(document, form),
    }
}

fn collect_targets(document: &EditDocument, pages: &[ObjectHandle]) -> Result<Vec<ContentTarget>> {
    let mut targets = BTreeSet::new();
    for &page in pages {
        targets.insert(ContentTarget::Page(page));
    }
    for handle in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let subtype = dictionary
            .get(b"Subtype".as_slice())
            .map(|value| document.resolve_owned_value(value))
            .transpose()?;
        if matches!(subtype, Some(Some(OwnedObject::Name(name))) if name == b"Form")
            && matches!(object, OwnedObject::Stream { .. })
        {
            targets.insert(ContentTarget::Form(handle));
        }
    }
    Ok(targets.into_iter().collect())
}

fn xobjects(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ObjectHandle>> {
    let Some(dictionary) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, value) in dictionary {
        if let OwnedObject::Reference(handle) = value {
            out.insert(name, handle);
        }
    }
    Ok(out)
}

fn subtype(document: &EditDocument, handle: ObjectHandle) -> Result<Option<Vec<u8>>> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    let Some(value) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn new_raster_scanner(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<RasterScanner> {
    let xobjects = xobjects(document, resources)?;
    let mut image_names = HashSet::new();
    for (name, handle) in &xobjects {
        if subtype(document, *handle)?.as_deref() == Some(b"Image") {
            image_names.insert(name.clone());
        }
    }
    Ok(RasterScanner::new(xobjects, image_names))
}

fn scan_target(
    document: &EditDocument,
    target: ContentTarget,
    resources: &OwnedDictionary,
    content: &[u8],
) -> Result<Option<RasterScanner>> {
    let mut scanner = new_raster_scanner(document, resources)?;
    flpdf::parse_detached_content_stream(content, "raster-layout normalization", &mut scanner)?;
    if !scanner.complete {
        return Ok(None);
    }
    let _ = target;
    Ok(Some(scanner))
}

struct TargetScanResult {
    scanner: RasterScanner,
    vector_analysis: Option<ProcessingVectorAnalysis>,
    hidden_ranges: Option<Vec<(usize, usize)>>,
}

fn scan_target_shared(
    document: &EditDocument,
    target: ContentTarget,
    resources: &OwnedDictionary,
    content: &[u8],
    hidden_context: Option<&HiddenTextSharedContext>,
    page_numbers: &BTreeMap<ObjectHandle, usize>,
    collect_vector: bool,
) -> Result<Option<TargetScanResult>> {
    let ContentTarget::Page(page) = target else {
        return Ok(
            scan_target(document, target, resources, content)?.map(|scanner| TargetScanResult {
                scanner,
                vector_analysis: None,
                hidden_ranges: None,
            }),
        );
    };

    let page_number = page_numbers.get(&page).copied().unwrap_or(1);
    if collect_vector {
        let mut scanner = RasterVectorScanner::new(document, resources)?;
        let hidden_ranges = if let Some(context) = hidden_context {
            Some(scan_physical_hidden_text_with_callback_hayro(
                document,
                page,
                page_number,
                context,
                content,
                &mut scanner,
            )?)
        } else {
            flpdf::parse_detached_content_stream(
                content,
                "shared raster/vector page content",
                &mut scanner,
            )?;
            None
        };
        let (raster, vector_analysis) = scanner.finish();
        if !raster.complete {
            return Ok(None);
        }
        return Ok(Some(TargetScanResult {
            scanner: raster,
            vector_analysis: Some(vector_analysis),
            hidden_ranges,
        }));
    }

    let mut scanner = new_raster_scanner(document, resources)?;
    let hidden_ranges = if let Some(context) = hidden_context {
        Some(scan_physical_hidden_text_with_callback_hayro(
            document,
            page,
            page_number,
            context,
            content,
            &mut scanner,
        )?)
    } else {
        flpdf::parse_detached_content_stream(content, "raster-layout normalization", &mut scanner)?;
        None
    };
    if !scanner.complete {
        return Ok(None);
    }
    Ok(Some(TargetScanResult {
        scanner,
        vector_analysis: None,
        hidden_ranges,
    }))
}

fn current_number(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => Some(value as f64),
        Some(OwnedObject::Real(value)) => Some(value),
        _ => None,
    })
}

fn current_number_array4(document: &EditDocument, value: &OwnedObject) -> Result<Option<[f64; 4]>> {
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != 4 {
        return Ok(None);
    }
    let mut out = [0.0; 4];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = current_number(document, Some(value))? else {
            return Ok(None);
        };
        if !number.is_finite() {
            return Ok(None);
        }
        out[index] = number;
    }
    Ok(Some(out))
}

fn page_visible_rect(document: &EditDocument, page: ObjectHandle) -> Result<Option<Rect>> {
    for key in [b"CropBox".as_slice(), b"MediaBox".as_slice()] {
        let Some(value) = document.inherited_page_value(page, key)? else {
            continue;
        };
        let Some(values) = current_number_array4(document, &value)? else {
            continue;
        };
        let rect = Rect {
            x0: values[0].min(values[2]),
            y0: values[1].min(values[3]),
            x1: values[0].max(values[2]),
            y1: values[1].max(values[3]),
        };
        if rect.x1 > rect.x0 && rect.y1 > rect.y0 {
            return Ok(Some(rect));
        }
    }
    Ok(None)
}

fn current_bool(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Boolean(value)) => Some(value),
        _ => None,
    })
}

fn dimensions(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<Option<(u32, u32)>> {
    let Some(width) = current_number(document, dictionary.get(b"Width".as_slice()))? else {
        return Ok(None);
    };
    let Some(height) = current_number(document, dictionary.get(b"Height".as_slice()))? else {
        return Ok(None);
    };
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Ok(None);
    }
    let width = width.round();
    let height = height.round();
    if width > f64::from(u32::MAX) || height > f64::from(u32::MAX) {
        return Ok(None);
    }
    Ok(Some((width as u32, height as u32)))
}

fn is_non_null(document: &EditDocument, value: Option<&OwnedObject>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    Ok(!matches!(
        document.resolve_owned_value(value)?,
        None | Some(OwnedObject::Null)
    ))
}

fn color_components(document: &EditDocument, value: &OwnedObject) -> Result<Option<usize>> {
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    match value {
        OwnedObject::Name(name) => Ok(match name.as_slice() {
            b"DeviceGray" | b"G" | b"CalGray" => Some(1),
            b"DeviceRGB" | b"RGB" | b"CalRGB" | b"Lab" => Some(3),
            b"DeviceCMYK" | b"CMYK" => Some(4),
            _ => None,
        }),
        OwnedObject::Array(values) => {
            let Some(first) = values.first() else {
                return Ok(None);
            };
            let Some(OwnedObject::Name(kind)) = document.resolve_owned_value(first)? else {
                return Ok(None);
            };
            match kind.as_slice() {
                b"Indexed" | b"I" => Ok(Some(1)),
                b"ICCBased" => {
                    let Some(profile) = values.get(1) else {
                        return Ok(None);
                    };
                    let Some(profile) = document.resolve_owned_value(profile)? else {
                        return Ok(None);
                    };
                    let Some(dictionary) = profile.as_dictionary() else {
                        return Ok(None);
                    };
                    let Some(n) = current_number(document, dictionary.get(b"N".as_slice()))? else {
                        return Ok(None);
                    };
                    let n = n.round() as i64;
                    Ok(usize::try_from(n).ok().filter(|n| (1..=4).contains(n)))
                }
                _ => Ok(None),
            }
        }
        _ => Ok(None),
    }
}

#[derive(Debug, Clone)]
struct AlphaPlane {
    data: Arc<[u8]>,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone)]
struct SampleImage {
    width: u32,
    height: u32,
    components: usize,
    /// Preserve compact source color coding (notably DCT/JPX) when a rewrite
    /// would otherwise decode the image and re-emit the color plane as Flate.
    preserve_encoded_color: bool,
    encoded_color_bytes: usize,
    encoded_mask_bytes: usize,
    data: Arc<[u8]>,
    alpha: Option<AlphaPlane>,
    mask_width: u32,
    mask_height: u32,
    semantic_key: [u8; 32],
    dictionary_template: OwnedDictionary,
    interpolate: bool,
}

#[derive(Default)]
struct RasterImageDecodeCache {
    entries: HashMap<ObjectHandle, Option<SampleImage>>,
    decoded_bytes: usize,
    hits: usize,
    misses: usize,
    clears: usize,
}

impl RasterImageDecodeCache {
    fn image_bytes(image: &SampleImage) -> usize {
        image
            .data
            .len()
            .saturating_add(image.alpha.as_ref().map_or(0, |alpha| alpha.data.len()))
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.decoded_bytes = 0;
        self.clears = self.clears.saturating_add(1);
    }

    fn get_or_decode(
        &mut self,
        document: &EditDocument,
        handle: ObjectHandle,
    ) -> Result<Option<SampleImage>> {
        if let Some(cached) = self.entries.get(&handle) {
            self.hits = self.hits.saturating_add(1);
            return Ok(cached.clone());
        }

        self.misses = self.misses.saturating_add(1);
        let decoded = image_info(document, handle, true)?;
        let bytes = decoded.as_ref().map_or(0, Self::image_bytes);

        // Keep this a bounded performance cache, not a second document model.
        // SampleImage's raster buffers are Arc-backed, so cache hits only clone
        // small metadata and shared pointers. Clearing the whole cache is cheap
        // and deterministic when the budget is exhausted.
        if bytes <= SHARED_IMAGE_CACHE_MAX_BYTES {
            if self.entries.len() >= SHARED_IMAGE_CACHE_MAX_ENTRIES
                || self.decoded_bytes.saturating_add(bytes) > SHARED_IMAGE_CACHE_MAX_BYTES
            {
                self.clear();
            }
            self.decoded_bytes = self.decoded_bytes.saturating_add(bytes);
            self.entries.insert(handle, decoded.clone());
        }

        Ok(decoded)
    }
}

#[derive(Debug, Clone)]
struct DeferredTile {
    draw_index: usize,
    source: ObjectHandle,
    resource_name: Vec<u8>,
    original_range: (usize, usize),
    source_size: (u32, u32),
    page_rect: Rect,
    effective_ppi: (f64, f64),
}

impl DeferredTile {
    fn from_draw(draw_index: usize, draw: &RasterDraw, image: &SampleImage) -> Option<Self> {
        let page_rect = Rect::from_ctm(draw.ctm);
        let width_pt = (page_rect.x1 - page_rect.x0).abs();
        let height_pt = (page_rect.y1 - page_rect.y0).abs();
        if !width_pt.is_finite()
            || !height_pt.is_finite()
            || width_pt <= MATRIX_EPSILON
            || height_pt <= MATRIX_EPSILON
        {
            return None;
        }
        let ppi_x = 72.0 * f64::from(image.width) / width_pt;
        let ppi_y = 72.0 * f64::from(image.height) / height_pt;
        (ppi_x.is_finite() && ppi_y.is_finite() && ppi_x > 0.0 && ppi_y > 0.0).then(|| Self {
            draw_index,
            source: draw.target,
            resource_name: draw.resource_name.clone(),
            original_range: (draw.range_start, draw.range_end),
            source_size: (image.width, image.height),
            page_rect,
            effective_ppi: (ppi_x, ppi_y),
        })
    }
}

fn unpack_mask_samples(data: &[u8], width: u32, height: u32, bpc: u8) -> Option<Vec<u8>> {
    let max = (1_u16 << bpc) - 1;
    let row_bits = usize::try_from(width).ok()?.checked_mul(usize::from(bpc))?;
    let row_bytes = row_bits.div_ceil(8);
    let mut out = Vec::with_capacity(usize::try_from(width.checked_mul(height)?).ok()?);
    for row in 0..usize::try_from(height).ok()? {
        let bytes = data.get(row.checked_mul(row_bytes)?..(row + 1).checked_mul(row_bytes)?)?;
        let mut bit = 0usize;
        for _ in 0..width {
            let byte = *bytes.get(bit / 8)?;
            let shift = 8usize.checked_sub(usize::from(bpc))?.checked_sub(bit % 8)?;
            let sample = (u16::from(byte) >> shift) & max;
            out.push(((sample * 255 + max / 2) / max) as u8);
            bit += usize::from(bpc);
        }
    }
    Some(out)
}

fn unpack_packed_samples(data: &[u8], width: u32, height: u32, bpc: u8) -> Option<Vec<u8>> {
    if !matches!(bpc, 1 | 2 | 4) {
        return None;
    }
    let mask = (1_u16 << bpc) - 1;
    let row_bits = usize::try_from(width).ok()?.checked_mul(usize::from(bpc))?;
    let row_bytes = row_bits.div_ceil(8);
    let mut out = Vec::with_capacity(usize::try_from(width.checked_mul(height)?).ok()?);
    for row in 0..usize::try_from(height).ok()? {
        let bytes = data.get(row.checked_mul(row_bytes)?..(row + 1).checked_mul(row_bytes)?)?;
        let mut bit = 0usize;
        for _ in 0..width {
            let byte = *bytes.get(bit / 8)?;
            let shift = 8usize.checked_sub(usize::from(bpc))?.checked_sub(bit % 8)?;
            out.push(((u16::from(byte) >> shift) & mask) as u8);
            bit += usize::from(bpc);
        }
    }
    Some(out)
}

fn decode_pair(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<(f64, f64)>> {
    let Some(value) = dictionary.get(b"Decode".as_slice()) else {
        return Ok(None);
    };
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != 2 {
        return Ok(None);
    }
    let Some(min) = current_number(document, values.first())? else {
        return Ok(None);
    };
    let Some(max) = current_number(document, values.get(1))? else {
        return Ok(None);
    };
    Ok(Some((min, max)))
}

fn decode_mask_stream(
    document: &EditDocument,
    value: &OwnedObject,
    stencil_semantics: bool,
) -> Result<Option<AlphaPlane>> {
    let Some(stream) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, .. } = &stream else {
        return Ok(None);
    };
    let Some((width, height)) = dimensions(document, dictionary)? else {
        return Ok(None);
    };
    let Some(bpc) = current_number(document, dictionary.get(b"BitsPerComponent".as_slice()))?
        .or_else(|| {
            current_number(document, dictionary.get(b"BPC".as_slice()))
                .ok()
                .flatten()
        })
    else {
        return Ok(None);
    };
    let bpc = bpc.round() as i64;
    let Ok(bpc) = u8::try_from(bpc) else {
        return Ok(None);
    };
    if !matches!(bpc, 1 | 2 | 4 | 8) {
        return Ok(None);
    }
    let decoded = match document.decoded_owned_stream_data(&stream, flpdf::DecodeLevel::Specialized)
    {
        Ok(decoded) => decoded,
        Err(error) => {
            if *DEBUG_RASTER {
                eprintln!("raster-layout: skipping undecodable mask stream: {error}");
            }
            return Ok(None);
        }
    };
    let Some(mut alpha) = unpack_mask_samples(&decoded, width, height, bpc) else {
        return Ok(None);
    };
    let (decode_min, decode_max) = decode_pair(document, dictionary)?.unwrap_or((0.0, 1.0));
    if decode_min != 0.0 || decode_max != 1.0 || stencil_semantics {
        for sample in &mut alpha {
            let mut value = decode_min + (f64::from(*sample) / 255.0) * (decode_max - decode_min);
            value = value.clamp(0.0, 1.0);
            if stencil_semantics {
                value = 1.0 - value;
            }
            *sample = (value * 255.0 + 0.5) as u8;
        }
    }
    Ok(Some(AlphaPlane {
        data: alpha.into(),
        width,
        height,
    }))
}

fn color_key_alpha(
    document: &EditDocument,
    mask: &OwnedObject,
    samples: &[u8],
    width: u32,
    height: u32,
    components: usize,
) -> Result<Option<AlphaPlane>> {
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(mask)? else {
        return Ok(None);
    };
    if values.len() != components.saturating_mul(2) {
        return Ok(None);
    }
    let mut ranges = Vec::with_capacity(components);
    for pair in values.as_chunks::<2>().0 {
        let Some(min) = current_number(document, pair.first())? else {
            return Ok(None);
        };
        let Some(max) = current_number(document, pair.get(1))? else {
            return Ok(None);
        };
        ranges.push((min.round() as i64, max.round() as i64));
    }
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|n| n.checked_mul(components));
    if expected != Some(samples.len()) {
        return Ok(None);
    }
    let mut alpha = Vec::with_capacity(samples.len() / components);
    for pixel in samples.chunks_exact(components) {
        let transparent = pixel
            .iter()
            .zip(&ranges)
            .all(|(&sample, &(min, max))| i64::from(sample) >= min && i64::from(sample) <= max);
        alpha.push(if transparent { 0 } else { 255 });
    }
    Ok(Some(AlphaPlane {
        data: alpha.into(),
        width,
        height,
    }))
}

fn indexed_palette(
    document: &EditDocument,
    color_space: &OwnedObject,
) -> Result<Option<(usize, usize, Vec<u8>)>> {
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(color_space)? else {
        return Ok(None);
    };
    let Some(first) = values.first() else {
        return Ok(None);
    };
    let Some(OwnedObject::Name(kind)) = document.resolve_owned_value(first)? else {
        return Ok(None);
    };
    if !matches!(kind.as_slice(), b"Indexed" | b"I") || values.len() < 4 {
        return Ok(None);
    }
    let Some(base) = document.resolve_owned_value(&values[1])? else {
        return Ok(None);
    };
    let base_components = match base {
        OwnedObject::Name(name) => match name.as_slice() {
            b"DeviceGray" | b"G" => 1,
            b"DeviceRGB" | b"RGB" => 3,
            b"DeviceCMYK" | b"CMYK" => 4,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let Some(hival) = current_number(document, values.get(2))? else {
        return Ok(None);
    };
    let hival = hival.round();
    if !hival.is_finite() || !(0.0..=255.0).contains(&hival) {
        return Ok(None);
    }
    let hival = hival as usize;
    let Some(lookup) = document.resolve_owned_value(&values[3])? else {
        return Ok(None);
    };
    let bytes = match lookup {
        OwnedObject::String(bytes) => bytes,
        OwnedObject::Stream { .. } => {
            match document.decoded_owned_stream_data(&lookup, flpdf::DecodeLevel::Specialized) {
                Ok(bytes) => bytes,
                Err(_) => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    let needed = (hival + 1).checked_mul(base_components);
    if needed.is_none_or(|needed| bytes.len() < needed) {
        return Ok(None);
    }
    Ok(Some((base_components, hival, bytes)))
}

fn expand_indexed_samples(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    color_space: &OwnedObject,
    samples: &[u8],
    bpc: u8,
) -> Result<Option<(Vec<u8>, usize)>> {
    let Some((components, hival, palette)) = indexed_palette(document, color_space)? else {
        return Ok(None);
    };
    let decode = decode_pair(document, dictionary)?;
    let max_sample = f64::from((1_u16 << bpc) - 1);
    let mut out = Vec::with_capacity(samples.len().saturating_mul(components));
    for &sample in samples {
        let index = if let Some((min, max)) = decode {
            let mapped = min + (f64::from(sample) / max_sample) * (max - min);
            mapped.round().clamp(0.0, hival as f64) as usize
        } else {
            usize::from(sample).min(hival)
        };
        let start = index
            .checked_mul(components)
            .ok_or_else(|| Error::Invalid("Indexed palette offset overflow".to_owned()))?;
        let end = start
            .checked_add(components)
            .ok_or_else(|| Error::Invalid("Indexed palette range overflow".to_owned()))?;
        let Some(color) = palette.get(start..end) else {
            return Ok(None);
        };
        out.extend_from_slice(color);
    }
    Ok(Some((out, components)))
}

fn hash_semantic_bytes(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_semantic_dictionary(hasher: &mut Sha256, dictionary: &OwnedDictionary) {
    hasher.update((dictionary.len() as u64).to_le_bytes());
    for (key, value) in dictionary {
        hash_semantic_bytes(hasher, key);
        hash_semantic_object(hasher, value);
    }
}

fn hash_semantic_object(hasher: &mut Sha256, object: &OwnedObject) {
    match object {
        OwnedObject::Null => hasher.update([0]),
        OwnedObject::Boolean(value) => hasher.update([1, u8::from(*value)]),
        OwnedObject::Integer(value) => {
            hasher.update([2]);
            hasher.update(value.to_le_bytes());
        }
        OwnedObject::Real(value) => {
            hasher.update([3]);
            hasher.update(value.to_bits().to_le_bytes());
        }
        OwnedObject::Name(value) => {
            hasher.update([4]);
            hash_semantic_bytes(hasher, value);
        }
        OwnedObject::String(value) => {
            hasher.update([5]);
            hash_semantic_bytes(hasher, value);
        }
        OwnedObject::Reference(handle) => {
            hasher.update([6]);
            match handle {
                ObjectHandle::Existing(id) => {
                    hasher.update([0]);
                    hasher.update(id.number().to_le_bytes());
                    hasher.update(id.generation().to_le_bytes());
                }
                ObjectHandle::New(id) => {
                    hasher.update([1]);
                    hasher.update((id.index() as u64).to_le_bytes());
                }
            }
        }
        OwnedObject::Array(values) => {
            hasher.update([7]);
            hasher.update((values.len() as u64).to_le_bytes());
            for value in values {
                hash_semantic_object(hasher, value);
            }
        }
        OwnedObject::Dictionary(dictionary) => {
            hasher.update([8]);
            hash_semantic_dictionary(hasher, dictionary);
        }
        OwnedObject::Stream { dictionary, data } => {
            hasher.update([9]);
            hash_semantic_dictionary(hasher, dictionary);
            match data {
                StreamData::Source(id) => {
                    hasher.update([0]);
                    hasher.update(id.number().to_le_bytes());
                    hasher.update(id.generation().to_le_bytes());
                }
                StreamData::Owned(bytes) => {
                    hasher.update([1]);
                    hash_semantic_bytes(hasher, bytes);
                }
            }
        }
    }
}

fn semantic_dictionary_key(dictionary: &OwnedDictionary) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"pdf-redox-raster-semantic-v1");
    hash_semantic_dictionary(&mut hasher, dictionary);
    hasher.finalize().into()
}

fn filter_contains_name(
    document: &EditDocument,
    value: &OwnedObject,
    names: &[&[u8]],
) -> Result<bool> {
    let Some(value) = document.resolve_owned_value(value)? else {
        return Ok(false);
    };
    match value {
        OwnedObject::Name(name) => Ok(names.contains(&name.as_slice())),
        OwnedObject::Array(values) => {
            for value in &values {
                if filter_contains_name(document, value, names)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

fn preserves_compact_color_encoding(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<bool> {
    let Some(filter) = dictionary
        .get(b"Filter".as_slice())
        .or_else(|| dictionary.get(b"F".as_slice()))
    else {
        return Ok(false);
    };
    filter_contains_name(
        document,
        filter,
        &[b"DCTDecode", b"DCT", b"JPXDecode", b"JPX"],
    )
}

fn encoded_stream_payload_len(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<usize>> {
    let Some(object) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { data, .. } = &object else {
        return Ok(None);
    };
    Ok(Some(data.bytes(document.source())?.len()))
}

fn image_info(
    document: &EditDocument,
    handle: ObjectHandle,
    bake_masks: bool,
) -> Result<Option<SampleImage>> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(None);
    };
    let OwnedObject::Stream { dictionary, data } = &object else {
        return Ok(None);
    };
    let encoded_color_bytes = data.bytes(document.source())?.len();
    if current_bool(document, dictionary.get(b"ImageMask".as_slice()))?.unwrap_or(false)
        || current_bool(document, dictionary.get(b"IM".as_slice()))?.unwrap_or(false)
    {
        return Ok(None);
    }
    let Some((width, height)) = dimensions(document, dictionary)? else {
        return Ok(None);
    };
    let Some(bpc) = current_number(document, dictionary.get(b"BitsPerComponent".as_slice()))?
    else {
        return Ok(None);
    };
    let bpc = bpc.round();
    if !bpc.is_finite() {
        return Ok(None);
    }
    let Ok(bpc) = u8::try_from(bpc as i64) else {
        return Ok(None);
    };
    if !matches!(bpc, 1 | 2 | 4 | 8) {
        return Ok(None);
    }
    let color_space = dictionary
        .get(b"ColorSpace".as_slice())
        .or_else(|| dictionary.get(b"CS".as_slice()));
    let Some(color_space) = color_space else {
        return Ok(None);
    };
    let Some(mut components) = color_components(document, color_space)? else {
        return Ok(None);
    };
    let indexed = indexed_palette(document, color_space)?.is_some();
    if bpc != 8 && !indexed {
        return Ok(None);
    }
    let raw_decoded = match document.decoded_owned_stream_data(&object, flpdf::DecodeLevel::All) {
        Ok(decoded) => decoded,
        Err(error) => {
            if *DEBUG_RASTER {
                eprintln!("raster-layout: skipping undecodable image {handle:?}: {error}");
            }
            return Ok(None);
        }
    };
    let mut decoded = if indexed && bpc != 8 {
        let Some(samples) = unpack_packed_samples(&raw_decoded, width, height, bpc) else {
            return Ok(None);
        };
        samples
    } else {
        raw_decoded
    };
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|n| n.checked_mul(components));
    let Some(expected) = expected else {
        return Ok(None);
    };
    if decoded.len() < expected {
        return Ok(None);
    }
    // Some producers append harmless bytes after the declared image sample grid.
    // Renderers ignore those bytes because Width/Height/BPC define the raster payload.
    decoded.truncate(expected);
    if current_number(document, dictionary.get(b"SMaskInData".as_slice()))?
        .is_some_and(|value| value != 0.0)
    {
        return Ok(None);
    }

    let preserve_encoded_color = preserves_compact_color_encoding(document, dictionary)?;
    let has_smask = is_non_null(document, dictionary.get(b"SMask".as_slice()))?;
    let has_mask = is_non_null(document, dictionary.get(b"Mask".as_slice()))?;
    let encoded_mask_bytes = if has_smask {
        match dictionary.get(b"SMask".as_slice()) {
            Some(smask) => encoded_stream_payload_len(document, smask)?.unwrap_or(0),
            None => 0,
        }
    } else if has_mask {
        match dictionary.get(b"Mask".as_slice()) {
            Some(mask) => encoded_stream_payload_len(document, mask)?.unwrap_or(0),
            None => 0,
        }
    } else {
        0
    };
    if (has_smask || has_mask) && !bake_masks {
        return Ok(None);
    }

    let mut alpha = None;
    let mut mask_width = width;
    let mut mask_height = height;
    if bake_masks {
        if let Some(smask) = dictionary.get(b"SMask".as_slice())
            && is_non_null(document, Some(smask))?
        {
            alpha = decode_mask_stream(document, smask, false)?;
        } else if let Some(mask) = dictionary.get(b"Mask".as_slice())
            && is_non_null(document, Some(mask))?
        {
            match document.resolve_owned_value(mask)? {
                Some(OwnedObject::Array(_)) => {
                    alpha = color_key_alpha(document, mask, &decoded, width, height, components)?;
                }
                Some(OwnedObject::Stream { ref dictionary, .. }) => {
                    let stencil = current_bool(document, dictionary.get(b"ImageMask".as_slice()))?
                        .unwrap_or(false);
                    alpha = decode_mask_stream(document, mask, stencil)?;
                }
                _ => return Ok(None),
            }
        }
        if (has_smask || has_mask) && alpha.is_none() {
            return Ok(None);
        }
        if let Some(alpha) = &alpha {
            mask_width = alpha.width;
            mask_height = alpha.height;
        }
    }

    if indexed {
        let Some((expanded, expanded_components)) =
            expand_indexed_samples(document, dictionary, color_space, &decoded, bpc)?
        else {
            return Ok(None);
        };
        decoded = expanded;
        components = expanded_components;
    }

    let interpolate = current_bool(document, dictionary.get(b"Interpolate".as_slice()))?
        .or(current_bool(document, dictionary.get(b"I".as_slice()))?)
        .unwrap_or(false);

    let mut template = dictionary.clone();
    for key in [
        b"Length".as_slice(),
        b"Filter".as_slice(),
        b"DecodeParms".as_slice(),
        b"F".as_slice(),
        b"DP".as_slice(),
        b"Width".as_slice(),
        b"Height".as_slice(),
        b"W".as_slice(),
        b"H".as_slice(),
        b"SMask".as_slice(),
        b"Mask".as_slice(),
        // Obsolete Image XObject identifier; resource bindings carry identity.
        // It has no rendering semantics and must not prevent fragments from
        // converging to one reconstructed raster.
        b"Name".as_slice(),
    ] {
        template.remove(key);
    }
    template.remove(b"ColorSpace".as_slice());
    template.remove(b"CS".as_slice());
    if indexed {
        template.remove(b"Decode".as_slice());
        template.remove(b"D".as_slice());
        let color_space = match components {
            1 => b"DeviceGray".to_vec(),
            3 => b"DeviceRGB".to_vec(),
            4 => b"DeviceCMYK".to_vec(),
            _ => return Ok(None),
        };
        template.insert(b"ColorSpace".to_vec(), OwnedObject::Name(color_space));
        template.insert(b"BitsPerComponent".to_vec(), OwnedObject::Integer(8));
    } else {
        // Resource dictionaries often wrap the same ICCBased/Cal/etc. color space
        // in thousands of distinct indirect objects. The wrapper identity has no
        // rendering semantics; compare and re-emit the resolved color-space value.
        let Some(resolved_color_space) = document.resolve_owned_value(color_space)? else {
            return Ok(None);
        };
        template.insert(b"ColorSpace".to_vec(), resolved_color_space);
    }
    let semantic_key = semantic_dictionary_key(&template);
    Ok(Some(SampleImage {
        width,
        height,
        components,
        preserve_encoded_color,
        encoded_color_bytes,
        encoded_mask_bytes,
        data: decoded.into(),
        alpha,
        mask_width,
        mask_height,
        semantic_key,
        dictionary_template: template,
        interpolate,
    }))
}

fn image_paint_states(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<HashMap<Vec<u8>, ImagePaintState>> {
    let Some(states) = resolved_dictionary(document, resources.get(b"ExtGState".as_slice()))?
    else {
        return Ok(HashMap::new());
    };
    let mut out = HashMap::new();
    for (name, value) in states {
        let Some(state) = resolved_dictionary(document, Some(&value))? else {
            continue;
        };
        let alpha = current_number(document, state.get(b"ca".as_slice()))?
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        let normal_blend = match state.get(b"BM".as_slice()) {
            None => true,
            Some(value) => match document.resolve_owned_value(value)? {
                None | Some(OwnedObject::Null) => true,
                Some(OwnedObject::Name(name)) => name == b"Normal",
                _ => false,
            },
        };
        out.insert(
            name,
            ImagePaintState {
                alpha,
                normal_blend,
            },
        );
    }
    Ok(out)
}

fn draw_paint_state(
    draw: &RasterDraw,
    states: &HashMap<Vec<u8>, ImagePaintState>,
) -> ImagePaintState {
    draw.gs_name
        .as_ref()
        .and_then(|name| states.get(name))
        .copied()
        .unwrap_or_default()
}

fn image_fully_transparent(image: &SampleImage) -> bool {
    image
        .alpha
        .as_ref()
        .is_some_and(|alpha| alpha.data.iter().all(|&sample| sample == 0))
}

fn image_fully_opaque(image: &SampleImage) -> bool {
    image
        .alpha
        .as_ref()
        .is_none_or(|alpha| alpha.data.iter().all(|&sample| sample == 255))
}

fn remove_content_ranges(input: &[u8], mut ranges: Vec<(usize, usize)>) -> Result<Vec<u8>> {
    if ranges.is_empty() {
        return Ok(input.to_vec());
    }
    ranges.sort_unstable();
    let mut merged = Vec::<(usize, usize)>::new();
    for (start, end) in ranges {
        if end < start || end > input.len() {
            return Err(Error::Invalid("invalid content removal range".to_owned()));
        }
        if let Some(last) = merged.last_mut()
            && start <= last.1
        {
            last.1 = last.1.max(end);
        } else {
            merged.push((start, end));
        }
    }
    let mut out = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for (start, end) in merged {
        if start < cursor {
            return Err(Error::Invalid(
                "overlapping content removal range".to_owned(),
            ));
        }
        out.extend_from_slice(&input[cursor..start]);
        out.push(b' ');
        cursor = end;
    }
    out.extend_from_slice(&input[cursor..]);
    Ok(out)
}

fn remove_raster_draws(
    content: &[u8],
    draws: &[RasterDraw],
    removed: &HashSet<usize>,
) -> Result<Vec<u8>> {
    let ranges = removed
        .iter()
        .map(|&index| {
            let draw = &draws[index];
            (draw.range_start, draw.range_end)
        })
        .collect::<Vec<_>>();
    remove_content_ranges(content, ranges)
}

#[derive(Debug, Clone)]
struct OpaqueCover {
    rect: Rect,
    clip_polygon: Option<Vec<(f64, f64)>>,
}

fn rect_covered_by_union(target: Rect, covers: &[Rect], epsilon: f64) -> bool {
    let clipped = covers
        .iter()
        .filter_map(|cover| cover.intersection(target))
        .collect::<Vec<_>>();
    if clipped.is_empty() {
        return false;
    }
    if clipped.iter().any(|cover| cover.contains(target, epsilon)) {
        return true;
    }

    let mut xs = Vec::with_capacity(clipped.len() * 2 + 2);
    xs.extend([target.x0, target.x1]);
    for cover in &clipped {
        xs.push(cover.x0.max(target.x0));
        xs.push(cover.x1.min(target.x1));
    }
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() <= epsilon);

    for pair in xs.windows(2) {
        let left = pair[0];
        let right = pair[1];
        if right - left <= epsilon {
            continue;
        }
        let mut ys = clipped
            .iter()
            .filter(|cover| cover.x0 <= left + epsilon && cover.x1 + epsilon >= right)
            .filter_map(|cover| {
                let y0 = cover.y0.max(target.y0);
                let y1 = cover.y1.min(target.y1);
                (y1 - y0 > epsilon).then_some((y0, y1))
            })
            .collect::<Vec<_>>();
        if ys.is_empty() {
            return false;
        }
        ys.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));
        let mut covered_to = target.y0;
        for (y0, y1) in ys {
            if y0 > covered_to + epsilon {
                return false;
            }
            covered_to = covered_to.max(y1);
            if covered_to + epsilon >= target.y1 {
                break;
            }
        }
        if covered_to + epsilon < target.y1 {
            return false;
        }
    }
    true
}

fn rect_covered_by_union_in_polygon(
    target: Rect,
    covers: &[Rect],
    polygon: &[(f64, f64)],
    epsilon: f64,
) -> bool {
    let clipped = covers
        .iter()
        .filter_map(|cover| cover.intersection(target))
        .collect::<Vec<_>>();
    let mut xs = Vec::with_capacity(clipped.len() * 2 + 2);
    xs.extend([target.x0, target.x1]);
    for cover in &clipped {
        xs.push(cover.x0.max(target.x0));
        xs.push(cover.x1.min(target.x1));
    }
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() <= epsilon);

    for pair in xs.windows(2) {
        let left = pair[0];
        let right = pair[1];
        if right - left <= epsilon {
            continue;
        }
        let mut ys = clipped
            .iter()
            .filter(|cover| cover.x0 <= left + epsilon && cover.x1 + epsilon >= right)
            .filter_map(|cover| {
                let y0 = cover.y0.max(target.y0);
                let y1 = cover.y1.min(target.y1);
                (y1 - y0 > epsilon).then_some((y0, y1))
            })
            .collect::<Vec<_>>();
        ys.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

        let mut cursor = target.y0;
        for (y0, y1) in ys {
            if y0 > cursor + epsilon {
                let gap = Rect {
                    x0: left,
                    y0: cursor,
                    x1: right,
                    y1: y0,
                };
                if convex_polygon_intersects_rect(polygon, gap) {
                    return false;
                }
            }
            cursor = cursor.max(y1);
            if cursor + epsilon >= target.y1 {
                break;
            }
        }
        if cursor + epsilon < target.y1 {
            let gap = Rect {
                x0: left,
                y0: cursor,
                x1: right,
                y1: target.y1,
            };
            if convex_polygon_intersects_rect(polygon, gap) {
                return false;
            }
        }
    }
    true
}

fn target_covered_by_opaque_union(
    target: Rect,
    target_polygon: Option<&[(f64, f64)]>,
    covers: &[OpaqueCover],
    epsilon: f64,
) -> bool {
    let usable = covers
        .iter()
        .filter(
            |cover| match (target_polygon, cover.clip_polygon.as_deref()) {
                (_, None) => true,
                (Some(target), Some(cover_polygon)) => target == cover_polygon,
                (None, Some(_)) => false,
            },
        )
        .map(|cover| cover.rect)
        .collect::<Vec<_>>();
    match target_polygon {
        Some(polygon) => rect_covered_by_union_in_polygon(target, &usable, polygon, epsilon),
        None => rect_covered_by_union(target, &usable, epsilon),
    }
}

fn axis_aligned_candidate_rect(draw: &RasterDraw) -> Option<Option<Rect>> {
    if draw.ctm.b.abs() > MATRIX_EPSILON || draw.ctm.c.abs() > MATRIX_EPSILON {
        return None;
    }
    let rect = Rect::from_ctm(draw.ctm);
    if draw.clip_complex {
        return Some(Some(rect));
    }
    let rect = match draw.clip {
        Some(clip) => rect.intersection(clip),
        None => Some(rect),
    };
    let Some(rect) = rect else {
        return Some(None);
    };
    if let Some(polygon) = draw.clip_polygon.as_deref() {
        if convex_polygon_contains_rect(polygon, rect) {
            Some(Some(rect))
        } else if !convex_polygon_intersects_rect(polygon, rect) {
            Some(None)
        } else {
            // Partially clipped by a known polygon. The unclipped rectangle is a conservative
            // superset for dead-paint testing, but not safe as an opaque-cover contribution.
            Some(Some(rect))
        }
    } else {
        Some(Some(rect))
    }
}

fn axis_aligned_opaque_cover_rect(draw: &RasterDraw) -> Option<Rect> {
    if draw.ctm.b.abs() > MATRIX_EPSILON || draw.ctm.c.abs() > MATRIX_EPSILON || draw.clip_complex {
        return None;
    }
    let rect = Rect::from_ctm(draw.ctm);
    let rect = match draw.clip {
        Some(clip) => rect.intersection(clip)?,
        None => rect,
    };
    Some(rect)
}

fn intersect_scope(rect: Rect, scope: Option<Rect>) -> Option<Rect> {
    match scope {
        Some(scope) => rect.intersection(scope),
        None => Some(rect),
    }
}

struct HiddenRasterPruneResult {
    rewritten: Vec<u8>,
    transparent: usize,
    occluded: usize,
    removed: HashSet<usize>,
    visibility: HashMap<ObjectHandle, Option<SampleImage>>,
}

struct HiddenRasterPruneContext<'a> {
    scope: Option<Rect>,
    prune_occluded: bool,
    image_cache: &'a mut RasterImageDecodeCache,
    stats: &'a mut RasterLayoutStats,
}

fn prune_hidden_raster_paints(
    document: &EditDocument,
    resources: &OwnedDictionary,
    content: &[u8],
    scanner: &RasterScanner,
    context: HiddenRasterPruneContext<'_>,
) -> Result<HiddenRasterPruneResult> {
    let HiddenRasterPruneContext {
        scope,
        prune_occluded,
        image_cache,
        stats,
    } = context;
    if scanner.draws.is_empty() {
        return Ok(HiddenRasterPruneResult {
            rewritten: content.to_vec(),
            transparent: 0,
            occluded: 0,
            removed: HashSet::new(),
            visibility: HashMap::new(),
        });
    }
    let state_started = Instant::now();
    let states = image_paint_states(document, resources)?;
    stats.hidden_state_us = stats
        .hidden_state_us
        .saturating_add(state_started.elapsed().as_micros() as u64);
    let visibility_started = Instant::now();
    let mut visibility = HashMap::<ObjectHandle, Option<SampleImage>>::new();
    for draw in &scanner.draws {
        if visibility.contains_key(&draw.target) {
            continue;
        }
        visibility.insert(
            draw.target,
            image_cache.get_or_decode(document, draw.target)?,
        );
    }
    stats.hidden_visibility_us = stats
        .hidden_visibility_us
        .saturating_add(visibility_started.elapsed().as_micros() as u64);

    let coverage_started = Instant::now();
    let mut transparent = HashSet::new();
    for (index, draw) in scanner.draws.iter().enumerate() {
        let state = draw_paint_state(draw, &states);
        let mask_transparent = visibility
            .get(&draw.target)
            .and_then(Option::as_ref)
            .is_some_and(image_fully_transparent);
        if state.alpha <= ALPHA_INVISIBLE || mask_transparent {
            transparent.insert(index);
        }
    }

    let mut occluded = HashSet::new();
    let mut opaque_coverage = Vec::<OpaqueCover>::new();
    if prune_occluded {
        for paint in scanner.coverage_paints.iter().rev().copied() {
            match paint {
                CoveragePaint::RectFill(fill_index) => {
                    let fill = &scanner.rect_fills[fill_index];
                    if fill.clip_complex {
                        continue;
                    }
                    let state = fill
                        .gs_name
                        .as_ref()
                        .and_then(|name| states.get(name))
                        .copied()
                        .unwrap_or_default();
                    if state.alpha < ALPHA_OPAQUE || !state.normal_blend {
                        continue;
                    }
                    let cover = match fill.clip {
                        Some(clip) => fill.rect.intersection(clip),
                        None => Some(fill.rect),
                    }
                    .filter(|rect| {
                        fill.clip_polygon
                            .as_deref()
                            .is_none_or(|polygon| convex_polygon_contains_rect(polygon, *rect))
                    })
                    .and_then(|rect| intersect_scope(rect, scope));
                    if let Some(cover) = cover {
                        opaque_coverage.push(OpaqueCover {
                            rect: cover,
                            clip_polygon: fill.clip_polygon.clone(),
                        });
                    }
                }
                CoveragePaint::Image(index) => {
                    if transparent.contains(&index) {
                        continue;
                    }
                    let draw = &scanner.draws[index];
                    let Some(visible_rect) = axis_aligned_candidate_rect(draw) else {
                        continue;
                    };
                    let Some(visible_rect) =
                        visible_rect.and_then(|rect| intersect_scope(rect, scope))
                    else {
                        occluded.insert(index);
                        continue;
                    };
                    if target_covered_by_opaque_union(
                        visible_rect,
                        draw.clip_polygon.as_deref(),
                        &opaque_coverage,
                        1.0e-6,
                    ) {
                        occluded.insert(index);
                    }

                    let state = draw_paint_state(draw, &states);
                    if state.alpha < ALPHA_OPAQUE || !state.normal_blend {
                        continue;
                    }
                    let Some(image) = visibility.get(&draw.target).and_then(Option::as_ref) else {
                        continue;
                    };
                    if image_fully_opaque(image)
                        && let Some(cover_rect) = axis_aligned_opaque_cover_rect(draw)
                        && let Some(cover_rect) = intersect_scope(cover_rect, scope)
                    {
                        opaque_coverage.push(OpaqueCover {
                            rect: cover_rect,
                            clip_polygon: draw.clip_polygon.clone(),
                        });
                    }
                }
            }
        }
    }

    stats.hidden_coverage_us = stats
        .hidden_coverage_us
        .saturating_add(coverage_started.elapsed().as_micros() as u64);
    let rewrite_started = Instant::now();
    let mut removed = transparent.clone();
    removed.extend(occluded.iter().copied());
    let rewritten = remove_raster_draws(content, &scanner.draws, &removed)?;
    stats.hidden_rewrite_us = stats
        .hidden_rewrite_us
        .saturating_add(rewrite_started.elapsed().as_micros() as u64);
    Ok(HiddenRasterPruneResult {
        rewritten,
        transparent: transparent.len(),
        occluded: occluded.len(),
        removed,
        visibility,
    })
}

fn compress_flate(bytes: &[u8], level: i32) -> Result<Vec<u8>> {
    let level = u32::try_from(level.clamp(0, 9)).unwrap_or(9);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn pack_binary_samples(data: &[u8], width: u32, height: u32, components: usize) -> Option<Vec<u8>> {
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    let samples_per_row = width.checked_mul(components)?;
    if components == 0
        || data.len() != samples_per_row.checked_mul(height)?
        || !data.iter().all(|&value| matches!(value, 0 | 255))
    {
        return None;
    }

    // PDF image rows start on byte boundaries. Within each row, component
    // samples are packed consecutively MSB-first, including RGB/CMYK samples.
    let row_bytes = samples_per_row.div_ceil(8);
    let mut out = vec![0u8; row_bytes.checked_mul(height)?];
    for y in 0..height {
        let src_row = y * samples_per_row;
        let dst_row = y * row_bytes;
        for sample in 0..samples_per_row {
            if data[src_row + sample] == 255 {
                out[dst_row + sample / 8] |= 0x80 >> (sample % 8);
            }
        }
    }
    Some(out)
}

fn pack_binary_gray_samples(data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    pack_binary_samples(data, width, height, 1)
}

fn is_device_gray(dictionary: &OwnedDictionary) -> bool {
    matches!(
        dictionary.get(b"ColorSpace".as_slice()),
        Some(OwnedObject::Name(name)) if matches!(name.as_slice(), b"DeviceGray" | b"G")
    )
}

fn is_device_rgb(dictionary: &OwnedDictionary) -> bool {
    matches!(
        dictionary.get(b"ColorSpace".as_slice()),
        Some(OwnedObject::Name(name)) if matches!(name.as_slice(), b"DeviceRGB" | b"RGB")
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StencilColor {
    Gray(u8),
    Rgb([u8; 3]),
    Cmyk([u8; 4]),
}

impl StencilColor {
    fn content_operator(self) -> Vec<u8> {
        fn component(value: u8) -> String {
            let value = f64::from(value) / 255.0;
            let mut text = format!("{value:.6}");
            while text.contains('.') && text.ends_with('0') {
                text.pop();
            }
            if text.ends_with('.') {
                text.pop();
            }
            text
        }
        match self {
            Self::Gray(gray) => format!("{} g ", component(gray)).into_bytes(),
            Self::Rgb(rgb) => format!(
                "{} {} {} rg ",
                component(rgb[0]),
                component(rgb[1]),
                component(rgb[2])
            )
            .into_bytes(),
            Self::Cmyk(cmyk) => format!(
                "{} {} {} {} k ",
                component(cmyk[0]),
                component(cmyk[1]),
                component(cmyk[2]),
                component(cmyk[3])
            )
            .into_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ImageEncodingStats {
    binary_packed: bool,
    binary_mask_packed: bool,
    stencil_color: Option<StencilColor>,
    relaxed_stencil: bool,
    encoded_bytes_saved: u64,
}

fn constant_device_color(info: &SampleImage, data: &[u8]) -> Option<StencilColor> {
    if info.dictionary_template.contains_key(b"Decode".as_slice())
        || info.dictionary_template.contains_key(b"D".as_slice())
    {
        return None;
    }
    let color_space = match info.dictionary_template.get(b"ColorSpace".as_slice()) {
        Some(OwnedObject::Name(name)) => name.as_slice(),
        _ => return None,
    };
    let components = info.components;
    if data.is_empty() || !data.len().is_multiple_of(components) {
        return None;
    }
    let first = data.get(..components)?;
    if !data.chunks_exact(components).all(|pixel| pixel == first) {
        return None;
    }
    match (color_space, first) {
        (b"DeviceGray" | b"G", [gray]) => Some(StencilColor::Gray(*gray)),
        (b"DeviceRGB" | b"RGB", [r, g, b]) => Some(StencilColor::Rgb([*r, *g, *b])),
        (b"DeviceCMYK" | b"CMYK", [c, m, y, k]) => Some(StencilColor::Cmyk([*c, *m, *y, *k])),
        _ => None,
    }
}

fn constant_visible_device_color(
    info: &SampleImage,
    data: &[u8],
    alpha: &[u8],
) -> Option<StencilColor> {
    if info.dictionary_template.contains_key(b"Decode".as_slice())
        || info.dictionary_template.contains_key(b"D".as_slice())
    {
        return None;
    }
    let color_space = match info.dictionary_template.get(b"ColorSpace".as_slice()) {
        Some(OwnedObject::Name(name)) => name.as_slice(),
        _ => return None,
    };
    let components = info.components;
    if components == 0 || data.len() != alpha.len().checked_mul(components)? {
        return None;
    }

    let mut visible = data
        .chunks_exact(components)
        .zip(alpha)
        .filter_map(|(pixel, &alpha)| (alpha != 0).then_some(pixel));
    let first = visible.next()?;
    if !visible.all(|pixel| pixel == first) {
        return None;
    }
    match (color_space, first) {
        (b"DeviceGray" | b"G", [gray]) => Some(StencilColor::Gray(*gray)),
        (b"DeviceRGB" | b"RGB", [r, g, b]) => Some(StencilColor::Rgb([*r, *g, *b])),
        (b"DeviceCMYK" | b"CMYK", [c, m, y, k]) => Some(StencilColor::Cmyk([*c, *m, *y, *k])),
        _ => None,
    }
}

fn pack_binary_stencil_alpha(data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    if data.len() != width.checked_mul(height)? || !data.iter().all(|&v| matches!(v, 0 | 255)) {
        return None;
    }
    let row_bytes = width.div_ceil(8);
    let mut out = vec![0u8; row_bytes.checked_mul(height)?];
    for y in 0..height {
        for x in 0..width {
            // PDF ImageMask with default /Decode [0 1]: 0 paints, 1 is transparent.
            if data[y * width + x] == 0 {
                out[y * row_bytes + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    Some(out)
}

#[derive(Debug, Clone, Copy)]
struct ImageEncodingContext {
    flate_level: i32,
    exact_raster_rendering: bool,
    source_color_budget: Option<usize>,
}

#[derive(Debug)]
struct PreparedAlpha {
    data: Vec<u8>,
    bits_per_component: i64,
}

#[derive(Debug)]
struct PreparedImage {
    dictionary: OwnedDictionary,
    data: Vec<u8>,
    alpha: Option<PreparedAlpha>,
    encoding: ImageEncodingStats,
}

impl PreparedImage {
    fn payload_bytes(&self) -> usize {
        self.data
            .len()
            .saturating_add(self.alpha.as_ref().map_or(0, |alpha| alpha.data.len()))
    }
}

fn prepare_image(
    info: &SampleImage,
    width: u32,
    height: u32,
    data: &[u8],
    alpha: Option<Vec<u8>>,
    context: ImageEncodingContext,
) -> Result<PreparedImage> {
    let ImageEncodingContext {
        flate_level,
        exact_raster_rendering,
        source_color_budget,
    } = context;
    let mut encoded_8bit = None;
    let mut encoding = ImageEncodingStats::default();

    // A constant Device* color behind strictly binary alpha is exactly a PDF
    // stencil image. In normal mode we also allow samples hidden by alpha=0
    // to differ: that can change only resampled antialiasing, and avoids an
    // otherwise redundant color plane. Exact mode keeps those samples.
    if let Some(alpha) = alpha.as_deref()
        && let Some(stencil) = pack_binary_stencil_alpha(alpha, width, height)
        && let Some((color, relaxed_stencil)) = constant_device_color(info, data)
            .map(|color| (color, false))
            .or_else(|| {
                (!exact_raster_rendering)
                    .then(|| constant_visible_device_color(info, data, alpha))
                    .flatten()
                    .map(|color| (color, true))
            })
    {
        let stencil_encoded = compress_flate(&stencil, flate_level)?;
        let alpha_8bit_encoded = compress_flate(alpha, flate_level)?;
        // Include the tiny color operator in the comparison. Object/dictionary
        // overhead further favors the stencil because it replaces two streams.
        let stencil_cost = stencil_encoded
            .len()
            .saturating_add(color.content_operator().len());
        // If the stencil already beats the compressed alpha plane by itself,
        // adding any encoded color payload cannot change the winner. Avoid
        // feeding the full 8-bit color plane through Flate in that common case.
        let normal_cost = if stencil_cost < alpha_8bit_encoded.len() {
            alpha_8bit_encoded.len()
        } else {
            let encoded = compress_flate(data, flate_level)?;
            let cost = encoded.len().saturating_add(alpha_8bit_encoded.len());
            encoded_8bit = Some(encoded);
            cost
        };
        if stencil_cost < normal_cost {
            let dictionary = BTreeMap::from([
                (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
                (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
                (b"Width".to_vec(), OwnedObject::Integer(i64::from(width))),
                (b"Height".to_vec(), OwnedObject::Integer(i64::from(height))),
                (b"ImageMask".to_vec(), OwnedObject::Boolean(true)),
                (b"BitsPerComponent".to_vec(), OwnedObject::Integer(1)),
                (
                    b"Filter".to_vec(),
                    OwnedObject::Name(b"FlateDecode".to_vec()),
                ),
            ]);
            encoding.stencil_color = Some(color);
            encoding.binary_packed = true;
            encoding.relaxed_stencil = relaxed_stencil;
            encoding.encoded_bytes_saved =
                u64::try_from(normal_cost - stencil_cost).unwrap_or(u64::MAX);
            return Ok(PreparedImage {
                dictionary,
                data: stencil_encoded,
                alpha: None,
                encoding,
            });
        }
    }

    let encoded_8bit = match encoded_8bit {
        Some(encoded) => encoded,
        None => compress_flate(data, flate_level)?,
    };

    let mut dictionary = info.dictionary_template.clone();
    dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec()));
    dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec()));
    dictionary.insert(b"Width".to_vec(), OwnedObject::Integer(i64::from(width)));
    dictionary.insert(b"Height".to_vec(), OwnedObject::Integer(i64::from(height)));
    dictionary.insert(
        b"Filter".to_vec(),
        OwnedObject::Name(b"FlateDecode".to_vec()),
    );
    dictionary.remove(b"DecodeParms".as_slice());

    let prepared_alpha = if let Some(alpha) = alpha {
        let alpha_8bit = compress_flate(&alpha, flate_level)?;
        let (alpha_data, alpha_bpc) =
            if let Some(packed) = pack_binary_gray_samples(&alpha, width, height) {
                let encoded = compress_flate(&packed, flate_level)?;
                if encoded.len() < alpha_8bit.len() {
                    encoding.binary_mask_packed = true;
                    encoding.encoded_bytes_saved = encoding.encoded_bytes_saved.saturating_add(
                        u64::try_from(alpha_8bit.len() - encoded.len()).unwrap_or(u64::MAX),
                    );
                    (encoded, 1)
                } else {
                    (alpha_8bit, 8)
                }
            } else {
                (alpha_8bit, 8)
            };
        Some(PreparedAlpha {
            data: alpha_data,
            bits_per_component: alpha_bpc,
        })
    } else {
        None
    };

    let binary_components = if info.components == 1 && is_device_gray(&dictionary) {
        Some(1)
    } else if info.components == 3
        && is_device_rgb(&dictionary)
        && !dictionary.contains_key(b"Decode".as_slice())
        && !dictionary.contains_key(b"D".as_slice())
    {
        // Preserve DeviceRGB and its separate SMask exactly; only reduce each
        // binary component sample from 8 bits to 1 bit. Unlike stencil
        // conversion, this keeps color-plane interpolation semantics intact.
        Some(3)
    } else {
        None
    };
    let mut encoded = if let Some(components) = binary_components
        && let Some(packed) = pack_binary_samples(data, width, height, components)
    {
        let encoded_1bit = compress_flate(&packed, flate_level)?;
        if encoded_1bit.len() < encoded_8bit.len() {
            dictionary.insert(b"BitsPerComponent".to_vec(), OwnedObject::Integer(1));
            encoding.binary_packed = true;
            encoding.encoded_bytes_saved = encoding.encoded_bytes_saved.saturating_add(
                u64::try_from(encoded_8bit.len() - encoded_1bit.len()).unwrap_or(u64::MAX),
            );
            encoded_1bit
        } else {
            dictionary.insert(b"BitsPerComponent".to_vec(), OwnedObject::Integer(8));
            encoded_8bit
        }
    } else {
        dictionary.insert(b"BitsPerComponent".to_vec(), OwnedObject::Integer(8));
        encoded_8bit
    };

    // Reconstructed DCT/JPX-backed color should stay compact. Start at q85.
    // When a stripe plan knows the aggregate source color-stream budget, step
    // quality down only as far as needed to fit that budget. This keeps already
    // good q85 results unchanged while avoiding a merged JPEG that is larger
    // than the compact strips it replaces.
    if info.preserve_encoded_color
        && !encoding.binary_packed
        && matches!(info.components, 1 | 3 | 4)
    {
        let jpeg_q85 = flpdf::job::encode_jpeg_raster(
            width,
            height,
            info.components,
            data,
            RECONSTRUCTED_JPEG_QUALITY,
        )?;
        let mut best_jpeg = (jpeg_q85.len() < encoded.len()).then_some(jpeg_q85);
        if let (Some(budget), Some(q85)) = (source_color_budget, best_jpeg.as_ref())
            && q85.len() > budget
        {
            for quality in [82, 80, 78, 75] {
                let jpeg =
                    flpdf::job::encode_jpeg_raster(width, height, info.components, data, quality)?;
                if jpeg.len() < best_jpeg.as_ref().map_or(usize::MAX, Vec::len) {
                    let fits_source = jpeg.len() <= budget;
                    best_jpeg = Some(jpeg);
                    if fits_source {
                        break;
                    }
                }
            }
        }
        if let Some(jpeg) = best_jpeg {
            dictionary.insert(b"Filter".to_vec(), OwnedObject::Name(b"DCTDecode".to_vec()));
            dictionary.insert(b"BitsPerComponent".to_vec(), OwnedObject::Integer(8));
            encoded = jpeg;
        }
    }

    Ok(PreparedImage {
        dictionary,
        data: encoded,
        alpha: prepared_alpha,
        encoding,
    })
}

fn install_prepared_image(
    document: &mut EditDocument,
    mut prepared: PreparedImage,
    width: u32,
    height: u32,
) -> (ObjectHandle, ImageEncodingStats) {
    if let Some(alpha) = prepared.alpha.take() {
        let alpha_stream = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
            dictionary: BTreeMap::from([
                (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
                (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
                (b"Width".to_vec(), OwnedObject::Integer(i64::from(width))),
                (b"Height".to_vec(), OwnedObject::Integer(i64::from(height))),
                (
                    b"ColorSpace".to_vec(),
                    OwnedObject::Name(b"DeviceGray".to_vec()),
                ),
                (
                    b"BitsPerComponent".to_vec(),
                    OwnedObject::Integer(alpha.bits_per_component),
                ),
                (
                    b"Filter".to_vec(),
                    OwnedObject::Name(b"FlateDecode".to_vec()),
                ),
            ]),
            data: StreamData::Owned(alpha.data),
        }));
        prepared
            .dictionary
            .insert(b"SMask".to_vec(), OwnedObject::Reference(alpha_stream));
    }
    let handle = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
        dictionary: prepared.dictionary,
        data: StreamData::Owned(prepared.data),
    }));
    (handle, prepared.encoding)
}

#[derive(Debug, Clone, Copy)]
struct Basis {
    ux: (f64, f64),
    uy: (f64, f64),
}

fn basis(draw: &RasterDraw, image: &SampleImage) -> Option<Basis> {
    let ux = (
        draw.ctm.a / f64::from(image.width),
        draw.ctm.b / f64::from(image.width),
    );
    let uy = (
        draw.ctm.c / f64::from(image.height),
        draw.ctm.d / f64::from(image.height),
    );
    let det = ux.0 * uy.1 - ux.1 * uy.0;
    (det.abs() > MATRIX_EPSILON && det.is_finite()).then_some(Basis { ux, uy })
}

fn rel_close(a: f64, b: f64, rel: f64) -> bool {
    (a - b).abs() <= rel * a.abs().max(b.abs()).max(1.0e-12)
}

fn basis_close(a: Basis, b: Basis) -> bool {
    rel_close(a.ux.0, b.ux.0, BASIS_REL_EPSILON)
        && rel_close(a.ux.1, b.ux.1, BASIS_REL_EPSILON)
        && rel_close(a.uy.0, b.uy.0, BASIS_REL_EPSILON)
        && rel_close(a.uy.1, b.uy.1, BASIS_REL_EPSILON)
}

fn basis_coordinates(basis: Basis, dx: f64, dy: f64) -> Option<(f64, f64)> {
    let det = basis.ux.0 * basis.uy.1 - basis.ux.1 * basis.uy.0;
    if det.abs() <= MATRIX_EPSILON || !det.is_finite() {
        return None;
    }
    Some((
        (dx * basis.uy.1 - dy * basis.uy.0) / det,
        (dy * basis.ux.0 - dx * basis.ux.1) / det,
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StripeAxis {
    Horizontal,
    Vertical,
}

fn stripe_link(
    a: &RasterDraw,
    b: &RasterDraw,
    ia: &SampleImage,
    ib: &SampleImage,
    max_gap_pixels: f64,
) -> Option<StripeAxis> {
    if a.epoch != b.epoch
        || a.paint_generation != b.paint_generation
        || a.gs_name != b.gs_name
        || a.rendering_intent != b.rendering_intent
        || ia.semantic_key != ib.semantic_key
        || ia.interpolate
        || ib.interpolate
        || (ia.width <= 5 && ia.height <= 5)
        || (ib.width <= 5 && ib.height <= 5)
    {
        return None;
    }
    let ba = basis(a, ia)?;
    let bb = basis(b, ib)?;
    if !basis_close(ba, bb) {
        return None;
    }
    let (dx, dy) = basis_coordinates(ba, b.ctm.e - a.ctm.e, b.ctm.f - a.ctm.f)?;
    let tol = max_gap_pixels.max(0.0);

    let vertical_shape = ia.width == ib.width
        && u64::from(ia.height) * 2 <= u64::from(ia.width)
        && u64::from(ib.height) * 2 <= u64::from(ib.width);
    if vertical_shape
        && dx.abs() <= tol
        && ((dy - f64::from(ia.height)).abs() <= tol || (dy + f64::from(ib.height)).abs() <= tol)
    {
        return Some(StripeAxis::Vertical);
    }

    let horizontal_shape = ia.height == ib.height
        && u64::from(ia.width) * 2 <= u64::from(ia.height)
        && u64::from(ib.width) * 2 <= u64::from(ib.height);
    if horizontal_shape
        && dy.abs() <= tol
        && ((dx - f64::from(ia.width)).abs() <= tol || (dx + f64::from(ib.width)).abs() <= tol)
    {
        return Some(StripeAxis::Horizontal);
    }
    None
}

#[derive(Debug)]
struct BackgroundPaint {
    data: Vec<u8>,
    desired_ctm: Matrix,
}

#[derive(Debug)]
struct MergePlan {
    members: Vec<usize>,
    first_member: usize,
    image: SampleImage,
    width: u32,
    height: u32,
    data: Vec<u8>,
    alpha: Option<Vec<u8>>,
    source_color_budget: Option<usize>,
    desired_ctm: Matrix,
    background: Option<BackgroundPaint>,
    alpha_crop_hint: Option<PixelCrop>,
    kind: MergeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MergeKind {
    Stripe,
    PixelCluster,
    NativeFragment,
    AlphaCrop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PixelCrop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

fn crop_total_margin_pixels(width: u32, height: u32, crop: PixelCrop) -> Option<u32> {
    let removed_x = width.checked_sub(crop.width)?;
    let removed_y = height.checked_sub(crop.height)?;
    removed_x.checked_add(removed_y)
}

fn crop_is_worthwhile(width: u32, height: u32, crop: PixelCrop) -> bool {
    crop_total_margin_pixels(width, height, crop)
        .is_some_and(|removed| removed >= MIN_TOTAL_CROP_MARGIN_PIXELS)
}

#[derive(Debug, Clone, Copy)]
struct AlphaCropCacheEntry {
    image: ObjectHandle,
    encoding: ImageEncodingStats,
    source_crop: PixelCrop,
    fast_reuse: bool,
}

fn crop_samples(
    data: &[u8],
    width: u32,
    height: u32,
    components: usize,
    crop: PixelCrop,
) -> Option<Vec<u8>> {
    if crop.x.checked_add(crop.width)? > width || crop.y.checked_add(crop.height)? > height {
        return None;
    }
    let row_bytes = usize::try_from(crop.width).ok()?.checked_mul(components)?;
    let mut out = Vec::with_capacity(row_bytes.checked_mul(usize::try_from(crop.height).ok()?)?);
    for y in crop.y..crop.y.checked_add(crop.height)? {
        let start_pixel = u64::from(y)
            .checked_mul(u64::from(width))?
            .checked_add(u64::from(crop.x))?;
        let start = usize::try_from(start_pixel).ok()?.checked_mul(components)?;
        out.extend_from_slice(data.get(start..start.checked_add(row_bytes)?)?);
    }
    Some(out)
}

fn alpha_crop(alpha: &[u8], width: u32, height: u32) -> Option<PixelCrop> {
    let mut left = width;
    let mut top = height;
    let mut right = 0u32;
    let mut bottom = 0u32;
    let mut any = false;
    for y in 0..height {
        for x in 0..width {
            let index = usize::try_from(u64::from(y) * u64::from(width) + u64::from(x)).ok()?;
            if *alpha.get(index)? == 0 {
                continue;
            }
            any = true;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    any.then_some(PixelCrop {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

fn dominant_border_sample(
    data: &[u8],
    width: u32,
    height: u32,
    components: usize,
) -> Option<Vec<u8>> {
    if width == 0 || height == 0 || components == 0 {
        return None;
    }
    let mut counts = HashMap::<Vec<u8>, usize>::new();
    let mut samples = 0usize;
    let mut add = |x: u32, y: u32| -> Option<()> {
        let pixel = usize::try_from(u64::from(y) * u64::from(width) + u64::from(x)).ok()?;
        let start = pixel.checked_mul(components)?;
        let sample = data.get(start..start.checked_add(components)?)?.to_vec();
        *counts.entry(sample).or_default() += 1;
        samples += 1;
        Some(())
    };
    for x in 0..width {
        add(x, 0)?;
        if height > 1 {
            add(x, height - 1)?;
        }
    }
    for y in 1..height.saturating_sub(1) {
        add(0, y)?;
        if width > 1 {
            add(width - 1, y)?;
        }
    }
    let (sample, count) = counts.into_iter().max_by_key(|(_, count)| *count)?;
    (count.saturating_mul(100) >= samples.saturating_mul(35)).then_some(sample)
}

fn background_crop(
    data: &[u8],
    width: u32,
    height: u32,
    components: usize,
    background: &[u8],
    tolerance: u8,
) -> Option<Option<PixelCrop>> {
    let mut left = width;
    let mut top = height;
    let mut right = 0u32;
    let mut bottom = 0u32;
    let mut any = false;
    for y in 0..height {
        for x in 0..width {
            let pixel = usize::try_from(u64::from(y) * u64::from(width) + u64::from(x)).ok()?;
            let start = pixel.checked_mul(components)?;
            let sample = data.get(start..start.checked_add(components)?)?;
            let different = sample
                .iter()
                .zip(background)
                .any(|(&a, &b)| a.abs_diff(b) > tolerance);
            if !different {
                continue;
            }
            any = true;
            left = left.min(x);
            top = top.min(y);
            right = right.max(x + 1);
            bottom = bottom.max(y + 1);
        }
    }
    if !any {
        return Some(None);
    }
    Some(Some(PixelCrop {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }))
}

fn crop_plan_geometry_to(plan: &mut MergePlan, crop: PixelCrop) -> Option<u64> {
    if crop.x == 0 && crop.y == 0 && crop.width == plan.width && crop.height == plan.height {
        return Some(0);
    }
    let old_width = plan.width;
    let old_height = plan.height;
    let old_pixels = u64::from(old_width).checked_mul(u64::from(old_height))?;
    let new_pixels = u64::from(crop.width).checked_mul(u64::from(crop.height))?;
    let bottom_margin = old_height.checked_sub(crop.y.checked_add(crop.height)?)?;
    let x = f64::from(crop.x) / f64::from(old_width);
    let y = f64::from(bottom_margin) / f64::from(old_height);
    let sx = f64::from(crop.width) / f64::from(old_width);
    let sy = f64::from(crop.height) / f64::from(old_height);
    let old = plan.desired_ctm;
    plan.desired_ctm = Matrix::new(
        old.a * sx,
        old.b * sx,
        old.c * sy,
        old.d * sy,
        old.e + old.a * x + old.c * y,
        old.f + old.b * x + old.d * y,
    );
    plan.width = crop.width;
    plan.height = crop.height;
    old_pixels.checked_sub(new_pixels)
}

fn crop_plan_to(plan: &mut MergePlan, crop: PixelCrop) -> Option<u64> {
    let old_width = plan.width;
    let old_height = plan.height;
    let removed_pixels = crop_plan_geometry_to(plan, crop)?;
    plan.data = crop_samples(
        &plan.data,
        old_width,
        old_height,
        plan.image.components,
        crop,
    )?;
    if let Some(alpha) = plan.alpha.take() {
        let cropped = crop_samples(&alpha, old_width, old_height, 1, crop)?;
        if cropped.iter().any(|&sample| sample != 255) {
            plan.alpha = Some(cropped);
        }
    }
    Some(removed_pixels)
}

fn crop_merge_plan(plan: &mut MergePlan, config: &RasterLayoutConfig) -> Option<(bool, bool, u64)> {
    let mut transparent_cropped = false;
    let mut background_cropped = false;
    let mut removed_pixels = 0u64;

    if plan.kind == MergeKind::AlphaCrop && plan.data.is_empty() {
        plan.data = plan.image.data.to_vec();
        plan.alpha = plan.image.alpha.as_ref().map(|alpha| alpha.data.to_vec());
    }

    let transparent_crop = plan.alpha_crop_hint.take().or_else(|| {
        plan.alpha
            .as_ref()
            .and_then(|alpha| alpha_crop(alpha, plan.width, plan.height))
    });
    if config.crop_transparent
        && let Some(crop) = transparent_crop
        && crop_is_worthwhile(plan.width, plan.height, crop)
    {
        removed_pixels = removed_pixels.checked_add(crop_plan_to(plan, crop)?)?;
        transparent_cropped = true;
    }

    let alpha_opaque = plan
        .alpha
        .as_ref()
        .is_none_or(|alpha| alpha.iter().all(|&sample| sample == 255));
    if config.crop_background && alpha_opaque {
        let background =
            dominant_border_sample(&plan.data, plan.width, plan.height, plan.image.components)?;
        match background_crop(
            &plan.data,
            plan.width,
            plan.height,
            plan.image.components,
            &background,
            config.background_tolerance,
        )? {
            None if crop_is_worthwhile(
                plan.width,
                plan.height,
                PixelCrop {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                },
            ) =>
            {
                let old_pixels = u64::from(plan.width).checked_mul(u64::from(plan.height))?;
                plan.width = 1;
                plan.height = 1;
                plan.data = background;
                plan.alpha = None;
                removed_pixels = removed_pixels.checked_add(old_pixels.saturating_sub(1))?;
                background_cropped = old_pixels > 1;
            }
            Some(crop) if crop_is_worthwhile(plan.width, plan.height, crop) => {
                let original_ctm = plan.desired_ctm;
                let old_pixels = crop_plan_to(plan, crop)?;
                plan.background = Some(BackgroundPaint {
                    data: background,
                    desired_ctm: original_ctm,
                });
                plan.alpha = None;
                removed_pixels = removed_pixels.checked_add(old_pixels)?;
                background_cropped = true;
            }
            None | Some(_) => {}
        }
    }
    Some((transparent_cropped, background_cropped, removed_pixels))
}

fn build_stripe_plan(
    draws: &[RasterDraw],
    infos: &HashMap<ObjectHandle, SampleImage>,
    members: &[usize],
    axis: StripeAxis,
    max_pixels: u64,
) -> Option<MergePlan> {
    let first_draw = draws.get(*members.first()?)?;
    let first_info = infos.get(&first_draw.target)?;
    let b = basis(first_draw, first_info)?;
    let mut placements = Vec::with_capacity(members.len());
    for &index in members {
        let draw = draws.get(index)?;
        let info = infos.get(&draw.target)?;
        if info.semantic_key != first_info.semantic_key || !basis_close(b, basis(draw, info)?) {
            return None;
        }
        if info
            .alpha
            .as_ref()
            .is_some_and(|a| a.width != info.width || a.height != info.height)
        {
            return None;
        }
        let (x, y) = basis_coordinates(
            b,
            draw.ctm.e - first_draw.ctm.e,
            draw.ctm.f - first_draw.ctm.f,
        )?;
        placements.push((index, x, y));
    }

    let (min_x, min_y, max_x, max_y) = match axis {
        StripeAxis::Vertical => {
            let min_y = placements
                .iter()
                .map(|(_, _, y)| *y)
                .fold(f64::INFINITY, f64::min);
            let mut max_y = f64::NEG_INFINITY;
            for (index, _, y) in &placements {
                let info = infos.get(&draws[*index].target)?;
                max_y = max_y.max(*y + f64::from(info.height));
            }
            (0.0, min_y, f64::from(first_info.width), max_y)
        }
        StripeAxis::Horizontal => {
            let min_x = placements
                .iter()
                .map(|(_, x, _)| *x)
                .fold(f64::INFINITY, f64::min);
            let mut max_x = f64::NEG_INFINITY;
            for (index, x, _) in &placements {
                let info = infos.get(&draws[*index].target)?;
                max_x = max_x.max(*x + f64::from(info.width));
            }
            (min_x, 0.0, max_x, f64::from(first_info.height))
        }
    };
    let width = (max_x - min_x).round();
    let height = (max_y - min_y).round();
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return None;
    }
    let width = u32::try_from(width as u64).ok()?;
    let height = u32::try_from(height as u64).ok()?;
    if u64::from(width).checked_mul(u64::from(height))? > max_pixels {
        return None;
    }
    let pixel_count = usize::try_from(u64::from(width) * u64::from(height)).ok()?;
    let mut data = vec![0; pixel_count.checked_mul(first_info.components)?];
    let needs_alpha = members.iter().any(|index| {
        infos
            .get(&draws[*index].target)
            .is_some_and(|i| i.alpha.is_some())
    });
    let mut alpha = needs_alpha.then(|| vec![0; pixel_count]);

    for (index, x, y) in placements {
        let info = infos.get(&draws[index].target)?;
        let x = (x - min_x).round() as i64;
        let y = (y - min_y).round() as i64;
        if x < 0 || y < 0 {
            return None;
        }
        let x = u32::try_from(x).ok()?;
        let y = u32::try_from(y).ok()?;
        if x.checked_add(info.width)? > width || y.checked_add(info.height)? > height {
            return None;
        }
        // PDF image samples are top-down while our placement y coordinate is bottom-up.
        let top = height.checked_sub(y.checked_add(info.height)?)?;
        for row in 0..info.height {
            let src = usize::try_from(u64::from(row) * u64::from(info.width))
                .ok()?
                .checked_mul(info.components)?;
            let dst = usize::try_from(
                (u64::from(top + row) * u64::from(width) + u64::from(x))
                    * u64::try_from(info.components).ok()?,
            )
            .ok()?;
            let len = usize::try_from(info.width)
                .ok()?
                .checked_mul(info.components)?;
            data.get_mut(dst..dst + len)?
                .copy_from_slice(info.data.get(src..src + len)?);
        }
        if let Some(out_alpha) = alpha.as_mut() {
            for row in 0..info.height {
                let src = usize::try_from(u64::from(row) * u64::from(info.width)).ok()?;
                let dst =
                    usize::try_from(u64::from(top + row) * u64::from(width) + u64::from(x)).ok()?;
                let len = usize::try_from(info.width).ok()?;
                if let Some(source) = info.alpha.as_ref() {
                    out_alpha
                        .get_mut(dst..dst + len)?
                        .copy_from_slice(source.data.get(src..src + len)?);
                } else {
                    out_alpha.get_mut(dst..dst + len)?.fill(255);
                }
            }
        }
    }

    let origin = (
        first_draw.ctm.e + b.ux.0 * min_x + b.uy.0 * min_y,
        first_draw.ctm.f + b.ux.1 * min_x + b.uy.1 * min_y,
    );
    let desired_ctm = Matrix::new(
        b.ux.0 * f64::from(width),
        b.ux.1 * f64::from(width),
        b.uy.0 * f64::from(height),
        b.uy.1 * f64::from(height),
        origin.0,
        origin.1,
    );
    let first_member = *members.iter().min()?;
    let mut source_images = HashSet::new();
    let source_color_budget = members.iter().try_fold(0usize, |total, index| {
        let target = draws[*index].target;
        if !source_images.insert(target) {
            return Some(total);
        }
        let info = infos.get(&target)?;
        total.checked_add(info.encoded_color_bytes)
    })?;
    Some(MergePlan {
        members: members.to_vec(),
        first_member,
        image: first_info.clone(),
        width,
        height,
        data,
        alpha,
        source_color_budget: first_info
            .preserve_encoded_color
            .then_some(source_color_budget),
        desired_ctm,
        background: None,
        alpha_crop_hint: None,
        kind: MergeKind::Stripe,
    })
}

fn find_stripe_plans(
    draws: &[RasterDraw],
    infos: &HashMap<ObjectHandle, SampleImage>,
    used: &HashSet<usize>,
    config: &RasterLayoutConfig,
) -> Vec<MergePlan> {
    if !config.merge_stripes {
        return Vec::new();
    }
    let mut plans = Vec::new();
    let mut index = 0usize;
    while index < draws.len() {
        if used.contains(&index) {
            index += 1;
            continue;
        }
        let Some(info) = infos.get(&draws[index].target) else {
            index += 1;
            continue;
        };
        if info.interpolate {
            index += 1;
            continue;
        }
        let mut members = vec![index];
        let mut axis = None;
        let mut next = index + 1;
        while next < draws.len() && !used.contains(&next) {
            let Some(next_info) = infos.get(&draws[next].target) else {
                break;
            };
            let Some(link) = stripe_link(
                &draws[*members.last().unwrap_or(&index)],
                &draws[next],
                infos
                    .get(&draws[*members.last().unwrap_or(&index)].target)
                    .unwrap_or(info),
                next_info,
                f64::from(config.stripe_max_gap_pixels),
            ) else {
                break;
            };
            if axis.is_some_and(|axis| axis != link) {
                break;
            }
            axis = Some(link);
            members.push(next);
            next += 1;
        }
        if members.len() >= config.stripe_min_paints
            && let Some(axis) = axis
            && let Some(plan) = build_stripe_plan(
                draws,
                infos,
                &members,
                axis,
                config.max_reconstructed_pixels,
            )
        {
            plans.push(plan);
            index = next;
            continue;
        }
        index += 1;
    }
    plans
}

#[derive(Debug, Clone, Copy)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Rect {
    fn from_ctm(ctm: Matrix) -> Self {
        let points = [
            ctm.transform(0.0, 0.0),
            ctm.transform(1.0, 0.0),
            ctm.transform(0.0, 1.0),
            ctm.transform(1.0, 1.0),
        ];
        Self {
            x0: points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
            y0: points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
            x1: points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
            y1: points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }

    fn gap(self, other: Self) -> f64 {
        let dx = if self.x1 < other.x0 {
            other.x0 - self.x1
        } else if other.x1 < self.x0 {
            self.x0 - other.x1
        } else {
            0.0
        };
        let dy = if self.y1 < other.y0 {
            other.y0 - self.y1
        } else if other.y1 < self.y0 {
            self.y0 - other.y1
        } else {
            0.0
        };
        dx.hypot(dy)
    }

    fn intersects(self, other: Self) -> bool {
        self.x0 < other.x1 && self.x1 > other.x0 && self.y0 < other.y1 && self.y1 > other.y0
    }

    fn intersection(self, other: Self) -> Option<Self> {
        let out = Self {
            x0: self.x0.max(other.x0),
            y0: self.y0.max(other.y0),
            x1: self.x1.min(other.x1),
            y1: self.y1.min(other.y1),
        };
        (out.x1 > out.x0 && out.y1 > out.y0).then_some(out)
    }

    fn contains(self, other: Self, epsilon: f64) -> bool {
        self.x0 <= other.x0 + epsilon
            && self.y0 <= other.y0 + epsilon
            && self.x1 + epsilon >= other.x1
            && self.y1 + epsilon >= other.y1
    }
}

#[derive(Debug)]
struct Dsu {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Dsu {
    fn new(len: usize) -> Self {
        Self {
            parent: (0..len).collect(),
            size: vec![1; len],
        }
    }

    fn find(&mut self, index: usize) -> usize {
        if self.parent[index] != index {
            self.parent[index] = self.find(self.parent[index]);
        }
        self.parent[index]
    }

    fn union(&mut self, a: usize, b: usize) {
        let mut a = self.find(a);
        let mut b = self.find(b);
        if a == b {
            return;
        }
        if self.size[a] < self.size[b] {
            std::mem::swap(&mut a, &mut b);
        }
        self.parent[b] = a;
        self.size[a] += self.size[b];
    }
}

fn median(mut values: Vec<f64>) -> Option<f64> {
    values.retain(|value| value.is_finite() && *value > MATRIX_EPSILON);
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    Some(values[values.len() / 2])
}

fn resize_nearest(
    source: &[u8],
    source_size: (u32, u32),
    target_size: (u32, u32),
    channels: usize,
    flips: (bool, bool),
) -> Option<Vec<u8>> {
    let (source_width, source_height) = source_size;
    let (target_width, target_height) = target_size;
    let (flip_x, flip_y) = flips;
    let mut out = vec![
        0;
        usize::try_from(u64::from(target_width) * u64::from(target_height))
            .ok()?
            .checked_mul(channels)?
    ];
    for y in 0..target_height {
        let mut sy =
            u32::try_from(u64::from(y) * u64::from(source_height) / u64::from(target_height))
                .ok()?;
        if flip_y {
            sy = source_height - 1 - sy;
        }
        for x in 0..target_width {
            let mut sx =
                u32::try_from(u64::from(x) * u64::from(source_width) / u64::from(target_width))
                    .ok()?;
            if flip_x {
                sx = source_width - 1 - sx;
            }
            let src = usize::try_from(
                (u64::from(sy) * u64::from(source_width) + u64::from(sx))
                    * u64::try_from(channels).ok()?,
            )
            .ok()?;
            let dst = usize::try_from(
                (u64::from(y) * u64::from(target_width) + u64::from(x))
                    * u64::try_from(channels).ok()?,
            )
            .ok()?;
            out.get_mut(dst..dst + channels)?
                .copy_from_slice(source.get(src..src + channels)?);
        }
    }
    Some(out)
}

fn build_pixel_cluster_plan(
    draws: &[RasterDraw],
    infos: &HashMap<ObjectHandle, SampleImage>,
    members: &[usize],
    config: &RasterLayoutConfig,
    native_pitch: Option<(f64, f64)>,
) -> Option<MergePlan> {
    let debug = *DEBUG_RASTER;
    let first = *members.first()?;
    let first_info = infos.get(&draws[first].target)?;
    let mut bounds = Rect::from_ctm(draws[first].ctm);
    let mut pitch_x = Vec::new();
    let mut pitch_y = Vec::new();
    let mut min_extent_x = f64::INFINITY;
    let mut min_extent_y = f64::INFINITY;
    for &index in members {
        let draw = draws.get(index)?;
        let info = infos.get(&draw.target)?;
        if info.components != first_info.components
            || info.dictionary_template != first_info.dictionary_template
            || info.interpolate
            || draw.ctm.b.abs() > MATRIX_EPSILON
            || draw.ctm.c.abs() > MATRIX_EPSILON
            || info.mask_width > info.width
            || info.mask_height > info.height
            || info
                .alpha
                .as_ref()
                .is_some_and(|alpha| alpha.width != info.width || alpha.height != info.height)
        {
            if debug {
                eprintln!(
                    "pixel-build reject=source-compat index={index} size={}x{} mask={}x{} alpha={:?}",
                    info.width,
                    info.height,
                    info.mask_width,
                    info.mask_height,
                    info.alpha.as_ref().map(|a| (a.width, a.height))
                );
            }
            return None;
        }
        let rect = Rect::from_ctm(draw.ctm);
        bounds = bounds.union(rect);
        let extent_x = rect.x1 - rect.x0;
        let extent_y = rect.y1 - rect.y0;
        min_extent_x = min_extent_x.min(extent_x);
        min_extent_y = min_extent_y.min(extent_y);
        pitch_x.push(extent_x / f64::from(info.width));
        pitch_y.push(extent_y / f64::from(info.height));
    }
    let (pitch_x, pitch_y) = native_pitch.unwrap_or((
        median(pitch_x)?.min(min_extent_x),
        median(pitch_y)?.min(min_extent_y),
    ));
    if !pitch_x.is_finite()
        || !pitch_y.is_finite()
        || pitch_x <= MATRIX_EPSILON
        || pitch_y <= MATRIX_EPSILON
    {
        return None;
    }
    if debug {
        eprintln!(
            "pixel-build n={} bounds=({:.6},{:.6})-({:.6},{:.6}) pitch=({:.9},{:.9})",
            members.len(),
            bounds.x0,
            bounds.y0,
            bounds.x1,
            bounds.y1,
            pitch_x,
            pitch_y
        );
    }
    let width = ((bounds.x1 - bounds.x0) / pitch_x).round().max(1.0);
    let height = ((bounds.y1 - bounds.y0) / pitch_y).round().max(1.0);
    if !width.is_finite() || !height.is_finite() {
        return None;
    }
    let width = u32::try_from(width as u64).ok()?;
    let height = u32::try_from(height as u64).ok()?;
    let pixels = u64::from(width).checked_mul(u64::from(height))?;
    if pixels > config.max_reconstructed_pixels {
        return None;
    }
    let mut data = vec![
        0;
        usize::try_from(pixels)
            .ok()?
            .checked_mul(first_info.components)?
    ];
    let mut alpha = vec![0; usize::try_from(pixels).ok()?];
    let mut has_partial_overlap = false;

    for &index in members {
        let draw = &draws[index];
        let info = infos.get(&draw.target)?;
        let rect = Rect::from_ctm(draw.ctm);
        let x0 = ((rect.x0 - bounds.x0) / pitch_x).round() as i64;
        let x1 = ((rect.x1 - bounds.x0) / pitch_x).round() as i64;
        let y_from_top = ((bounds.y1 - rect.y1) / pitch_y).round() as i64;
        let y_bottom = ((bounds.y1 - rect.y0) / pitch_y).round() as i64;
        if x0 < 0 || y_from_top < 0 || x1 <= x0 || y_bottom <= y_from_top {
            if debug {
                eprintln!(
                    "pixel-build reject=quantized-empty index={index} rect=({:.6},{:.6})-({:.6},{:.6}) q=({x0},{y_from_top})-({x1},{y_bottom})",
                    rect.x0, rect.y0, rect.x1, rect.y1
                );
            }
            return None;
        }
        let target_width = u32::try_from(x1 - x0).ok()?;
        let target_height = u32::try_from(y_bottom - y_from_top).ok()?;
        let x0 = u32::try_from(x0).ok()?;
        let y0 = u32::try_from(y_from_top).ok()?;
        if x0.checked_add(target_width)? > width || y0.checked_add(target_height)? > height {
            if debug {
                eprintln!(
                    "pixel-build reject=outside index={index} pos={x0},{y0} size={target_width}x{target_height} canvas={width}x{height}"
                );
            }
            return None;
        }
        let source = resize_nearest(
            &info.data,
            (info.width, info.height),
            (target_width, target_height),
            info.components,
            (draw.ctm.a < 0.0, draw.ctm.d < 0.0),
        )?;
        let source_alpha = if let Some(source_alpha) = &info.alpha {
            resize_nearest(
                &source_alpha.data,
                (source_alpha.width, source_alpha.height),
                (target_width, target_height),
                1,
                (draw.ctm.a < 0.0, draw.ctm.d < 0.0),
            )?
        } else {
            vec![255; usize::try_from(u64::from(target_width) * u64::from(target_height)).ok()?]
        };
        for row in 0..target_height {
            for col in 0..target_width {
                let src_pixel =
                    usize::try_from(u64::from(row) * u64::from(target_width) + u64::from(col))
                        .ok()?;
                let dst_pixel =
                    usize::try_from(u64::from(y0 + row) * u64::from(width) + u64::from(x0 + col))
                        .ok()?;
                let src_alpha = source_alpha[src_pixel];
                if src_alpha == 0 {
                    continue;
                }
                if alpha[dst_pixel] != 0 && (alpha[dst_pixel] != 255 || src_alpha != 255) {
                    has_partial_overlap = true;
                    break;
                }
                let src = src_pixel.checked_mul(info.components)?;
                let dst = dst_pixel.checked_mul(info.components)?;
                data.get_mut(dst..dst + info.components)?
                    .copy_from_slice(source.get(src..src + info.components)?);
                alpha[dst_pixel] = src_alpha;
            }
            if has_partial_overlap {
                break;
            }
        }
        if has_partial_overlap {
            if debug {
                eprintln!("pixel-build reject=partial-overlap index={index}");
            }
            return None;
        }
    }

    let alpha = alpha.iter().any(|&value| value != 255).then_some(alpha);
    let desired_ctm = Matrix::new(
        bounds.x1 - bounds.x0,
        0.0,
        0.0,
        bounds.y1 - bounds.y0,
        bounds.x0,
        bounds.y0,
    );
    Some(MergePlan {
        members: members.to_vec(),
        first_member: *members.iter().min()?,
        image: first_info.clone(),
        width,
        height,
        data,
        alpha,
        source_color_budget: None,
        desired_ctm,
        background: None,
        alpha_crop_hint: None,
        kind: MergeKind::PixelCluster,
    })
}

fn raster_draw_insertion_member(draws: &[RasterDraw], members: &[usize]) -> Option<usize> {
    let (&first, &last) = members.first().zip(members.last())?;
    let member_set = members.iter().copied().collect::<HashSet<_>>();
    let member_rects = members
        .iter()
        .filter_map(|&index| {
            draws
                .get(index)
                .map(|draw| (index, Rect::from_ctm(draw.ctm)))
        })
        .collect::<Vec<_>>();

    // The merged image may move across an interleaved raster only when every
    // constituent it actually overlaps was originally on the same side of that
    // raster. Each blocker therefore contributes either an upper (insert before)
    // or lower (insert after) bound. A blocker with overlapping constituents on
    // both sides cannot be crossed without changing z-order.
    let mut lower_bound = first;
    let mut upper_bound = last;
    for index in first..=last {
        if member_set.contains(&index) {
            continue;
        }
        let draw = draws.get(index)?;
        let rect = Rect::from_ctm(draw.ctm);
        let mut before = 0usize;
        let mut after = 0usize;
        for &(member_index, member_rect) in &member_rects {
            if !member_rect.intersects(rect) {
                continue;
            }
            if member_index < index {
                before += 1;
            } else if member_index > index {
                after += 1;
            }
            if before > 0 && after > 0 {
                if *DEBUG_RASTER {
                    eprintln!(
                        "raster-z-blocker index={index} target={:?} overlaps_before={before} overlaps_after={after} rect=({:.4},{:.4})-({:.4},{:.4})",
                        draw.target, rect.x0, rect.y0, rect.x1, rect.y1
                    );
                }
                return None;
            }
        }
        if before > 0 {
            upper_bound = upper_bound.min(index.saturating_sub(1));
        } else if after > 0 {
            lower_bound = lower_bound.max(index.saturating_add(1));
        }
        if lower_bound > upper_bound {
            return None;
        }
    }

    members
        .iter()
        .copied()
        .find(|&index| index >= lower_bound && index <= upper_bound)
}

fn raster_draw_z_order_partitions(
    draws: &[RasterDraw],
    members: &[usize],
    min_size: usize,
) -> Vec<(Vec<usize>, usize)> {
    let mut out = Vec::new();
    let mut stack = vec![members.to_vec()];
    while let Some(group) = stack.pop() {
        if group.len() < min_size {
            continue;
        }
        if let Some(insertion) = raster_draw_insertion_member(draws, &group) {
            out.push((group, insertion));
            continue;
        }
        if group.len() < min_size.saturating_mul(2).max(2) {
            continue;
        }
        // Content order is the natural split for incompatible z-order strata.
        // The native pixel pitch was already established by the parent spatial
        // component, so child groups do not each need to contain an anchor.
        let middle = group.len() / 2;
        let left = group[..middle].to_vec();
        let right = group[middle..].to_vec();
        stack.push(right);
        stack.push(left);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NativeFragmentAxis {
    Horizontal,
    Vertical,
}

fn native_fragment_axis_roles(
    info: &SampleImage,
    config: &RasterLayoutConfig,
) -> [(Option<NativeFragmentAxis>, bool, bool); 2] {
    let short_max = u32::from(config.pixel_cluster_max_source_dimension);
    let fragment_max = u32::from(config.native_fragment_max_long_dimension);
    let anchor_min = u32::from(config.native_anchor_min_long_dimension);
    // Thin pieces provide the pathological evidence. Once such pieces exist, a
    // larger multi-row/multi-column raster at the same physical pixel pitch may
    // anchor and bridge the component: these are often the intact regions on
    // either side of a scanline that a broken producer exploded into tiny runs.
    let horizontal_fragment_shape = info.height <= short_max;
    let vertical_fragment_shape = info.width <= short_max;
    let horizontal_anchor = info.width >= anchor_min;
    let vertical_anchor = info.height >= anchor_min;
    let horizontal =
        (horizontal_fragment_shape || horizontal_anchor).then_some(NativeFragmentAxis::Horizontal);
    let vertical =
        (vertical_fragment_shape || vertical_anchor).then_some(NativeFragmentAxis::Vertical);
    [
        (
            horizontal,
            horizontal_fragment_shape && info.width <= fragment_max,
            horizontal_anchor,
        ),
        (
            vertical,
            vertical_fragment_shape && info.height <= fragment_max,
            vertical_anchor,
        ),
    ]
}

fn native_pixel_pitch(draw: &RasterDraw, info: &SampleImage) -> Option<(f64, f64)> {
    let rect = Rect::from_ctm(draw.ctm);
    let x = (rect.x1 - rect.x0) / f64::from(info.width);
    let y = (rect.y1 - rect.y0) / f64::from(info.height);
    (x.is_finite() && y.is_finite() && x > MATRIX_EPSILON && y > MATRIX_EPSILON).then_some((x, y))
}

fn native_pitch_close(a: (f64, f64), b: (f64, f64)) -> bool {
    const REL: f64 = 0.015;
    rel_close(a.0, b.0, REL) && rel_close(a.1, b.1, REL)
}

fn find_native_fragment_plans(
    target: ContentTarget,
    draws: &[RasterDraw],
    infos: &HashMap<ObjectHandle, SampleImage>,
    used: &HashSet<usize>,
    config: &RasterLayoutConfig,
    user_unit: f64,
) -> Vec<MergePlan> {
    if !config.reconstruct_pixel_clusters || !matches!(target, ContentTarget::Page(_)) {
        return Vec::new();
    }

    // Thin fragments are grouped by rendering semantics and physical source-pixel
    // pitch. A long intact raster is the strongest native-resolution anchor, but a
    // heavily fragmented component with many mutually coherent strips may self-anchor
    // from their common pitch. This remains separate from the generic <=5x5 sprite
    // path, which may need to infer a placement lattice unrelated to source pixels.
    let mut groups: HashMap<NativeFragmentGroupKey, Vec<NativeFragmentEntry>> = HashMap::new();
    for (index, draw) in draws.iter().enumerate() {
        if used.contains(&index) {
            continue;
        }
        let Some(info) = infos.get(&draw.target) else {
            continue;
        };
        if info.interpolate
            || info.mask_width > info.width
            || info.mask_height > info.height
            || draw.ctm.b.abs() > MATRIX_EPSILON
            || draw.ctm.c.abs() > MATRIX_EPSILON
        {
            continue;
        }
        let Some(pitch) = native_pixel_pitch(draw, info) else {
            continue;
        };
        for (axis, is_fragment, is_anchor) in native_fragment_axis_roles(info, config) {
            let Some(axis) = axis else {
                continue;
            };
            groups
                .entry((
                    draw.epoch,
                    draw.gs_name.clone(),
                    draw.rendering_intent.clone(),
                    info.semantic_key,
                    match axis {
                        NativeFragmentAxis::Horizontal => 0,
                        NativeFragmentAxis::Vertical => 1,
                    },
                ))
                .or_default()
                .push((index, is_fragment, is_anchor, pitch));
        }
    }

    if *DEBUG_RASTER {
        let eligible = groups.values().map(Vec::len).sum::<usize>();
        let anchored = groups
            .values()
            .filter(|entries| entries.iter().any(|entry| entry.2))
            .map(Vec::len)
            .sum::<usize>();
        eprintln!(
            "native-fragment target={target:?} groups={} eligible_paints={eligible} anchored_group_paints={anchored}",
            groups.len()
        );
    }
    let max_gap = f64::from(config.pixel_cluster_max_gap_mm) * POINTS_PER_MM;
    let mut candidates = Vec::<MergePlan>::new();
    let mut debug_components = [0usize; 6];
    for ((_, _, _, _, axis_id), mut entries) in groups {
        if !entries.iter().any(|entry| entry.1) {
            continue;
        }
        entries.sort_by(|a, b| {
            let ar = Rect::from_ctm(draws[a.0].ctm);
            let br = Rect::from_ctm(draws[b.0].ctm);
            ar.x0.total_cmp(&br.x0)
        });
        let rects = entries
            .iter()
            .map(|entry| {
                let mut rect = Rect::from_ctm(draws[entry.0].ctm);
                rect.x0 *= user_unit;
                rect.x1 *= user_unit;
                rect.y0 *= user_unit;
                rect.y1 *= user_unit;
                rect
            })
            .collect::<Vec<_>>();
        let mut dsu = Dsu::new(entries.len());
        for left in 0..entries.len() {
            for right in left + 1..entries.len() {
                if rects[right].x0 > rects[left].x1 + max_gap {
                    break;
                }
                if native_pitch_close(entries[left].3, entries[right].3)
                    && rects[left].gap(rects[right]) <= max_gap
                {
                    dsu.union(left, right);
                }
            }
        }
        let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
        for local in 0..entries.len() {
            let root = dsu.find(local);
            components.entry(root).or_default().push(local);
        }
        for locals in components.into_values() {
            debug_components[0] += 1;
            let anchor = locals
                .iter()
                .copied()
                .filter(|&local| entries[local].2)
                .max_by_key(|&local| {
                    let info = &infos[&draws[entries[local].0].target];
                    match axis_id {
                        0 => info.width,
                        _ => info.height,
                    }
                });
            let fragment_count = locals.iter().filter(|&&local| entries[local].1).count();
            if fragment_count < config.pixel_cluster_min_paints {
                continue;
            }
            debug_components[2] += 1;
            let anchor_pitch = if let Some(anchor) = anchor {
                debug_components[1] += 1;
                entries[anchor].3
            } else {
                // With no intact anchor, require a much stronger fragmentation signal
                // before trusting the source-image pitch itself as the native lattice.
                let self_anchor_min = config
                    .fragmented_paint_threshold
                    .max(config.pixel_cluster_min_paints);
                if fragment_count < self_anchor_min {
                    continue;
                }
                let Some(pitch_x) =
                    median(locals.iter().map(|&local| entries[local].3.0).collect())
                else {
                    continue;
                };
                let Some(pitch_y) =
                    median(locals.iter().map(|&local| entries[local].3.1).collect())
                else {
                    continue;
                };
                (pitch_x, pitch_y)
            };
            if locals
                .iter()
                .any(|&local| !native_pitch_close(anchor_pitch, entries[local].3))
            {
                continue;
            }
            debug_components[3] += 1;
            let fragment_draws = locals
                .iter()
                .filter(|&&local| entries[local].1)
                .map(|&local| entries[local].0)
                .collect::<HashSet<_>>();
            let mut members = locals
                .iter()
                .map(|&local| entries[local].0)
                .collect::<Vec<_>>();
            members.sort_unstable();
            members.dedup();
            for (subgroup, insertion_member) in
                raster_draw_z_order_partitions(draws, &members, config.pixel_cluster_min_paints)
            {
                let subgroup_fragments = subgroup
                    .iter()
                    .filter(|member| fragment_draws.contains(member))
                    .count();
                if subgroup_fragments < config.pixel_cluster_min_paints {
                    continue;
                }
                debug_components[4] += 1;
                if let Some(mut plan) =
                    build_pixel_cluster_plan(draws, infos, &subgroup, config, Some(anchor_pitch))
                {
                    plan.first_member = insertion_member;
                    plan.kind = MergeKind::NativeFragment;
                    candidates.push(plan);
                    debug_components[5] += 1;
                }
            }
        }
    }
    if *DEBUG_RASTER {
        eprintln!(
            "native-fragment funnel target={target:?} components={} anchored={} enough_fragments={} pitch_coherent={} zsafe={} built={}",
            debug_components[0],
            debug_components[1],
            debug_components[2],
            debug_components[3],
            debug_components[4],
            debug_components[5]
        );
    }

    // A square tiny sprite may participate in both horizontal and vertical
    // candidate graphs. Prefer the reconstruction that absorbs the most source
    // paints, then greedily keep disjoint plans.
    candidates.sort_by_key(|plan| std::cmp::Reverse(plan.members.len()));
    let mut claimed = HashSet::new();
    let mut out = Vec::new();
    for plan in candidates {
        if plan.members.iter().any(|member| claimed.contains(member)) {
            continue;
        }
        claimed.extend(plan.members.iter().copied());
        out.push(plan);
    }
    out
}

fn find_pixel_cluster_plans(
    target: ContentTarget,
    draws: &[RasterDraw],
    infos: &HashMap<ObjectHandle, SampleImage>,
    deferred_tiles: &[DeferredTile],
    used: &HashSet<usize>,
    config: &RasterLayoutConfig,
    user_unit: f64,
) -> Vec<MergePlan> {
    if !config.reconstruct_pixel_clusters || !matches!(target, ContentTarget::Page(_)) {
        return Vec::new();
    }
    let mut groups: HashMap<PixelGroupKey, Vec<usize>> = HashMap::new();
    for tile in deferred_tiles {
        let index = tile.draw_index;
        if used.contains(&index) {
            continue;
        }
        let Some(draw) = draws.get(index) else {
            continue;
        };
        let Some(info) = infos.get(&tile.source) else {
            continue;
        };
        if info.interpolate
            || info.mask_width > info.width
            || info.mask_height > info.height
            || draw.ctm.b.abs() > MATRIX_EPSILON
            || draw.ctm.c.abs() > MATRIX_EPSILON
        {
            continue;
        }
        groups
            .entry((
                draw.epoch,
                draw.gs_name.clone(),
                draw.rendering_intent.clone(),
            ))
            .or_default()
            .push(index);
    }

    if *DEBUG_RASTER {
        let mut sizes = groups.values().map(Vec::len).collect::<Vec<_>>();
        sizes.sort_unstable_by(|a, b| b.cmp(a));
        eprintln!(
            "pixel-groups target={target:?} epochs={} largest={:?}",
            groups.len(),
            sizes.into_iter().take(12).collect::<Vec<_>>()
        );
    }
    let max_gap = f64::from(config.pixel_cluster_max_gap_mm) * POINTS_PER_MM;
    let mut plans = Vec::new();
    for (_, mut indices) in groups {
        if indices.len() < config.pixel_cluster_min_paints {
            continue;
        }
        indices.sort_by(|&a, &b| {
            let ar = Rect::from_ctm(draws[a].ctm);
            let br = Rect::from_ctm(draws[b].ctm);
            ar.x0.total_cmp(&br.x0)
        });
        let rects = indices
            .iter()
            .map(|&index| {
                let mut rect = Rect::from_ctm(draws[index].ctm);
                rect.x0 *= user_unit;
                rect.x1 *= user_unit;
                rect.y0 *= user_unit;
                rect.y1 *= user_unit;
                rect
            })
            .collect::<Vec<_>>();
        let mut dsu = Dsu::new(indices.len());
        for left in 0..indices.len() {
            for right in left + 1..indices.len() {
                if rects[right].x0 > rects[left].x1 + max_gap {
                    break;
                }
                if rects[left].gap(rects[right]) <= max_gap {
                    dsu.union(left, right);
                }
            }
        }
        let mut components: HashMap<usize, Vec<usize>> = HashMap::new();
        for (local, &index) in indices.iter().enumerate() {
            let root = dsu.find(local);
            components.entry(root).or_default().push(index);
        }
        if *DEBUG_RASTER {
            let mut cs = components.values().map(Vec::len).collect::<Vec<_>>();
            cs.sort_unstable_by(|a, b| b.cmp(a));
            if cs.first().copied().unwrap_or(0) >= config.pixel_cluster_min_paints {
                eprintln!(
                    "pixel-components target={target:?} largest={:?}",
                    cs.into_iter().take(12).collect::<Vec<_>>()
                );
            }
        }
        for mut members in components.into_values() {
            if members.len() < config.pixel_cluster_min_paints {
                continue;
            }
            members.sort_unstable();
            // Interleaved rasters are fine when every constituent they overlap is
            // on the same side. Pick an insertion member satisfying all resulting
            // before/after constraints rather than requiring stream adjacency.
            let insertion_member = raster_draw_insertion_member(draws, &members);
            if *DEBUG_RASTER {
                eprintln!(
                    "pixel-candidate target={target:?} n={} insertion={insertion_member:?}",
                    members.len()
                );
            }
            let Some(insertion_member) = insertion_member else {
                continue;
            };
            if let Some(mut plan) = build_pixel_cluster_plan(draws, infos, &members, config, None) {
                plan.first_member = insertion_member;
                if *DEBUG_RASTER {
                    eprintln!(
                        "pixel-plan target={target:?} n={} out={}x{}",
                        members.len(),
                        plan.width,
                        plan.height
                    );
                }
                plans.push(plan);
            } else if *DEBUG_RASTER {
                let mut unique = HashSet::new();
                let mut templates = HashSet::new();
                for &i in &members {
                    if let Some(info) = infos.get(&draws[i].target) {
                        unique.insert((draws[i].target, info.width, info.height, info.components));
                        templates.insert(info.semantic_key);
                    }
                }
                eprintln!(
                    "pixel-plan-rejected target={target:?} n={} sources={:?} templates={} first_template={:?}",
                    members.len(),
                    unique,
                    templates.len(),
                    templates.iter().next()
                );
            }
        }
    }
    plans
}

fn inverse(matrix: Matrix) -> Option<Matrix> {
    let det = matrix.a * matrix.d - matrix.b * matrix.c;
    if !det.is_finite() || det.abs() <= MATRIX_EPSILON {
        return None;
    }
    let a = matrix.d / det;
    let b = -matrix.b / det;
    let c = -matrix.c / det;
    let d = matrix.a / det;
    let e = -(a * matrix.e + c * matrix.f);
    let f = -(b * matrix.e + d * matrix.f);
    Some(Matrix::new(a, b, c, d, e, f))
}

fn relative_matrix(current: Matrix, desired: Matrix) -> Option<Matrix> {
    let mut relative = inverse(current)?;
    relative.concat(desired);
    Some(relative)
}

fn unique_xobject_name(xobjects: &OwnedDictionary, suffix: &mut usize) -> Vec<u8> {
    loop {
        let name = format!("RL{}", *suffix).into_bytes();
        *suffix += 1;
        if !xobjects.contains_key(&name) {
            return name;
        }
    }
}

fn install_target(
    document: &mut EditDocument,
    target: ContentTarget,
    resources: OwnedDictionary,
    content: Vec<u8>,
) -> Result<()> {
    match target {
        ContentTarget::Page(page) => {
            let stream = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
                dictionary: OwnedDictionary::new(),
                data: StreamData::Owned(content),
            }));
            let object = match page {
                ObjectHandle::Existing(id) => document.edit_object(id)?,
                ObjectHandle::New(id) => document
                    .overlay_mut()
                    .added_mut(id)
                    .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
            };
            if let Some(dictionary) = object.as_dictionary_mut() {
                dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                dictionary.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
            }
        }
        ContentTarget::Form(form) => {
            let object = match form {
                ObjectHandle::Existing(id) => document.edit_object(id)?,
                ObjectHandle::New(id) => document
                    .overlay_mut()
                    .added_mut(id)
                    .ok_or_else(|| Error::MissingNewObject { index: id.index() })?,
            };
            if let OwnedObject::Stream { dictionary, data } = object {
                dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
                dictionary.remove(b"Filter".as_slice());
                dictionary.remove(b"DecodeParms".as_slice());
                *data = StreamData::Owned(content);
            }
        }
    }
    Ok(())
}

fn page_user_unit(document: &EditDocument, page: ObjectHandle) -> Result<f64> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(1.0);
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(1.0);
    };
    Ok(
        current_number(document, dictionary.get(b"UserUnit".as_slice()))?
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(1.0),
    )
}

struct RewriteInput<'a> {
    content: &'a [u8],
    draws: &'a [RasterDraw],
}

struct ApplyPlansContext<'a> {
    target: ContentTarget,
    resources: OwnedDictionary,
    input: RewriteInput<'a>,
    config: &'a RasterLayoutConfig,
    flate_level: i32,
    alpha_crop_cache: &'a mut HashMap<ObjectHandle, AlphaCropCacheEntry>,
}

#[derive(Debug, Default)]
struct ApplyPlansResult {
    changed: bool,
    consumed_draws: HashSet<usize>,
    added_resource_names: BTreeSet<Vec<u8>>,
}

fn record_image_encoding_stats(stats: &mut RasterLayoutStats, encoding: ImageEncodingStats) {
    stats.binary_images_packed += usize::from(encoding.binary_packed);
    stats.binary_masks_packed += usize::from(encoding.binary_mask_packed);
    stats.stencil_images_emitted += usize::from(encoding.stencil_color.is_some());
    stats.relaxed_stencil_images_emitted += usize::from(encoding.relaxed_stencil);
    stats.binary_image_encoded_bytes_saved = stats
        .binary_image_encoded_bytes_saved
        .saturating_add(encoding.encoded_bytes_saved);
}

fn apply_plans(
    document: &mut EditDocument,
    context: ApplyPlansContext<'_>,
    mut plans: Vec<MergePlan>,
    stats: &mut RasterLayoutStats,
) -> Result<ApplyPlansResult> {
    let target = context.target;
    let mut resources = context.resources;
    let input = context.input;
    let config = context.config;
    let flate_level = context.flate_level;
    let alpha_crop_cache = context.alpha_crop_cache;
    if plans.is_empty() {
        return Ok(ApplyPlansResult::default());
    }
    plans.sort_by_key(|plan| plan.first_member);
    let mut xobjects =
        resolved_dictionary(document, resources.get(b"XObject".as_slice()))?.unwrap_or_default();
    let mut suffix = 1usize;
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut claimed = HashSet::new();
    let mut result = ApplyPlansResult::default();

    for mut plan in plans {
        if plan.members.iter().any(|index| !claimed.insert(*index)) {
            continue;
        }
        let first_draw = &input.draws[plan.first_member];
        let source_crop_hint = plan.alpha_crop_hint;
        let cached_crop = if plan.kind == MergeKind::AlphaCrop {
            alpha_crop_cache.get(&first_draw.target).copied()
        } else {
            None
        };
        let crop_result = if let Some(cached) = cached_crop.filter(|cached| cached.fast_reuse) {
            crop_plan_geometry_to(&mut plan, cached.source_crop)
                .map(|removed_pixels| (true, false, removed_pixels))
        } else {
            crop_merge_plan(&mut plan, config)
        };
        let Some((transparent_cropped, background_cropped, removed_pixels)) = crop_result else {
            continue;
        };
        let Some(relative) = relative_matrix(first_draw.replace_ctm, plan.desired_ctm) else {
            continue;
        };
        let mask_baked = plan.image.alpha.is_some() || plan.alpha.is_some();

        // Prepare all prospective streams before mutating the graph. This lets
        // singleton alpha-crop plans compare real compressed payload cost with
        // the current source image+mask and disappear cleanly when the crop
        // would make the PDF larger.
        let prepared_background = if let Some(background) = plan.background.take() {
            let Some(background_relative) =
                relative_matrix(first_draw.replace_ctm, background.desired_ctm)
            else {
                continue;
            };
            let prepared = prepare_image(
                &plan.image,
                1,
                1,
                &background.data,
                None,
                ImageEncodingContext {
                    flate_level,
                    exact_raster_rendering: config.exact_raster_rendering,
                    source_color_budget: None,
                },
            )?;
            Some((background_relative, prepared))
        } else {
            None
        };

        let mut prepared_main = if cached_crop.is_none() {
            Some(prepare_image(
                &plan.image,
                plan.width,
                plan.height,
                &plan.data,
                plan.alpha,
                ImageEncodingContext {
                    flate_level,
                    exact_raster_rendering: config.exact_raster_rendering,
                    source_color_budget: plan.source_color_budget,
                },
            )?)
        } else {
            None
        };

        if plan.kind == MergeKind::AlphaCrop
            && cached_crop.is_none()
            && let Some(prepared) = prepared_main.as_ref()
        {
            let source_payload = plan
                .image
                .encoded_color_bytes
                .saturating_add(plan.image.encoded_mask_bytes);
            let candidate_payload = prepared.payload_bytes().saturating_add(
                prepared_background
                    .as_ref()
                    .map_or(0, |(_, background)| background.payload_bytes()),
            );
            if source_payload > 0 && candidate_payload >= source_payload {
                for member in &plan.members {
                    claimed.remove(member);
                }
                continue;
            }
        }

        stats.transparent_margins_cropped += usize::from(transparent_cropped);
        stats.background_margins_cropped += usize::from(background_cropped);
        stats.cropped_pixels_removed = stats.cropped_pixels_removed.saturating_add(removed_pixels);

        let background = if let Some((background_relative, prepared)) = prepared_background {
            let (image, encoding) = install_prepared_image(document, prepared, 1, 1);
            record_image_encoding_stats(stats, encoding);
            let name = unique_xobject_name(&xobjects, &mut suffix);
            xobjects.insert(name.clone(), OwnedObject::Reference(image));
            result.added_resource_names.insert(name.clone());
            Some((background_relative, name))
        } else {
            None
        };

        let (name, encoding, image_created) = if let Some(cached) = cached_crop {
            let name = unique_xobject_name(&xobjects, &mut suffix);
            xobjects.insert(name.clone(), OwnedObject::Reference(cached.image));
            result.added_resource_names.insert(name.clone());
            (name, cached.encoding, false)
        } else {
            let Some(prepared) = prepared_main.take() else {
                return Err(Error::Invalid(
                    "missing prepared image for uncached raster plan".to_owned(),
                ));
            };
            let (image, encoding) =
                install_prepared_image(document, prepared, plan.width, plan.height);
            record_image_encoding_stats(stats, encoding);
            let name = unique_xobject_name(&xobjects, &mut suffix);
            xobjects.insert(name.clone(), OwnedObject::Reference(image));
            result.added_resource_names.insert(name.clone());
            if plan.kind == MergeKind::AlphaCrop
                && let Some(source_crop) = source_crop_hint
            {
                alpha_crop_cache.insert(
                    first_draw.target,
                    AlphaCropCacheEntry {
                        image,
                        encoding,
                        source_crop,
                        fast_reuse: !background_cropped,
                    },
                );
            }
            (name, encoding, true)
        };
        let mut replacement = b"q ".to_vec();
        if let Some(gs) = &first_draw.gs_name {
            replacement.push(b'/');
            replacement.extend_from_slice(gs.strip_prefix(b"/").unwrap_or(gs));
            replacement.extend_from_slice(b" gs ");
        }
        if let Some(intent) = &first_draw.rendering_intent {
            replacement.push(b'/');
            replacement.extend_from_slice(intent.strip_prefix(b"/").unwrap_or(intent));
            replacement.extend_from_slice(b" ri ");
        }
        if let Some((background_relative, background_name)) = background {
            replacement
                .extend_from_slice(format!("q {} cm /", background_relative.unparse()).as_bytes());
            replacement.extend_from_slice(&background_name);
            replacement.extend_from_slice(b" Do Q ");
        }
        replacement.extend_from_slice(b"q ");
        if let Some(color) = encoding.stencil_color {
            replacement.extend_from_slice(&color.content_operator());
        }
        replacement.extend_from_slice(format!("{} cm /", relative.unparse()).as_bytes());
        replacement.extend_from_slice(&name);
        replacement.extend_from_slice(b" Do Q Q");
        replacements.push((first_draw.range_start, first_draw.range_end, replacement));
        for &member in &plan.members {
            if member == plan.first_member {
                continue;
            }
            let draw = &input.draws[member];
            replacements.push((draw.range_start, draw.range_end, Vec::new()));
        }
        result.consumed_draws.extend(plan.members.iter().copied());
        if plan.kind == MergeKind::PixelCluster {
            stats.deferred_tile_paints_consumed = stats
                .deferred_tile_paints_consumed
                .saturating_add(plan.members.len());
        }
        match plan.kind {
            MergeKind::Stripe => {
                stats.stripe_groups_merged += 1;
                stats.stripe_paints_merged += plan.members.len();
            }
            MergeKind::PixelCluster => {
                stats.pixel_clusters_reconstructed += 1;
                stats.pixel_paints_reconstructed += plan.members.len();
            }
            MergeKind::NativeFragment => {
                stats.native_fragment_groups_reconstructed += 1;
                stats.native_fragment_paints_reconstructed += plan.members.len();
            }
            MergeKind::AlphaCrop => {}
        }
        if mask_baked && image_created {
            stats.masks_baked += 1;
        }
    }
    if replacements.is_empty() {
        return Ok(result);
    }
    replacements.sort_by_key(|(start, _, _)| *start);
    let mut rewritten = Vec::with_capacity(input.content.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        if start < cursor || end < start || end > input.content.len() {
            return Err(Error::Invalid(
                "overlapping/invalid raster-layout content replacement".to_owned(),
            ));
        }
        rewritten.extend_from_slice(&input.content[cursor..start]);
        rewritten.extend_from_slice(&replacement);
        cursor = end;
    }
    rewritten.extend_from_slice(&input.content[cursor..]);
    resources.insert(b"XObject".to_vec(), OwnedObject::Dictionary(xobjects));
    install_target(document, target, resources, rewritten)?;
    result.changed = true;
    Ok(result)
}

fn append_alpha_crop_plans(
    draws: &[RasterDraw],
    infos: &HashMap<ObjectHandle, SampleImage>,
    claimed: &HashSet<usize>,
    alpha_crop_bounds_cache: &mut HashMap<ObjectHandle, Option<PixelCrop>>,
    plans: &mut Vec<MergePlan>,
) {
    for (index, draw) in draws.iter().enumerate() {
        if claimed.contains(&index) {
            continue;
        }
        let Some(info) = infos.get(&draw.target) else {
            continue;
        };
        // Cropping a DCT/JPX-backed image requires decoding the compact color
        // payload and currently re-emits it as Flate. Keep the native encoded
        // color image plus its mask instead; mask-only normalization must not
        // turn a small JPEG into a large lossless RGB stream.
        if info.preserve_encoded_color {
            continue;
        }
        let Some(alpha) = info.alpha.as_ref() else {
            continue;
        };
        if alpha.width != info.width || alpha.height != info.height {
            continue;
        }
        let crop = if let Some(cached) = alpha_crop_bounds_cache.get(&draw.target) {
            *cached
        } else {
            let crop = alpha_crop(&alpha.data, info.width, info.height);
            alpha_crop_bounds_cache.insert(draw.target, crop);
            crop
        };
        let Some(crop) = crop else {
            continue;
        };
        if !crop_is_worthwhile(info.width, info.height, crop) {
            continue;
        }
        plans.push(MergePlan {
            members: vec![index],
            first_member: index,
            image: info.clone(),
            width: info.width,
            height: info.height,
            data: Vec::new(),
            alpha: None,
            source_color_budget: None,
            desired_ctm: draw.ctm,
            background: None,
            alpha_crop_hint: Some(crop),
            kind: MergeKind::AlphaCrop,
        });
    }
}

#[derive(Clone, Copy)]
struct MergePlanContext<'a> {
    target: ContentTarget,
    scanner: &'a RasterScanner,
    initial_used: &'a HashSet<usize>,
    predecoded_images: Option<&'a HashMap<ObjectHandle, Option<SampleImage>>>,
    config: &'a RasterLayoutConfig,
    user_unit: f64,
}

fn build_merge_plans_for_scanner(
    document: &EditDocument,
    context: MergePlanContext<'_>,
    alpha_crop_bounds_cache: &mut HashMap<ObjectHandle, Option<PixelCrop>>,
    stats: &mut RasterLayoutStats,
) -> Result<Vec<MergePlan>> {
    let MergePlanContext {
        target,
        scanner,
        initial_used,
        predecoded_images,
        config,
        user_unit,
    } = context;
    let enough_for_merge = scanner.draws.len()
        >= config
            .stripe_min_paints
            .min(config.pixel_cluster_min_paints);
    if !enough_for_merge && !config.crop_transparent {
        return Ok(Vec::new());
    }

    let image_materialize_started = Instant::now();
    let mut infos = HashMap::new();
    for (index, draw) in scanner.draws.iter().enumerate() {
        if initial_used.contains(&index) || infos.contains_key(&draw.target) {
            continue;
        }
        if !enough_for_merge && config.crop_transparent {
            let Some(object) = document.current_owned_object(draw.target)? else {
                continue;
            };
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };
            let has_smask = is_non_null(document, dictionary.get(b"SMask".as_slice()))?;
            let has_mask = is_non_null(document, dictionary.get(b"Mask".as_slice()))?;
            if !has_smask && !has_mask {
                continue;
            }
        }
        if let Some(cached) = predecoded_images.and_then(|cache| cache.get(&draw.target)) {
            match cached {
                Some(info) if config.bake_masks || info.alpha.is_none() => {
                    infos.insert(draw.target, info.clone());
                }
                // Hidden-visibility analysis always requests baked masks. If it
                // produced alpha, the non-baking planner would reject this image
                // before decoding; if it produced None, both modes reject it.
                Some(_) | None => {}
            }
            continue;
        }
        if let Some(info) = image_info(document, draw.target, config.bake_masks)? {
            infos.insert(draw.target, info);
        }
    }
    stats.image_materialize_us = stats
        .image_materialize_us
        .saturating_add(image_materialize_started.elapsed().as_micros() as u64);
    if *DEBUG_RASTER && !scanner.draws.is_empty() {
        let max_dim = u32::from(config.pixel_cluster_max_source_dimension);
        let tiny_draws = scanner
            .draws
            .iter()
            .enumerate()
            .filter(|(index, _)| !initial_used.contains(index))
            .filter(|(_, draw)| {
                infos
                    .get(&draw.target)
                    .is_some_and(|info| info.width <= max_dim && info.height <= max_dim)
            })
            .map(|(_, draw)| draw)
            .collect::<Vec<_>>();
        let tiny = tiny_draws.len();
        let mut clip_none = 0usize;
        let mut clip_full = 0usize;
        let mut clip_partial = 0usize;
        let mut clip_complex = 0usize;
        for draw in tiny_draws {
            if draw.clip_complex {
                clip_complex += 1;
                continue;
            }
            match draw.clip {
                None => clip_none += 1,
                Some(clip) if clip.contains(Rect::from_ctm(draw.ctm), 1.0e-6) => clip_full += 1,
                Some(_) => clip_partial += 1,
            }
        }
        eprintln!(
            "raster-target {target:?}: decoded_images={} tiny_draws={} clips none/full/partial/complex={}/{}/{}/{}",
            infos.len(),
            tiny,
            clip_none,
            clip_full,
            clip_partial,
            clip_complex
        );
    }
    let max_dim = u32::from(config.pixel_cluster_max_source_dimension);
    let deferred_tiles = scanner
        .draws
        .iter()
        .enumerate()
        .filter(|(index, _)| !initial_used.contains(index))
        .filter_map(|(index, draw)| {
            let info = infos.get(&draw.target)?;
            (info.width <= max_dim && info.height <= max_dim)
                .then(|| DeferredTile::from_draw(index, draw, info))
                .flatten()
        })
        .collect::<Vec<_>>();
    stats.deferred_tile_candidates = stats
        .deferred_tile_candidates
        .saturating_add(deferred_tiles.len());
    if *DEBUG_RASTER && !deferred_tiles.is_empty() {
        let (min_ppi, max_ppi) =
            deferred_tiles
                .iter()
                .fold((f64::INFINITY, 0.0_f64), |(min_ppi, max_ppi), tile| {
                    let ppi = tile.effective_ppi.0.min(tile.effective_ppi.1);
                    (min_ppi.min(ppi), max_ppi.max(ppi))
                });
        let first = &deferred_tiles[0];
        eprintln!(
            "deferred-tiles target={target:?} n={} effective_ppi={min_ppi:.1}..{max_ppi:.1} first=index:{} source:{:?} name:{} size:{}x{} range:{}..{} rect=({:.3},{:.3})-({:.3},{:.3})",
            deferred_tiles.len(),
            first.draw_index,
            first.source,
            String::from_utf8_lossy(&first.resource_name),
            first.source_size.0,
            first.source_size.1,
            first.original_range.0,
            first.original_range.1,
            first.page_rect.x0,
            first.page_rect.y0,
            first.page_rect.x1,
            first.page_rect.y1,
        );
    }
    if infos.is_empty() {
        return Ok(Vec::new());
    }

    let mut used = initial_used.clone();
    let native_plan_started = Instant::now();
    let native_fragment_plans = if enough_for_merge {
        find_native_fragment_plans(target, &scanner.draws, &infos, &used, config, user_unit)
    } else {
        Vec::new()
    };
    stats.native_plan_us = stats
        .native_plan_us
        .saturating_add(native_plan_started.elapsed().as_micros() as u64);
    for plan in &native_fragment_plans {
        used.extend(plan.members.iter().copied());
    }
    let stripe_plan_started = Instant::now();
    let stripe_plans = if enough_for_merge {
        find_stripe_plans(&scanner.draws, &infos, &used, config)
    } else {
        Vec::new()
    };
    stats.stripe_plan_us = stats
        .stripe_plan_us
        .saturating_add(stripe_plan_started.elapsed().as_micros() as u64);
    for plan in &stripe_plans {
        used.extend(plan.members.iter().copied());
    }
    let mut plans = native_fragment_plans;
    plans.extend(stripe_plans);
    let pixel_plan_started = Instant::now();
    if enough_for_merge {
        plans.extend(find_pixel_cluster_plans(
            target,
            &scanner.draws,
            &infos,
            &deferred_tiles,
            &used,
            config,
            user_unit,
        ));
    }
    stats.pixel_plan_us = stats
        .pixel_plan_us
        .saturating_add(pixel_plan_started.elapsed().as_micros() as u64);

    let mut claimed = initial_used.clone();
    for plan in &plans {
        claimed.extend(plan.members.iter().copied());
    }
    if config.crop_transparent {
        append_alpha_crop_plans(
            &scanner.draws,
            &infos,
            &claimed,
            alpha_crop_bounds_cache,
            &mut plans,
        );
    }
    Ok(plans)
}

pub(crate) fn normalize_raster_layout_hayro(
    document: &mut EditDocument,
    config: &RasterLayoutConfig,
    flate_level: i32,
    mut vector_cache: Option<&mut BTreeMap<ObjectHandle, ProcessingVectorAnalysis>>,
) -> Result<RasterLayoutStats> {
    if *DEBUG_RASTER {
        eprintln!(
            "raster-layout entry enabled={} fragment_threshold={} pixel_gap_mm={}",
            config.enabled, config.fragmented_paint_threshold, config.pixel_cluster_max_gap_mm
        );
    }
    if !config.enabled {
        return Ok(RasterLayoutStats::default());
    }
    let mut stats = RasterLayoutStats {
        inline_inventory_complete: true,
        page_vector_inventory_complete: true,
        page_hidden_text_inventory_complete: true,
        resource_inventory_complete: true,
        ..RasterLayoutStats::default()
    };
    let mut staged_xobjects = HashSet::new();
    let mut image_cache = RasterImageDecodeCache::default();
    let mut alpha_crop_cache = HashMap::<ObjectHandle, AlphaCropCacheEntry>::new();
    let mut alpha_crop_bounds_cache = HashMap::<ObjectHandle, Option<PixelCrop>>::new();
    let hidden_context: Option<HiddenTextSharedContext> = if config.prune_hidden_paints {
        Some(hidden_text_shared_context_hayro(document)?)
    } else {
        None
    };
    let pages = document.page_handles()?;
    stats.page_count = pages.len();
    let page_numbers = pages
        .iter()
        .copied()
        .enumerate()
        .map(|(index, page)| (page, index + 1))
        .collect::<BTreeMap<_, _>>();

    for target in collect_targets(document, &pages)? {
        let scan_started = Instant::now();
        let Some(mut resources) = target_resources(document, target)? else {
            // A resource-less Form borrows its caller's resource scope. The
            // per-target inventory cannot represent that dependency exactly,
            // so force the canonical borrowing-aware resource-pruning path.
            stats.resource_inventory_complete = false;
            if matches!(target, ContentTarget::Page(_)) {
                stats.page_vector_inventory_complete = false;
                stats.page_hidden_text_inventory_complete = false;
            }
            stats.target_scan_us = stats
                .target_scan_us
                .saturating_add(scan_started.elapsed().as_micros() as u64);
            continue;
        };
        let mut content = target_content(document, target)?;
        let collect_vector = vector_cache.is_some() && matches!(target, ContentTarget::Page(_));
        let Some(scan) = scan_target_shared(
            document,
            target,
            &resources,
            &content,
            hidden_context.as_ref(),
            &page_numbers,
            collect_vector,
        )?
        else {
            stats.inline_inventory_complete = false;
            stats.resource_inventory_complete = false;
            if matches!(target, ContentTarget::Page(_)) {
                stats.page_vector_inventory_complete = false;
                stats.page_hidden_text_inventory_complete = false;
                stats.resource_inventory_complete = false;
            }
            stats.target_scan_us = stats
                .target_scan_us
                .saturating_add(scan_started.elapsed().as_micros() as u64);
            if *DEBUG_RASTER {
                eprintln!(
                    "raster-target {target:?}: scanner incomplete content={}",
                    content.len()
                );
            }
            continue;
        };
        let mut scanner = scan.scanner;
        let mut shared_hidden_ranges = scan.hidden_ranges;
        if let (ContentTarget::Page(page), Some(analysis), Some(cache)) =
            (target, scan.vector_analysis, vector_cache.as_deref_mut())
        {
            cache.insert(page, analysis);
        }
        stats.target_scan_us = stats
            .target_scan_us
            .saturating_add(scan_started.elapsed().as_micros() as u64);

        if scanner.inline_occurrences > 0 {
            if scanner.inline_occurrences < config.fragmented_paint_threshold {
                stats.inline_occurrences_remaining = stats
                    .inline_occurrences_remaining
                    .saturating_add(scanner.inline_occurrences);
                if matches!(target, ContentTarget::Page(_)) {
                    stats.page_rect_fill_paints_seen = stats
                        .page_rect_fill_paints_seen
                        .saturating_add(scanner.rect_fills.len());
                    stats.page_vector_merge_candidate |= scanner.vector_merge_candidate;
                    let hidden_candidate =
                        scanner.text_with_ext_gstate || scanner.covering_paint_after_text;
                    stats.page_physical_hidden_text_candidate |= hidden_candidate;
                    if hidden_candidate && let ContentTarget::Page(page) = target {
                        stats.page_hidden_text_candidates.insert(page);
                    }
                }
                continue;
            }
            let inline_started = Instant::now();
            let inline_target = match target {
                ContentTarget::Page(page) => InlineContentTarget::Page(page),
                ContentTarget::Form(form) => InlineContentTarget::Form(form),
            };
            let inline = externalize_fragmented_inline_target_hayro(
                document,
                inline_target,
                config.fragmented_paint_threshold,
            )?;
            stats.inline_externalize_us = stats
                .inline_externalize_us
                .saturating_add(inline_started.elapsed().as_micros() as u64);
            stats.inline.scopes_rewritten = stats
                .inline
                .scopes_rewritten
                .saturating_add(inline.stats.scopes_rewritten);
            stats.inline.occurrences_externalized = stats
                .inline
                .occurrences_externalized
                .saturating_add(inline.stats.occurrences_externalized);
            stats.inline.xobjects_created = stats
                .inline
                .xobjects_created
                .saturating_add(inline.stats.xobjects_created);
            stats.inline.xobject_references_reused = stats
                .inline
                .xobject_references_reused
                .saturating_add(inline.stats.xobject_references_reused);
            staged_xobjects.extend(inline.staged_xobjects);
            if inline.stats.scopes_rewritten == 0 {
                continue;
            }

            if let ContentTarget::Page(page) = target
                && let Some(cache) = vector_cache.as_deref_mut()
            {
                cache.remove(&page);
            }
            let rescan_started = Instant::now();
            let Some(updated_resources) = target_resources(document, target)? else {
                continue;
            };
            resources = updated_resources;
            content = target_content(document, target)?;
            let Some(rescanned) = scan_target_shared(
                document,
                target,
                &resources,
                &content,
                hidden_context.as_ref(),
                &page_numbers,
                collect_vector,
            )?
            else {
                stats.inline_inventory_complete = false;
                if matches!(target, ContentTarget::Page(_)) {
                    stats.page_vector_inventory_complete = false;
                    stats.page_hidden_text_inventory_complete = false;
                }
                continue;
            };
            stats.target_scan_us = stats
                .target_scan_us
                .saturating_add(rescan_started.elapsed().as_micros() as u64);
            scanner = rescanned.scanner;
            shared_hidden_ranges = rescanned.hidden_ranges;
            if let (ContentTarget::Page(page), Some(analysis), Some(cache)) = (
                target,
                rescanned.vector_analysis,
                vector_cache.as_deref_mut(),
            ) {
                cache.insert(page, analysis);
            }
            if scanner.inline_occurrences != 0 {
                stats.inline_inventory_complete = false;
                continue;
            }
        }

        stats.inline_occurrences_remaining = stats
            .inline_occurrences_remaining
            .saturating_add(scanner.inline_occurrences);
        if matches!(target, ContentTarget::Page(_)) {
            stats.page_rect_fill_paints_seen = stats
                .page_rect_fill_paints_seen
                .saturating_add(scanner.rect_fills.len());
            stats.page_vector_merge_candidate |= scanner.vector_merge_candidate;
            let hidden_candidate =
                scanner.text_with_ext_gstate || scanner.covering_paint_after_text;
            stats.page_physical_hidden_text_candidate |= hidden_candidate;
            if hidden_candidate && let ContentTarget::Page(page) = target {
                stats.page_hidden_text_candidates.insert(page);
            }
            if *DEBUG_VECTOR {
                eprintln!(
                    "vector-proof target={target:?} rect_fills={} candidate={}",
                    scanner.rect_fills.len(),
                    scanner.vector_merge_candidate
                );
            }
        }

        let mut pending_pruned_content = None;
        let mut pruned_draws = HashSet::new();
        let mut predecoded_images = None;
        if config.prune_hidden_paints {
            let hidden_prune_started = Instant::now();
            let scope = match target {
                ContentTarget::Page(page) => page_visible_rect(document, page)?,
                ContentTarget::Form(_) => None,
            };
            let HiddenRasterPruneResult {
                rewritten,
                transparent,
                occluded,
                removed,
                visibility,
            } = prune_hidden_raster_paints(
                document,
                &resources,
                &content,
                &scanner,
                HiddenRasterPruneContext {
                    scope,
                    prune_occluded: config.prune_occluded_raster_paints,
                    image_cache: &mut image_cache,
                    stats: &mut stats,
                },
            )?;
            predecoded_images = Some(visibility);
            stats.hidden_prune_us = stats
                .hidden_prune_us
                .saturating_add(hidden_prune_started.elapsed().as_micros() as u64);
            if transparent != 0 || occluded != 0 {
                stats.transparent_paints_pruned += transparent;
                stats.occluded_raster_paints_pruned += occluded;
                pending_pruned_content = Some(rewritten);
                pruned_draws = removed;
            }
        }
        if *DEBUG_RASTER && !scanner.draws.is_empty() {
            eprintln!(
                "raster-target {target:?}: content={} image_draws={}",
                content.len(),
                scanner.draws.len()
            );
        }

        let user_unit = match target {
            ContentTarget::Page(page) => page_user_unit(document, page)?,
            ContentTarget::Form(_) => 1.0,
        };
        let mut plans = build_merge_plans_for_scanner(
            document,
            MergePlanContext {
                target,
                scanner: &scanner,
                initial_used: &pruned_draws,
                predecoded_images: predecoded_images.as_ref(),
                config,
                user_unit,
            },
            &mut alpha_crop_bounds_cache,
            &mut stats,
        )?;

        if scanner.resource_pending_operands {
            stats.resource_inventory_complete = false;
        } else if plans.is_empty() {
            let usage = scanner.resource_usage_after_removing(&pruned_draws);
            match target {
                ContentTarget::Page(page) => {
                    stats.page_resource_names.insert(page, usage.names);
                    stats
                        .page_resource_names_by_type
                        .insert(page, usage.by_type);
                }
                ContentTarget::Form(form) => {
                    stats.form_resource_names.insert(form, usage.names);
                    stats
                        .form_resource_names_by_type
                        .insert(form, usage.by_type);
                }
            }
        }

        if !plans.is_empty()
            && scanner.text_paints > 0
            && let ContentTarget::Page(page) = target
        {
            stats.page_physical_hidden_text_candidate = true;
            stats.page_hidden_text_candidates.insert(page);
            // A completed shared hidden-text scan with no prunable ranges is
            // already a complete proof for this page. Raster reconstruction
            // may change operator bytes, but with no pending prune rewrite it
            // does not invalidate an empty removal set, so do not rescan the
            // entire page later just because raster plans were applied.
            if pending_pruned_content.is_none()
                && shared_hidden_ranges.as_ref().is_some_and(Vec::is_empty)
            {
                stats.page_hidden_text_shared_complete.insert(page);
            }
        }

        if plans.is_empty() {
            let mut removal_ranges = pruned_draws
                .iter()
                .map(|&index| {
                    let draw = &scanner.draws[index];
                    (draw.range_start, draw.range_end)
                })
                .collect::<Vec<_>>();
            if let Some(hidden_ranges) = shared_hidden_ranges.take() {
                stats.shared_hidden_text_paints_pruned = stats
                    .shared_hidden_text_paints_pruned
                    .saturating_add(hidden_ranges.len());
                removal_ranges.extend(hidden_ranges);
                if let ContentTarget::Page(page) = target {
                    stats.page_hidden_text_shared_complete.insert(page);
                }
            }
            if !removal_ranges.is_empty() {
                if let ContentTarget::Page(page) = target
                    && let Some(cache) = vector_cache.as_deref_mut()
                {
                    cache.remove(&page);
                }
                let rewritten = remove_content_ranges(&content, removal_ranges)?;
                install_target(document, target, resources.clone(), rewritten)?;
            }
            continue;
        }

        if let Some(rewritten) = pending_pruned_content {
            if let ContentTarget::Page(page) = target
                && let Some(cache) = vector_cache.as_deref_mut()
            {
                cache.remove(&page);
            }
            install_target(document, target, resources.clone(), rewritten)?;

            // Mixed prune+reconstruction scopes are uncommon. Preserve the previous
            // conservative behavior there: rescan the post-prune content and build
            // plans against the exact new operator/range layout. Hidden-text results
            // from the original token offsets are deliberately discarded.
            let rescan_started = Instant::now();
            content = target_content(document, target)?;
            let Some(rescanned) = scan_target(document, target, &resources, &content)? else {
                continue;
            };
            stats.target_scan_us = stats
                .target_scan_us
                .saturating_add(rescan_started.elapsed().as_micros() as u64);
            scanner = rescanned;
            pruned_draws.clear();
            plans = build_merge_plans_for_scanner(
                document,
                MergePlanContext {
                    target,
                    scanner: &scanner,
                    initial_used: &HashSet::new(),
                    predecoded_images: predecoded_images.as_ref(),
                    config,
                    user_unit,
                },
                &mut alpha_crop_bounds_cache,
                &mut stats,
            )?;
        }

        let apply_plans_started = Instant::now();
        let applied = apply_plans(
            document,
            ApplyPlansContext {
                target,
                resources,
                input: RewriteInput {
                    content: &content,
                    draws: &scanner.draws,
                },
                config,
                flate_level,
                alpha_crop_cache: &mut alpha_crop_cache,
            },
            plans,
            &mut stats,
        )?;
        stats.apply_plans_us = stats
            .apply_plans_us
            .saturating_add(apply_plans_started.elapsed().as_micros() as u64);
        if applied.changed
            && let ContentTarget::Page(page) = target
            && let Some(cache) = vector_cache.as_deref_mut()
        {
            cache.remove(&page);
        }

        if !scanner.resource_pending_operands {
            let mut consumed = pruned_draws.clone();
            consumed.extend(applied.consumed_draws.iter().copied());
            let mut usage = scanner.resource_usage_after_removing(&consumed);
            usage
                .names
                .extend(applied.added_resource_names.iter().cloned());
            usage
                .by_type
                .entry(b"XObject".to_vec())
                .or_default()
                .extend(applied.added_resource_names);
            match target {
                ContentTarget::Page(page) => {
                    stats.page_resource_names.insert(page, usage.names);
                    stats
                        .page_resource_names_by_type
                        .insert(page, usage.by_type);
                }
                ContentTarget::Form(form) => {
                    stats.form_resource_names.insert(form, usage.names);
                    stats
                        .form_resource_names_by_type
                        .insert(form, usage.by_type);
                }
            }
        } else {
            stats.resource_inventory_complete = false;
        }
    }
    let staging_cleanup_started = Instant::now();
    stats.staging_xobject_entries_removed =
        cleanup_fragmented_inline_staging_hayro(document, &staged_xobjects)?;
    stats.staging_cleanup_us = staging_cleanup_started.elapsed().as_micros() as u64;
    if *DEBUG_RASTER {
        eprintln!(
            "raster image decode cache: hits={} misses={} entries={} decoded_bytes={} clears={}",
            image_cache.hits,
            image_cache.misses,
            image_cache.entries.len(),
            image_cache.decoded_bytes,
            image_cache.clears
        );
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectId;

    fn test_draw(object: i32, x: f64) -> RasterDraw {
        let ctm = Matrix::new(1.0, 0.0, 0.0, 1.0, x, 0.0);
        RasterDraw {
            target: ObjectHandle::Existing(ObjectId::new(object, 0)),
            resource_name: format!("Im{object}").into_bytes(),
            ctm,
            replace_ctm: ctm,
            range_start: 0,
            range_end: 0,
            epoch: 0,
            paint_generation: 0,
            gs_name: None,
            rendering_intent: None,
            clip: None,
            clip_polygon: None,
            clip_complex: false,
        }
    }

    #[test]
    fn vector_inventory_flags_contained_fill_candidate_after_ext_gstate() {
        let mut scanner = RasterScanner::new(BTreeMap::new(), HashSet::new());
        let input = b"/GS0 gs 0 0 2 2 re f 0 0 10 10 re f";
        assert!(
            flpdf::parse_detached_content_stream(
                input,
                "vector inventory containment",
                &mut scanner,
            )
            .is_ok()
        );
        assert!(scanner.vector_merge_candidate);
    }

    #[test]
    fn vector_inventory_flags_redundant_contained_repaint() {
        let mut scanner = RasterScanner::new(BTreeMap::new(), HashSet::new());
        let input = b"0 0 10 10 re f 2 2 1 1 re f";
        assert!(
            flpdf::parse_detached_content_stream(
                input,
                "vector inventory contained repaint",
                &mut scanner,
            )
            .is_ok()
        );
        assert!(scanner.vector_merge_candidate);
    }

    #[test]
    fn resource_inventory_tracks_namespaces_separately() {
        let mut scanner = RasterScanner::new(BTreeMap::new(), HashSet::new());
        let input = b"/Shared gs /Shared scn /Sh0 sh";
        assert!(
            flpdf::parse_detached_content_stream(input, "typed resource inventory", &mut scanner)
                .is_ok()
        );
        let usage = scanner.resource_usage_after_removing(&HashSet::new());
        assert_eq!(
            usage.by_type.get(b"ExtGState".as_slice()),
            Some(&BTreeSet::from([b"Shared".to_vec()]))
        );
        assert_eq!(
            usage.by_type.get(b"Pattern".as_slice()),
            Some(&BTreeSet::from([b"Shared".to_vec()]))
        );
        assert_eq!(
            usage.by_type.get(b"Shading".as_slice()),
            Some(&BTreeSet::from([b"Sh0".to_vec()]))
        );
    }

    #[test]
    fn resource_inventory_does_not_reuse_a_stale_name() {
        let mut scanner = RasterScanner::new(BTreeMap::new(), HashSet::new());
        let input = b"/Im0 Do 1 Do";
        assert!(
            flpdf::parse_detached_content_stream(input, "stale resource name", &mut scanner)
                .is_ok()
        );
        let usage = scanner.resource_usage_after_removing(&HashSet::new());
        assert_eq!(
            usage.by_type.get(b"XObject".as_slice()),
            Some(&BTreeSet::from([b"Im0".to_vec()]))
        );
    }

    #[test]
    fn packed_samples_ignore_row_padding_bits() {
        assert_eq!(
            unpack_packed_samples(&[0x12, 0x30], 3, 1, 4),
            Some(vec![1, 2, 3])
        );
        assert_eq!(
            unpack_packed_samples(&[0b1010_0000], 3, 1, 1),
            Some(vec![1, 0, 1])
        );
    }

    #[test]
    fn z_order_solver_finds_member_after_same_side_blockers() {
        let draws = vec![
            test_draw(1, 0.0),
            test_draw(2, 10.0),
            test_draw(3, 10.0),
            test_draw(4, 20.0),
            test_draw(5, 20.0),
        ];
        assert_eq!(raster_draw_insertion_member(&draws, &[0, 2, 4]), Some(4));
    }

    #[test]
    fn z_order_solver_rejects_blocker_between_overlapping_members() {
        let draws = vec![test_draw(1, 0.0), test_draw(2, 0.0), test_draw(3, 0.0)];
        assert_eq!(raster_draw_insertion_member(&draws, &[0, 2]), None);
    }

    #[test]
    fn redundant_collinear_clip_vertices_still_form_rectangle() {
        let mut scanner = RasterScanner::new(BTreeMap::new(), HashSet::new());
        scanner.begin_line_path(750.0, 6661.67);
        scanner.add_line_point(1162.0, 6661.67);
        scanner.add_line_point(1162.0, 6468.67);
        scanner.add_line_point(956.0, 6468.67);
        scanner.add_line_point(750.0, 6468.67);
        scanner.path_closed = true;
        let rect = scanner.path_as_axis_aligned_rect();
        assert!(rect.is_some());
        let Some(rect) = rect else {
            return;
        };
        assert!((rect.x0 - 750.0).abs() < 1.0e-9);
        assert!((rect.x1 - 1162.0).abs() < 1.0e-9);
        assert!((rect.y0 - 6468.67).abs() < 1.0e-9);
        assert!((rect.y1 - 6661.67).abs() < 1.0e-9);
    }

    #[test]
    fn rectangle_union_covers_target_without_single_cover() {
        let target = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let covers = [
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 5.0,
                y1: 10.0,
            },
            Rect {
                x0: 5.0,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
            },
        ];
        assert!(rect_covered_by_union(target, &covers, 1.0e-6));
    }

    #[test]
    fn rectangle_union_rejects_real_gap() {
        let target = Rect {
            x0: 0.0,
            y0: 0.0,
            x1: 10.0,
            y1: 10.0,
        };
        let covers = [
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 4.9,
                y1: 10.0,
            },
            Rect {
                x0: 5.1,
                y0: 0.0,
                x1: 10.0,
                y1: 10.0,
            },
        ];
        assert!(!rect_covered_by_union(target, &covers, 1.0e-6));
    }

    #[test]
    fn rectangular_clip_reduces_visible_raster_extent() {
        let mut draw = test_draw(1, 0.0);
        draw.ctm = Matrix::new(10.0, 0.0, 0.0, 10.0, 0.0, 0.0);
        draw.clip = Some(Rect {
            x0: 2.0,
            y0: 3.0,
            x1: 8.0,
            y1: 9.0,
        });
        let rect = match axis_aligned_candidate_rect(&draw) {
            Some(Some(rect)) => rect,
            _ => panic!("expected visible axis-aligned clipped rectangle"),
        };
        assert!((rect.x0 - 2.0).abs() < 1.0e-9);
        assert!((rect.y0 - 3.0).abs() < 1.0e-9);
        assert!((rect.x1 - 8.0).abs() < 1.0e-9);
        assert!((rect.y1 - 9.0).abs() < 1.0e-9);
    }

    #[test]
    fn binary_gray_samples_pack_msb_first_per_row() {
        let data = [255, 0, 255, 0, 0, 255, 0, 255, 255, 255];
        assert_eq!(
            pack_binary_gray_samples(&data, 5, 2),
            Some(vec![0b1010_0000, 0b1011_1000])
        );
        assert_eq!(pack_binary_gray_samples(&[0, 127], 2, 1), None);
    }

    #[test]
    fn binary_rgb_samples_pack_components_msb_first_and_pad_each_row() {
        assert_eq!(
            pack_binary_samples(&[0, 0, 0, 255, 255, 255], 2, 1, 3),
            Some(vec![0b0001_1100])
        );
        assert_eq!(
            pack_binary_samples(&[0, 0, 0, 255, 255, 255], 1, 2, 3),
            Some(vec![0b0000_0000, 0b1110_0000])
        );
        assert_eq!(pack_binary_samples(&[0, 0, 127], 1, 1, 3), None);
        assert_eq!(pack_binary_samples(&[0, 0], 1, 1, 3), None);
    }

    #[test]
    fn visible_stencil_color_ignores_only_transparent_samples() {
        let info = SampleImage {
            width: 4,
            height: 1,
            components: 3,
            preserve_encoded_color: false,
            encoded_color_bytes: 0,
            encoded_mask_bytes: 0,
            data: Vec::<u8>::new().into(),
            alpha: None,
            mask_width: 4,
            mask_height: 1,
            semantic_key: [0; 32],
            dictionary_template: OwnedDictionary::from([(
                b"ColorSpace".to_vec(),
                OwnedObject::Name(b"DeviceRGB".to_vec()),
            )]),
            interpolate: false,
        };
        let data = [
            0, 0, 0, // visible black
            255, 255, 255, // transparent white
            0, 0, 0, // visible black
            255, 0, 0, // transparent red
        ];
        assert_eq!(
            constant_visible_device_color(&info, &data, &[255, 0, 255, 0]),
            Some(StencilColor::Rgb([0, 0, 0]))
        );
        assert_eq!(
            constant_visible_device_color(&info, &data, &[255, 0, 255, 255]),
            None
        );
        assert_eq!(
            constant_visible_device_color(&info, &data, &[0, 0, 0, 0]),
            None
        );
    }

    #[test]
    fn stencil_alpha_pack_uses_zero_bits_for_painted_pixels() {
        let alpha = [255, 0, 255, 0, 0, 255, 0, 255, 255, 255];
        // Default PDF ImageMask /Decode [0 1]: zero paints, one is transparent.
        assert_eq!(
            pack_binary_stencil_alpha(&alpha, 5, 2),
            Some(vec![0b0101_1000, 0b0100_0000])
        );
        assert_eq!(pack_binary_stencil_alpha(&[0, 127], 2, 1), None);
    }

    fn stripe_test_image(width: u32, height: u32, data: Vec<u8>) -> SampleImage {
        SampleImage {
            width,
            height,
            components: 1,
            preserve_encoded_color: false,
            encoded_color_bytes: 0,
            encoded_mask_bytes: 0,
            data: data.into(),
            alpha: None,
            mask_width: width,
            mask_height: height,
            semantic_key: [0x53; 32],
            dictionary_template: OwnedDictionary::new(),
            interpolate: false,
        }
    }

    #[test]
    fn scanner_splits_stripe_generation_across_text_paint() {
        let im1 = ObjectHandle::Existing(ObjectId::new(1, 0));
        let im2 = ObjectHandle::Existing(ObjectId::new(2, 0));
        let mut scanner = RasterScanner::new(
            BTreeMap::from([(b"Im1".to_vec(), im1), (b"Im2".to_vec(), im2)]),
            HashSet::from([b"Im1".to_vec(), b"Im2".to_vec()]),
        );
        let input = b"/Im1 Do (overlay) Tj /Im2 Do";
        assert!(
            flpdf::parse_detached_content_stream(input, "stripe text z-order", &mut scanner)
                .is_ok()
        );
        assert_eq!(scanner.draws.len(), 2);
        assert_ne!(
            scanner.draws[0].paint_generation,
            scanner.draws[1].paint_generation
        );
    }

    #[test]
    fn stripe_link_rejects_intervening_non_image_paint_generation() {
        let a = test_draw(1, 0.0);
        let mut b = test_draw(2, 2.0);
        b.paint_generation = 1;
        let ia = stripe_test_image(2, 6, vec![0; 12]);
        let ib = stripe_test_image(2, 6, vec![0; 12]);
        assert_eq!(stripe_link(&a, &b, &ia, &ib, 0.0), None);
    }

    #[test]
    fn vertical_stripe_plan_reassembles_source_pixels_in_raster_order() {
        let mut top = test_draw(1, 0.0);
        top.ctm = Matrix::new(6.0, 0.0, 0.0, 2.0, 10.0, 22.0);
        top.replace_ctm = top.ctm;
        let mut bottom = test_draw(2, 0.0);
        bottom.ctm = Matrix::new(6.0, 0.0, 0.0, 2.0, 10.0, 20.0);
        bottom.replace_ctm = bottom.ctm;
        let draws = vec![top, bottom];
        let infos = HashMap::from([
            (draws[0].target, stripe_test_image(6, 2, (1..=12).collect())),
            (
                draws[1].target,
                stripe_test_image(6, 2, (13..=24).collect()),
            ),
        ]);
        assert_eq!(
            stripe_link(
                &draws[0],
                &draws[1],
                &infos[&draws[0].target],
                &infos[&draws[1].target],
                0.0
            ),
            Some(StripeAxis::Vertical)
        );
        let plan = match build_stripe_plan(&draws, &infos, &[0, 1], StripeAxis::Vertical, 1024) {
            Some(plan) => plan,
            None => panic!("expected vertical stripe plan"),
        };
        assert_eq!((plan.width, plan.height), (6, 4));
        assert_eq!(plan.data, (1..=24).collect::<Vec<_>>());
        assert_eq!(
            plan.desired_ctm,
            Matrix::new(6.0, 0.0, 0.0, 4.0, 10.0, 20.0)
        );
    }

    #[test]
    fn horizontal_stripe_plan_reassembles_source_pixels_row_by_row() {
        let mut left = test_draw(1, 0.0);
        left.ctm = Matrix::new(2.0, 0.0, 0.0, 6.0, 10.0, 20.0);
        left.replace_ctm = left.ctm;
        let mut right = test_draw(2, 0.0);
        right.ctm = Matrix::new(2.0, 0.0, 0.0, 6.0, 12.0, 20.0);
        right.replace_ctm = right.ctm;
        let draws = vec![left, right];
        let infos = HashMap::from([
            (draws[0].target, stripe_test_image(2, 6, (1..=12).collect())),
            (
                draws[1].target,
                stripe_test_image(2, 6, (13..=24).collect()),
            ),
        ]);
        assert_eq!(
            stripe_link(
                &draws[0],
                &draws[1],
                &infos[&draws[0].target],
                &infos[&draws[1].target],
                0.0
            ),
            Some(StripeAxis::Horizontal)
        );
        let plan = match build_stripe_plan(&draws, &infos, &[0, 1], StripeAxis::Horizontal, 1024) {
            Some(plan) => plan,
            None => panic!("expected horizontal stripe plan"),
        };
        assert_eq!((plan.width, plan.height), (4, 6));
        assert_eq!(
            plan.data,
            vec![
                1, 2, 13, 14, 3, 4, 15, 16, 5, 6, 17, 18, 7, 8, 19, 20, 9, 10, 21, 22, 11, 12, 23,
                24,
            ]
        );
        assert_eq!(
            plan.desired_ctm,
            Matrix::new(4.0, 0.0, 0.0, 6.0, 10.0, 20.0)
        );
    }

    #[test]
    fn vertical_stripe_plan_uses_geometry_not_content_order() {
        let mut bottom = test_draw(1, 0.0);
        bottom.ctm = Matrix::new(6.0, 0.0, 0.0, 2.0, 10.0, 20.0);
        bottom.replace_ctm = bottom.ctm;
        let mut top = test_draw(2, 0.0);
        top.ctm = Matrix::new(6.0, 0.0, 0.0, 2.0, 10.0, 22.0);
        top.replace_ctm = top.ctm;
        let draws = vec![bottom, top];
        let infos = HashMap::from([
            (
                draws[0].target,
                stripe_test_image(6, 2, (13..=24).collect()),
            ),
            (draws[1].target, stripe_test_image(6, 2, (1..=12).collect())),
        ]);
        assert_eq!(
            stripe_link(
                &draws[0],
                &draws[1],
                &infos[&draws[0].target],
                &infos[&draws[1].target],
                0.0
            ),
            Some(StripeAxis::Vertical)
        );
        let plan = match build_stripe_plan(&draws, &infos, &[0, 1], StripeAxis::Vertical, 1024) {
            Some(plan) => plan,
            None => panic!("expected reverse-order vertical stripe plan"),
        };
        assert_eq!(plan.data, (1..=24).collect::<Vec<_>>());
        assert_eq!(
            plan.desired_ctm,
            Matrix::new(6.0, 0.0, 0.0, 4.0, 10.0, 20.0)
        );
    }

    #[test]
    fn rotated_vertical_stripe_plan_preserves_affine_geometry() {
        // Per-source-pixel basis: ux=(0,1), uy=(-1,0), i.e. 90-degree rotation.
        let mut top = test_draw(1, 0.0);
        top.ctm = Matrix::new(0.0, 6.0, -2.0, 0.0, 8.0, 20.0);
        top.replace_ctm = top.ctm;
        let mut bottom = test_draw(2, 0.0);
        bottom.ctm = Matrix::new(0.0, 6.0, -2.0, 0.0, 10.0, 20.0);
        bottom.replace_ctm = bottom.ctm;
        let draws = vec![top, bottom];
        let infos = HashMap::from([
            (draws[0].target, stripe_test_image(6, 2, (1..=12).collect())),
            (
                draws[1].target,
                stripe_test_image(6, 2, (13..=24).collect()),
            ),
        ]);
        assert_eq!(
            stripe_link(
                &draws[0],
                &draws[1],
                &infos[&draws[0].target],
                &infos[&draws[1].target],
                0.0
            ),
            Some(StripeAxis::Vertical)
        );
        let plan = match build_stripe_plan(&draws, &infos, &[0, 1], StripeAxis::Vertical, 1024) {
            Some(plan) => plan,
            None => panic!("expected rotated vertical stripe plan"),
        };
        assert_eq!(plan.data, (1..=24).collect::<Vec<_>>());
        assert_eq!(
            plan.desired_ctm,
            Matrix::new(0.0, 6.0, -4.0, 0.0, 10.0, 20.0)
        );
    }

    #[test]
    fn stripe_plan_composes_missing_and_explicit_alpha_without_reordering() {
        let mut top = test_draw(1, 0.0);
        top.ctm = Matrix::new(6.0, 0.0, 0.0, 2.0, 10.0, 22.0);
        top.replace_ctm = top.ctm;
        let mut bottom = test_draw(2, 0.0);
        bottom.ctm = Matrix::new(6.0, 0.0, 0.0, 2.0, 10.0, 20.0);
        bottom.replace_ctm = bottom.ctm;
        let draws = vec![top, bottom];
        let top_image = stripe_test_image(6, 2, vec![10; 12]);
        let mut bottom_image = stripe_test_image(6, 2, vec![20; 12]);
        bottom_image.alpha = Some(AlphaPlane {
            data: vec![0, 32, 64, 96, 128, 160, 192, 224, 255, 224, 192, 160].into(),
            width: 6,
            height: 2,
        });
        let infos = HashMap::from([
            (draws[0].target, top_image),
            (draws[1].target, bottom_image),
        ]);
        let plan = match build_stripe_plan(&draws, &infos, &[0, 1], StripeAxis::Vertical, 1024) {
            Some(plan) => plan,
            None => panic!("expected alpha stripe plan"),
        };
        let alpha = match plan.alpha {
            Some(alpha) => alpha,
            None => panic!("expected combined alpha"),
        };
        assert_eq!(alpha.len(), 24);
        assert_eq!(&alpha[..12], &[255; 12]);
        assert_eq!(
            &alpha[12..],
            &[0, 32, 64, 96, 128, 160, 192, 224, 255, 224, 192, 160]
        );
    }

    #[test]
    fn transparent_crop_finds_nonzero_alpha_bounds() {
        let alpha = vec![
            0, 0, 0, 0, //
            0, 255, 255, 0, //
            0, 255, 255, 0, //
            0, 0, 0, 0,
        ];
        assert_eq!(
            alpha_crop(&alpha, 4, 4),
            Some(PixelCrop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            })
        );
    }

    #[test]
    fn alpha_crop_preserves_compact_encoded_color_images() {
        let draw = test_draw(1, 0.0);
        let mut image = stripe_test_image(4, 4, (0..16).collect());
        image.preserve_encoded_color = true;
        image.alpha = Some(AlphaPlane {
            data: vec![0, 0, 0, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 0, 0].into(),
            width: 4,
            height: 4,
        });
        let target = draw.target;
        let infos = HashMap::from([(target, image)]);
        let mut plans = Vec::new();
        let mut alpha_crop_bounds_cache = HashMap::new();
        append_alpha_crop_plans(
            &[draw],
            &infos,
            &HashSet::new(),
            &mut alpha_crop_bounds_cache,
            &mut plans,
        );
        assert!(plans.is_empty());
    }

    #[test]
    fn standalone_alpha_image_skips_tiny_crop_plan() {
        let draw = test_draw(1, 0.0);
        let mut image = stripe_test_image(4, 4, (0..16).collect());
        image.alpha = Some(AlphaPlane {
            data: vec![0, 0, 0, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 0, 0].into(),
            width: 4,
            height: 4,
        });
        let target = draw.target;
        let infos = HashMap::from([(target, image)]);
        let mut plans = Vec::new();
        let mut alpha_crop_bounds_cache = HashMap::new();
        append_alpha_crop_plans(
            &[draw],
            &infos,
            &HashSet::new(),
            &mut alpha_crop_bounds_cache,
            &mut plans,
        );
        assert_eq!(
            alpha_crop_bounds_cache.get(&target).copied().flatten(),
            Some(PixelCrop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            })
        );
        assert!(plans.is_empty());
    }

    #[test]
    fn standalone_alpha_image_accepts_twenty_pixel_total_crop() {
        let draw = test_draw(1, 0.0);
        let mut image = stripe_test_image(30, 20, vec![0; 30 * 20]);
        let mut alpha = vec![0; 30 * 20];
        for y in 5..15usize {
            for x in 5..25usize {
                alpha[y * 30 + x] = 255;
            }
        }
        image.alpha = Some(AlphaPlane {
            data: alpha.into(),
            width: 30,
            height: 20,
        });
        let target = draw.target;
        let infos = HashMap::from([(target, image)]);
        let mut plans = Vec::new();
        let mut alpha_crop_bounds_cache = HashMap::new();
        append_alpha_crop_plans(
            &[draw],
            &infos,
            &HashSet::new(),
            &mut alpha_crop_bounds_cache,
            &mut plans,
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(
            plans[0].alpha_crop_hint,
            Some(PixelCrop {
                x: 5,
                y: 5,
                width: 20,
                height: 10,
            })
        );
        assert_eq!(
            plans[0]
                .alpha_crop_hint
                .and_then(|crop| crop_total_margin_pixels(30, 20, crop)),
            Some(MIN_TOTAL_CROP_MARGIN_PIXELS)
        );
    }

    #[test]
    fn semantic_dictionary_key_is_structural_and_value_sensitive() {
        let a = OwnedDictionary::from([
            (b"A".to_vec(), OwnedObject::Integer(7)),
            (
                b"B".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Name(b"DeviceRGB".to_vec()),
                    OwnedObject::Real(1.25),
                ]),
            ),
        ]);
        let b = a.clone();
        let mut changed = a.clone();
        changed.insert(b"A".to_vec(), OwnedObject::Integer(8));

        assert_eq!(semantic_dictionary_key(&a), semantic_dictionary_key(&b));
        assert_ne!(
            semantic_dictionary_key(&a),
            semantic_dictionary_key(&changed)
        );
    }

    #[test]
    fn background_crop_finds_uniform_border() {
        let mut data = vec![255u8; 4 * 4 * 3];
        for y in 1..3usize {
            for x in 1..3usize {
                let index = (y * 4 + x) * 3;
                data[index..index + 3].fill(0);
            }
        }
        let background = match dominant_border_sample(&data, 4, 4, 3) {
            Some(background) => background,
            None => panic!("expected white border"),
        };
        assert_eq!(background, vec![255, 255, 255]);
        assert_eq!(
            background_crop(&data, 4, 4, 3, &background, 0),
            Some(Some(PixelCrop {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            }))
        );
    }

    #[test]
    fn crop_plan_updates_transform_in_pixel_coordinates() {
        let mut plan = MergePlan {
            members: vec![0],
            first_member: 0,
            image: SampleImage {
                width: 4,
                height: 4,
                components: 1,
                preserve_encoded_color: false,
                encoded_color_bytes: 0,
                encoded_mask_bytes: 0,
                data: vec![0; 16].into(),
                alpha: None,
                mask_width: 4,
                mask_height: 4,
                semantic_key: [0; 32],
                dictionary_template: OwnedDictionary::new(),
                interpolate: false,
            },
            width: 4,
            height: 4,
            data: (0..16).collect(),
            alpha: None,
            source_color_budget: None,
            desired_ctm: Matrix::new(40.0, 0.0, 0.0, 20.0, 10.0, 30.0),
            background: None,
            alpha_crop_hint: None,
            kind: MergeKind::PixelCluster,
        };
        assert_eq!(
            crop_plan_to(
                &mut plan,
                PixelCrop {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                }
            ),
            Some(12)
        );
        assert_eq!(plan.width, 2);
        assert_eq!(plan.height, 2);
        assert_eq!(plan.data, vec![5, 6, 9, 10]);
        assert_eq!(
            plan.desired_ctm,
            Matrix::new(20.0, 0.0, 0.0, 10.0, 20.0, 35.0)
        );
    }
}
