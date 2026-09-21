use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{decoded_content_value, replace_page_content, resolved_dictionary},
};
use flate2::{Compression, write::ZlibEncoder};
use flpdf::content_stream::ContentScalar;
use flpdf::{Matrix, ObjectHandle as FlObjectHandle, ObjectHandleParserCallbacks, ParseControl};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
};

const MIN_MICROSTROKE_RUN: usize = 500;
const MIN_RUN_SAVINGS_BYTES: usize = 1024;
const MIN_RUN_SAVINGS_PERCENT: usize = 20;
const MIN_PITCH: f64 = 0.04;
const MAX_PITCH: f64 = 0.25;
const MAX_RASTER_DIMENSION: usize = 16_384;
const MAX_RASTER_PIXELS: usize = 100_000_000;
const MAX_PAGE_RASTER_PITCH_PT: f64 = 72.0 / 450.0;
const MIN_STROKE_RASTER_PIXELS: f64 = 1.5;

#[doc(hidden)]
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MicrostrokeRasterStats {
    pub pages_rewritten: usize,
    pub runs_rasterized: usize,
    pub strokes_rasterized: usize,
    pub image_payload_bytes: usize,
    pub estimated_flate_bytes_saved: usize,
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
}

fn operand_numbers(operands: &[Operand], expected: usize) -> Option<Vec<f64>> {
    if operands.len() != expected {
        return None;
    }
    let mut values = Vec::with_capacity(expected);
    for operand in operands {
        values.push(operand.value.number()?);
    }
    Some(values)
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
    fn safe_for_stencil(self) -> bool {
        self.stroke_alpha_opaque
            && self.fill_alpha_opaque
            && self.normal_blend
            && self.no_soft_mask
            && self.stroke_overprint_disabled
            && self.fill_overprint_disabled
    }

    fn invalidate(&mut self) {
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

    fn apply_ext_gstate(&mut self, patch: ExtGStatePatch) {
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

    fn invalidate_frame(&mut self) {
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
            .and_then(|operand| operand.value.name())
            .and_then(|name| self.ext_gstates.get(name.as_ref()).copied());
        if let Some(patch) = patch {
            self.state.apply_ext_gstate(patch);
        } else {
            self.state.safety.invalidate();
        }
    }

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
            // These do not alter stroking geometry/color/safety.
            b"g" | b"rg" | b"k" | b"cs" | b"sc" | b"scn" | b"M" | b"ri" | b"i" | b"BX" | b"EX"
            | b"W" | b"W*" | b"BMC" | b"BDC" | b"EMC" | b"MP" | b"DP" | b"BT" | b"ET" | b"Tc"
            | b"Tw" | b"Tz" | b"TL" | b"Tf" | b"Tr" | b"Ts" | b"Td" | b"TD" | b"Tm" | b"T*"
            | b"Tj" | b"TJ" | b"'" | b"\"" | b"Do" | b"sh" => self.invalidate_frame(),
            _ => self.invalidate_frame(),
        }
        self.operands.clear();
    }
}

impl ObjectHandleParserCallbacks for MicroStrokeScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

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
            self.invalidate_frame();
            self.operands.clear();
        } else {
            self.operands.push(Operand {
                value: OperandValue::Handle(object),
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
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
            if previous.state != next.state
                || previous.end > next.start
                || !pdf_whitespace(&input[previous.end..next.start])
            {
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

fn infer_pitch(lines: &[LineSegment]) -> Option<f64> {
    let mut counts = BTreeMap::<i64, usize>::new();
    let mut add = |value: f64| {
        let value = value.abs();
        if value.is_finite() && (MIN_PITCH..=MAX_PITCH).contains(&value) {
            let key = (value * 10_000.0).round() as i64;
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
    Some(key as f64 / 10_000.0)
}

#[derive(Debug)]
struct RasterizedRun {
    x0: f64,
    y0: f64,
    width_user: f64,
    height_user: f64,
    width: u32,
    height: u32,
    packed: Vec<u8>,
}

fn paint_bit(packed: &mut [u8], row_bytes: usize, x: usize, y: usize) {
    let Some(byte) = packed.get_mut(y.saturating_mul(row_bytes).saturating_add(x / 8)) else {
        return;
    };
    *byte &= !(0x80 >> (x % 8));
}

fn max_linear_scale(matrix: Matrix) -> Option<f64> {
    let trace =
        matrix.a * matrix.a + matrix.b * matrix.b + matrix.c * matrix.c + matrix.d * matrix.d;
    let determinant = matrix.a * matrix.d - matrix.b * matrix.c;
    if !trace.is_finite()
        || !determinant.is_finite()
        || trace <= 0.0
        || determinant.abs() <= f64::EPSILON
    {
        return None;
    }
    let discriminant = (trace * trace - 4.0 * determinant * determinant)
        .max(0.0)
        .sqrt();
    let scale2 = 0.5 * (trace + discriminant);
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

fn worthwhile_raster_savings(before: usize, after: usize) -> bool {
    if after >= before {
        return false;
    }
    let saved = before - after;
    saved >= MIN_RUN_SAVINGS_BYTES
        && saved.saturating_mul(100) >= before.saturating_mul(MIN_RUN_SAVINGS_PERCENT)
}

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

    let pad = run.state.width * 0.5 + sample_pitch;
    xmin -= pad;
    ymin -= pad;
    xmax += pad;
    ymax += pad;
    let scale = 1.0 / sample_pitch;
    let width = ((xmax - xmin) * scale).ceil() as usize + 1;
    let height = ((ymax - ymin) * scale).ceil() as usize + 1;
    if width == 0
        || height == 0
        || width > MAX_RASTER_DIMENSION
        || height > MAX_RASTER_DIMENSION
        || width.checked_mul(height)? > MAX_RASTER_PIXELS
    {
        return None;
    }
    let row_bytes = width.div_ceil(8);
    let mut packed = vec![0xff; row_bytes.checked_mul(height)?];
    let radius = run.state.width * scale * 0.5;

    for &((ux0, uy0), (ux1, uy1)) in &run.lines {
        let x0 = (ux0 - xmin) * scale;
        let y0 = (ymax - uy0) * scale;
        let x1 = (ux1 - xmin) * scale;
        let y1 = (ymax - uy1) * scale;
        let dx = x1 - x0;
        let dy = y1 - y0;
        let len2 = dx * dx + dy * dy;
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
        let min_x = (x0.min(x1) - radius - 1.0).floor().max(0.0) as usize;
        let max_x = (x0.max(x1) + radius + 1.0).ceil().min((width - 1) as f64) as usize;
        let min_y = (y0.min(y1) - radius - 1.0).floor().max(0.0) as usize;
        let max_y = (y0.max(y1) + radius + 1.0).ceil().min((height - 1) as f64) as usize;

        for y in min_y..=max_y {
            let py = y as f64 + 0.5;
            for x in min_x..=max_x {
                let px = x as f64 + 0.5;
                let raw_t = ((px - x0) * dx + (py - y0) * dy) * inv_len2;
                let paints = match run.state.cap {
                    0 => {
                        if !(0.0..=1.0).contains(&raw_t) {
                            false
                        } else {
                            let qx = x0 + raw_t * dx;
                            let qy = y0 + raw_t * dy;
                            let ex = px - qx;
                            let ey = py - qy;
                            ex * ex + ey * ey <= radius2
                        }
                    }
                    1 => {
                        let t = raw_t.clamp(0.0, 1.0);
                        let qx = x0 + t * dx;
                        let qy = y0 + t * dy;
                        let ex = px - qx;
                        let ey = py - qy;
                        ex * ex + ey * ey <= radius2
                    }
                    2 => {
                        if raw_t < -extension || raw_t > 1.0 + extension {
                            false
                        } else {
                            let t = raw_t.clamp(0.0, 1.0);
                            let qx = x0 + t * dx;
                            let qy = y0 + t * dy;
                            let cross = ((px - qx) * -dy + (py - qy) * dx).abs();
                            cross <= square_cap_cross_limit
                        }
                    }
                    _ => false,
                };
                if paints {
                    paint_bit(&mut packed, row_bytes, x, y);
                }
            }
        }
    }

    Some(RasterizedRun {
        x0: xmin,
        y0: ymin,
        width_user: width as f64 / scale,
        height_user: height as f64 / scale,
        width: u32::try_from(width).ok()?,
        height: u32::try_from(height).ok()?,
        packed,
    })
}

fn compress_flate(data: &[u8], level: i32) -> Result<Vec<u8>> {
    let level = u32::try_from(level.clamp(0, 9)).unwrap_or(9);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

fn image_payload(raster: &RasterizedRun, flate_level: i32) -> Result<(Vec<u8>, OwnedDictionary)> {
    let data = compress_flate(&raster.packed, flate_level)?;
    let dictionary = BTreeMap::from([
        (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
        (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
        (
            b"Width".to_vec(),
            OwnedObject::Integer(i64::from(raster.width)),
        ),
        (
            b"Height".to_vec(),
            OwnedObject::Integer(i64::from(raster.height)),
        ),
        (b"ImageMask".to_vec(), OwnedObject::Boolean(true)),
        (b"BitsPerComponent".to_vec(), OwnedObject::Integer(1)),
        (
            b"Filter".to_vec(),
            OwnedObject::Name(b"FlateDecode".to_vec()),
        ),
    ]);
    Ok((data, dictionary))
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
        text = "0".to_owned();
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

fn opaque_number(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(current_number(document, value)?.is_some_and(
        |value| value.is_finite() && (value - 1.0).abs() <= 1.0e-12,
    )))
}

fn disabled_bool(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(current_bool(document, value)? == Some(false)))
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
        const SUPPORTED_EXT_GSTATE_KEYS: &[&[u8]] = &[
            b"Type", b"CA", b"ca", b"BM", b"SMask", b"OP", b"op", b"OPM", b"AIS", b"SA",
        ];
        let supported_keys = state
            .keys()
            .all(|key| SUPPORTED_EXT_GSTATE_KEYS.contains(&key.as_slice()));
        let alpha_is_shape_safe = match state.get(b"AIS".as_slice()) {
            None => true,
            Some(value) => current_bool(document, value)? == Some(false),
        };
        // Stroke adjustment is a device-space rendering hint. Either boolean value is
        // acceptable here because this pass is explicitly freezing the vector field into
        // a raster representation; reject only malformed/non-boolean values.
        let stroke_adjust_supported = match state.get(b"SA".as_slice()) {
            None => true,
            Some(value) => current_bool(document, value)?.is_some(),
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
    strokes: usize,
}

pub(crate) fn rasterize_pathological_microstrokes_hayro(
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
        let resources = page_effective_resources(document, page)?;
        let user_unit = document.current_owned_object(page)?.and_then(|object| {
            object
                .as_dictionary()
                .and_then(|dictionary| dictionary.get(b"UserUnit".as_slice()))
                .cloned()
        });
        let user_unit = match user_unit.as_ref() {
            Some(value) => {
                let Some(value) = current_number(document, value)?
                    .filter(|value| value.is_finite() && *value > 0.0)
                else {
                    continue;
                };
                value
            }
            None => 1.0,
        };
        let mut scanner = MicroStrokeScanner::new(ext_gstate_patches(document, &resources)?);
        flpdf::parse_detached_content_stream(&decoded, "microstroke rasterization", &mut scanner)?;
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
            let (data, dictionary) = image_payload(&raster, flate_level)?;
            let name = next_image_name(&mut name_index, &mut occupied);
            let replacement = replacement_bytes(&name, &run, &raster);
            let local_before = compressed_len(&decoded[run.start..run.end], flate_level)?;
            let local_after = data
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
                dictionary,
                data,
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
            let handle = ObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
                dictionary: candidate.dictionary.clone(),
                data: StreamData::Owned(candidate.data.clone()),
            }));
            installed.push((candidate.name.clone(), handle));
        }
        replace_page_content(document, page, rewritten)?;
        for (name, handle) in installed {
            install_page_xobject(document, page, name, handle)?;
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
    fn groups_only_adjacent_identical_microstroke_state() {
        let input = b"q 1 0 0 1 10 20 cm 0 0 m .1 0 l S Q q 1 0 0 1 10.1 20 cm 0 0 m .1 0 l S Q";
        let mut scanner = MicroStrokeScanner::new(BTreeMap::new());
        flpdf::parse_detached_content_stream(input, "microstroke test", &mut scanner)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(scanner.blocks.len(), 2);
        let runs = grouped_runs(input, &scanner.blocks);
        assert!(runs.is_empty());
        assert_eq!(scanner.blocks[0].p0, (10.0, 20.0));
        assert_eq!(scanner.blocks[1].p0, (10.1, 20.0));
    }

    #[test]
    fn fractional_line_cap_is_not_silently_truncated() {
        let input = b"1.5 J q 1 0 0 1 10 20 cm 0 0 m .1 0 l S Q";
        let mut scanner = MicroStrokeScanner::new(BTreeMap::new());
        flpdf::parse_detached_content_stream(input, "microstroke fractional cap", &mut scanner)
            .unwrap_or_else(|error| panic!("{error}"));
        assert_eq!(scanner.blocks.len(), 1);
        assert!(!scanner.blocks[0].state.raster_safe());
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
                let y = index as f64 * 0.12;
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
    fn continuous_micro_polyline_is_preserved_as_vector_data() {
        let lines = (0..MIN_MICROSTROKE_RUN)
            .map(|index| {
                let x0 = index as f64 * 0.12;
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
    fn binary_raster_uses_zero_bits_for_ink() {
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
        let raster = rasterize_run(&run, 0.12, 1.0).unwrap_or_else(|| panic!("raster"));
        assert!(raster.packed.iter().any(|byte| *byte != 0xff));
    }
}
