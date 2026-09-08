use crate::Result;
use crate::config::HiddenTextPolicy;
use crate::report::{
    HiddenTextAction, HiddenTextCategory, HiddenTextFinding, HiddenTextMechanism, PageRect,
};
use flpdf::{
    DecodeLevel, Matrix, ObjectHandle, ObjectHandleParserCallbacks, ObjectRef, PageObjectHelper,
    ParseControl, Pdf, Rectangle,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek};
use std::rc::Rc;

const ALPHA_INVISIBLE: f64 = 0.001;
const ALPHA_OPAQUE: f64 = 0.995;
const COVERAGE_THRESHOLD: f64 = 0.97;

#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Rect {
    fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self {
            x0: x0.min(x1),
            y0: y0.min(y1),
            x1: x0.max(x1),
            y1: y0.max(y1),
        }
    }

    fn from_rectangle(rect: Rectangle) -> Self {
        Self::new(rect.llx, rect.lly, rect.urx, rect.ury)
    }

    fn area(self) -> f64 {
        (self.x1 - self.x0).max(0.0) * (self.y1 - self.y0).max(0.0)
    }

    fn intersect(self, other: Self) -> Option<Self> {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);
        (x1 > x0 && y1 > y0).then_some(Self { x0, y0, x1, y1 })
    }

    fn coverage_of(self, target: Self) -> f64 {
        let area = target.area();
        if area <= f64::EPSILON {
            return 0.0;
        }
        self.intersect(target).map_or(0.0, |r| r.area() / area)
    }

    fn to_public(self) -> PageRect {
        PageRect {
            x0: self.x0,
            y0: self.y0,
            x1: self.x1,
            y1: self.y1,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum PaintKind {
    FillRect { dark: bool },
    Image,
}

#[derive(Debug, Clone, Copy)]
struct PaintEvent {
    order: usize,
    bounds: Rect,
    kind: PaintKind,
}

#[derive(Debug, Clone, Copy)]
enum Color {
    Gray(f64),
    Rgb(f64, f64, f64),
    Cmyk(f64, f64, f64, f64),
    Unknown,
}

impl Color {
    fn is_dark(self) -> bool {
        let luminance = match self {
            Self::Gray(g) => g,
            Self::Rgb(r, g, b) => (0.2126 * r) + (0.7152 * g) + (0.0722 * b),
            Self::Cmyk(c, m, y, k) => {
                let r = 1.0 - (c * (1.0 - k) + k);
                let g = 1.0 - (m * (1.0 - k) + k);
                let b = 1.0 - (y * (1.0 - k) + k);
                (0.2126 * r) + (0.7152 * g) + (0.0722 * b)
            }
            Self::Unknown => return false,
        };
        luminance <= 0.18
    }
}

#[derive(Debug, Clone, Copy)]
struct ExtGStateInfo {
    fill_alpha: Option<f64>,
    stroke_alpha: Option<f64>,
    normal_blend: bool,
}

#[derive(Debug, Clone)]
struct FontInfo {
    widths: HashMap<u32, f64>,
    default_width: f64,
    unicode: HashMap<Vec<u8>, String>,
    max_code_bytes: usize,
    identity_two_byte: bool,
}

impl Default for FontInfo {
    fn default() -> Self {
        Self {
            widths: HashMap::new(),
            default_width: 500.0,
            unicode: HashMap::new(),
            max_code_bytes: 1,
            identity_two_byte: false,
        }
    }
}

impl FontInfo {
    fn codes<'a>(&self, bytes: &'a [u8]) -> Vec<&'a [u8]> {
        if bytes.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut pos = 0;
        while pos < bytes.len() {
            let mut matched = None;
            let max = self.max_code_bytes.min(bytes.len() - pos).max(1);
            for len in (1..=max).rev() {
                if self.unicode.contains_key(&bytes[pos..pos + len]) {
                    matched = Some(len);
                    break;
                }
            }
            let len = matched.unwrap_or_else(|| {
                if self.identity_two_byte && bytes.len() - pos >= 2 {
                    2
                } else {
                    1
                }
            });
            out.push(&bytes[pos..pos + len]);
            pos += len;
        }
        out
    }

    fn decode(&self, bytes: &[u8]) -> String {
        let mut out = String::new();
        for code in self.codes(bytes) {
            if let Some(mapped) = self.unicode.get(code) {
                out.push_str(mapped);
            } else if code.len() == 1 && code[0].is_ascii() {
                let ch = code[0] as char;
                if !ch.is_control() || matches!(ch, '\t' | '\n' | '\r') {
                    out.push(ch);
                }
            }
        }
        out
    }

    fn code_value(code: &[u8]) -> u32 {
        code.iter()
            .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
    }

    fn glyph_width(&self, code: &[u8]) -> f64 {
        self.widths
            .get(&Self::code_value(code))
            .copied()
            .unwrap_or(self.default_width)
    }

    fn advance(&self, bytes: &[u8], text: &TextState) -> f64 {
        let hscale = text.horizontal_scale / 100.0;
        self.codes(bytes)
            .into_iter()
            .map(|code| {
                let mut advance = (self.glyph_width(code) / 1000.0) * text.font_size;
                advance += text.char_spacing;
                if code.len() == 1 && code[0] == b' ' {
                    advance += text.word_spacing;
                }
                advance * hscale
            })
            .sum()
    }
}

#[derive(Debug, Clone)]
struct Resources {
    fonts: BTreeMap<Vec<u8>, FontInfo>,
    ext_gstates: BTreeMap<Vec<u8>, ExtGStateInfo>,
    images: BTreeMap<Vec<u8>, bool>,
    properties: BTreeMap<Vec<u8>, Option<ObjectRef>>,
}

#[derive(Debug, Clone, Copy)]
struct GraphicsState {
    ctm: Matrix,
    fill_alpha: f64,
    stroke_alpha: f64,
    fill_color: Color,
    normal_blend: bool,
    clip: Rect,
}

#[derive(Debug, Clone)]
struct TextState {
    font: Vec<u8>,
    font_size: f64,
    char_spacing: f64,
    word_spacing: f64,
    horizontal_scale: f64,
    leading: f64,
    render_mode: i64,
    rise: f64,
    matrix: Matrix,
    line_matrix: Matrix,
}

impl Default for TextState {
    fn default() -> Self {
        Self {
            font: Vec::new(),
            font_size: 0.0,
            char_spacing: 0.0,
            word_spacing: 0.0,
            horizontal_scale: 100.0,
            leading: 0.0,
            render_mode: 0,
            rise: 0.0,
            matrix: Matrix::default(),
            line_matrix: Matrix::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct MarkedState {
    optional_hidden: bool,
    actual_text: Option<String>,
    artifact: bool,
}

#[derive(Debug, Clone)]
struct OperandSpan {
    object: ObjectHandle,
    offset: usize,
}

#[derive(Debug, Clone)]
struct TextEvent {
    id: String,
    page_number: usize,
    operator_index: usize,
    order: usize,
    span_start: usize,
    span_end: usize,
    raw: Vec<u8>,
    text: String,
    bounds: Option<Rect>,
    initial_mechanism: Option<HiddenTextMechanism>,
    actual_text: Option<String>,
    artifact: bool,
}

#[derive(Debug, Clone, Copy, Default)]
struct PathState {
    single_rect: Option<Rect>,
    only_single_rect: bool,
}

impl PathState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn set_rect(&mut self, rect: Rect) {
        if self.single_rect.is_none() {
            self.single_rect = Some(rect);
            self.only_single_rect = true;
        } else {
            self.only_single_rect = false;
        }
    }

    fn mark_complex(&mut self) {
        self.only_single_rect = false;
    }
}

struct PageScanner<'a> {
    page_number: usize,
    crop: Rect,
    resources: &'a Resources,
    hidden_ocgs: &'a BTreeSet<ObjectRef>,
    base_ocg_off: bool,
    on_ocgs: &'a BTreeSet<ObjectRef>,
    graphics: GraphicsState,
    graphics_stack: Vec<(GraphicsState, TextState)>,
    text: TextState,
    marked: Vec<MarkedState>,
    operands: Vec<OperandSpan>,
    operator_index: usize,
    path: PathState,
    pending_clip: bool,
    text_events: Vec<TextEvent>,
    paints: Vec<PaintEvent>,
}

impl<'a> PageScanner<'a> {
    fn new(
        page_number: usize,
        crop: Rect,
        resources: &'a Resources,
        hidden_ocgs: &'a BTreeSet<ObjectRef>,
        base_ocg_off: bool,
        on_ocgs: &'a BTreeSet<ObjectRef>,
    ) -> Self {
        Self {
            page_number,
            crop,
            resources,
            hidden_ocgs,
            base_ocg_off,
            on_ocgs,
            graphics: GraphicsState {
                ctm: Matrix::default(),
                fill_alpha: 1.0,
                stroke_alpha: 1.0,
                fill_color: Color::Gray(0.0),
                normal_blend: true,
                clip: crop,
            },
            graphics_stack: Vec::new(),
            text: TextState::default(),
            marked: Vec::new(),
            operands: Vec::new(),
            operator_index: 0,
            path: PathState::default(),
            pending_clip: false,
            text_events: Vec::new(),
            paints: Vec::new(),
        }
    }

    fn number(object: &ObjectHandle) -> Option<f64> {
        if object.as_integer().is_some() {
            return object.as_integer().map(|v| v as f64);
        }
        object.as_real()
    }

    fn numbers(&self) -> Option<Vec<f64>> {
        self.operands
            .iter()
            .map(|operand| Self::number(&operand.object))
            .collect()
    }

    fn name_at(&self, index: usize) -> Option<Vec<u8>> {
        self.operands.get(index)?.object.as_name()
    }

    fn marked_actual_text(&self) -> Option<String> {
        self.marked
            .iter()
            .rev()
            .find_map(|state| state.actual_text.clone())
    }

    fn marked_artifact(&self) -> bool {
        self.marked.iter().any(|state| state.artifact)
    }

    fn optional_hidden(&self) -> bool {
        self.marked.iter().any(|state| state.optional_hidden)
    }

    fn font(&self) -> FontInfo {
        self.resources
            .fonts
            .get(&self.text.font)
            .cloned()
            .unwrap_or_default()
    }

    fn combined_text_matrix(&self) -> Matrix {
        let mut combined = self.graphics.ctm;
        combined.concat(self.text.matrix);
        combined
    }

    fn text_bounds(&self, advance: f64) -> Option<Rect> {
        if !advance.is_finite() || !self.text.font_size.is_finite() {
            return None;
        }
        let size = self.text.font_size.abs();
        if size <= 1e-9 {
            return None;
        }
        let local = Rectangle::new(
            0.0_f64.min(advance),
            self.text.rise - (0.25 * size),
            0.0_f64.max(advance),
            self.text.rise + (0.9 * size),
        );
        let transformed = self.combined_text_matrix().transform_rectangle(local);
        let rect = Rect::from_rectangle(transformed);
        (rect.area().is_finite() && rect.area() > 1e-12).then_some(rect)
    }

    fn show_text(
        &mut self,
        strings: Vec<Vec<u8>>,
        tj_adjustments: Vec<f64>,
        span_start: usize,
        span_end: usize,
    ) {
        let font = self.font();
        let mut raw = Vec::new();
        let mut decoded = String::new();
        let mut advance = 0.0;
        for (index, bytes) in strings.iter().enumerate() {
            raw.extend_from_slice(bytes);
            decoded.push_str(&font.decode(bytes));
            advance += font.advance(bytes, &self.text);
            if let Some(adjustment) = tj_adjustments.get(index) {
                advance += adjustment;
            }
        }

        let bounds = self.text_bounds(advance);
        let applicable_alpha = match self.text.render_mode {
            1 | 5 => self.graphics.stroke_alpha,
            2 | 6 => self.graphics.fill_alpha.max(self.graphics.stroke_alpha),
            3 | 7 => 0.0,
            _ => self.graphics.fill_alpha,
        };
        let initial_mechanism = if self.text.render_mode == 3 {
            Some(HiddenTextMechanism::RenderingModeInvisible)
        } else if self.text.render_mode == 7 {
            Some(HiddenTextMechanism::ClipOnlyRenderingMode)
        } else if applicable_alpha <= ALPHA_INVISIBLE {
            Some(HiddenTextMechanism::ZeroOpacity)
        } else if self.optional_hidden() {
            Some(HiddenTextMechanism::OptionalContentHidden)
        } else if bounds.is_none() {
            Some(HiddenTextMechanism::DegenerateTransform)
        } else if bounds.is_some_and(|rect| self.crop.intersect(rect).is_none()) {
            Some(HiddenTextMechanism::OutsideCropBox)
        } else if bounds.is_some_and(|rect| self.graphics.clip.intersect(rect).is_none()) {
            Some(HiddenTextMechanism::ClippedOut)
        } else {
            None
        };

        let id = format!("p{}-o{}", self.page_number, self.operator_index);
        self.text_events.push(TextEvent {
            id,
            page_number: self.page_number,
            operator_index: self.operator_index,
            order: self.operator_index,
            span_start,
            span_end,
            raw,
            text: decoded,
            bounds,
            initial_mechanism,
            actual_text: self.marked_actual_text(),
            artifact: self.marked_artifact(),
        });

        self.text.matrix.translate(advance, 0.0);
    }

    fn show_single(&mut self, bytes: Vec<u8>, span_start: usize, span_end: usize) {
        self.show_text(vec![bytes], Vec::new(), span_start, span_end);
    }

    fn show_tj_array(&mut self, array: &[ObjectHandle], span_start: usize, span_end: usize) {
        let mut strings = Vec::new();
        let mut adjustments = Vec::new();
        let hscale = self.text.horizontal_scale / 100.0;
        let mut pending_adjustment = 0.0;
        for item in array {
            if let Some(bytes) = item.as_string() {
                if !strings.is_empty() {
                    adjustments.push(pending_adjustment);
                }
                pending_adjustment = 0.0;
                strings.push(bytes);
            } else if let Some(value) = Self::number(item) {
                pending_adjustment += (-value / 1000.0) * self.text.font_size * hscale;
            }
        }
        if !strings.is_empty() {
            adjustments.push(pending_adjustment);
            self.show_text(strings, adjustments, span_start, span_end);
        }
    }

    fn new_line(&mut self) {
        self.text.line_matrix.translate(0.0, -self.text.leading);
        self.text.matrix = self.text.line_matrix;
    }

    fn property_hidden(&self, name: &[u8]) -> bool {
        self.resources
            .properties
            .get(name)
            .is_some_and(|object_ref| {
                object_ref.is_some_and(|object_ref| {
                    if self.base_ocg_off {
                        !self.on_ocgs.contains(&object_ref)
                    } else {
                        self.hidden_ocgs.contains(&object_ref)
                    }
                })
            })
    }

    fn begin_marked_content(&mut self, with_properties: bool) {
        let tag = self.name_at(0).unwrap_or_default();
        let artifact = tag == b"Artifact";
        let mut optional_hidden = false;
        let mut actual_text = None;
        if with_properties && let Some(properties) = self.operands.get(1).map(|o| &o.object) {
            if let Some(name) = properties.as_name() {
                if tag == b"OC" {
                    optional_hidden = self.property_hidden(&name);
                }
            } else if properties.as_dictionary().is_some() {
                if tag == b"OC"
                    && let Some(object_ref) = properties.object_ref()
                {
                    optional_hidden = if self.base_ocg_off {
                        !self.on_ocgs.contains(&object_ref)
                    } else {
                        self.hidden_ocgs.contains(&object_ref)
                    };
                }
                if let Ok(value) = properties.try_get_key(b"/ActualText")
                    && let Some(bytes) = value.as_string()
                {
                    actual_text = Some(decode_pdf_text_string(&bytes));
                }
            }
        }
        self.marked.push(MarkedState {
            optional_hidden,
            actual_text,
            artifact,
        });
    }

    fn finish_path(&mut self, fill: bool) {
        if self.pending_clip {
            if self.path.only_single_rect
                && let Some(rect) = self.path.single_rect
                && let Some(intersection) = self.graphics.clip.intersect(rect)
            {
                self.graphics.clip = intersection;
            }
            self.pending_clip = false;
        }
        if fill
            && self.path.only_single_rect
            && self.graphics.fill_alpha >= ALPHA_OPAQUE
            && self.graphics.normal_blend
            && let Some(bounds) = self.path.single_rect
        {
            self.paints.push(PaintEvent {
                order: self.operator_index,
                bounds,
                kind: PaintKind::FillRect {
                    dark: self.graphics.fill_color.is_dark(),
                },
            });
        }
        self.path.reset();
    }

    fn apply_operator(&mut self, operator: &[u8], span_start: usize, span_end: usize) {
        match operator {
            b"q" => self.graphics_stack.push((self.graphics, self.text.clone())),
            b"Q" => {
                if let Some((graphics, text)) = self.graphics_stack.pop() {
                    self.graphics = graphics;
                    self.text = text;
                }
            }
            b"cm" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 6) {
                    self.graphics
                        .ctm
                        .concat(Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5]));
                }
            }
            b"BT" => {
                self.text.matrix = Matrix::default();
                self.text.line_matrix = Matrix::default();
            }
            b"Tf" => {
                if self.operands.len() >= 2 {
                    if let Some(font) = self.name_at(0) {
                        self.text.font = font;
                    }
                    if let Some(size) = Self::number(&self.operands[1].object) {
                        self.text.font_size = size;
                    }
                }
            }
            b"Tc" => {
                self.text.char_spacing = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.char_spacing)
            }
            b"Tw" => {
                self.text.word_spacing = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.word_spacing)
            }
            b"Tz" => {
                self.text.horizontal_scale = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.horizontal_scale)
            }
            b"TL" => {
                self.text.leading = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.leading)
            }
            b"Tr" => {
                self.text.render_mode = self
                    .operands
                    .first()
                    .and_then(|o| o.object.as_integer())
                    .unwrap_or(self.text.render_mode)
            }
            b"Ts" => {
                self.text.rise = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.rise)
            }
            b"Tm" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 6) {
                    let matrix = Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5]);
                    self.text.matrix = matrix;
                    self.text.line_matrix = matrix;
                }
            }
            b"Td" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 2) {
                    self.text.line_matrix.translate(v[0], v[1]);
                    self.text.matrix = self.text.line_matrix;
                }
            }
            b"TD" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 2) {
                    self.text.leading = -v[1];
                    self.text.line_matrix.translate(v[0], v[1]);
                    self.text.matrix = self.text.line_matrix;
                }
            }
            b"T*" => self.new_line(),
            b"Tj" => {
                if let Some(bytes) = self.operands.first().and_then(|o| o.object.as_string()) {
                    self.show_single(bytes, span_start, span_end);
                }
            }
            b"TJ" => {
                if let Some(array) = self.operands.first().and_then(|o| o.object.as_array()) {
                    self.show_tj_array(&array, span_start, span_end);
                }
            }
            b"'" => {
                self.new_line();
                if let Some(bytes) = self.operands.first().and_then(|o| o.object.as_string()) {
                    self.show_single(bytes, span_start, span_end);
                }
            }
            b"\"" => {
                if self.operands.len() >= 3 {
                    if let Some(value) = Self::number(&self.operands[0].object) {
                        self.text.word_spacing = value;
                    }
                    if let Some(value) = Self::number(&self.operands[1].object) {
                        self.text.char_spacing = value;
                    }
                    self.new_line();
                    if let Some(bytes) = self.operands[2].object.as_string() {
                        self.show_single(bytes, span_start, span_end);
                    }
                }
            }
            b"g" => {
                if let Some(v) = self.numbers().and_then(|v| v.first().copied()) {
                    self.graphics.fill_color = Color::Gray(v);
                }
            }
            b"rg" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 3) {
                    self.graphics.fill_color = Color::Rgb(v[0], v[1], v[2]);
                }
            }
            b"k" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 4) {
                    self.graphics.fill_color = Color::Cmyk(v[0], v[1], v[2], v[3]);
                }
            }
            b"cs" | b"sc" | b"scn" => self.graphics.fill_color = Color::Unknown,
            b"gs" => {
                if let Some(name) = self.name_at(0)
                    && let Some(info) = self.resources.ext_gstates.get(&name)
                {
                    if let Some(alpha) = info.fill_alpha {
                        self.graphics.fill_alpha = alpha;
                    }
                    if let Some(alpha) = info.stroke_alpha {
                        self.graphics.stroke_alpha = alpha;
                    }
                    self.graphics.normal_blend = info.normal_blend;
                }
            }
            b"re" => {
                if let Some(v) = self.numbers().filter(|v| v.len() >= 4) {
                    let rect = Rectangle::new(v[0], v[1], v[0] + v[2], v[1] + v[3]);
                    self.path.set_rect(Rect::from_rectangle(
                        self.graphics.ctm.transform_rectangle(rect),
                    ));
                }
            }
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" => self.path.mark_complex(),
            b"W" | b"W*" => self.pending_clip = true,
            b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" => self.finish_path(true),
            b"S" | b"s" | b"n" => self.finish_path(false),
            b"Do" => {
                if self.graphics.fill_alpha >= ALPHA_OPAQUE
                    && self.graphics.normal_blend
                    && let Some(name) = self.name_at(0)
                    && self.resources.images.get(&name).copied().unwrap_or(false)
                {
                    let bounds = Rect::from_rectangle(
                        self.graphics
                            .ctm
                            .transform_rectangle(Rectangle::new(0.0, 0.0, 1.0, 1.0)),
                    );
                    self.paints.push(PaintEvent {
                        order: self.operator_index,
                        bounds,
                        kind: PaintKind::Image,
                    });
                }
            }
            b"BMC" => self.begin_marked_content(false),
            b"BDC" => self.begin_marked_content(true),
            b"EMC" => {
                self.marked.pop();
            }
            _ => {}
        }
    }

    fn finish(self) -> PageScan {
        let page_area = self.crop.area();
        let page_images: Vec<Rect> = self
            .paints
            .iter()
            .filter_map(|paint| match paint.kind {
                PaintKind::Image if page_area > 0.0 && paint.bounds.area() / page_area >= 0.45 => {
                    Some(paint.bounds)
                }
                _ => None,
            })
            .collect();
        let explicit_invisible_count = self
            .text_events
            .iter()
            .filter(|text| {
                matches!(
                    text.initial_mechanism,
                    Some(HiddenTextMechanism::RenderingModeInvisible)
                        | Some(HiddenTextMechanism::ZeroOpacity)
                )
            })
            .count();

        let mut findings = Vec::new();
        for text in self.text_events {
            let mut mechanism = text.initial_mechanism;
            let mut covering_paint = None;
            if mechanism.is_none()
                && let Some(bounds) = text.bounds
            {
                covering_paint = self
                    .paints
                    .iter()
                    .filter(|paint| paint.order > text.order)
                    .filter(|paint| paint.bounds.coverage_of(bounds) >= COVERAGE_THRESHOLD)
                    .min_by_key(|paint| paint.order)
                    .copied();
                if let Some(paint) = covering_paint {
                    mechanism = Some(match paint.kind {
                        PaintKind::FillRect { .. } => HiddenTextMechanism::CoveredByOpaqueFill,
                        PaintKind::Image => HiddenTextMechanism::CoveredByImage,
                    });
                }
            }
            let Some(mechanism) = mechanism else {
                continue;
            };

            let has_page_image = text.bounds.is_some_and(|bounds| {
                page_images
                    .iter()
                    .any(|image| image.coverage_of(bounds) >= COVERAGE_THRESHOLD)
            });
            let (category, suggested_action, confidence) = if text.actual_text.is_some() {
                (
                    HiddenTextCategory::Accessibility,
                    HiddenTextAction::Keep,
                    0.9,
                )
            } else if mechanism == HiddenTextMechanism::OptionalContentHidden {
                (
                    HiddenTextCategory::HiddenLayer,
                    HiddenTextAction::Keep,
                    0.95,
                )
            } else if mechanism == HiddenTextMechanism::OutsideCropBox {
                (
                    HiddenTextCategory::OutsidePage,
                    HiddenTextAction::Keep,
                    0.95,
                )
            } else if matches!(
                mechanism,
                HiddenTextMechanism::RenderingModeInvisible
                    | HiddenTextMechanism::ZeroOpacity
                    | HiddenTextMechanism::CoveredByImage
            ) && has_page_image
                && explicit_invisible_count >= 3
            {
                (HiddenTextCategory::OcrOverlay, HiddenTextAction::Keep, 0.92)
            } else if let Some(PaintEvent {
                kind: PaintKind::FillRect { dark: true },
                bounds,
                ..
            }) = covering_paint
            {
                if page_area > 0.0 && bounds.area() / page_area <= 0.4 {
                    (
                        HiddenTextCategory::LikelyRedactionLeak,
                        HiddenTextAction::Remove,
                        0.94,
                    )
                } else {
                    (
                        HiddenTextCategory::OtherInvisible,
                        HiddenTextAction::Keep,
                        0.65,
                    )
                }
            } else {
                (
                    HiddenTextCategory::OtherInvisible,
                    HiddenTextAction::Keep,
                    0.7,
                )
            };
            let display_text = text
                .actual_text
                .clone()
                .unwrap_or_else(|| text.text.clone());
            findings.push(InternalFinding {
                public: HiddenTextFinding {
                    id: text.id,
                    page_number: text.page_number,
                    operator_index: text.operator_index,
                    mechanism,
                    category,
                    suggested_action,
                    text: display_text,
                    raw_hex: hex(&text.raw),
                    bounds: text.bounds.map(Rect::to_public),
                    confidence,
                    artifact: text.artifact,
                },
                span_start: text.span_start,
                span_end: text.span_end,
            });
        }
        PageScan { findings }
    }
}

impl ObjectHandleParserCallbacks for PageScanner<'_> {
    fn handle_object(
        &mut self,
        object: ObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            let span_start = self
                .operands
                .first()
                .map_or(offset, |operand| operand.offset);
            let span_end = offset.saturating_add(length);
            self.apply_operator(&operator, span_start, span_end);
            self.operands.clear();
            self.operator_index += 1;
        } else if object.as_inline_image().is_none() {
            self.operands.push(OperandSpan { object, offset });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct InternalFinding {
    public: HiddenTextFinding,
    span_start: usize,
    span_end: usize,
}

#[derive(Debug, Clone)]
struct PageScan {
    findings: Vec<InternalFinding>,
}

#[derive(Debug, Default)]
pub(crate) struct HiddenTextApplyStats {
    pub removed: usize,
}

struct OptionalContentState {
    off: BTreeSet<ObjectRef>,
    on: BTreeSet<ObjectRef>,
    base_off: bool,
}

pub(crate) fn analyze_hidden_text<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<Vec<HiddenTextFinding>> {
    let ocg = optional_content_state(pdf)?;
    let page_refs = flpdf::pages::page_refs(pdf)?;
    let mut findings = Vec::new();
    for (index, page_ref) in page_refs.into_iter().enumerate() {
        let scan = scan_page(pdf, page_ref, index + 1, &ocg)?;
        findings.extend(scan.findings.into_iter().map(|finding| finding.public));
    }
    Ok(findings)
}

pub(crate) fn apply_hidden_text_policy<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    policy: &HiddenTextPolicy,
) -> Result<HiddenTextApplyStats> {
    if policy.remove_categories.is_empty() && policy.overrides.is_empty() {
        return Ok(HiddenTextApplyStats::default());
    }
    let ocg = optional_content_state(pdf)?;
    let page_refs = flpdf::pages::page_refs(pdf)?;
    let mut stats = HiddenTextApplyStats::default();
    for (index, page_ref) in page_refs.into_iter().enumerate() {
        let mut scan = scan_page(pdf, page_ref, index + 1, &ocg)?;
        scan.findings
            .retain(|finding| policy.should_remove(&finding.public));
        if scan.findings.is_empty() {
            continue;
        }

        let page = pdf.get_object_handle(page_ref);
        pdf.resolve(&page)?;
        page.coalesce_content_streams()?;
        pdf.mark_object_handle_dirty(&page)?;
        let contents = page.try_get_key(b"/Contents")?;
        let decoded = contents.get_stream_data(DecodeLevel::Specialized)?;

        // Coalescing uses the same newline-joining semantics as page parsing,
        // but rescan if a damaged input caused the provider route to differ.
        let rescanned = scan_page(pdf, page_ref, index + 1, &ocg)?;
        let selected: BTreeSet<String> = scan
            .findings
            .iter()
            .map(|finding| finding.public.id.clone())
            .collect();
        let mut ranges: Vec<(usize, usize)> = rescanned
            .findings
            .into_iter()
            .filter(|finding| selected.contains(&finding.public.id))
            .map(|finding| (finding.span_start, finding.span_end))
            .collect();
        if ranges.is_empty() {
            continue;
        }
        ranges.sort_unstable();
        let rewritten = remove_ranges(decoded.as_ref(), &ranges);
        stats.removed += ranges.len();
        contents.replace_stream_data(
            Rc::new(rewritten),
            Some(ObjectHandle::null()),
            Some(ObjectHandle::null()),
        );
        pdf.mark_object_handle_dirty(&contents)?;
    }
    Ok(stats)
}

fn remove_ranges(input: &[u8], ranges: &[(usize, usize)]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0;
    for &(start, end) in ranges {
        let start = start.min(input.len()).max(cursor);
        let end = end.min(input.len()).max(start);
        output.extend_from_slice(&input[cursor..start]);
        output.push(b' ');
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    output
}

fn scan_page<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    page_ref: ObjectRef,
    page_number: usize,
    ocg: &OptionalContentState,
) -> Result<PageScan> {
    let (crop, resources) = {
        let mut helper = PageObjectHelper::new(page_ref, pdf);
        let crop = helper.get_crop_box(false, false)?;
        let crop = Rect::from_rectangle(crop.try_get_array_as_rectangle()?);
        let resources = build_resources(&helper.get_resources(false)?, ocg)?;
        (crop, resources)
    };
    let page = pdf.get_object_handle(page_ref);
    pdf.resolve(&page)?;
    let mut scanner = PageScanner::new(
        page_number,
        crop,
        &resources,
        &ocg.off,
        ocg.base_off,
        &ocg.on,
    );
    page.parse_page_contents(&mut scanner)?;
    Ok(scanner.finish())
}

fn build_resources(resources: &ObjectHandle, ocg: &OptionalContentState) -> Result<Resources> {
    Ok(Resources {
        fonts: build_fonts(resources)?,
        ext_gstates: build_ext_gstates(resources)?,
        images: build_images(resources)?,
        properties: build_properties(resources, ocg)?,
    })
}

fn resource_dictionary(resources: &ObjectHandle, key: &[u8]) -> Result<Option<ObjectHandle>> {
    let value = resources.try_get_key(key)?;
    if value.try_is_dictionary()? {
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

fn normalized_resource_key(key: &[u8]) -> Vec<u8> {
    key.strip_prefix(b"/").unwrap_or(key).to_vec()
}

fn build_fonts(resources: &ObjectHandle) -> Result<BTreeMap<Vec<u8>, FontInfo>> {
    let Some(fonts) = resource_dictionary(resources, b"/Font")? else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for key in fonts.try_get_keys()? {
        let font = fonts.try_get_key(&key)?;
        out.insert(normalized_resource_key(&key), font_info(&font)?);
    }
    Ok(out)
}

fn font_info(font: &ObjectHandle) -> Result<FontInfo> {
    let mut info = FontInfo::default();
    let subtype = font.try_get_key(b"/Subtype")?.as_name().unwrap_or_default();
    let encoding = font
        .try_get_key(b"/Encoding")?
        .as_name()
        .unwrap_or_default();
    info.identity_two_byte =
        subtype == b"Type0" && matches!(encoding.as_slice(), b"Identity-H" | b"Identity-V");
    if info.identity_two_byte {
        info.max_code_bytes = 2;
    }

    let to_unicode = font.try_get_key(b"/ToUnicode")?;
    if to_unicode.as_stream_dict().is_some()
        && let Ok(data) = to_unicode.get_stream_data(DecodeLevel::Generalized)
    {
        info.unicode = parse_to_unicode(data.as_ref());
        if let Some(max) = info.unicode.keys().map(Vec::len).max() {
            info.max_code_bytes = info.max_code_bytes.max(max);
        }
    }

    if subtype == b"Type0" {
        let descendants = font
            .try_get_key(b"/DescendantFonts")?
            .try_get_array_as_vector()?;
        if let Some(descendant) = descendants.first() {
            let dw = descendant.try_get_key(b"/DW")?;
            if dw.try_is_number()? {
                info.default_width = dw.try_get_numeric_value()?;
            } else {
                info.default_width = 1000.0;
            }
            parse_cid_widths(&descendant.try_get_key(b"/W")?, &mut info.widths)?;
        }
    } else {
        let first = font.try_get_key(b"/FirstChar")?;
        let first = if first.try_is_integer()? {
            first.try_get_int_value()?.max(0) as u32
        } else {
            0
        };
        let widths = font.try_get_key(b"/Widths")?.try_get_array_as_vector()?;
        for (offset, width) in widths.iter().enumerate() {
            if width.try_is_number()? {
                info.widths.insert(
                    first + u32::try_from(offset).unwrap_or(u32::MAX),
                    width.try_get_numeric_value()?,
                );
            }
        }
    }
    Ok(info)
}

fn parse_cid_widths(widths: &ObjectHandle, out: &mut HashMap<u32, f64>) -> Result<()> {
    let items = widths.try_get_array_as_vector()?;
    let mut i = 0;
    while i < items.len() {
        let Some(start) = items[i].as_integer().and_then(|v| u32::try_from(v).ok()) else {
            i += 1;
            continue;
        };
        i += 1;
        let Some(next) = items.get(i) else {
            break;
        };
        if let Some(array) = next.as_array() {
            for (offset, width) in array.iter().enumerate() {
                if width.try_is_number()? {
                    out.insert(
                        start + u32::try_from(offset).unwrap_or(u32::MAX),
                        width.try_get_numeric_value()?,
                    );
                }
            }
            i += 1;
        } else if let Some(end) = next.as_integer().and_then(|v| u32::try_from(v).ok()) {
            i += 1;
            let Some(width) = items.get(i) else {
                break;
            };
            if width.try_is_number()? {
                let value = width.try_get_numeric_value()?;
                for code in start..=end.min(start.saturating_add(65_535)) {
                    out.insert(code, value);
                }
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    Ok(())
}

fn build_ext_gstates(resources: &ObjectHandle) -> Result<BTreeMap<Vec<u8>, ExtGStateInfo>> {
    let Some(states) = resource_dictionary(resources, b"/ExtGState")? else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for key in states.try_get_keys()? {
        let state = states.try_get_key(&key)?;
        let ca = state.try_get_key(b"/ca")?;
        let ca = ca
            .try_is_number()?
            .then(|| ca.try_get_numeric_value())
            .transpose()?;
        let cap_a = state.try_get_key(b"/CA")?;
        let cap_a = cap_a
            .try_is_number()?
            .then(|| cap_a.try_get_numeric_value())
            .transpose()?;
        let blend = state.try_get_key(b"/BM")?;
        let normal_blend = blend.is_null() || blend.try_is_name_and_equals(b"Normal")?;
        out.insert(
            normalized_resource_key(&key),
            ExtGStateInfo {
                fill_alpha: ca,
                stroke_alpha: cap_a,
                normal_blend,
            },
        );
    }
    Ok(out)
}

fn build_images(resources: &ObjectHandle) -> Result<BTreeMap<Vec<u8>, bool>> {
    let Some(xobjects) = resource_dictionary(resources, b"/XObject")? else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for key in xobjects.try_get_keys()? {
        let object = xobjects.try_get_key(&key)?;
        let is_image = object.as_stream_dict().is_some_and(|dict| {
            dict.try_get_key(b"/Subtype")
                .is_ok_and(|v| v.try_is_name_and_equals(b"Image").unwrap_or(false))
        });
        let unmasked = is_image
            && object.as_stream_dict().is_some_and(|dict| {
                !dict.try_get_keys().is_ok_and(|keys| {
                    keys.contains(b"/SMask".as_slice()) || keys.contains(b"/Mask".as_slice())
                })
            });
        out.insert(normalized_resource_key(&key), unmasked);
    }
    Ok(out)
}

fn build_properties(
    resources: &ObjectHandle,
    _ocg: &OptionalContentState,
) -> Result<BTreeMap<Vec<u8>, Option<ObjectRef>>> {
    let Some(properties) = resource_dictionary(resources, b"/Properties")? else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for key in properties.try_get_keys()? {
        let value = properties.try_get_key(&key)?;
        out.insert(normalized_resource_key(&key), value.object_ref());
    }
    Ok(out)
}

fn optional_content_state<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<OptionalContentState> {
    let mut out = OptionalContentState {
        off: BTreeSet::new(),
        on: BTreeSet::new(),
        base_off: false,
    };
    let root = pdf.trailer().try_get_key(b"/Root")?;
    let oc_properties = root.try_get_key(b"/OCProperties")?;
    if !oc_properties.try_is_dictionary()? {
        return Ok(out);
    }
    let default = oc_properties.try_get_key(b"/D")?;
    if !default.try_is_dictionary()? {
        return Ok(out);
    }
    let base = default.try_get_key(b"/BaseState")?;
    out.base_off = base.try_is_name_and_equals(b"OFF")?;
    for (key, target) in [
        (b"/OFF".as_slice(), &mut out.off),
        (b"/ON".as_slice(), &mut out.on),
    ] {
        for object in default.try_get_key(key)?.try_get_array_as_vector()? {
            if let Some(object_ref) = object.object_ref() {
                target.insert(object_ref);
            }
        }
    }
    Ok(out)
}

fn parse_to_unicode(data: &[u8]) -> HashMap<Vec<u8>, String> {
    let tokens = cmap_tokens(data);
    let mut out = HashMap::new();
    let mut index = 0;
    while index < tokens.len() {
        match tokens[index].as_slice() {
            b"beginbfchar" => {
                index += 1;
                while index + 1 < tokens.len() && tokens[index].as_slice() != b"endbfchar" {
                    if let (Some(source), Some(dest)) =
                        (hex_token(&tokens[index]), hex_token(&tokens[index + 1]))
                    {
                        out.insert(source, decode_utf16be(&dest));
                    }
                    index += 2;
                }
            }
            b"beginbfrange" => {
                index += 1;
                while index + 2 < tokens.len() && tokens[index].as_slice() != b"endbfrange" {
                    let (Some(start), Some(end)) =
                        (hex_token(&tokens[index]), hex_token(&tokens[index + 1]))
                    else {
                        index += 1;
                        continue;
                    };
                    index += 2;
                    let start_num = bytes_to_u32(&start);
                    let end_num = bytes_to_u32(&end);
                    if tokens.get(index).is_some_and(|t| t.as_slice() == b"[") {
                        index += 1;
                        let mut code = start_num;
                        while index < tokens.len()
                            && tokens[index].as_slice() != b"]"
                            && code <= end_num
                        {
                            if let Some(dest) = hex_token(&tokens[index]) {
                                out.insert(u32_to_bytes(code, start.len()), decode_utf16be(&dest));
                            }
                            code = code.saturating_add(1);
                            index += 1;
                        }
                        if index < tokens.len() && tokens[index].as_slice() == b"]" {
                            index += 1;
                        }
                    } else if let Some(dest) = tokens.get(index).and_then(|t| hex_token(t)) {
                        let mut dest_num = bytes_to_u32(&dest);
                        for code in start_num..=end_num.min(start_num.saturating_add(65_535)) {
                            let mapped = u32_to_bytes(dest_num, dest.len());
                            out.insert(u32_to_bytes(code, start.len()), decode_utf16be(&mapped));
                            dest_num = dest_num.saturating_add(1);
                        }
                        index += 1;
                    } else {
                        index += 1;
                    }
                }
            }
            _ => index += 1,
        }
    }
    out
}

fn cmap_tokens(data: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if data[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if data[i] == b'%' {
            while i < data.len() && !matches!(data[i], b'\n' | b'\r') {
                i += 1;
            }
            continue;
        }
        if matches!(data[i], b'[' | b']') {
            out.push(vec![data[i]]);
            i += 1;
            continue;
        }
        if data[i] == b'<' && data.get(i + 1) != Some(&b'<') {
            let start = i;
            i += 1;
            while i < data.len() && data[i] != b'>' {
                i += 1;
            }
            i = (i + 1).min(data.len());
            out.push(data[start..i].to_vec());
            continue;
        }
        let start = i;
        while i < data.len()
            && !data[i].is_ascii_whitespace()
            && !matches!(data[i], b'[' | b']' | b'<')
        {
            i += 1;
        }
        if i > start {
            out.push(data[start..i].to_vec());
        } else {
            i += 1;
        }
    }
    out
}

fn hex_token(token: &[u8]) -> Option<Vec<u8>> {
    if token.len() < 2 || token.first() != Some(&b'<') || token.last() != Some(&b'>') {
        return None;
    }
    let mut digits = token[1..token.len() - 1]
        .iter()
        .copied()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    if digits.len() % 2 == 1 {
        digits.push(b'0');
    }
    let mut out = Vec::with_capacity(digits.len() / 2);
    for pair in digits.as_chunks::<2>().0 {
        let high = hex_nibble(pair[0])?;
        let low = hex_nibble(pair[1])?;
        out.push((high << 4) | low);
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn bytes_to_u32(bytes: &[u8]) -> u32 {
    bytes
        .iter()
        .fold(0_u32, |value, byte| (value << 8) | u32::from(*byte))
}

fn u32_to_bytes(value: u32, len: usize) -> Vec<u8> {
    (0..len)
        .rev()
        .map(|shift| ((value >> (shift * 8)) & 0xff) as u8)
        .collect()
}

fn decode_utf16be(bytes: &[u8]) -> String {
    if !bytes.len().is_multiple_of(2) {
        return String::new();
    }
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_start_matches('\u{feff}')
        .to_owned()
}

fn decode_pdf_text_string(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xfe, 0xff]) {
        return decode_utf16be(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe])
        && rest.len() % 2 == 0
    {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    String::from_utf8_lossy(bytes).into_owned()
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_to_unicode_bfchar_and_bfrange() {
        let cmap = br#"
            2 beginbfchar
            <01> <0041>
            <02> <03A9>
            endbfchar
            1 beginbfrange
            <10> <12> <0061>
            endbfrange
        "#;
        let map = parse_to_unicode(cmap);
        assert_eq!(map.get(&vec![0x01]).map(String::as_str), Some("A"));
        assert_eq!(map.get(&vec![0x02]).map(String::as_str), Some("Ω"));
        assert_eq!(map.get(&vec![0x10]).map(String::as_str), Some("a"));
        assert_eq!(map.get(&vec![0x12]).map(String::as_str), Some("c"));
    }

    #[test]
    fn removes_non_overlapping_ranges_without_concatenating_tokens() {
        assert_eq!(
            remove_ranges(b"abc Tj 123 Tj xyz", &[(0, 6), (7, 13)]),
            b"    xyz"
        );
    }

    fn event(
        id: &str,
        order: usize,
        bounds: Rect,
        mechanism: Option<HiddenTextMechanism>,
    ) -> TextEvent {
        TextEvent {
            id: id.to_owned(),
            page_number: 1,
            operator_index: order,
            order,
            span_start: order * 10,
            span_end: order * 10 + 5,
            raw: id.as_bytes().to_vec(),
            text: id.to_owned(),
            bounds: Some(bounds),
            initial_mechanism: mechanism,
            actual_text: None,
            artifact: false,
        }
    }

    fn finish_events(events: Vec<TextEvent>, paints: Vec<PaintEvent>) -> Vec<HiddenTextFinding> {
        let resources = Resources {
            fonts: BTreeMap::new(),
            ext_gstates: BTreeMap::new(),
            images: BTreeMap::new(),
            properties: BTreeMap::new(),
        };
        let ocgs = BTreeSet::new();
        let mut scanner = PageScanner::new(
            1,
            Rect::new(0.0, 0.0, 100.0, 100.0),
            &resources,
            &ocgs,
            false,
            &ocgs,
        );
        scanner.text_events = events;
        scanner.paints = paints;
        scanner
            .finish()
            .findings
            .into_iter()
            .map(|finding| finding.public)
            .collect()
    }

    #[test]
    fn classifies_page_image_invisible_text_as_ocr_overlay() {
        let bounds = Rect::new(10.0, 10.0, 20.0, 20.0);
        let events = (0..3)
            .map(|index| {
                event(
                    &format!("ocr-{index}"),
                    index + 10,
                    bounds,
                    Some(HiddenTextMechanism::RenderingModeInvisible),
                )
            })
            .collect();
        let findings = finish_events(
            events,
            vec![PaintEvent {
                order: 1,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                kind: PaintKind::Image,
            }],
        );

        assert_eq!(findings.len(), 3);
        assert!(findings.iter().all(|finding| {
            finding.category == HiddenTextCategory::OcrOverlay
                && finding.suggested_action == HiddenTextAction::Keep
        }));
    }

    #[test]
    fn classifies_small_dark_cover_as_likely_redaction_leak() {
        let findings = finish_events(
            vec![event("secret", 1, Rect::new(10.0, 10.0, 20.0, 20.0), None)],
            vec![PaintEvent {
                order: 2,
                bounds: Rect::new(9.0, 9.0, 21.0, 21.0),
                kind: PaintKind::FillRect { dark: true },
            }],
        );

        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].mechanism,
            HiddenTextMechanism::CoveredByOpaqueFill
        );
        assert_eq!(
            findings[0].category,
            HiddenTextCategory::LikelyRedactionLeak
        );
        assert_eq!(findings[0].suggested_action, HiddenTextAction::Remove);
        assert!(findings[0].confidence >= 0.9);
    }

    #[test]
    fn does_not_call_large_dark_occlusion_a_redaction_leak() {
        let findings = finish_events(
            vec![event(
                "ambiguous",
                1,
                Rect::new(10.0, 10.0, 20.0, 20.0),
                None,
            )],
            vec![PaintEvent {
                order: 2,
                bounds: Rect::new(0.0, 0.0, 100.0, 100.0),
                kind: PaintKind::FillRect { dark: true },
            }],
        );

        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].category, HiddenTextCategory::OtherInvisible);
        assert_eq!(findings[0].suggested_action, HiddenTextAction::Keep);
    }

    #[test]
    fn keeps_accessibility_hidden_layer_and_outside_page_text() {
        let bounds = Rect::new(10.0, 10.0, 20.0, 20.0);
        let mut accessibility = event(
            "glyphs",
            1,
            bounds,
            Some(HiddenTextMechanism::RenderingModeInvisible),
        );
        accessibility.actual_text = Some("accessible replacement".to_owned());
        let findings = finish_events(
            vec![
                accessibility,
                event(
                    "layer",
                    2,
                    bounds,
                    Some(HiddenTextMechanism::OptionalContentHidden),
                ),
                event(
                    "outside",
                    3,
                    bounds,
                    Some(HiddenTextMechanism::OutsideCropBox),
                ),
            ],
            Vec::new(),
        );

        assert_eq!(findings[0].category, HiddenTextCategory::Accessibility);
        assert_eq!(findings[0].text, "accessible replacement");
        assert_eq!(findings[1].category, HiddenTextCategory::HiddenLayer);
        assert_eq!(findings[2].category, HiddenTextCategory::OutsidePage);
        assert!(
            findings
                .iter()
                .all(|finding| finding.suggested_action == HiddenTextAction::Keep)
        );
    }

    #[test]
    fn keeps_clipped_and_zero_opacity_text_when_semantics_are_uncertain() {
        let bounds = Rect::new(10.0, 10.0, 20.0, 20.0);
        let findings = finish_events(
            vec![
                event("clipped", 1, bounds, Some(HiddenTextMechanism::ClippedOut)),
                event(
                    "transparent",
                    2,
                    bounds,
                    Some(HiddenTextMechanism::ZeroOpacity),
                ),
            ],
            Vec::new(),
        );

        assert!(findings.iter().all(|finding| {
            finding.category == HiddenTextCategory::OtherInvisible
                && finding.suggested_action == HiddenTextAction::Keep
        }));
    }

    #[test]
    fn per_finding_override_wins_over_category_policy() {
        let finding = finish_events(
            vec![event("secret", 1, Rect::new(10.0, 10.0, 20.0, 20.0), None)],
            vec![PaintEvent {
                order: 2,
                bounds: Rect::new(9.0, 9.0, 21.0, 21.0),
                kind: PaintKind::FillRect { dark: true },
            }],
        )
        .remove(0);

        let mut policy = HiddenTextPolicy::default();
        policy
            .remove_categories
            .insert(HiddenTextCategory::LikelyRedactionLeak);
        assert!(policy.should_remove(&finding));

        policy
            .overrides
            .insert(finding.id.clone(), HiddenTextAction::Keep);
        assert!(!policy.should_remove(&finding));

        policy
            .remove_categories
            .remove(&HiddenTextCategory::LikelyRedactionLeak);
        policy
            .overrides
            .insert(finding.id.clone(), HiddenTextAction::Remove);
        assert!(policy.should_remove(&finding));
    }
}
