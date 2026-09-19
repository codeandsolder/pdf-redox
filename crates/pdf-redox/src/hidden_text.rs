use crate::config::HiddenTextPolicy;
use crate::report::{
    HiddenTextAction, HiddenTextCategory, HiddenTextFinding, HiddenTextMechanism, PageRect,
};
use crate::{EditDocument, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result};
use flpdf::content_stream::ContentScalar;
use flpdf::{
    DecodeLevel, Matrix, ObjectHandle, ObjectHandleParserCallbacks, ObjectRef, ParseControl,
    Rectangle,
};
#[cfg(test)]
use flpdf::{PageObjectHelper, Pdf};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
#[cfg(test)]
use std::io::{Read, Seek};
#[cfg(test)]
use std::rc::Rc;

const ALPHA_INVISIBLE: f64 = 0.001;
const ALPHA_OPAQUE: f64 = 0.995;
const COVERAGE_THRESHOLD: f64 = 0.97;

type ObjectKey = (i32, i32);

fn flpdf_object_key(reference: ObjectRef) -> ObjectKey {
    (
        i32::try_from(reference.number).unwrap_or(i32::MAX),
        i32::from(reference.generation),
    )
}

fn cow_object_key(handle: CowObjectHandle) -> Option<ObjectKey> {
    match handle {
        CowObjectHandle::Existing(id) => Some((id.number(), id.generation())),
        CowObjectHandle::New(_) => None,
    }
}

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
    properties: BTreeMap<Vec<u8>, Option<ObjectKey>>,
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
enum OperandObject {
    Scalar(ContentScalar),
    Handle(ObjectHandle),
}

impl OperandObject {
    fn as_integer(&self) -> Option<i64> {
        match self {
            Self::Scalar(value) => value.as_integer(),
            Self::Handle(value) => value.as_integer(),
        }
    }

    fn as_real(&self) -> Option<f64> {
        match self {
            Self::Scalar(value) => value.as_real(),
            Self::Handle(value) => value.as_real(),
        }
    }

    fn as_name(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Scalar(value) => value.as_name().map(Cow::Borrowed),
            Self::Handle(value) => value.as_name().map(Cow::Owned),
        }
    }

    fn as_string(&self) -> Option<Vec<u8>> {
        match self {
            Self::Scalar(value) => value.as_string().map(ToOwned::to_owned),
            Self::Handle(value) => value.as_string(),
        }
    }

    fn as_array(&self) -> Option<Vec<ObjectHandle>> {
        match self {
            Self::Scalar(_) => None,
            Self::Handle(value) => value.as_array(),
        }
    }

    fn handle(&self) -> Option<&ObjectHandle> {
        match self {
            Self::Scalar(_) => None,
            Self::Handle(value) => Some(value),
        }
    }
}

#[derive(Debug, Clone)]
struct OperandSpan {
    object: OperandObject,
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
    hidden_ocgs: &'a BTreeSet<ObjectKey>,
    base_ocg_off: bool,
    on_ocgs: &'a BTreeSet<ObjectKey>,
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
        hidden_ocgs: &'a BTreeSet<ObjectKey>,
        base_ocg_off: bool,
        on_ocgs: &'a BTreeSet<ObjectKey>,
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

    fn number(object: &OperandObject) -> Option<f64> {
        if object.as_integer().is_some() {
            return object.as_integer().map(|v| v as f64);
        }
        object.as_real()
    }

    fn handle_number(object: &ObjectHandle) -> Option<f64> {
        object
            .as_integer()
            .map(|value| value as f64)
            .or_else(|| object.as_real())
    }

    fn numbers(&self) -> Option<SmallVec<[f64; 6]>> {
        let mut values = SmallVec::new();
        for operand in &self.operands {
            values.push(Self::number(&operand.object)?);
        }
        Some(values)
    }

    fn name_at(&self, index: usize) -> Option<Cow<'_, [u8]>> {
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
        strings: &[Vec<u8>],
        tj_adjustments: &[f64],
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
        self.show_text(&[bytes], &[], span_start, span_end);
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
            } else if let Some(value) = Self::handle_number(item) {
                pending_adjustment += (-value / 1000.0) * self.text.font_size * hscale;
            }
        }
        if !strings.is_empty() {
            adjustments.push(pending_adjustment);
            self.show_text(&strings, &adjustments, span_start, span_end);
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
        let artifact = tag.as_ref() == b"Artifact";
        let mut optional_hidden = false;
        let mut actual_text = None;
        if with_properties && let Some(properties) = self.operands.get(1).map(|o| &o.object) {
            if let Some(name) = properties.as_name() {
                if tag.as_ref() == b"OC" {
                    optional_hidden = self.property_hidden(&name);
                }
            } else if let Some(properties) = properties.handle()
                && properties.as_dictionary().is_some()
            {
                if tag.as_ref() == b"OC"
                    && let Some(object_ref) = properties.object_ref()
                {
                    optional_hidden = if self.base_ocg_off {
                        !self.on_ocgs.contains(&flpdf_object_key(object_ref))
                    } else {
                        self.hidden_ocgs.contains(&flpdf_object_key(object_ref))
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
                        self.text.font = font.into_owned();
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
                    .unwrap_or(self.text.char_spacing);
            }
            b"Tw" => {
                self.text.word_spacing = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.word_spacing);
            }
            b"Tz" => {
                self.text.horizontal_scale = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.horizontal_scale);
            }
            b"TL" => {
                self.text.leading = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.leading);
            }
            b"Tr" => {
                self.text.render_mode = self
                    .operands
                    .first()
                    .and_then(|o| o.object.as_integer())
                    .unwrap_or(self.text.render_mode);
            }
            b"Ts" => {
                self.text.rise = self
                    .numbers()
                    .and_then(|v| v.first().copied())
                    .unwrap_or(self.text.rise);
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
                    && let Some(info) = self.resources.ext_gstates.get(name.as_ref())
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
                    && self
                        .resources
                        .images
                        .get(name.as_ref())
                        .copied()
                        .unwrap_or(false)
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
                    Some(
                        HiddenTextMechanism::RenderingModeInvisible
                            | HiddenTextMechanism::ZeroOpacity
                    )
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
            self.operands.push(OperandSpan {
                object: OperandObject::Scalar(scalar),
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
        let span_start = self
            .operands
            .first()
            .map_or(offset, |operand| operand.offset);
        let span_end = offset.saturating_add(length);
        self.apply_operator(operator, span_start, span_end);
        self.operands.clear();
        self.operator_index += 1;
        Ok(ParseControl::Continue)
    }

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
            self.operands.push(OperandSpan {
                object: OperandObject::Handle(object),
                offset,
            });
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

const LARGE_DIAGONAL_TEXT_STRONG_MIN_EFFECTIVE_SIZE_PT: f64 = 24.0;
const LARGE_DIAGONAL_TEXT_REPEAT_MIN_EFFECTIVE_SIZE_PT: f64 = 20.0;
const LARGE_DIAGONAL_TEXT_MIN_AXIS_ANGLE_DEGREES: f64 = 15.0;
const LARGE_DIAGONAL_TEXT_REPEAT_MIN_OCCURRENCES: usize = 3;
const LARGE_DIAGONAL_TEXT_DENSE_PAGE_MIN_OCCURRENCES: usize = 8;
const LARGE_DIAGONAL_TEXT_SIZE_EPSILON_PT: f64 = 0.01;
const LARGE_DIAGONAL_TEXT_ANGLE_EPSILON_DEGREES: f64 = 0.01;

#[derive(Debug, Clone)]
struct LargeDiagonalTextObject {
    start: usize,
    saw_text: bool,
    all_strong_qualify: bool,
    all_repeat_qualify: bool,
    payload: Vec<u8>,
}

#[derive(Debug, Clone)]
struct LargeDiagonalTextCandidate {
    range: (usize, usize),
    strong: bool,
    repeat_eligible: bool,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
struct LargeDiagonalTextScanner {
    graphics_ctm: Matrix,
    graphics_stack: Vec<(Matrix, TextState)>,
    text: TextState,
    operands: Vec<OperandSpan>,
    object: Option<LargeDiagonalTextObject>,
    candidates: Vec<LargeDiagonalTextCandidate>,
}

impl LargeDiagonalTextScanner {
    fn number(object: &OperandObject) -> Option<f64> {
        object
            .as_integer()
            .map(|value| value as f64)
            .or_else(|| object.as_real())
    }

    fn numbers(&self) -> Option<SmallVec<[f64; 6]>> {
        let mut values = SmallVec::new();
        for operand in &self.operands {
            values.push(Self::number(&operand.object)?);
        }
        Some(values)
    }

    fn text_show_payload(&self, operator: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        let mut push = |bytes: Vec<u8>| {
            if bytes.is_empty() {
                return;
            }
            payload.push(0xff);
            payload.extend_from_slice(&bytes);
        };
        match operator {
            b"Tj" | b"'" => {
                if let Some(bytes) = self
                    .operands
                    .first()
                    .and_then(|operand| operand.object.as_string())
                {
                    push(bytes);
                }
            }
            b"TJ" => {
                if let Some(array) = self
                    .operands
                    .first()
                    .and_then(|operand| operand.object.as_array())
                {
                    for item in array {
                        if let Some(bytes) = item.as_string() {
                            push(bytes);
                        }
                    }
                }
            }
            b"\"" => {
                if let Some(bytes) = self
                    .operands
                    .get(2)
                    .and_then(|operand| operand.object.as_string())
                {
                    push(bytes);
                }
            }
            _ => {}
        }
        payload
    }

    fn show_metrics(&self) -> Option<(f64, f64)> {
        let mut combined = self.graphics_ctm;
        combined.concat(self.text.matrix);
        let size = self.text.font_size.abs();
        if !size.is_finite() || size <= 0.0 {
            return None;
        }
        let effective_size = size * combined.c.hypot(combined.d);
        let horizontal_scale_sign = if self.text.horizontal_scale.is_sign_negative() {
            -1.0
        } else {
            1.0
        };
        let x = combined.a * horizontal_scale_sign;
        let y = combined.b * horizontal_scale_sign;
        if !effective_size.is_finite() || !x.is_finite() || !y.is_finite() || x.hypot(y) <= 1.0e-12
        {
            return None;
        }
        let angle = y.atan2(x).to_degrees().rem_euclid(180.0);
        let horizontal_distance = angle.min(180.0 - angle);
        let axis_distance = horizontal_distance.min((90.0 - horizontal_distance).abs());
        Some((effective_size, axis_distance))
    }

    fn selected_ranges(&self) -> Vec<(usize, usize)> {
        let mut repeated = HashMap::<&[u8], usize>::new();
        for candidate in &self.candidates {
            if candidate.repeat_eligible && !candidate.payload.is_empty() {
                *repeated.entry(candidate.payload.as_slice()).or_default() += 1;
            }
        }
        let medium_count = self
            .candidates
            .iter()
            .filter(|candidate| candidate.repeat_eligible)
            .count();
        self.candidates
            .iter()
            .filter(|candidate| {
                candidate.strong
                    || (candidate.repeat_eligible
                        && (medium_count >= LARGE_DIAGONAL_TEXT_DENSE_PAGE_MIN_OCCURRENCES
                            || repeated
                                .get(candidate.payload.as_slice())
                                .copied()
                                .unwrap_or_default()
                                >= LARGE_DIAGONAL_TEXT_REPEAT_MIN_OCCURRENCES))
            })
            .map(|candidate| candidate.range)
            .collect()
    }

    fn apply_operator(&mut self, operator: &[u8], span_start: usize, span_end: usize) {
        match operator {
            b"q" => self
                .graphics_stack
                .push((self.graphics_ctm, self.text.clone())),
            b"Q" => {
                if let Some((ctm, text)) = self.graphics_stack.pop() {
                    self.graphics_ctm = ctm;
                    self.text = text;
                }
            }
            b"cm" => {
                if let Some(values) = self.numbers().filter(|values| values.len() >= 6) {
                    self.graphics_ctm.concat(Matrix::new(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                    ));
                }
            }
            b"BT" => {
                self.text.matrix = Matrix::default();
                self.text.line_matrix = Matrix::default();
                self.object = Some(LargeDiagonalTextObject {
                    start: span_start,
                    saw_text: false,
                    all_strong_qualify: true,
                    all_repeat_qualify: true,
                    payload: Vec::new(),
                });
            }
            b"ET" => {
                if let Some(object) = self.object.take()
                    && object.saw_text
                    && (object.all_strong_qualify || object.all_repeat_qualify)
                {
                    self.candidates.push(LargeDiagonalTextCandidate {
                        range: (object.start, span_end),
                        strong: object.all_strong_qualify,
                        repeat_eligible: object.all_repeat_qualify,
                        payload: object.payload,
                    });
                }
            }
            b"Tf" => {
                if self.operands.len() >= 2
                    && let Some(size) = Self::number(&self.operands[1].object)
                {
                    self.text.font_size = size;
                }
            }
            b"Tz" => {
                if let Some(scale) = self.numbers().and_then(|values| values.first().copied()) {
                    self.text.horizontal_scale = scale;
                }
            }
            b"Tm" => {
                if let Some(values) = self.numbers().filter(|values| values.len() >= 6) {
                    let matrix = Matrix::new(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                    );
                    self.text.matrix = matrix;
                    self.text.line_matrix = matrix;
                }
            }
            b"Td" | b"TD" => {
                if let Some(values) = self.numbers().filter(|values| values.len() >= 2) {
                    self.text.line_matrix.translate(values[0], values[1]);
                    self.text.matrix = self.text.line_matrix;
                }
            }
            b"T*" => self.text.matrix = self.text.line_matrix,
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                let payload = self.text_show_payload(operator);
                if payload.is_empty() {
                    return;
                }
                let (strong, repeat_eligible) =
                    self.show_metrics().map_or((false, false), |(size, axis)| {
                        let diagonal = axis + LARGE_DIAGONAL_TEXT_ANGLE_EPSILON_DEGREES
                            >= LARGE_DIAGONAL_TEXT_MIN_AXIS_ANGLE_DEGREES;
                        (
                            diagonal
                                && size + LARGE_DIAGONAL_TEXT_SIZE_EPSILON_PT
                                    >= LARGE_DIAGONAL_TEXT_STRONG_MIN_EFFECTIVE_SIZE_PT,
                            diagonal
                                && size + LARGE_DIAGONAL_TEXT_SIZE_EPSILON_PT
                                    >= LARGE_DIAGONAL_TEXT_REPEAT_MIN_EFFECTIVE_SIZE_PT,
                        )
                    });
                if let Some(object) = self.object.as_mut() {
                    object.saw_text = true;
                    object.all_strong_qualify &= strong;
                    object.all_repeat_qualify &= repeat_eligible;
                    object.payload.extend_from_slice(&payload);
                }
            }
            _ => {}
        }
    }
}

impl ObjectHandleParserCallbacks for LargeDiagonalTextScanner {
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
            self.operands.push(OperandSpan {
                object: OperandObject::Scalar(scalar),
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
        let span_start = self
            .operands
            .first()
            .map_or(offset, |operand| operand.offset);
        let span_end = offset.saturating_add(length);
        self.apply_operator(operator, span_start, span_end);
        self.operands.clear();
        Ok(ParseControl::Continue)
    }

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
        } else if object.as_inline_image().is_none() {
            self.operands.push(OperandSpan {
                object: OperandObject::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

struct OptionalContentState {
    off: BTreeSet<ObjectKey>,
    on: BTreeSet<ObjectKey>,
    base_off: bool,
}

#[cfg(test)]
#[expect(
    dead_code,
    reason = "retained only until final flpdf/Hayro hidden-text parity sweep"
)]
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

fn replace_page_content_hayro(
    document: &mut EditDocument,
    page: CowObjectHandle,
    decoded: Vec<u8>,
) -> Result<()> {
    let stream = CowObjectHandle::New(document.overlay_mut().add(OwnedObject::Stream {
        dictionary: OwnedDictionary::new(),
        data: crate::StreamData::Owned(decoded),
    }));
    let object = match page {
        CowObjectHandle::Existing(id) => document.edit_object(id)?,
        CowObjectHandle::New(id) => document
            .overlay_mut()
            .added_mut(id)
            .ok_or_else(|| crate::Error::MissingNewObject { index: id.index() })?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Contents".to_vec(), OwnedObject::Reference(stream));
    }
    Ok(())
}

pub(crate) fn analyze_hidden_text_hayro(document: &EditDocument) -> Result<Vec<HiddenTextFinding>> {
    let ocg = optional_content_state_hayro(document)?;
    let pages = document.page_handles()?;
    let mut findings = Vec::new();
    for (index, page) in pages.into_iter().enumerate() {
        let scan = scan_page_hayro(document, page, index + 1, &ocg)?;
        findings.extend(scan.findings.into_iter().map(|finding| finding.public));
    }
    Ok(findings)
}

pub(crate) fn apply_hidden_text_policy_hayro(
    document: &mut EditDocument,
    policy: &HiddenTextPolicy,
) -> Result<HiddenTextApplyStats> {
    if policy.remove_categories.is_empty() && policy.overrides.is_empty() {
        return Ok(HiddenTextApplyStats::default());
    }
    let ocg = optional_content_state_hayro(document)?;
    let pages = document.page_handles()?;
    let mut stats = HiddenTextApplyStats::default();
    for (index, page) in pages.into_iter().enumerate() {
        let mut scan = scan_page_hayro(document, page, index + 1, &ocg)?;
        scan.findings
            .retain(|finding| policy.should_remove(&finding.public));
        if scan.findings.is_empty() {
            continue;
        }
        let decoded = page_content_bytes_hayro(document, page)?;
        let selected: BTreeSet<String> = scan
            .findings
            .iter()
            .map(|finding| finding.public.id.clone())
            .collect();
        let mut ranges: Vec<(usize, usize)> = scan_page_hayro(document, page, index + 1, &ocg)?
            .findings
            .into_iter()
            .filter(|finding| selected.contains(&finding.public.id))
            .map(|finding| (finding.span_start, finding.span_end))
            .collect();
        if ranges.is_empty() {
            continue;
        }
        ranges.sort_unstable();
        stats.removed += ranges.len();
        replace_page_content_hayro(document, page, remove_ranges(&decoded, &ranges))?;
    }
    Ok(stats)
}

/// Remove self-contained large diagonal `BT..ET` text objects.
///
/// This is intentionally opt-in: it targets watermark/stamp-style text while
/// preserving horizontal/vertical page text and mixed text objects.
fn pdf_token_delimiter(byte: u8) -> bool {
    matches!(
        byte,
        0 | b'\t'
            | b'\n'
            | 0x0c
            | b'\r'
            | b' '
            | b'('
            | b')'
            | b'<'
            | b'>'
            | b'['
            | b']'
            | b'{'
            | b'}'
            | b'/'
            | b'%'
    )
}

fn previous_pdf_number(input: &[u8], mut end: usize) -> Option<(f64, usize)> {
    while end > 0 && matches!(input[end - 1], 0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ') {
        end -= 1;
    }
    if end == 0 {
        return None;
    }
    // Anything other than plain whitespace between an operator and its matrix
    // operands is deliberately treated as ambiguous by the fast prefilter.
    if pdf_token_delimiter(input[end - 1]) && !matches!(input[end - 1], b'+' | b'-' | b'.') {
        return None;
    }
    let mut start = end;
    while start > 0 && !pdf_token_delimiter(input[start - 1]) {
        start -= 1;
    }
    let token = std::str::from_utf8(&input[start..end]).ok()?;
    Some((token.parse::<f64>().ok()?, start))
}

/// Cheap necessary-condition test for diagonal text.
///
/// A diagonal text baseline requires at least one non-axis-aligned `cm` or
/// `Tm` linear transform. Exact horizontal/vertical/reflected matrices are
/// closed under composition, so a page containing only those matrices cannot
/// produce the diagonal watermark geometry targeted by this pass. Any lexical
/// ambiguity returns `true` and falls back to the full content parser.
fn may_contain_diagonal_text_transform(input: &[u8]) -> bool {
    let mut index = 0usize;
    while index + 1 < input.len() {
        let operator = &input[index..index + 2];
        if operator != b"cm" && operator != b"Tm" {
            index += 1;
            continue;
        }
        let before_ok = index == 0 || pdf_token_delimiter(input[index - 1]);
        let after_ok = index + 2 == input.len() || pdf_token_delimiter(input[index + 2]);
        if !before_ok || !after_ok {
            index += 2;
            continue;
        }
        let mut cursor = index;
        let mut reversed = [0.0_f64; 6];
        for value in &mut reversed {
            let Some((number, start)) = previous_pdf_number(input, cursor) else {
                return true;
            };
            *value = number;
            cursor = start;
        }
        let a = reversed[5];
        let b = reversed[4];
        let c = reversed[3];
        let d = reversed[2];
        if ![a, b, c, d].into_iter().all(f64::is_finite) {
            return true;
        }
        let axis_aligned = (b == 0.0 && c == 0.0) || (a == 0.0 && d == 0.0);
        if !axis_aligned {
            return true;
        }
        index += 2;
    }
    false
}

pub(crate) fn remove_large_diagonal_text_hayro(
    document: &mut EditDocument,
) -> Result<HiddenTextApplyStats> {
    let pages = document.page_handles()?;
    let mut stats = HiddenTextApplyStats::default();
    for page in pages {
        let decoded = page_content_bytes_hayro(document, page)?;
        if !may_contain_diagonal_text_transform(&decoded) {
            continue;
        }
        let mut scanner = LargeDiagonalTextScanner::default();
        flpdf::parse_detached_content_stream(
            &decoded,
            "large diagonal text removal",
            &mut scanner,
        )?;
        let mut ranges = scanner.selected_ranges();
        if ranges.is_empty() {
            continue;
        }
        ranges.sort_unstable();
        stats.removed = stats.removed.saturating_add(ranges.len());
        replace_page_content_hayro(document, page, remove_ranges(&decoded, &ranges))?;
    }
    Ok(stats)
}

/// Remove text paint that is physically absent from the default appearance.
///
/// This intentionally preserves semantic OCR/accessibility layers even when
/// they are visually hidden. Zero-opacity text is physically absent; occlusion
/// findings are removed only when the analyzer itself classifies them as safe
/// to remove, because approximate glyph bounds are not a proof of invisibility.
pub(crate) fn prune_physically_hidden_text_hayro(
    document: &mut EditDocument,
    candidate_pages: Option<&BTreeSet<CowObjectHandle>>,
) -> Result<HiddenTextApplyStats> {
    let ocg = optional_content_state_hayro(document)?;
    let pages = document.page_handles()?;
    let mut stats = HiddenTextApplyStats::default();
    for (index, page) in pages.into_iter().enumerate() {
        if candidate_pages.is_some_and(|pages| !pages.contains(&page)) {
            continue;
        }
        let scan = scan_page_hayro(document, page, index + 1, &ocg)?;
        let ranges = physical_hidden_ranges(scan);
        if ranges.is_empty() {
            continue;
        }
        let decoded = page_content_bytes_hayro(document, page)?;
        stats.removed += ranges.len();
        replace_page_content_hayro(document, page, remove_ranges(&decoded, &ranges))?;
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

fn resolved_dictionary_hayro(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|value| value.as_dictionary().cloned()))
}

fn resolved_array_hayro(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Vec<OwnedObject>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Array(values)) => values,
        _ => Vec::new(),
    })
}

fn resolved_name_hayro(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Vec<u8>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => name,
        _ => Vec::new(),
    })
}

fn resolved_u32_hayro(document: &EditDocument, value: &OwnedObject) -> Result<Option<u32>> {
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => u32::try_from(value).ok(),
        _ => None,
    })
}

fn build_fonts_hayro(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, FontInfo>> {
    let Some(fonts) = resolved_dictionary_hayro(document, resources.get(b"Font".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (key, value) in &fonts {
        let Some(font) = resolved_dictionary_hayro(document, Some(value))? else {
            continue;
        };
        out.insert(key.clone(), font_info_hayro(document, &font)?);
    }
    Ok(out)
}

fn font_info_hayro(document: &EditDocument, font: &OwnedDictionary) -> Result<FontInfo> {
    let mut info = FontInfo::default();
    let subtype = resolved_name_hayro(document, font.get(b"Subtype".as_slice()))?;
    let encoding = resolved_name_hayro(document, font.get(b"Encoding".as_slice()))?;
    info.identity_two_byte =
        subtype == b"Type0" && matches!(encoding.as_slice(), b"Identity-H" | b"Identity-V");
    if info.identity_two_byte {
        info.max_code_bytes = 2;
    }

    if let Some(to_unicode) = font.get(b"ToUnicode".as_slice())
        && let Some(stream) = document.resolve_owned_value(to_unicode)?
        && matches!(stream, OwnedObject::Stream { .. })
        && let Ok(data) = document.decoded_owned_stream_data(&stream, DecodeLevel::Generalized)
    {
        info.unicode = parse_to_unicode(&data);
        if let Some(max) = info.unicode.keys().map(Vec::len).max() {
            info.max_code_bytes = info.max_code_bytes.max(max);
        }
    }

    if subtype == b"Type0" {
        let descendants = resolved_array_hayro(document, font.get(b"DescendantFonts".as_slice()))?;
        if let Some(descendant) = descendants.first()
            && let Some(descendant) = resolved_dictionary_hayro(document, Some(descendant))?
        {
            if let Some(dw) = descendant.get(b"DW".as_slice())
                && let Some(value) = owned_number_value(document, dw)?
            {
                info.default_width = value;
            } else {
                info.default_width = 1000.0;
            }
            if let Some(widths) = descendant.get(b"W".as_slice()) {
                parse_cid_widths_hayro(document, widths, &mut info.widths)?;
            }
        }
    } else {
        let first = match font.get(b"FirstChar".as_slice()) {
            Some(value) => resolved_u32_hayro(document, value)?.unwrap_or(0),
            None => 0,
        };
        let widths = resolved_array_hayro(document, font.get(b"Widths".as_slice()))?;
        for (offset, width) in widths.iter().enumerate() {
            if let Some(width) = owned_number_value(document, width)? {
                info.widths.insert(
                    first.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX)),
                    width,
                );
            }
        }
    }
    Ok(info)
}

fn parse_cid_widths_hayro(
    document: &EditDocument,
    widths: &OwnedObject,
    out: &mut HashMap<u32, f64>,
) -> Result<()> {
    let items = resolved_array_hayro(document, Some(widths))?;
    let mut index = 0;
    while index < items.len() {
        let Some(start) = resolved_u32_hayro(document, &items[index])? else {
            index += 1;
            continue;
        };
        index += 1;
        let Some(next) = items.get(index) else {
            break;
        };
        let array = resolved_array_hayro(document, Some(next))?;
        if !array.is_empty() {
            for (offset, width) in array.iter().enumerate() {
                if let Some(width) = owned_number_value(document, width)? {
                    out.insert(
                        start.saturating_add(u32::try_from(offset).unwrap_or(u32::MAX)),
                        width,
                    );
                }
            }
            index += 1;
            continue;
        }
        if let Some(end) = resolved_u32_hayro(document, next)? {
            index += 1;
            let Some(width) = items.get(index) else {
                break;
            };
            if let Some(width) = owned_number_value(document, width)? {
                for code in start..=end.min(start.saturating_add(65_535)) {
                    out.insert(code, width);
                }
            }
            index += 1;
        } else {
            index += 1;
        }
    }
    Ok(())
}

fn build_ext_gstates_hayro(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ExtGStateInfo>> {
    let Some(states) = resolved_dictionary_hayro(document, resources.get(b"ExtGState".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (key, value) in &states {
        let Some(state) = resolved_dictionary_hayro(document, Some(value))? else {
            continue;
        };
        let fill_alpha = match state.get(b"ca".as_slice()) {
            Some(value) => owned_number_value(document, value)?,
            None => None,
        };
        let stroke_alpha = match state.get(b"CA".as_slice()) {
            Some(value) => owned_number_value(document, value)?,
            None => None,
        };
        let normal_blend = match state.get(b"BM".as_slice()) {
            None => true,
            Some(value) => match document.resolve_owned_value(value)? {
                None | Some(OwnedObject::Null) => true,
                Some(OwnedObject::Name(name)) => name == b"Normal",
                _ => false,
            },
        };
        out.insert(
            key.clone(),
            ExtGStateInfo {
                fill_alpha,
                stroke_alpha,
                normal_blend,
            },
        );
    }
    Ok(out)
}

fn build_images_hayro(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, bool>> {
    let Some(xobjects) = resolved_dictionary_hayro(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (key, value) in &xobjects {
        let Some(object) = document.resolve_owned_value(value)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = object else {
            out.insert(key.clone(), false);
            continue;
        };
        let is_image =
            resolved_name_hayro(document, dictionary.get(b"Subtype".as_slice()))? == b"Image";
        let unmasked = is_image
            && !dictionary.contains_key(b"SMask".as_slice())
            && !dictionary.contains_key(b"Mask".as_slice());
        out.insert(key.clone(), unmasked);
    }
    Ok(out)
}

fn build_properties_hayro(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, Option<ObjectKey>>> {
    let Some(value) = resources.get(b"Properties".as_slice()) else {
        return Ok(BTreeMap::new());
    };
    let Some(properties) = document.resolve_owned_value(value)? else {
        return Ok(BTreeMap::new());
    };
    let Some(properties) = properties.as_dictionary() else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (key, value) in properties {
        let object_key = match value {
            OwnedObject::Reference(handle) => cow_object_key(*handle),
            _ => None,
        };
        out.insert(key.clone(), object_key);
    }
    Ok(out)
}

fn build_resources_hayro(
    document: &EditDocument,
    resources_value: Option<OwnedObject>,
    _ocg: &OptionalContentState,
) -> Result<Resources> {
    let resources = match resources_value {
        Some(value) => document.resolve_owned_value(&value)?,
        None => None,
    };
    let dictionary = resources
        .as_ref()
        .and_then(OwnedObject::as_dictionary)
        .cloned()
        .unwrap_or_default();
    Ok(Resources {
        fonts: build_fonts_hayro(document, &dictionary)?,
        ext_gstates: build_ext_gstates_hayro(document, &dictionary)?,
        images: build_images_hayro(document, &dictionary)?,
        properties: build_properties_hayro(document, &dictionary)?,
    })
}

fn optional_content_state_hayro(document: &EditDocument) -> Result<OptionalContentState> {
    let mut out = OptionalContentState {
        off: BTreeSet::new(),
        on: BTreeSet::new(),
        base_off: false,
    };
    let catalog = CowObjectHandle::Existing(document.source().catalog_id());
    let Some(catalog) = document.current_owned_object(catalog)? else {
        return Ok(out);
    };
    let Some(catalog) = catalog.as_dictionary() else {
        return Ok(out);
    };
    let Some(ocp) = catalog.get(b"OCProperties".as_slice()) else {
        return Ok(out);
    };
    let Some(ocp) = document.resolve_owned_value(ocp)? else {
        return Ok(out);
    };
    let Some(ocp) = ocp.as_dictionary() else {
        return Ok(out);
    };
    let Some(default) = ocp.get(b"D".as_slice()) else {
        return Ok(out);
    };
    let Some(default) = document.resolve_owned_value(default)? else {
        return Ok(out);
    };
    let Some(default) = default.as_dictionary() else {
        return Ok(out);
    };
    if let Some(base) = default.get(b"BaseState".as_slice()) {
        out.base_off = matches!(document.resolve_owned_value(base)?, Some(OwnedObject::Name(name)) if name == b"OFF");
    }
    for (key, target) in [
        (b"OFF".as_slice(), &mut out.off),
        (b"ON".as_slice(), &mut out.on),
    ] {
        let Some(value) = default.get(key) else {
            continue;
        };
        let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
            continue;
        };
        for value in values {
            if let OwnedObject::Reference(handle) = value
                && let Some(key) = cow_object_key(handle)
            {
                target.insert(key);
            }
        }
    }
    Ok(out)
}

fn decoded_content_value_hayro(
    document: &EditDocument,
    value: &OwnedObject,
    output: &mut Vec<u8>,
) -> Result<()> {
    let value = match value {
        OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
            value
        }
        value => value.clone(),
    };
    match value {
        OwnedObject::Stream { .. } => {
            let bytes = document.decoded_owned_stream_data(&value, DecodeLevel::Specialized)?;
            if !output.is_empty() && output.last() != Some(&b'\n') {
                output.push(b'\n');
            }
            output.extend_from_slice(&bytes);
        }
        OwnedObject::Array(values) => {
            for value in values {
                decoded_content_value_hayro(document, &value, output)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn page_content_bytes_hayro(document: &EditDocument, page: CowObjectHandle) -> Result<Vec<u8>> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(Vec::new());
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    decoded_content_value_hayro(document, contents, &mut output)?;
    Ok(output)
}

fn owned_number_value(document: &EditDocument, value: &OwnedObject) -> Result<Option<f64>> {
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => Some(value as f64),
        Some(OwnedObject::Real(value)) => Some(value),
        _ => None,
    })
}

fn owned_number_array<const N: usize>(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<[f64; N]>> {
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != N {
        return Ok(None);
    }
    let mut out = [0.0; N];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = owned_number_value(document, value)? else {
            return Ok(None);
        };
        out[index] = number;
    }
    Ok(Some(out))
}

fn page_crop_hayro(document: &EditDocument, page: CowObjectHandle) -> Result<Rect> {
    for key in [b"CropBox".as_slice(), b"MediaBox".as_slice()] {
        if let Some(value) = document.inherited_page_value(page, key)?
            && let Some(values) = owned_number_array::<4>(document, &value)?
        {
            return Ok(Rect::new(values[0], values[1], values[2], values[3]));
        }
    }
    Ok(Rect::new(0.0, 0.0, 612.0, 792.0))
}

pub(crate) struct HiddenTextSharedContext {
    ocg: OptionalContentState,
}

pub(crate) fn hidden_text_shared_context_hayro(
    document: &EditDocument,
) -> Result<HiddenTextSharedContext> {
    Ok(HiddenTextSharedContext {
        ocg: optional_content_state_hayro(document)?,
    })
}

struct TeeCallbacks<'a, A, B> {
    first: &'a mut A,
    second: &'a mut B,
}

impl<A, B> ObjectHandleParserCallbacks for TeeCallbacks<'_, A, B>
where
    A: ObjectHandleParserCallbacks,
    B: ObjectHandleParserCallbacks,
{
    const HANDLES_CONTENT_SCALARS: bool = A::HANDLES_CONTENT_SCALARS && B::HANDLES_CONTENT_SCALARS;

    fn content_size(&mut self, size: usize) -> flpdf::Result<()> {
        self.first.content_size(size)?;
        self.second.content_size(size)
    }

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        debug_assert!(Self::HANDLES_CONTENT_SCALARS);
        let first = self.first.handle_scalar(scalar.clone(), offset, length)?;
        let second = self.second.handle_scalar(scalar, offset, length)?;
        if matches!(first, ParseControl::Stop) || matches!(second, ParseControl::Stop) {
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
        let first = self.first.handle_operator(operator, offset, length)?;
        let second = self.second.handle_operator(operator, offset, length)?;
        if matches!(first, ParseControl::Stop) || matches!(second, ParseControl::Stop) {
            Ok(ParseControl::Stop)
        } else {
            Ok(ParseControl::Continue)
        }
    }

    fn handle_object(
        &mut self,
        object: ObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        let first = self.first.handle_object(object.clone(), offset, length)?;
        let second = self.second.handle_object(object, offset, length)?;
        Ok(
            if matches!(first, ParseControl::Stop) || matches!(second, ParseControl::Stop) {
                ParseControl::Stop
            } else {
                ParseControl::Continue
            },
        )
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        self.first.handle_eof()?;
        self.second.handle_eof()
    }
}

fn should_prune_physically_hidden(finding: &HiddenTextFinding) -> bool {
    if matches!(
        finding.category,
        HiddenTextCategory::OcrOverlay | HiddenTextCategory::Accessibility
    ) {
        return false;
    }

    match finding.mechanism {
        // Alpha-zero text is physically absent regardless of approximate glyph
        // geometry, so it is safe to remove when it is not a semantic layer.
        HiddenTextMechanism::ZeroOpacity => true,
        // Coverage is inferred from approximate text bounds. Respect the
        // analyzer's conservative classification instead of deleting ambiguous
        // occlusion findings that it explicitly recommends keeping.
        HiddenTextMechanism::CoveredByOpaqueFill | HiddenTextMechanism::CoveredByImage => {
            finding.suggested_action == HiddenTextAction::Remove
        }
        _ => false,
    }
}

fn physical_hidden_ranges(scan: PageScan) -> Vec<(usize, usize)> {
    let mut ranges = scan
        .findings
        .into_iter()
        .filter(|finding| should_prune_physically_hidden(&finding.public))
        .map(|finding| (finding.span_start, finding.span_end))
        .collect::<Vec<_>>();
    ranges.sort_unstable();
    ranges
}

pub(crate) fn scan_physical_hidden_text_with_callback_hayro<C>(
    document: &EditDocument,
    page: CowObjectHandle,
    page_number: usize,
    context: &HiddenTextSharedContext,
    content: &[u8],
    other: &mut C,
) -> Result<Vec<(usize, usize)>>
where
    C: ObjectHandleParserCallbacks,
{
    let crop = page_crop_hayro(document, page)?;
    let resources = build_resources_hayro(
        document,
        document.inherited_page_value(page, b"Resources")?,
        &context.ocg,
    )?;
    let mut scanner = PageScanner::new(
        page_number,
        crop,
        &resources,
        &context.ocg.off,
        context.ocg.base_off,
        &context.ocg.on,
    );
    let mut tee = TeeCallbacks {
        first: other,
        second: &mut scanner,
    };
    flpdf::parse_detached_content_stream(
        content,
        "shared raster/hidden-text page content",
        &mut tee,
    )?;
    Ok(physical_hidden_ranges(scanner.finish()))
}

fn scan_page_hayro(
    document: &EditDocument,
    page: CowObjectHandle,
    page_number: usize,
    ocg: &OptionalContentState,
) -> Result<PageScan> {
    let crop = page_crop_hayro(document, page)?;
    let resources = build_resources_hayro(
        document,
        document.inherited_page_value(page, b"Resources")?,
        ocg,
    )?;
    let content = page_content_bytes_hayro(document, page)?;
    let mut scanner = PageScanner::new(
        page_number,
        crop,
        &resources,
        &ocg.off,
        ocg.base_off,
        &ocg.on,
    );
    flpdf::parse_detached_content_stream(&content, "Hayro/COW page content", &mut scanner)?;
    Ok(scanner.finish())
}

#[cfg(test)]
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

#[cfg(test)]
fn build_resources(resources: &ObjectHandle, ocg: &OptionalContentState) -> Result<Resources> {
    Ok(Resources {
        fonts: build_fonts(resources)?,
        ext_gstates: build_ext_gstates(resources)?,
        images: build_images(resources)?,
        properties: build_properties(resources, ocg)?,
    })
}

#[cfg(test)]
fn resource_dictionary(resources: &ObjectHandle, key: &[u8]) -> Result<Option<ObjectHandle>> {
    let value = resources.try_get_key(key)?;
    if value.try_is_dictionary()? {
        Ok(Some(value))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
fn normalized_resource_key(key: &[u8]) -> Vec<u8> {
    key.strip_prefix(b"/").unwrap_or(key).to_vec()
}

#[cfg(test)]
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

#[cfg(test)]
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
        let descendants = font.try_get_key(b"/DescendantFonts")?;
        let descendants = if descendants.try_is_array()? {
            descendants.try_get_array_as_vector()?
        } else {
            Vec::new()
        };
        if let Some(descendant) = descendants.first() {
            let dw = descendant.try_get_key(b"/DW")?;
            if dw.try_is_number()? {
                info.default_width = dw.try_get_numeric_value()?;
            } else {
                info.default_width = 1000.0;
            }
            let widths = descendant.try_get_key(b"/W")?;
            if widths.try_is_array()? {
                parse_cid_widths(&widths, &mut info.widths)?;
            }
        }
    } else {
        let first = font.try_get_key(b"/FirstChar")?;
        let first = if first.try_is_integer()? {
            first.try_get_int_value()?.max(0) as u32
        } else {
            0
        };
        let widths = font.try_get_key(b"/Widths")?;
        let widths = if widths.try_is_array()? {
            widths.try_get_array_as_vector()?
        } else {
            Vec::new()
        };
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
fn build_properties(
    resources: &ObjectHandle,
    _ocg: &OptionalContentState,
) -> Result<BTreeMap<Vec<u8>, Option<ObjectKey>>> {
    let Some(properties) = resource_dictionary(resources, b"/Properties")? else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for key in properties.try_get_keys()? {
        let value = properties.try_get_key(&key)?;
        out.insert(
            normalized_resource_key(&key),
            value.object_ref().map(flpdf_object_key),
        );
    }
    Ok(out)
}

#[cfg(test)]
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
                target.insert(flpdf_object_key(object_ref));
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

    fn large_diagonal_ranges(input: &[u8]) -> Vec<(usize, usize)> {
        let mut scanner = LargeDiagonalTextScanner::default();
        assert!(
            flpdf::parse_detached_content_stream(input, "large diagonal text test", &mut scanner)
                .is_ok()
        );
        scanner.selected_ranges()
    }

    #[test]
    fn diagonal_prefilter_skips_axis_aligned_matrices() {
        assert!(!may_contain_diagonal_text_transform(
            b"q 1 0 0 1 10 20 cm BT 1 0 0 1 30 40 Tm (x) Tj ET Q"
        ));
        assert!(!may_contain_diagonal_text_transform(
            b"BT 0 1 -1 0 30 40 Tm (vertical) Tj ET"
        ));
    }

    #[test]
    fn diagonal_prefilter_keeps_rotated_or_ambiguous_content() {
        assert!(may_contain_diagonal_text_transform(
            b"BT 0.93969 0.34202 -0.34202 0.93969 0 0 Tm (stamp) Tj ET"
        ));
        assert!(may_contain_diagonal_text_transform(
            b"BT weird Tm (x) Tj ET"
        ));
    }

    #[test]
    fn large_diagonal_text_scanner_removes_watermark_like_text_object() {
        let input =
            b"q 0.707106 0.707106 -0.707106 0.707106 0 0 cm BT /F1 48 Tf (PRELIMINARY) Tj ET Q";
        let ranges = large_diagonal_ranges(input);
        assert_eq!(ranges.len(), 1);
        assert_eq!(
            &input[ranges[0].0..ranges[0].1],
            b"BT /F1 48 Tf (PRELIMINARY) Tj ET"
        );
    }

    #[test]
    fn large_diagonal_text_scanner_removes_repeated_medium_watermark() {
        let input = b"BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 0 0 Tm (user timestamp) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 20 20 Tm (user timestamp) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 40 40 Tm (user timestamp) Tj ET";
        assert_eq!(large_diagonal_ranges(input).len(), 3);
    }

    #[test]
    fn large_diagonal_text_scanner_removes_dense_medium_tiled_page() {
        let input = b"BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 0 0 Tm (tile01) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 10 10 Tm (tile02) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 20 20 Tm (tile03) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 30 30 Tm (tile04) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 40 40 Tm (tile05) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 50 50 Tm (tile06) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 60 60 Tm (tile07) Tj ET \
BT /F1 20 Tf 0.93969 0.34202 -0.34202 0.93969 70 70 Tm (tile08) Tj ET";
        assert_eq!(large_diagonal_ranges(input).len(), 8);
    }

    #[test]
    fn large_diagonal_text_scanner_keeps_single_medium_diagonal_label() {
        let input = b"BT /F1 23.4 Tf 0.866025 0.5 -0.5 0.866025 0 0 Tm (C/NO:) Tj ET";
        assert!(large_diagonal_ranges(input).is_empty());
    }

    #[test]
    fn large_diagonal_text_scanner_keeps_large_horizontal_and_vertical_text() {
        let horizontal = b"BT /F1 72 Tf 1 0 0 1 0 0 Tm (TITLE) Tj ET";
        let vertical = b"BT /F1 72 Tf 0 1 -1 0 0 0 Tm (SIDE) Tj ET";
        assert!(large_diagonal_ranges(horizontal).is_empty());
        assert!(large_diagonal_ranges(vertical).is_empty());
    }

    #[test]
    fn large_diagonal_text_scanner_keeps_small_diagonal_text() {
        let input = b"BT /F1 12 Tf 0.707106 0.707106 -0.707106 0.707106 0 0 Tm (label) Tj ET";
        assert!(large_diagonal_ranges(input).is_empty());
    }

    #[test]
    fn large_diagonal_text_scanner_keeps_mixed_text_object() {
        let input = b"BT /F1 48 Tf 0.707106 0.707106 -0.707106 0.707106 0 0 Tm (DRAFT) Tj 1 0 0 1 0 0 Tm (keep) Tj ET";
        assert!(large_diagonal_ranges(input).is_empty());
    }

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
    fn physical_prune_respects_ambiguous_occlusion_policy() {
        let ambiguous = finish_events(
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
        assert_eq!(ambiguous[0].suggested_action, HiddenTextAction::Keep);
        assert!(!should_prune_physically_hidden(&ambiguous[0]));

        let redaction = finish_events(
            vec![event("secret", 1, Rect::new(10.0, 10.0, 20.0, 20.0), None)],
            vec![PaintEvent {
                order: 2,
                bounds: Rect::new(9.0, 9.0, 21.0, 21.0),
                kind: PaintKind::FillRect { dark: true },
            }],
        );
        assert_eq!(redaction[0].suggested_action, HiddenTextAction::Remove);
        assert!(should_prune_physically_hidden(&redaction[0]));

        let zero_opacity = finish_events(
            vec![event(
                "alpha-zero",
                1,
                Rect::new(10.0, 10.0, 20.0, 20.0),
                Some(HiddenTextMechanism::ZeroOpacity),
            )],
            Vec::new(),
        );
        assert!(should_prune_physically_hidden(&zero_opacity[0]));
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
