use crate::geometry::Matrix;
use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    bilevel::{BilevelCodec, BilevelImagePayload, BilevelRaster, compress_flate},
    content::{
        decoded_content_value, effective_page_resources, install_page_resource, page_user_unit,
        replace_page_content, resolved_bool_value, resolved_dictionary, resolved_number_value,
    },
};
use std::collections::{BTreeMap, BTreeSet};

const MIN_MICROSTROKE_RUN: usize = 500;
const MIN_RUN_SAVINGS_BYTES: usize = 1024;
const MIN_RUN_SAVINGS_PERCENT: usize = 20;
const MIN_PITCH: f64 = 0.04;
const MAX_PITCH: f64 = 0.25;
const MAX_RASTER_DIMENSION: usize = 16_384;
const MAX_RASTER_PIXELS: usize = 100_000_000;
const MAX_PAGE_RASTER_PITCH_PT: f64 = 72.0 / 450.0;
const MIN_STROKE_RASTER_PIXELS: f64 = 1.5;

/// Internal-facing statistics from the pathological-microstroke rasterization pass.
///
/// This type is public only so the separate CLI crate can report dry-run diagnostics;
/// it is hidden from the normal library documentation surface.
#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MicrostrokeRasterStats {
    /// Number of pages whose content would be rewritten.
    pub pages_rewritten: usize,
    /// Number of qualifying microstroke runs selected for rasterization.
    pub runs_rasterized: usize,
    /// Number of individual strokes represented by the selected runs.
    pub strokes_rasterized: usize,
    /// Total encoded bytes of generated image payloads.
    pub image_payload_bytes: usize,
    /// Number of generated images encoded with CCITT Group 4.
    pub ccitt_images: usize,
    /// Number of generated images encoded with Flate.
    pub flate_images: usize,
    /// Estimated encoded bytes saved relative to the equivalent Flate representation.
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone)]
struct Operand {
    number: Option<f64>,
    name: Option<Vec<u8>>,
}

fn operand_numbers(operands: &[Operand], expected: usize) -> Option<Vec<f64>> {
    if operands.len() != expected {
        return None;
    }
    operands.iter().map(|operand| operand.number).collect()
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum StrokeColor {
    Gray(f64),
    Rgb([f64; 3]),
    Cmyk([f64; 4]),
}

impl StrokeColor {
    fn content_operator(self) -> Vec<u8> {
        match self {
            Self::Gray(g) => format!("{} g ", compact_real(g)).into_bytes(),
            Self::Rgb([r, g, b]) => format!(
                "{} {} {} rg ",
                compact_real(r),
                compact_real(g),
                compact_real(b)
            )
            .into_bytes(),
            Self::Cmyk([c, m, y, k]) => format!(
                "{} {} {} {} k ",
                compact_real(c),
                compact_real(m),
                compact_real(y),
                compact_real(k)
            )
            .into_bytes(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "named safety predicates make rasterization preconditions directly auditable"
)]
struct StrokeSafety {
    stroke_alpha_opaque: bool,
    fill_alpha_opaque: bool,
    normal_blend: bool,
    no_soft_mask: bool,
    stroke_overprint_disabled: bool,
    fill_overprint_disabled: bool,
}

impl Default for StrokeSafety {
    fn default() -> Self {
        Self {
            stroke_alpha_opaque: true,
            fill_alpha_opaque: true,
            normal_blend: true,
            no_soft_mask: true,
            stroke_overprint_disabled: true,
            fill_overprint_disabled: true,
        }
    }
}

impl StrokeSafety {
    const fn safe_for_stencil(self) -> bool {
        self.stroke_alpha_opaque
            && self.fill_alpha_opaque
            && self.normal_blend
            && self.no_soft_mask
            && self.stroke_overprint_disabled
            && self.fill_overprint_disabled
    }

    const fn invalidate(&mut self) {
        *self = Self {
            stroke_alpha_opaque: false,
            fill_alpha_opaque: false,
            normal_blend: false,
            no_soft_mask: false,
            stroke_overprint_disabled: false,
            fill_overprint_disabled: false,
        };
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ExtGStatePatch {
    supported: bool,
    stroke_alpha_opaque: Option<bool>,
    fill_alpha_opaque: Option<bool>,
    normal_blend: Option<bool>,
    no_soft_mask: Option<bool>,
    stroke_overprint_disabled: Option<bool>,
    fill_overprint_disabled: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct StrokeState {
    ctm: Matrix,
    width: f64,
    cap: u8,
    join: u8,
    color: Option<StrokeColor>,
    solid_dash: bool,
    safety: StrokeSafety,
}

impl Default for StrokeState {
    fn default() -> Self {
        Self {
            ctm: Matrix::default(),
            width: 1.0,
            cap: 0,
            join: 0,
            color: Some(StrokeColor::Gray(0.0)),
            solid_dash: true,
            safety: StrokeSafety::default(),
        }
    }
}

impl StrokeState {
    fn raster_safe(self) -> bool {
        self.width.is_finite()
            && self.width > 0.0
            && self.cap <= 2
            && self.join <= 2
            && self.color.is_some()
            && self.solid_dash
            && self.safety.safe_for_stencil()
    }

    const fn apply_ext_gstate(&mut self, patch: ExtGStatePatch) {
        if !patch.supported {
            self.safety.invalidate();
            return;
        }
        if let Some(value) = patch.stroke_alpha_opaque {
            self.safety.stroke_alpha_opaque = value;
        }
        if let Some(value) = patch.fill_alpha_opaque {
            self.safety.fill_alpha_opaque = value;
        }
        if let Some(value) = patch.normal_blend {
            self.safety.normal_blend = value;
        }
        if let Some(value) = patch.no_soft_mask {
            self.safety.no_soft_mask = value;
        }
        if let Some(value) = patch.stroke_overprint_disabled {
            self.safety.stroke_overprint_disabled = value;
        }
        if let Some(value) = patch.fill_overprint_disabled {
            self.safety.fill_overprint_disabled = value;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStage {
    ExpectTranslation,
    ExpectMove,
    ExpectLine,
    ExpectStroke,
    ExpectRestore,
    Invalid,
}

#[derive(Debug, Clone)]
struct BlockFrame {
    start: usize,
    base: StrokeState,
    stage: BlockStage,
    tx: f64,
    ty: f64,
    p0: Option<(f64, f64)>,
    p1: Option<(f64, f64)>,
}

#[derive(Debug, Clone)]
struct MicroStrokeBlock {
    start: usize,
    end: usize,
    state: StrokeState,
    p0: (f64, f64),
    p1: (f64, f64),
}

struct MicroStrokeScanner {
    operands: Vec<Operand>,
    state: StrokeState,
    stack: Vec<StrokeState>,
    ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>,
    frame: Option<BlockFrame>,
    path_nonempty: bool,
    blocks: Vec<MicroStrokeBlock>,
}

impl MicroStrokeScanner {
    fn new(ext_gstates: BTreeMap<Vec<u8>, ExtGStatePatch>) -> Self {
        Self {
            operands: Vec::new(),
            state: StrokeState::default(),
            stack: Vec::new(),
            ext_gstates,
            frame: None,
            path_nonempty: false,
            blocks: Vec::new(),
        }
    }

    const fn invalidate_frame(&mut self) {
        if let Some(frame) = &mut self.frame {
            frame.stage = BlockStage::Invalid;
        }
    }

    fn set_gray(&mut self) {
        let Some(values) = operand_numbers(&self.operands, 1) else {
            self.state.color = None;
            return;
        };
        self.state.color = values[0]
            .is_finite()
            .then_some(StrokeColor::Gray(values[0]));
    }

    fn set_rgb(&mut self) {
        let Some(values) = operand_numbers(&self.operands, 3) else {
            self.state.color = None;
            return;
        };
        self.state.color = values
            .iter()
            .all(|value| value.is_finite())
            .then_some(StrokeColor::Rgb([values[0], values[1], values[2]]));
    }

    fn set_cmyk(&mut self) {
        let Some(values) = operand_numbers(&self.operands, 4) else {
            self.state.color = None;
            return;
        };
        self.state.color =
            values
                .iter()
                .all(|value| value.is_finite())
                .then_some(StrokeColor::Cmyk([
                    values[0], values[1], values[2], values[3],
                ]));
    }

    fn concat_ctm(&mut self) {
        let Some(values) = operand_numbers(&self.operands, 6) else {
            self.state.safety.invalidate();
            return;
        };
        if !values.iter().all(|value| value.is_finite()) {
            self.state.safety.invalidate();
            return;
        }
        self.state.ctm.concat(Matrix::new(
            values[0], values[1], values[2], values[3], values[4], values[5],
        ));
    }

    fn apply_ext_gstate(&mut self) {
        let patch = self
            .operands
            .first()
            .and_then(|operand| operand.name.as_deref())
            .and_then(|name| self.ext_gstates.get(name).copied());
        if let Some(patch) = patch {
            self.state.apply_ext_gstate(patch);
        } else {
            self.state.safety.invalidate();
        }
    }

    fn instruction(
        &mut self,
        content: &[u8],
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
    ) {
        if &instruction.operator[..] == b"BI" {
            self.invalidate_frame();
            self.operands.clear();
            return;
        }
        self.operands = instruction
            .operands()
            .zip(instruction.operand_spans())
            .map(|(object, span)| Operand {
                number: crate::content_stream::operand_number(
                    object,
                    content.get(span).unwrap_or_default(),
                ),
                name: crate::content_stream::operand_name(object).map(ToOwned::to_owned),
            })
            .collect();
        let span = instruction.operator_span();
        self.process_operator(&instruction.operator[..], span.start, span.len());
    }

    fn scan(&mut self, content: &[u8]) -> crate::Result<()> {
        let incomplete = crate::content_stream::visit_instructions(content, |instruction| {
            self.instruction(content, instruction);
            Ok(())
        })?;
        if incomplete {
            self.invalidate_frame();
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_lines,
        reason = "PDF graphics operator state transitions must remain ordered and co-located for auditability"
    )]
    fn process_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        let end = offset.saturating_add(length);
        match operator {
            b"q" => {
                if self.frame.is_some() {
                    self.invalidate_frame();
                } else if !self.path_nonempty {
                    self.frame = Some(BlockFrame {
                        start: offset,
                        base: self.state,
                        stage: BlockStage::ExpectTranslation,
                        tx: 0.0,
                        ty: 0.0,
                        p0: None,
                        p1: None,
                    });
                }
                self.stack.push(self.state);
            }
            b"Q" => {
                let frame = self.frame.take();
                if let Some(frame) = frame
                    && frame.stage == BlockStage::ExpectRestore
                    && let (Some(p0), Some(p1)) = (frame.p0, frame.p1)
                    && [p0.0, p0.1, p1.0, p1.1].into_iter().all(f64::is_finite)
                {
                    self.blocks.push(MicroStrokeBlock {
                        start: frame.start,
                        end,
                        state: frame.base,
                        p0,
                        p1,
                    });
                }
                if let Some(state) = self.stack.pop() {
                    self.state = state;
                } else {
                    self.state.safety.invalidate();
                }
            }
            b"cm" => {
                if let Some(frame) = &mut self.frame {
                    if frame.stage == BlockStage::ExpectTranslation {
                        let values = operand_numbers(&self.operands, 6);
                        if let Some(values) = values.filter(|values| {
                            values.iter().all(|value| value.is_finite())
                                && (values[0] - 1.0).abs() <= 1.0e-12
                                && values[1].abs() <= 1.0e-12
                                && values[2].abs() <= 1.0e-12
                                && (values[3] - 1.0).abs() <= 1.0e-12
                        }) {
                            frame.tx = values[4];
                            frame.ty = values[5];
                            frame.stage = BlockStage::ExpectMove;
                        } else {
                            frame.stage = BlockStage::Invalid;
                        }
                    } else {
                        frame.stage = BlockStage::Invalid;
                    }
                }
                self.concat_ctm();
            }
            b"m" => {
                self.path_nonempty = true;
                if let Some(frame) = &mut self.frame {
                    if frame.stage == BlockStage::ExpectMove {
                        if let Some(values) = operand_numbers(&self.operands, 2) {
                            frame.p0 = Some((frame.tx + values[0], frame.ty + values[1]));
                            frame.stage = BlockStage::ExpectLine;
                        } else {
                            frame.stage = BlockStage::Invalid;
                        }
                    } else {
                        frame.stage = BlockStage::Invalid;
                    }
                }
            }
            b"l" => {
                self.path_nonempty = true;
                if let Some(frame) = &mut self.frame {
                    if frame.stage == BlockStage::ExpectLine {
                        if let Some(values) = operand_numbers(&self.operands, 2) {
                            frame.p1 = Some((frame.tx + values[0], frame.ty + values[1]));
                            frame.stage = BlockStage::ExpectStroke;
                        } else {
                            frame.stage = BlockStage::Invalid;
                        }
                    } else {
                        frame.stage = BlockStage::Invalid;
                    }
                }
            }
            b"S" => {
                if let Some(frame) = &mut self.frame {
                    if frame.stage == BlockStage::ExpectStroke {
                        frame.stage = BlockStage::ExpectRestore;
                    } else {
                        frame.stage = BlockStage::Invalid;
                    }
                }
                self.path_nonempty = false;
            }
            b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                self.invalidate_frame();
                self.path_nonempty = false;
            }
            b"re" | b"c" | b"v" | b"y" | b"h" => {
                self.invalidate_frame();
                self.path_nonempty = true;
            }
            b"w" => {
                self.invalidate_frame();
                self.state.width = operand_numbers(&self.operands, 1)
                    .and_then(|values| values[0].is_finite().then_some(values[0]))
                    .unwrap_or(f64::NAN);
            }
            b"J" => {
                self.invalidate_frame();
                self.state.cap = operand_numbers(&self.operands, 1)
                    .and_then(|values| match values[0] {
                        0.0 => Some(0),
                        1.0 => Some(1),
                        2.0 => Some(2),
                        _ => None,
                    })
                    .unwrap_or(u8::MAX);
            }
            b"j" => {
                self.invalidate_frame();
                self.state.join = operand_numbers(&self.operands, 1)
                    .and_then(|values| match values[0] {
                        0.0 => Some(0),
                        1.0 => Some(1),
                        2.0 => Some(2),
                        _ => None,
                    })
                    .unwrap_or(u8::MAX);
            }
            b"d" => {
                self.invalidate_frame();
                self.state.solid_dash = false;
            }
            b"G" => {
                self.invalidate_frame();
                self.set_gray();
            }
            b"RG" => {
                self.invalidate_frame();
                self.set_rgb();
            }
            b"K" => {
                self.invalidate_frame();
                self.set_cmyk();
            }
            b"CS" | b"SC" | b"SCN" => {
                self.invalidate_frame();
                self.state.color = None;
            }
            b"gs" => {
                self.invalidate_frame();
                self.apply_ext_gstate();
            }
            // Other operators either do not affect the tracked stroking state or are
            // conservatively treated as a barrier for candidate-frame reuse.
            _ => self.invalidate_frame(),
        }
        self.operands.clear();
    }
}

fn pdf_whitespace(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| matches!(*byte, 0 | 9 | 10 | 12 | 13 | 32))
}

type Point = (f64, f64);
type LineSegment = (Point, Point);

#[derive(Debug, Clone)]
struct MicroStrokeRun {
    start: usize,
    end: usize,
    state: StrokeState,
    lines: Vec<LineSegment>,
}

fn grouped_runs(input: &[u8], blocks: &[MicroStrokeBlock]) -> Vec<MicroStrokeRun> {
    let mut runs = Vec::new();
    let mut index = 0usize;
    while index < blocks.len() {
        let first = &blocks[index];
        let mut end_index = index + 1;
        while end_index < blocks.len() {
            let previous = &blocks[end_index - 1];
            let next = &blocks[end_index];
            let Some(gap) = input.get(previous.end..next.start) else {
                break;
            };
            if previous.state != next.state || !pdf_whitespace(gap) {
                break;
            }
            end_index = end_index.saturating_add(1);
        }
        if end_index.saturating_sub(index) >= MIN_MICROSTROKE_RUN {
            runs.push(MicroStrokeRun {
                start: first.start,
                end: blocks[end_index - 1].end,
                state: first.state,
                lines: blocks[index..end_index]
                    .iter()
                    .map(|block| (block.p0, block.p1))
                    .collect(),
            });
        }
        index = end_index;
    }
    runs
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "accepted pitches are bounded to 0.04..=0.25, so the scaled key is always 400..=2500"
)]
fn quantized_pitch_key(value: f64) -> Option<i32> {
    let value = value.abs();
    if !value.is_finite() || !(MIN_PITCH..=MAX_PITCH).contains(&value) {
        return None;
    }
    Some((value * 10_000.0).round() as i32)
}

fn infer_pitch(lines: &[LineSegment]) -> Option<f64> {
    let mut counts = BTreeMap::<i32, usize>::new();
    let mut add = |value: f64| {
        if let Some(key) = quantized_pitch_key(value) {
            *counts.entry(key).or_default() += 1;
        }
    };

    for &((x0, y0), (x1, y1)) in lines {
        add(x1 - x0);
        add(y1 - y0);
    }
    for pair in lines.windows(2) {
        add((pair[1].0).0 - (pair[0].0).0);
        add((pair[1].0).1 - (pair[0].0).1);
    }
    let (key, support) = counts
        .into_iter()
        .max_by_key(|(key, count)| (*count, std::cmp::Reverse(*key)))?;
    if support < 32 || support.saturating_mul(100) < lines.len() {
        return None;
    }
    Some(f64::from(key) / 10_000.0)
}

#[derive(Debug)]
struct RasterizedRun {
    x0: f64,
    y0: f64,
    width_user: f64,
    height_user: f64,
    mask: BilevelRaster,
}

fn max_linear_scale(matrix: Matrix) -> Option<f64> {
    let trace = matrix.d.mul_add(
        matrix.d,
        matrix
            .c
            .mul_add(matrix.c, matrix.b.mul_add(matrix.b, matrix.a * matrix.a)),
    );
    let determinant = matrix.b.mul_add(-matrix.c, matrix.a * matrix.d);
    if !trace.is_finite()
        || !determinant.is_finite()
        || trace <= 0.0
        || determinant.abs() <= f64::EPSILON
    {
        return None;
    }
    let discriminant = ((4.0 * determinant).mul_add(-determinant, trace * trace))
        .max(0.0)
        .sqrt();
    let scale2 = f64::midpoint(trace, discriminant);
    let scale = scale2.sqrt();
    (scale.is_finite() && scale > 0.0).then_some(scale)
}

fn raster_sample_pitch(state: StrokeState, geometry_pitch: f64, user_unit: f64) -> Option<f64> {
    if !geometry_pitch.is_finite()
        || geometry_pitch <= 0.0
        || !user_unit.is_finite()
        || user_unit <= 0.0
    {
        return None;
    }
    let page_scale = max_linear_scale(state.ctm)? * user_unit;
    if !page_scale.is_finite() || page_scale <= 0.0 {
        return None;
    }
    let quality_pitch = MAX_PAGE_RASTER_PITCH_PT / page_scale;
    let pitch = geometry_pitch.min(quality_pitch);
    (pitch.is_finite() && pitch > 0.0).then_some(pitch)
}

const fn worthwhile_raster_savings(before: usize, after: usize) -> bool {
    if after >= before {
        return false;
    }
    let saved = before - after;
    saved >= MIN_RUN_SAVINGS_BYTES
        && saved.saturating_mul(100) >= before.saturating_mul(MIN_RUN_SAVINGS_PERCENT)
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is finite, nonnegative, integral-valued by construction, and bounded by the 16384-pixel raster limit"
)]
#[expect(
    clippy::cast_sign_loss,
    reason = "the value is explicitly checked to be nonnegative before conversion"
)]
fn bounded_raster_index(value: f64) -> Option<usize> {
    if !value.is_finite() || !(0.0..=16_384.0).contains(&value) {
        return None;
    }
    Some(value as usize)
}

#[expect(
    clippy::too_many_lines,
    reason = "geometry bounds, cap semantics, and raster safety gates form one cohesive scan-conversion operation"
)]
fn rasterize_run(run: &MicroStrokeRun, pitch: f64, user_unit: f64) -> Option<RasterizedRun> {
    if !run.state.raster_safe() || !pitch.is_finite() || pitch <= 0.0 {
        return None;
    }
    let sample_pitch = raster_sample_pitch(run.state, pitch, user_unit)?;
    if run.state.width / sample_pitch < MIN_STROKE_RASTER_PIXELS {
        return None;
    }
    let mut xmin = f64::INFINITY;
    let mut ymin = f64::INFINITY;
    let mut xmax = f64::NEG_INFINITY;
    let mut ymax = f64::NEG_INFINITY;
    let mut short = 0usize;
    let mut lengths = Vec::with_capacity(run.lines.len());
    let mut forward_connected = 0usize;
    for &((x0, y0), (x1, y1)) in &run.lines {
        if ![x0, y0, x1, y1].into_iter().all(f64::is_finite) {
            return None;
        }
        xmin = xmin.min(x0).min(x1);
        ymin = ymin.min(y0).min(y1);
        xmax = xmax.max(x0).max(x1);
        ymax = ymax.max(y0).max(y1);
        let length = (x1 - x0).hypot(y1 - y0);
        if length <= f64::EPSILON {
            return None;
        }
        short += usize::from(length <= 2.0);
        lengths.push(length);
    }
    for pair in run.lines.windows(2) {
        let (_, previous_end) = pair[0];
        let (next_start, _) = pair[1];
        let gap = (previous_end.0 - next_start.0).hypot(previous_end.1 - next_start.1);
        if gap <= pitch * 0.05 {
            forward_connected = forward_connected.saturating_add(1);
        }
    }
    // A nearly continuous chain is much more likely to be useful raw vector data
    // (for example a sampled graph trace) than scan-converted/plotter soup.
    // Preserve it even in the explicitly lossy micro-vector rasterizer.
    if run.lines.len() > 1
        && forward_connected.saturating_mul(100)
            >= run.lines.len().saturating_sub(1).saturating_mul(90)
    {
        return None;
    }
    if short.saturating_mul(100) < run.lines.len().saturating_mul(65) {
        return None;
    }
    lengths.sort_by(f64::total_cmp);
    if lengths
        .get(lengths.len() / 2)
        .copied()
        .unwrap_or(f64::INFINITY)
        > 1.5
    {
        return None;
    }

    let pad = run.state.width.mul_add(0.5, sample_pitch);
    xmin -= pad;
    ymin -= pad;
    xmax += pad;
    ymax += pad;
    let scale = 1.0 / sample_pitch;
    let width = bounded_raster_index(((xmax - xmin) * scale).ceil())?.checked_add(1)?;
    let height = bounded_raster_index(((ymax - ymin) * scale).ceil())?.checked_add(1)?;
    if width == 0
        || height == 0
        || width > MAX_RASTER_DIMENSION
        || height > MAX_RASTER_DIMENSION
        || width.checked_mul(height)? > MAX_RASTER_PIXELS
    {
        return None;
    }
    let width_u32 = u32::try_from(width).ok()?;
    let height_u32 = u32::try_from(height).ok()?;
    let mut mask = BilevelRaster::transparent(width_u32, height_u32)?;
    let radius = run.state.width * scale * 0.5;

    for &((ux0, uy0), (ux1, uy1)) in &run.lines {
        let x0 = (ux0 - xmin) * scale;
        let y0 = (ymax - uy0) * scale;
        let x1 = (ux1 - xmin) * scale;
        let y1 = (ymax - uy1) * scale;
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len2 = dy.mul_add(dy, dx * dx);
        if len2 <= f64::EPSILON {
            continue;
        }
        let inv_len2 = 1.0 / len2;
        let radius2 = radius * radius;
        let length = if run.state.cap == 2 { len2.sqrt() } else { 0.0 };
        let extension = if run.state.cap == 2 {
            radius / length
        } else {
            0.0
        };
        let square_cap_cross_limit = radius * length;
        let min_x = bounded_raster_index((x0.min(x1) - radius - 1.0).floor().max(0.0))?;
        let max_x = bounded_raster_index(
            (x0.max(x1) + radius + 1.0)
                .ceil()
                .min(f64::from(width_u32 - 1)),
        )?;
        let min_y = bounded_raster_index((y0.min(y1) - radius - 1.0).floor().max(0.0))?;
        let max_y = bounded_raster_index(
            (y0.max(y1) + radius + 1.0)
                .ceil()
                .min(f64::from(height_u32 - 1)),
        )?;

        for y in min_y..=max_y {
            let py = f64::from(u32::try_from(y).ok()?) + 0.5;
            for x in min_x..=max_x {
                let px = f64::from(u32::try_from(x).ok()?) + 0.5;
                let raw_t = (py - y0).mul_add(dy, (px - x0) * dx) * inv_len2;
                let paints = match run.state.cap {
                    0 => {
                        if (0.0..=1.0).contains(&raw_t) {
                            let qx = raw_t.mul_add(dx, x0);
                            let qy = raw_t.mul_add(dy, y0);
                            let ex = px - qx;
                            let ey = py - qy;
                            ey.mul_add(ey, ex * ex) <= radius2
                        } else {
                            false
                        }
                    }
                    1 => {
                        let t = raw_t.clamp(0.0, 1.0);
                        let qx = t.mul_add(dx, x0);
                        let qy = t.mul_add(dy, y0);
                        let ex = px - qx;
                        let ey = py - qy;
                        ey.mul_add(ey, ex * ex) <= radius2
                    }
                    2 => {
                        if raw_t < -extension || raw_t > 1.0 + extension {
                            false
                        } else {
                            let t = raw_t.clamp(0.0, 1.0);
                            let qx = t.mul_add(dx, x0);
                            let qy = t.mul_add(dy, y0);
                            let cross = (py - qy).mul_add(dx, (px - qx) * -dy).abs();
                            cross <= square_cap_cross_limit
                        }
                    }
                    _ => false,
                };
                if paints {
                    mask.paint(x, y);
                }
            }
        }
    }

    Some(RasterizedRun {
        x0: xmin,
        y0: ymin,
        width_user: f64::from(width_u32) / scale,
        height_user: f64::from(height_u32) / scale,
        mask,
    })
}

fn image_payload(raster: &RasterizedRun, flate_level: i32) -> Result<BilevelImagePayload> {
    raster.mask.encode_image_mask(flate_level)
}

fn compact_real(value: f64) -> String {
    let mut text = format!("{value:.6}");
    while text.contains('.') && text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if text == "-0" {
        text.clear();
        text.push('0');
    }
    if let Some(rest) = text.strip_prefix("0.") {
        return format!(".{rest}");
    }
    if let Some(rest) = text.strip_prefix("-0.") {
        return format!("-.{rest}");
    }
    text
}

fn replacement_bytes(name: &[u8], run: &MicroStrokeRun, raster: &RasterizedRun) -> Vec<u8> {
    let mut matrix = Matrix::default();
    matrix.concat(Matrix::new(
        raster.width_user,
        0.0,
        0.0,
        raster.height_user,
        raster.x0,
        raster.y0,
    ));
    let mut out = b" q ".to_vec();
    if let Some(color) = run.state.color {
        out.extend_from_slice(&color.content_operator());
    }
    out.extend_from_slice(matrix.unparse().as_bytes());
    out.extend_from_slice(b" cm /");
    out.extend_from_slice(name);
    out.extend_from_slice(b" Do Q ");
    out
}

fn compressed_len(data: &[u8], flate_level: i32) -> Result<usize> {
    Ok(compress_flate(data, flate_level)?.len())
}

fn apply_replacements(input: &[u8], replacements: &[(usize, usize, Vec<u8>)]) -> Option<Vec<u8>> {
    let mut replacements = replacements.to_vec();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);
    let mut out = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        if start < cursor || end > input.len() || start > end {
            return None;
        }
        out.extend_from_slice(&input[cursor..start]);
        out.extend_from_slice(&replacement);
        cursor = end;
    }
    out.extend_from_slice(&input[cursor..]);
    Some(out)
}

fn opaque_number(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(resolved_number_value(document, value)?.is_some_and(
        |value| value.is_finite() && (value - 1.0).abs() <= 1.0e-12,
    )))
}

fn disabled_bool(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(resolved_bool_value(document, value)? == Some(false)))
}

fn ext_gstate_patches(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ExtGStatePatch>> {
    const SUPPORTED_EXT_GSTATE_KEYS: &[&[u8]] = &[
        b"Type", b"CA", b"ca", b"BM", b"SMask", b"OP", b"op", b"OPM", b"AIS", b"SA",
    ];
    let Some(states) = resolved_dictionary(document, resources.get(b"ExtGState".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, value) in states {
        let Some(state) = resolved_dictionary(document, Some(&value))? else {
            continue;
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
        let supported_keys = state
            .keys()
            .all(|key| SUPPORTED_EXT_GSTATE_KEYS.contains(&key.as_slice()));
        let alpha_is_shape_safe = match state.get(b"AIS".as_slice()) {
            None => true,
            Some(value) => resolved_bool_value(document, value)? == Some(false),
        };
        // Stroke adjustment is a device-space rendering hint. Either boolean value is
        // acceptable here because this pass is explicitly freezing the vector field into
        // a raster representation; reject only malformed/non-boolean values.
        let stroke_adjust_supported = match state.get(b"SA".as_slice()) {
            None => true,
            Some(value) => resolved_bool_value(document, value)?.is_some(),
        };
        out.insert(
            name,
            ExtGStatePatch {
                supported: supported_keys && alpha_is_shape_safe && stroke_adjust_supported,
                stroke_alpha_opaque: opaque_number(document, state.get(b"CA".as_slice()))?,
                fill_alpha_opaque: opaque_number(document, state.get(b"ca".as_slice()))?,
                normal_blend,
                no_soft_mask,
                stroke_overprint_disabled: disabled_bool(document, state.get(b"OP".as_slice()))?,
                fill_overprint_disabled: disabled_bool(document, state.get(b"op".as_slice()))?,
            },
        );
    }
    Ok(out)
}

fn next_image_name(index: &mut usize, occupied: &mut BTreeSet<Vec<u8>>) -> Vec<u8> {
    loop {
        let name = format!("PdfRedoxMicro{}", *index).into_bytes();
        *index = index.saturating_add(1);
        if occupied.insert(name.clone()) {
            return name;
        }
    }
}

struct Candidate {
    start: usize,
    end: usize,
    name: Vec<u8>,
    replacement: Vec<u8>,
    dictionary: OwnedDictionary,
    data: Vec<u8>,
    codec: BilevelCodec,
    strokes: usize,
}

#[expect(
    clippy::too_many_lines,
    reason = "candidate discovery, encoded-cost gating, and document rewrites form one ordered optimization pass"
)]
pub fn rasterize_pathological_microstrokes(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<MicrostrokeRasterStats> {
    let mut stats = MicrostrokeRasterStats::default();
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
        let mut decoded = Vec::new();
        decoded_content_value(document, contents, &mut decoded)?;
        let resources = effective_page_resources(document, page)?;
        let Some(user_unit) = page_user_unit(document, page)? else {
            continue;
        };
        let mut scanner = MicroStrokeScanner::new(ext_gstate_patches(document, &resources)?);
        scanner.scan(&decoded)?;
        let runs = grouped_runs(&decoded, &scanner.blocks);
        if runs.is_empty() {
            continue;
        }

        let mut occupied = BTreeSet::new();
        if let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
        {
            occupied.extend(xobjects.keys().cloned());
        }
        let mut name_index = 0usize;
        let mut candidates = Vec::new();
        for run in runs {
            if !run.state.raster_safe() {
                continue;
            }
            let Some(pitch) = infer_pitch(&run.lines) else {
                continue;
            };
            let Some(raster) = rasterize_run(&run, pitch, user_unit) else {
                continue;
            };
            let payload = image_payload(&raster, flate_level)?;
            let name = next_image_name(&mut name_index, &mut occupied);
            let replacement = replacement_bytes(&name, &run, &raster);
            let local_before = compressed_len(&decoded[run.start..run.end], flate_level)?;
            let local_after = payload
                .data
                .len()
                .saturating_add(replacement.len())
                .saturating_add(192);
            if !worthwhile_raster_savings(local_before, local_after) {
                continue;
            }
            candidates.push(Candidate {
                start: run.start,
                end: run.end,
                name,
                replacement,
                dictionary: payload.dictionary,
                data: payload.data,
                codec: payload.codec,
                strokes: run.lines.len(),
            });
        }
        if candidates.is_empty() {
            continue;
        }

        let replacements = candidates
            .iter()
            .map(|candidate| {
                (
                    candidate.start,
                    candidate.end,
                    candidate.replacement.clone(),
                )
            })
            .collect::<Vec<_>>();
        let Some(rewritten) = apply_replacements(&decoded, &replacements) else {
            continue;
        };
        let before = compressed_len(&decoded, flate_level)?;
        let image_cost = candidates
            .iter()
            .map(|candidate| candidate.data.len().saturating_add(192))
            .sum::<usize>();
        let after = compressed_len(&rewritten, flate_level)?.saturating_add(image_cost);
        if after >= before {
            continue;
        }

        let mut installed = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            let handle = ObjectHandle::New(document.add_object(OwnedObject::Stream {
                dictionary: candidate.dictionary.clone(),
                data: StreamData::Owned(candidate.data.clone()),
            }));
            installed.push((candidate.name.clone(), handle));
        }
        replace_page_content(document, page, rewritten)?;
        for (name, handle) in installed {
            install_page_resource(document, page, b"XObject", name, handle)?;
        }

        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.runs_rasterized = stats.runs_rasterized.saturating_add(candidates.len());
        stats.strokes_rasterized = stats.strokes_rasterized.saturating_add(
            candidates
                .iter()
                .map(|candidate| candidate.strokes)
                .sum::<usize>(),
        );
        stats.image_payload_bytes = stats.image_payload_bytes.saturating_add(
            candidates
                .iter()
                .map(|candidate| candidate.data.len())
                .sum::<usize>(),
        );
        stats.ccitt_images = stats.ccitt_images.saturating_add(
            candidates
                .iter()
                .filter(|candidate| candidate.codec == BilevelCodec::CcittGroup4)
                .count(),
        );
        stats.flate_images = stats.flate_images.saturating_add(
            candidates
                .iter()
                .filter(|candidate| candidate.codec == BilevelCodec::Flate)
                .count(),
        );
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before.saturating_sub(after));
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_only_adjacent_identical_microstroke_state() -> Result<()> {
        let input = b"q 1 0 0 1 10 20 cm 0 0 m .1 0 l S Q q 1 0 0 1 10.1 20 cm 0 0 m .1 0 l S Q";
        let mut scanner = MicroStrokeScanner::new(BTreeMap::new());
        scanner.scan(input)?;
        assert_eq!(scanner.blocks.len(), 2);
        let runs = grouped_runs(input, &scanner.blocks);
        assert!(runs.is_empty());
        assert_eq!(scanner.blocks[0].p0, (10.0, 20.0));
        assert_eq!(scanner.blocks[1].p0, (10.1, 20.0));
        Ok(())
    }

    #[test]
    fn fractional_line_cap_is_not_silently_truncated() -> Result<()> {
        let input = b"1.5 J q 1 0 0 1 10 20 cm 0 0 m .1 0 l S Q";
        let mut scanner = MicroStrokeScanner::new(BTreeMap::new());
        scanner.scan(input)?;
        assert_eq!(scanner.blocks.len(), 1);
        assert!(!scanner.blocks[0].state.raster_safe());
        Ok(())
    }

    #[test]
    fn singular_outer_ctm_is_not_rasterized() {
        let state = StrokeState {
            ctm: Matrix::new(1.0, 0.0, 0.0, 0.0, 0.0, 0.0),
            ..StrokeState::default()
        };
        assert!(raster_sample_pitch(state, 0.12, 1.0).is_none());
    }

    #[test]
    fn raster_pitch_accounts_for_outer_ctm_and_user_unit() {
        let identity = StrokeState::default();
        assert_eq!(raster_sample_pitch(identity, 0.1, 1.0), Some(0.1));

        let scaled = StrokeState {
            ctm: Matrix::new(4.0, 0.0, 0.0, 1.0, 0.0, 0.0),
            ..StrokeState::default()
        };
        let pitch = raster_sample_pitch(scaled, 0.1, 1.0).unwrap_or(f64::NAN);
        assert!((pitch - 0.04).abs() < 1.0e-12);

        let rotated_scaled = StrokeState {
            ctm: Matrix::new(0.0, 2.0, -2.0, 0.0, 0.0, 0.0),
            ..StrokeState::default()
        };
        let pitch = raster_sample_pitch(rotated_scaled, 0.1, 1.0).unwrap_or(f64::NAN);
        assert!((pitch - 0.08).abs() < 1.0e-12);

        let pitch = raster_sample_pitch(identity, 0.1, 2.0).unwrap_or(f64::NAN);
        assert!((pitch - 0.08).abs() < 1.0e-12);
    }

    #[test]
    fn lossy_rasterization_requires_material_encoded_savings() {
        assert!(!worthwhile_raster_savings(100_000, 98_000));
        assert!(worthwhile_raster_savings(100_000, 79_000));
        assert!(!worthwhile_raster_savings(4_000, 3_100));
        assert!(worthwhile_raster_savings(4_000, 2_900));
        assert!(!worthwhile_raster_savings(1_000, 0));
    }

    #[test]
    fn pitch_detector_finds_plotter_grid() {
        let lines = (0..600)
            .map(|index| {
                let y = f64::from(index) * 0.12;
                ((0.0, y), (0.12, y))
            })
            .collect::<Vec<_>>();
        assert_eq!(infer_pitch(&lines), Some(0.12));
    }

    #[test]
    fn unsupported_ext_gstate_disables_microstroke_rasterization() {
        let mut state = StrokeState::default();
        state.apply_ext_gstate(ExtGStatePatch {
            supported: false,
            ..ExtGStatePatch::default()
        });
        assert!(!state.raster_safe());
    }

    #[test]
    fn continuous_micro_polyline_is_preserved_as_vector_data() -> Result<()> {
        let run_length = u32::try_from(MIN_MICROSTROKE_RUN)
            .map_err(|_| crate::Error::Invalid("microstroke test run exceeds u32".to_owned()))?;
        let lines = (0..run_length)
            .map(|index| {
                let x0 = f64::from(index) * 0.12;
                ((x0, 0.0), (x0 + 0.12, 0.0))
            })
            .collect::<Vec<_>>();
        let run = MicroStrokeRun {
            start: 0,
            end: 0,
            state: StrokeState {
                width: 0.24,
                cap: 1,
                ..StrokeState::default()
            },
            lines,
        };
        assert!(rasterize_run(&run, 0.12, 1.0).is_none());
        Ok(())
    }

    #[test]
    fn degenerate_strokes_stay_vector() {
        let run = MicroStrokeRun {
            start: 0,
            end: 0,
            state: StrokeState {
                width: 0.24,
                cap: 1,
                ..StrokeState::default()
            },
            lines: vec![((0.0, 0.0), (0.0, 0.0)); MIN_MICROSTROKE_RUN],
        };
        assert!(rasterize_run(&run, 0.12, 1.0).is_none());
    }

    #[test]
    fn hairline_microstrokes_stay_vector() {
        let run = MicroStrokeRun {
            start: 0,
            end: 0,
            state: StrokeState {
                width: 0.05,
                cap: 1,
                ..StrokeState::default()
            },
            lines: vec![((0.0, 0.0), (1.0, 0.0)); MIN_MICROSTROKE_RUN],
        };
        assert!(rasterize_run(&run, 0.12, 1.0).is_none());
    }

    #[test]
    fn binary_raster_uses_zero_bits_for_ink() -> Result<()> {
        let run = MicroStrokeRun {
            start: 0,
            end: 0,
            state: StrokeState {
                width: 0.24,
                cap: 1,
                ..StrokeState::default()
            },
            lines: vec![((0.0, 0.0), (1.0, 0.0)); MIN_MICROSTROKE_RUN],
        };
        let raster = rasterize_run(&run, 0.12, 1.0)
            .ok_or_else(|| crate::Error::Invalid("expected raster".to_owned()))?;
        assert!(raster.mask.packed().iter().any(|byte| *byte != 0xff));
        Ok(())
    }
}
