//! Conservative page-level representation classification for corpus routing.
//!
//! This intentionally answers an optimization question rather than trying to
//! infer document provenance. A page is `Raster` only when page-covering image
//! paint can be proven and there is no visible native paint. Ambiguous paint,
//! complex clipping, inline images, `ExtGState` soft masks, or unsupported content
//! are kept out of the raster bucket; masks owned by an image stay raster semantics.

use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result,
    content::{form_content, form_resources, resolved_dictionary, resolved_number},
    content_stream::{instruction_operands, operand_numbers},
    geometry::{Matrix, Rect},
    report::{DocumentRepresentation, PageRepresentation},
};
use std::collections::{BTreeMap, HashSet};

const RASTER_PAGE_COVERAGE: f64 = 0.98;
const MIXED_PAGE_COVERAGE: f64 = 0.50;
const MATRIX_EPSILON: f64 = 1.0e-7;
const ALPHA_INVISIBLE: f64 = 0.001;
const MAX_COVERAGE_RECTS: usize = 4096;

#[derive(Debug, Clone, Default)]
pub struct PageRepresentationSummary {
    pub document: DocumentRepresentation,
    pub raster_pages: usize,
    pub native_pages: usize,
    pub mixed_pages: usize,
    pub blank_pages: usize,
    pub unknown_pages: usize,
    pub unknown_reasons: BTreeMap<&'static str, usize>,
}

#[derive(Debug)]
pub struct PageObservation {
    representation: PageRepresentation,
    unknown_reasons: BTreeMap<&'static str, usize>,
    incomplete_without_reason: bool,
}

impl PageRepresentationSummary {
    pub fn record(&mut self, observation: PageObservation) {
        for (reason, count) in observation.unknown_reasons {
            *self.unknown_reasons.entry(reason).or_default() += count;
        }
        if observation.incomplete_without_reason {
            *self
                .unknown_reasons
                .entry("incomplete-state-or-parse")
                .or_default() += 1;
        }
        match observation.representation {
            PageRepresentation::Raster => self.raster_pages = self.raster_pages.saturating_add(1),
            PageRepresentation::Native => self.native_pages = self.native_pages.saturating_add(1),
            PageRepresentation::Mixed => self.mixed_pages = self.mixed_pages.saturating_add(1),
            PageRepresentation::Blank => self.blank_pages = self.blank_pages.saturating_add(1),
            PageRepresentation::Unknown => {
                self.unknown_pages = self.unknown_pages.saturating_add(1);
            }
        }
    }

    pub fn record_unknown_page(&mut self) {
        self.unknown_pages = self.unknown_pages.saturating_add(1);
        *self.unknown_reasons.entry("missing-page-box").or_default() += 1;
    }

    pub const fn finish_document(&mut self) {
        self.document = classify_document(self);
    }
}

#[derive(Debug, Clone, Copy)]
struct GraphicsState {
    ctm: Matrix,
    fill_alpha: f64,
    stroke_alpha: f64,
    text_render_mode: u8,
    clip: Option<Rect>,
    clip_empty: bool,
    coverage_reliable: bool,
    soft_mask: bool,
}

impl GraphicsState {
    fn new(page: Rect) -> Self {
        Self {
            ctm: Matrix::default(),
            fill_alpha: 1.0,
            stroke_alpha: 1.0,
            text_render_mode: 0,
            clip: Some(page),
            clip_empty: false,
            coverage_reliable: true,
            soft_mask: false,
        }
    }
}

#[derive(Debug, Default)]
struct PagePaintFacts {
    image_rects: Vec<Rect>,
    image_paints: usize,
    visible_native_paints: usize,
    hidden_text_paints: usize,
    unknown_paints: usize,
    unknown_reasons: BTreeMap<&'static str, usize>,
    complete: bool,
}

impl PagePaintFacts {
    fn new() -> Self {
        Self {
            complete: true,
            ..Self::default()
        }
    }

    fn mark_unknown(&mut self, reason: &'static str) {
        self.unknown_paints = self.unknown_paints.saturating_add(1);
        *self.unknown_reasons.entry(reason).or_default() += 1;
    }

    const fn has_any_paint(&self) -> bool {
        self.image_paints > 0
            || self.visible_native_paints > 0
            || self.hidden_text_paints > 0
            || self.unknown_paints > 0
    }
}

pub struct PageClassifier<'a> {
    document: &'a EditDocument,
    page: Rect,
    facts: PagePaintFacts,
    state: GraphicsState,
    stack: Vec<GraphicsState>,
    active_forms: HashSet<ObjectHandle>,
    compatibility_depth: usize,
    path_rect: Option<Rect>,
    path_reliable: bool,
    pending_clip: bool,
}

impl<'a> PageClassifier<'a> {
    fn new(document: &'a EditDocument, page: Rect) -> Self {
        Self {
            document,
            page,
            facts: PagePaintFacts::new(),
            state: GraphicsState::new(page),
            stack: Vec::new(),
            active_forms: HashSet::new(),
            compatibility_depth: 0,
            path_rect: None,
            path_reliable: true,
            pending_clip: false,
        }
    }

    fn scan_content(&mut self, content: &[u8], resources: &OwnedDictionary) -> Result<bool> {
        crate::content_stream::visit_instructions(content, |instruction| {
            let operator = &instruction.operator[..];
            let operands = instruction_operands(content, instruction);
            self.observe_operator(operator, &operands, resources)
        })
    }

    pub fn observe_operator(
        &mut self,
        operator: &[u8],
        operands: &[crate::content_stream::InstructionOperand],
        resources: &OwnedDictionary,
    ) -> Result<()> {
        self.operator(operator, operands, resources)
    }

    pub fn finish(mut self, parse_incomplete: bool) -> PageObservation {
        if parse_incomplete || !self.stack.is_empty() {
            self.facts.complete = false;
        }
        let representation = classify_page(&self.facts, self.page);
        let incomplete_without_reason =
            !self.facts.complete && self.facts.unknown_reasons.is_empty();
        PageObservation {
            representation,
            unknown_reasons: self.facts.unknown_reasons,
            incomplete_without_reason,
        }
    }

    fn operator(
        &mut self,
        operator: &[u8],
        operands: &[crate::content_stream::InstructionOperand],
        resources: &OwnedDictionary,
    ) -> Result<()> {
        if self.handle_path_operator(operator, operands) {
            return Ok(());
        }
        match operator {
            b"q" => self.stack.push(self.state),
            b"Q" => {
                if let Some(state) = self.stack.pop() {
                    self.state = state;
                } else {
                    self.facts.complete = false;
                }
            }
            b"cm" => {
                if operands.len() != 6 {
                    self.facts.complete = false;
                } else if let Some(values) = operand_numbers(operands) {
                    self.state.ctm.concat(Matrix::new(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                    ));
                } else {
                    self.facts.complete = false;
                }
            }
            b"gs" => self.apply_ext_gstate(operands, resources)?,
            b"Tr" => {
                let Some(value) = operands.first().and_then(|operand| operand.number) else {
                    self.facts.complete = false;
                    return Ok(());
                };
                let rounded = value.round();
                if (value - rounded).abs() > f64::EPSILON || !(0.0..=7.0).contains(&rounded) {
                    self.facts.complete = false;
                } else {
                    self.state.text_render_mode = match rounded {
                        0.0 => 0,
                        1.0 => 1,
                        2.0 => 2,
                        3.0 => 3,
                        4.0 => 4,
                        5.0 => 5,
                        6.0 => 6,
                        7.0 => 7,
                        _ => unreachable!("range was checked above"),
                    };
                }
            }
            b"Do" => self.paint_xobject(operands, resources)?,
            b"BI" => {
                // Hayro deliberately treats the entire inline image as one
                // instruction. Until its dictionary/extent is surfaced here,
                // count it as image paint but keep coverage unproved.
                self.facts.image_paints = self.facts.image_paints.saturating_add(1);
                self.facts.mark_unknown("inline-image");
            }
            b"Tj" | b"TJ" | b"'" | b"\"" => {
                if self.text_is_visible() {
                    self.facts.visible_native_paints =
                        self.facts.visible_native_paints.saturating_add(1);
                } else {
                    self.facts.hidden_text_paints = self.facts.hidden_text_paints.saturating_add(1);
                }
            }
            b"BX" => self.compatibility_depth = self.compatibility_depth.saturating_add(1),
            b"EX" => {
                if self.compatibility_depth == 0 {
                    self.facts.complete = false;
                } else {
                    self.compatibility_depth -= 1;
                }
            }
            // Non-painting standard operators.
            b"w" | b"J" | b"j" | b"M" | b"d" | b"ri" | b"i" | b"g" | b"G" | b"rg" | b"RG"
            | b"k" | b"K" | b"cs" | b"CS" | b"sc" | b"SC" | b"scn" | b"SCN" | b"BT" | b"ET"
            | b"Tc" | b"Tw" | b"Tz" | b"TL" | b"Tf" | b"Ts" | b"Td" | b"TD" | b"Tm" | b"T*"
            | b"MP" | b"DP" | b"BMC" | b"BDC" | b"EMC" => {}
            _ if self.compatibility_depth > 0 => {}
            _ => {
                self.facts.mark_unknown("unknown-operator");
                self.facts.complete = false;
            }
        }
        Ok(())
    }

    fn handle_path_operator(
        &mut self,
        operator: &[u8],
        operands: &[crate::content_stream::InstructionOperand],
    ) -> bool {
        match operator {
            b"re" => self.append_rect_path(operands),
            b"m" | b"l" | b"c" | b"v" | b"y" => {
                self.path_reliable = false;
                self.path_rect = None;
            }
            b"h" => {}
            b"S" | b"s" => {
                self.apply_pending_clip();
                if self.state.stroke_alpha > ALPHA_INVISIBLE {
                    self.facts.visible_native_paints =
                        self.facts.visible_native_paints.saturating_add(1);
                }
                self.clear_path();
            }
            b"f" | b"F" | b"f*" => {
                self.apply_pending_clip();
                if self.state.fill_alpha > ALPHA_INVISIBLE {
                    self.facts.visible_native_paints =
                        self.facts.visible_native_paints.saturating_add(1);
                }
                self.clear_path();
            }
            b"B" | b"B*" | b"b" | b"b*" => {
                self.apply_pending_clip();
                if self.state.fill_alpha > ALPHA_INVISIBLE
                    || self.state.stroke_alpha > ALPHA_INVISIBLE
                {
                    self.facts.visible_native_paints =
                        self.facts.visible_native_paints.saturating_add(1);
                }
                self.clear_path();
            }
            b"sh" => {
                if self.state.fill_alpha > ALPHA_INVISIBLE {
                    self.facts.visible_native_paints =
                        self.facts.visible_native_paints.saturating_add(1);
                }
            }
            b"W" | b"W*" => self.pending_clip = true,
            b"n" => {
                self.apply_pending_clip();
                self.clear_path();
            }
            _ => return false,
        }
        true
    }

    fn append_rect_path(&mut self, operands: &[crate::content_stream::InstructionOperand]) {
        if !self.path_reliable || self.path_rect.is_some() || operands.len() != 4 {
            self.path_reliable = false;
            self.path_rect = None;
            return;
        }
        let Some(values) = operand_numbers(operands) else {
            self.path_reliable = false;
            return;
        };
        let Some(rect) = Rect::from_xywh(values[0], values[1], values[2], values[3]) else {
            self.path_reliable = false;
            return;
        };
        let Some(rect) = axis_aligned_transformed_rect(self.state.ctm, rect) else {
            self.path_reliable = false;
            return;
        };
        self.path_rect = Some(rect);
    }

    fn apply_pending_clip(&mut self) {
        if !self.pending_clip {
            return;
        }
        self.pending_clip = false;
        let Some(path_rect) = self.path_rect else {
            self.state.coverage_reliable = false;
            return;
        };
        if !self.path_reliable || self.state.clip_empty {
            if !self.path_reliable {
                self.state.coverage_reliable = false;
            }
            return;
        }
        self.state.clip = match self.state.clip {
            Some(clip) => {
                let intersection = clip.intersection(path_rect);
                self.state.clip_empty = intersection.is_none();
                intersection
            }
            None => Some(path_rect),
        };
    }

    const fn clear_path(&mut self) {
        self.path_rect = None;
        self.path_reliable = true;
        self.pending_clip = false;
    }

    fn text_is_visible(&self) -> bool {
        match self.state.text_render_mode {
            3 | 7 => false,
            0 | 4 => self.state.fill_alpha > ALPHA_INVISIBLE,
            1 | 5 => self.state.stroke_alpha > ALPHA_INVISIBLE,
            2 | 6 => {
                self.state.fill_alpha > ALPHA_INVISIBLE || self.state.stroke_alpha > ALPHA_INVISIBLE
            }
            _ => true,
        }
    }

    fn apply_ext_gstate(
        &mut self,
        operands: &[crate::content_stream::InstructionOperand],
        resources: &OwnedDictionary,
    ) -> Result<()> {
        let Some(name) = one_name_operand(operands) else {
            self.facts.complete = false;
            return Ok(());
        };
        let Some(ext_gstates) =
            resolved_dictionary(self.document, resources.get(b"ExtGState".as_slice()))?
        else {
            self.facts.complete = false;
            return Ok(());
        };
        let Some(dictionary) =
            resolved_dictionary(self.document, ext_gstates.get(name.as_slice()))?
        else {
            self.facts.complete = false;
            return Ok(());
        };

        if let Some(alpha) = resolved_number(self.document, dictionary.get(b"ca".as_slice()))? {
            if alpha.is_finite() {
                self.state.fill_alpha = alpha.clamp(0.0, 1.0);
            } else {
                self.facts.complete = false;
            }
        }
        if let Some(alpha) = resolved_number(self.document, dictionary.get(b"CA".as_slice()))? {
            if alpha.is_finite() {
                self.state.stroke_alpha = alpha.clamp(0.0, 1.0);
            } else {
                self.facts.complete = false;
            }
        }
        if let Some(value) = dictionary.get(b"SMask".as_slice()) {
            self.state.soft_mask = !resolved_name_is_none(self.document, value)?;
        }
        Ok(())
    }

    fn paint_xobject(
        &mut self,
        operands: &[crate::content_stream::InstructionOperand],
        resources: &OwnedDictionary,
    ) -> Result<()> {
        let Some(name) = one_name_operand(operands) else {
            self.facts.mark_unknown("xobject-resolution");
            self.facts.complete = false;
            return Ok(());
        };
        let Some(xobjects) =
            resolved_dictionary(self.document, resources.get(b"XObject".as_slice()))?
        else {
            self.facts.mark_unknown("xobject-resolution");
            self.facts.complete = false;
            return Ok(());
        };
        let Some(value) = xobjects.get(name.as_slice()) else {
            self.facts.mark_unknown("xobject-resolution");
            self.facts.complete = false;
            return Ok(());
        };
        let OwnedObject::Reference(handle) = value else {
            self.facts.mark_unknown("xobject-resolution");
            self.facts.complete = false;
            return Ok(());
        };

        if self.document.stream_subtype_is(*handle, b"Image")? {
            return self.paint_image(*handle);
        }
        if self.document.stream_subtype_is(*handle, b"Form")? {
            return self.paint_form(*handle, resources);
        }

        self.facts.mark_unknown("xobject-subtype");
        self.facts.complete = false;
        Ok(())
    }

    fn paint_image(&mut self, handle: ObjectHandle) -> Result<()> {
        self.facts.image_paints = self.facts.image_paints.saturating_add(1);
        if self.state.fill_alpha <= ALPHA_INVISIBLE || self.state.clip_empty {
            return Ok(());
        }
        if !self.state.coverage_reliable || self.state.soft_mask {
            self.facts.mark_unknown("coverage-or-soft-mask");
            return Ok(());
        }

        let Some(object) = self.document.current_owned_object(handle)? else {
            self.facts.complete = false;
            return Ok(());
        };
        let Some(dictionary) = object.as_dictionary() else {
            self.facts.complete = false;
            return Ok(());
        };
        // Image-owned masks are still raster semantics: they only modulate the
        // pixels of this image XObject. Their transparency does not make the
        // page native/mixed, so use the base image geometry for routing. This
        // is deliberately different from an ExtGState soft mask, which can be
        // a transparency group affecting arbitrary subsequent native paint.
        if !self.image_mask_semantics_are_raster(dictionary)? {
            self.facts.mark_unknown("image-mask");
            return Ok(());
        }

        let Some(mut rect) = axis_aligned_unit_rect(self.state.ctm) else {
            self.facts.mark_unknown("image-transform");
            return Ok(());
        };
        if let Some(clip) = self.state.clip {
            let Some(clipped) = rect.intersection(clip) else {
                return Ok(());
            };
            rect = clipped;
        }
        let Some(clipped) = rect.intersection(self.page) else {
            return Ok(());
        };
        if self.facts.image_rects.len() >= MAX_COVERAGE_RECTS {
            self.facts.mark_unknown("coverage-rect-cap");
            return Ok(());
        }
        self.facts.image_rects.push(clipped);
        Ok(())
    }

    fn image_mask_semantics_are_raster(&self, dictionary: &OwnedDictionary) -> Result<bool> {
        if let Some(smask) = dictionary.get(b"SMask".as_slice()) {
            match self.document.resolve_owned_value(smask)? {
                None | Some(OwnedObject::Null) => {}
                Some(OwnedObject::Stream { ref dictionary, .. }) => {
                    if !resolved_name_is_image(self.document, dictionary)? {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        if let Some(mask) = dictionary.get(b"Mask".as_slice()) {
            match self.document.resolve_owned_value(mask)? {
                None | Some(OwnedObject::Null | OwnedObject::Array(_)) => {}
                Some(OwnedObject::Stream { ref dictionary, .. }) => {
                    if !resolved_name_is_image(self.document, dictionary)? {
                        return Ok(false);
                    }
                }
                _ => return Ok(false),
            }
        }
        // A stencil image is itself raster content. It uses the current fill
        // color, but its painted geometry is still entirely bitmap-defined.
        Ok(true)
    }

    fn paint_form(
        &mut self,
        handle: ObjectHandle,
        inherited_resources: &OwnedDictionary,
    ) -> Result<()> {
        if !self.active_forms.insert(handle) {
            self.facts.mark_unknown("form-recursion");
            self.facts.complete = false;
            return Ok(());
        }

        let saved_state = self.state;
        let saved_stack = std::mem::take(&mut self.stack);
        let saved_compatibility_depth = self.compatibility_depth;
        let saved_path_rect = self.path_rect;
        let saved_path_reliable = self.path_reliable;
        let saved_pending_clip = self.pending_clip;
        self.compatibility_depth = 0;
        self.clear_path();
        let result = (|| {
            let Some(object) = self.document.current_owned_object(handle)? else {
                self.facts.complete = false;
                return Ok(());
            };
            let Some(dictionary) = object.as_dictionary() else {
                self.facts.complete = false;
                return Ok(());
            };
            if let Some(matrix) =
                resolved_matrix(self.document, dictionary.get(b"Matrix".as_slice()))?
            {
                self.state.ctm.concat(matrix);
            }
            if let Some(bbox) = resolved_rect(self.document, dictionary.get(b"BBox".as_slice()))? {
                if let Some(form_clip) = axis_aligned_transformed_rect(self.state.ctm, bbox) {
                    if !self.state.clip_empty {
                        self.state.clip = match self.state.clip {
                            Some(clip) => {
                                let intersection = clip.intersection(form_clip);
                                self.state.clip_empty = intersection.is_none();
                                intersection
                            }
                            None => Some(form_clip),
                        };
                    }
                } else {
                    self.state.coverage_reliable = false;
                }
            } else {
                self.state.coverage_reliable = false;
            }

            let resources = form_resources(self.document, handle)?
                .unwrap_or_else(|| inherited_resources.clone());
            let content = form_content(self.document, handle)?;
            if self.scan_content(&content, &resources)? {
                self.facts.complete = false;
            }
            Ok(())
        })();

        self.active_forms.remove(&handle);
        if !self.stack.is_empty() || self.compatibility_depth != 0 {
            self.facts.complete = false;
        }
        self.stack = saved_stack;
        self.compatibility_depth = saved_compatibility_depth;
        self.path_rect = saved_path_rect;
        self.path_reliable = saved_path_reliable;
        self.pending_clip = saved_pending_clip;
        self.state = saved_state;
        result
    }
}

fn one_name_operand(operands: &[crate::content_stream::InstructionOperand]) -> Option<Vec<u8>> {
    if operands.len() != 1 {
        return None;
    }
    operands[0].name.clone()
}

fn resolved_name_is_image(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<bool> {
    Ok(matches!(
        document.resolve_owned_value(dictionary.get(b"Subtype".as_slice()).unwrap_or(&OwnedObject::Null))?,
        Some(OwnedObject::Name(name)) if name.as_slice() == b"Image"
    ))
}

fn resolved_name_is_none(document: &EditDocument, value: &OwnedObject) -> Result<bool> {
    Ok(matches!(
       document.resolve_owned_value(value)?,
       Some(OwnedObject::Name(name)) if name.as_slice() == b"None"
    ))
}

fn resolved_matrix(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<Matrix>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != 6 {
        return Ok(None);
    }
    let mut numbers = [0.0; 6];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = resolved_number(document, Some(value))? else {
            return Ok(None);
        };
        if !number.is_finite() {
            return Ok(None);
        }
        numbers[index] = number;
    }
    Ok(Some(Matrix::from(numbers)))
}

fn resolved_rect(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<Rect>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != 4 {
        return Ok(None);
    }
    let mut numbers = [0.0; 4];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = resolved_number(document, Some(value))? else {
            return Ok(None);
        };
        if !number.is_finite() {
            return Ok(None);
        }
        numbers[index] = number;
    }
    let rect = Rect::new(numbers[0], numbers[1], numbers[2], numbers[3]);
    (rect.area() > f64::EPSILON)
        .then_some(rect)
        .map_or(Ok(None), |r| Ok(Some(r)))
}

fn page_visible_rect(document: &EditDocument, page: ObjectHandle) -> Result<Option<Rect>> {
    for key in [b"CropBox".as_slice(), b"MediaBox".as_slice()] {
        let Some(value) = document.inherited_page_value(page, key)? else {
            continue;
        };
        if let Some(rect) = resolved_rect(document, Some(&value))? {
            return Ok(Some(rect));
        }
    }
    Ok(None)
}

fn axis_aligned_unit_rect(matrix: Matrix) -> Option<Rect> {
    axis_aligned_transformed_rect(matrix, Rect::new(0.0, 0.0, 1.0, 1.0))
}

fn axis_aligned_transformed_rect(matrix: Matrix, rect: Rect) -> Option<Rect> {
    if ![
        matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f, rect.x0, rect.y0, rect.x1,
        rect.y1,
    ]
    .into_iter()
    .all(f64::is_finite)
    {
        return None;
    }
    let ordinary = matrix.b.abs() <= MATRIX_EPSILON && matrix.c.abs() <= MATRIX_EPSILON;
    let quarter_turn = matrix.a.abs() <= MATRIX_EPSILON && matrix.d.abs() <= MATRIX_EPSILON;
    if !ordinary && !quarter_turn {
        return None;
    }
    let points = [
        matrix.transform(rect.x0, rect.y0),
        matrix.transform(rect.x0, rect.y1),
        matrix.transform(rect.x1, rect.y0),
        matrix.transform(rect.x1, rect.y1),
    ];
    Some(Rect {
        x0: points
            .iter()
            .map(|point| point.0)
            .fold(f64::INFINITY, f64::min),
        y0: points
            .iter()
            .map(|point| point.1)
            .fold(f64::INFINITY, f64::min),
        x1: points
            .iter()
            .map(|point| point.0)
            .fold(f64::NEG_INFINITY, f64::max),
        y1: points
            .iter()
            .map(|point| point.1)
            .fold(f64::NEG_INFINITY, f64::max),
    })
}

fn union_coverage(rects: &[Rect], target: Rect) -> f64 {
    let target_area = target.area();
    if target_area <= f64::EPSILON || rects.is_empty() {
        return 0.0;
    }
    if rects
        .iter()
        .any(|rect| rect.contains(target, MATRIX_EPSILON))
    {
        return 1.0;
    }

    let clipped = rects
        .iter()
        .filter_map(|rect| rect.intersection(target))
        .collect::<Vec<_>>();
    if clipped.is_empty() {
        return 0.0;
    }

    let mut xs = Vec::with_capacity(clipped.len() * 2);
    for rect in &clipped {
        xs.extend([rect.x0, rect.x1]);
    }
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|left, right| (*left - *right).abs() <= MATRIX_EPSILON);

    let mut area = 0.0;
    for pair in xs.windows(2) {
        let left = pair[0];
        let right = pair[1];
        let width = right - left;
        if width <= MATRIX_EPSILON {
            continue;
        }
        let mut ys = clipped
            .iter()
            .filter(|rect| rect.x0 <= left + MATRIX_EPSILON && rect.x1 + MATRIX_EPSILON >= right)
            .map(|rect| (rect.y0, rect.y1))
            .collect::<Vec<_>>();
        ys.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.total_cmp(&b.1)));

        let mut covered = 0.0;
        let mut current: Option<(f64, f64)> = None;
        for (y0, y1) in ys {
            match current {
                None => current = Some((y0, y1)),
                Some((start, end)) if y0 <= end + MATRIX_EPSILON => {
                    current = Some((start, end.max(y1)));
                }
                Some((start, end)) => {
                    covered += end - start;
                    current = Some((y0, y1));
                }
            }
        }
        if let Some((start, end)) = current {
            covered += end - start;
        }
        area = f64::mul_add(width, covered, area);
    }

    (area / target_area).clamp(0.0, 1.0)
}

fn classify_page(facts: &PagePaintFacts, page: Rect) -> PageRepresentation {
    if !facts.has_any_paint() {
        return if facts.complete {
            PageRepresentation::Blank
        } else {
            PageRepresentation::Unknown
        };
    }

    let coverage = union_coverage(&facts.image_rects, page);
    let ambiguous = !facts.complete || facts.unknown_paints > 0;
    if coverage >= RASTER_PAGE_COVERAGE && facts.visible_native_paints == 0 && !ambiguous {
        PageRepresentation::Raster
    } else if coverage >= MIXED_PAGE_COVERAGE && facts.visible_native_paints > 0 {
        PageRepresentation::Mixed
    } else if ambiguous && facts.image_paints > 0 {
        PageRepresentation::Unknown
    } else {
        PageRepresentation::Native
    }
}

const fn classify_document(summary: &PageRepresentationSummary) -> DocumentRepresentation {
    let nonblank = summary
        .raster_pages
        .saturating_add(summary.native_pages)
        .saturating_add(summary.mixed_pages)
        .saturating_add(summary.unknown_pages);
    if nonblank == 0 {
        return DocumentRepresentation::Blank;
    }
    if summary.raster_pages == nonblank {
        return DocumentRepresentation::Raster;
    }
    if summary.native_pages == nonblank {
        return DocumentRepresentation::Native;
    }
    if summary.raster_pages.saturating_mul(10) >= nonblank.saturating_mul(9)
        && summary.unknown_pages == 0
    {
        return DocumentRepresentation::MostlyRaster;
    }
    if summary.unknown_pages == nonblank {
        return DocumentRepresentation::Unknown;
    }
    DocumentRepresentation::Mixed
}

pub fn begin_page_classifier(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<Option<PageClassifier<'_>>> {
    Ok(
        page_visible_rect(document, page)?
            .map(|page_rect| PageClassifier::new(document, page_rect)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{analyze_pdf_preflight, test_support::ClassicPdfBuilder};

    fn one_page_pdf(
        resources: &[u8],
        content: &[u8],
        extra: impl FnOnce(&mut ClassicPdfBuilder) -> Result<()>,
    ) -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        let page = format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources {} /Contents 4 0 R >>",
            String::from_utf8_lossy(resources)
        );
        pdf.object(3, page.as_bytes())?;
        pdf.stream(4, b"", content)?;
        extra(&mut pdf)?;
        pdf.finish(1)
    }

    fn image_object(pdf: &mut ClassicPdfBuilder, number: u32) -> Result<()> {
        pdf.stream(
            number,
            b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
            &[128],
        )
    }

    #[test]
    fn full_page_image_is_raster() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        assert_eq!(analysis.raster_page_count, 1);
        Ok(())
    }

    #[test]
    fn hidden_ocr_text_does_not_make_scan_mixed() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Im Do Q BT 3 Tr (ocr) Tj ET",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn visible_text_over_scan_is_mixed() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Im Do Q BT (visible) Tj ET",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Mixed
        );
        assert_eq!(analysis.mixed_page_count, 1);
        Ok(())
    }

    #[test]
    fn small_image_with_text_stays_native() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 20 0 0 20 0 0 cm /Im Do Q BT (visible) Tj ET",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Native
        );
        Ok(())
    }

    #[test]
    fn tiled_images_can_prove_raster_coverage() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 50 0 0 50 0 0 cm /Im Do Q q 50 0 0 50 50 0 cm /Im Do Q q 50 0 0 50 0 50 cm /Im Do Q q 50 0 0 50 50 50 cm /Im Do Q",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn image_inside_form_can_prove_raster_coverage() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Fm 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Fm Do Q",
            |pdf| {
                pdf.stream(
                    5,
                    b"/Type /XObject /Subtype /Form /BBox [0 0 1 1] /Resources << /XObject << /Im 6 0 R >> >>",
                    b"/Im Do",
                )?;
                image_object(pdf, 6)
            },
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn rectangular_full_page_clip_keeps_raster_proof() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"0 0 100 100 re W n q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn arbitrary_clip_keeps_raster_coverage_unproved() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"0 0 m 100 0 l 50 100 l h W n q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_ne!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        assert_eq!(analysis.unknown_page_count, 1);
        Ok(())
    }

    #[test]
    fn image_owned_soft_mask_is_still_raster_semantics() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| {
                pdf.stream(
                    5,
                    b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /SMask 6 0 R",
                    &[128],
                )?;
                pdf.stream(
                    6,
                    b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
                    &[255],
                )
            },
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn image_stencil_is_raster_semantics() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| {
                pdf.stream(
                    5,
                    b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ImageMask true /BitsPerComponent 1",
                    &[0],
                )
            },
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn color_key_mask_is_raster_semantics() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| {
                pdf.stream(
                    5,
                    b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8 /Mask [0 0]",
                    &[128],
                )
            },
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_eq!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }

    #[test]
    fn clipped_image_is_not_claimed_as_raster() -> Result<()> {
        let input = one_page_pdf(
            b"<< /XObject << /Im 5 0 R >> >>",
            b"0 0 50 100 re W n q 100 0 0 100 0 0 cm /Im Do Q",
            |pdf| image_object(pdf, 5),
        )?;
        let analysis = analyze_pdf_preflight(&input)?;
        assert_ne!(
            analysis.document_representation,
            DocumentRepresentation::Raster
        );
        Ok(())
    }
}
