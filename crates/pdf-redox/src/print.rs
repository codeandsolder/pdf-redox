use crate::Result;
use crate::content_stream::{
    ContentObject as ObjectHandle, ObjectHandleParserCallbacks, ParseControl,
};
use crate::geometry::Matrix;
use crate::images::ImageResizeTarget;
use std::collections::{BTreeMap, HashMap, HashSet};

const MAX_FORM_DEPTH: usize = 64;
const MIN_PLACEMENT_POINTS: f64 = 1.0e-9;
const MIN_DOWNSAMPLE_PIXEL_REDUCTION_PERCENT: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ImagePlacement {
    pub width_px: u32,
    pub height_px: u32,
    pub max_width_points: f64,
    pub max_height_points: f64,
    pub uses: usize,
}

impl ImagePlacement {
    pub fn target_dimensions(self, target_ppi: u32) -> (u32, u32) {
        fn target_axis(points: f64, pixels: u32, target_ppi: u32) -> u32 {
            let desired = (points * f64::from(target_ppi) / 72.0).ceil();
            if !desired.is_finite() || desired <= 1.0 {
                return 1.min(pixels);
            }
            if desired >= f64::from(pixels) {
                return pixels;
            }
            #[expect(
                clippy::cast_possible_truncation,
                reason = "desired is finite, positive, integral after ceil, and strictly below a u32 pixel bound"
            )]
            #[expect(
                clippy::cast_sign_loss,
                reason = "desired is finite, positive, integral after ceil, and strictly below a u32 pixel bound"
            )]
            {
                desired as u32
            }
        }

        (
            target_axis(self.max_width_points, self.width_px, target_ppi),
            target_axis(self.max_height_points, self.height_px, target_ppi),
        )
    }
}

fn parsed_number(object: &ObjectHandle) -> Option<f64> {
    object
        .as_integer()
        .and_then(crate::source::exact_i64_to_f64)
        .or_else(|| object.as_real())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrintPlacementStats {
    pub images_placed: usize,
    pub image_uses: usize,
    pub downsample_candidates: usize,
    pub existing_jpeg_resize_candidates: usize,
    pub flate_resize_candidates: usize,
    pub source_pixels: u64,
    pub target_pixels: u64,
    pub geometry_complete: bool,
}

#[derive(Debug, Clone, Copy)]
struct CowDrawEvent {
    target: crate::ObjectHandle,
    ctm: Matrix,
}

#[derive(Debug)]
struct CowPlacementScanner {
    xobjects: BTreeMap<Vec<u8>, crate::ObjectHandle>,
    ctm: Matrix,
    stack: Vec<Matrix>,
    operands: Vec<ObjectHandle>,
    draws: Vec<CowDrawEvent>,
    complete: bool,
}

impl CowPlacementScanner {
    const fn new(
        xobjects: BTreeMap<Vec<u8>, crate::ObjectHandle>,
        base_ctm: Matrix,
        complete: bool,
    ) -> Self {
        Self {
            xobjects,
            ctm: base_ctm,
            stack: Vec::new(),
            operands: Vec::new(),
            draws: Vec::new(),
            complete,
        }
    }

    fn operator(&mut self, operator: &[u8]) {
        match operator {
            b"q" => {
                if !self.operands.is_empty() {
                    self.complete = false;
                }
                self.stack.push(self.ctm);
            }
            b"Q" => {
                if !self.operands.is_empty() {
                    self.complete = false;
                }
                if let Some(ctm) = self.stack.pop() {
                    self.ctm = ctm;
                } else {
                    self.complete = false;
                }
            }
            b"cm" => {
                if self.operands.len() == 6 {
                    let values: Option<Vec<f64>> =
                        self.operands.iter().map(parsed_number).collect();
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
                if self.operands.len() == 1 {
                    if let Some(name) = self.operands[0].as_name() {
                        if let Some(target) = self.xobjects.get(&name).copied() {
                            self.draws.push(CowDrawEvent {
                                target,
                                ctm: self.ctm,
                            });
                        } else {
                            self.complete = false;
                        }
                    } else {
                        self.complete = false;
                    }
                } else {
                    self.complete = false;
                }
            }
            _ => {}
        }
    }
}

impl ObjectHandleParserCallbacks for CowPlacementScanner {
    fn handle_object(
        &mut self,
        object: ObjectHandle,
        _offset: usize,
        _length: usize,
    ) -> crate::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.operator(&operator);
            self.operands.clear();
        } else if object.as_inline_image().is_none() {
            self.operands.push(object);
        }
        Ok(ParseControl::Continue)
    }
    fn handle_eof(&mut self) -> crate::Result<()> {
        if !self.stack.is_empty() {
            self.complete = false;
        }
        Ok(())
    }
}

fn cow_resolved_dictionary(
    document: &crate::EditDocument,
    value: Option<&crate::OwnedObject>,
) -> Result<Option<crate::OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|value| match value {
            crate::OwnedObject::Dictionary(dictionary) => Some(dictionary),
            _ => None,
        }))
}

fn cow_xobjects(
    document: &crate::EditDocument,
    resources: Option<crate::OwnedObject>,
) -> Result<(BTreeMap<Vec<u8>, crate::ObjectHandle>, bool)> {
    let Some(resources) = resources else {
        return Ok((BTreeMap::new(), true));
    };
    let Some(resources) = cow_resolved_dictionary(document, Some(&resources))? else {
        return Ok((BTreeMap::new(), false));
    };
    let Some(xobjects) = cow_resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok((BTreeMap::new(), true));
    };
    let mut out = BTreeMap::new();
    let mut complete = true;
    for (name, value) in xobjects {
        if let crate::OwnedObject::Reference(handle) = value {
            out.insert(name, handle);
        } else {
            complete = false;
        }
    }
    Ok((out, complete))
}

fn cow_content_value(
    document: &crate::EditDocument,
    value: &crate::OwnedObject,
    out: &mut Vec<u8>,
) -> Result<()> {
    let value = match value {
        crate::OwnedObject::Reference(handle) => {
            let Some(value) = document.current_owned_object(*handle)? else {
                return Ok(());
            };
            value
        }
        value => value.clone(),
    };
    match value {
        crate::OwnedObject::Stream { .. } => {
            let bytes = document.decoded_owned_stream_data(&value)?;
            if !out.is_empty() && out.last() != Some(&b'\n') {
                out.push(b'\n');
            }
            out.extend_from_slice(&bytes);
        }
        crate::OwnedObject::Array(values) => {
            for value in values {
                cow_content_value(document, &value, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn cow_page_content(document: &crate::EditDocument, page: crate::ObjectHandle) -> Result<Vec<u8>> {
    let Some(page) = document.current_owned_object(page)? else {
        return Ok(Vec::new());
    };
    let Some(dictionary) = page.as_dictionary() else {
        return Ok(Vec::new());
    };
    let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    cow_content_value(document, contents, &mut out)?;
    Ok(out)
}

fn cow_number(
    document: &crate::EditDocument,
    value: Option<&crate::OwnedObject>,
) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(crate::OwnedObject::Integer(value)) => crate::source::exact_i64_to_f64(value),
        Some(crate::OwnedObject::Real(value)) => Some(value),
        _ => None,
    })
}

fn cow_number_array<const N: usize>(
    document: &crate::EditDocument,
    value: Option<&crate::OwnedObject>,
) -> Result<Option<[f64; N]>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let Some(crate::OwnedObject::Array(values)) = document.resolve_owned_value(value)? else {
        return Ok(None);
    };
    if values.len() != N {
        return Ok(None);
    }
    let mut out = [0.0; N];
    for (index, value) in values.iter().enumerate() {
        let Some(number) = cow_number(document, Some(value))? else {
            return Ok(None);
        };
        out[index] = number;
    }
    Ok(Some(out))
}

fn cow_form_matrix(
    document: &crate::EditDocument,
    dictionary: &crate::OwnedDictionary,
) -> Result<Matrix> {
    let Some(values) = cow_number_array::<6>(document, dictionary.get(b"Matrix".as_slice()))?
    else {
        return Ok(Matrix::default());
    };
    Ok(Matrix::new(
        values[0], values[1], values[2], values[3], values[4], values[5],
    ))
}

fn cow_image_dimensions(
    document: &crate::EditDocument,
    dictionary: &crate::OwnedDictionary,
) -> Result<Option<(u32, u32)>> {
    let Some(width) = cow_number(document, dictionary.get(b"Width".as_slice()))? else {
        return Ok(None);
    };
    let Some(height) = cow_number(document, dictionary.get(b"Height".as_slice()))? else {
        return Ok(None);
    };
    if !width.is_finite()
        || !height.is_finite()
        || width <= 0.0
        || height <= 0.0
        || width > f64::from(u32::MAX)
        || height > f64::from(u32::MAX)
    {
        return Ok(None);
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "finite positive image dimensions are range-checked against u32 before conversion"
    )]
    #[expect(
        clippy::cast_sign_loss,
        reason = "finite positive image dimensions are range-checked against u32 before conversion"
    )]
    Ok(Some((width as u32, height as u32)))
}

fn cow_subtype(
    document: &crate::EditDocument,
    handle: crate::ObjectHandle,
) -> Result<Option<Vec<u8>>> {
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
        Some(crate::OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn cow_record_image(
    document: &crate::EditDocument,
    placements: &mut HashMap<crate::ObjectHandle, ImagePlacement>,
    handle: crate::ObjectHandle,
    ctm: Matrix,
) -> Result<()> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(());
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(());
    };
    let Some((width_px, height_px)) = cow_image_dimensions(document, dictionary)? else {
        return Ok(());
    };
    let width_points = ctm.a.hypot(ctm.b);
    let height_points = ctm.c.hypot(ctm.d);
    if !width_points.is_finite()
        || !height_points.is_finite()
        || width_points <= MIN_PLACEMENT_POINTS
        || height_points <= MIN_PLACEMENT_POINTS
    {
        return Ok(());
    }
    placements
        .entry(handle)
        .and_modify(|placement| {
            placement.max_width_points = placement.max_width_points.max(width_points);
            placement.max_height_points = placement.max_height_points.max(height_points);
            placement.uses += 1;
        })
        .or_insert(ImagePlacement {
            width_px,
            height_px,
            max_width_points: width_points,
            max_height_points: height_points,
            uses: 1,
        });
    Ok(())
}

struct CowPlacementWalkState<'a> {
    placements: &'a mut HashMap<crate::ObjectHandle, ImagePlacement>,
    form_stack: &'a mut HashSet<crate::ObjectHandle>,
    complete: &'a mut bool,
}

fn cow_scan_form(
    document: &crate::EditDocument,
    form: crate::ObjectHandle,
    outer_ctm: Matrix,
    inherited_xobjects: &BTreeMap<Vec<u8>, crate::ObjectHandle>,
    state: &mut CowPlacementWalkState<'_>,
    depth: usize,
) -> Result<()> {
    if depth >= MAX_FORM_DEPTH || !state.form_stack.insert(form) {
        *state.complete = false;
        return Ok(());
    }
    let result = (|| {
        let Some(object) = document.current_owned_object(form)? else {
            *state.complete = false;
            return Ok(());
        };
        let Some(dictionary) = object.as_dictionary() else {
            *state.complete = false;
            return Ok(());
        };
        let mut base_ctm = outer_ctm;
        base_ctm.concat(cow_form_matrix(document, dictionary)?);
        let resources = dictionary.get(b"Resources".as_slice()).cloned();
        let (xobjects, scope_complete) = if resources.is_some() {
            cow_xobjects(document, resources)?
        } else {
            (inherited_xobjects.clone(), true)
        };
        if !scope_complete {
            *state.complete = false;
        }
        let content = document.decoded_stream_data(form)?;
        let mut scanner = CowPlacementScanner::new(xobjects, base_ctm, scope_complete);
        if crate::content_stream::parse_detached_content_stream(
            &content,
            "Hayro/COW form placement",
            &mut scanner,
        )
        .is_err()
            || !scanner.complete
        {
            *state.complete = false;
        }
        let current = scanner.xobjects.clone();
        for draw in scanner.draws {
            match cow_subtype(document, draw.target)?.as_deref() {
                Some(b"Image") => {
                    cow_record_image(document, state.placements, draw.target, draw.ctm)?;
                }
                Some(b"Form") => {
                    cow_scan_form(document, draw.target, draw.ctm, &current, state, depth + 1)?;
                }
                _ => *state.complete = false,
            }
        }
        Ok(())
    })();
    state.form_stack.remove(&form);
    result
}

#[derive(Debug, Default)]
pub struct PrintPlanHayro {
    pub stats: PrintPlacementStats,
    pub resize_targets: HashMap<(crate::ObjectHandle, crate::ObjectHandle), ImageResizeTarget>,
}

fn cow_image_resize_safe(
    document: &crate::EditDocument,
    image: crate::ObjectHandle,
    jpeg: bool,
) -> Result<bool> {
    let Some(object) = document.current_owned_object(image)? else {
        return Ok(false);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(false);
    };
    // Recovered 1-bit ImageMasks are geometry-like bilevel fields, not
    // continuous-tone images. Their native grid is intentionally preserved:
    // generic PPI downsampling either drops subpixel features or fattens them.
    if let Some(value) = dictionary.get(b"ImageMask".as_slice())
        && matches!(
            document.resolve_owned_value(value)?,
            Some(crate::OwnedObject::Boolean(true))
        )
    {
        return Ok(false);
    }
    for key in [
        b"SMask".as_slice(),
        b"Mask".as_slice(),
        b"Decode".as_slice(),
    ] {
        if let Some(value) = dictionary.get(key)
            && !matches!(
                document.resolve_owned_value(value)?,
                Some(crate::OwnedObject::Null) | None
            )
        {
            return Ok(false);
        }
    }
    if jpeg
        && dictionary.get(b"DecodeParms".as_slice()).is_some_and(|v| {
            !matches!(
                document.resolve_owned_value(v),
                Ok(Some(crate::OwnedObject::Null) | None)
            )
        })
    {
        return Ok(false);
    }
    if !matches!(
        document.resolve_owned_value(
            dictionary
                .get(b"BitsPerComponent".as_slice())
                .unwrap_or(&crate::OwnedObject::Null)
        )?,
        Some(crate::OwnedObject::Integer(8))
    ) {
        return Ok(false);
    }
    let Some(Some(crate::OwnedObject::Name(filter))) = dictionary
        .get(b"Filter".as_slice())
        .map(|value| document.resolve_owned_value(value))
        .transpose()?
    else {
        return Ok(false);
    };
    if jpeg && !matches!(filter.as_slice(), b"DCTDecode" | b"DCT") {
        return Ok(false);
    }
    if !jpeg && !matches!(filter.as_slice(), b"FlateDecode" | b"Fl") {
        return Ok(false);
    }
    let Some(Some(crate::OwnedObject::Name(color))) = dictionary
        .get(b"ColorSpace".as_slice())
        .map(|value| document.resolve_owned_value(value))
        .transpose()?
    else {
        return Ok(false);
    };
    Ok(matches!(color.as_slice(), b"DeviceGray" | b"DeviceRGB"))
}

#[expect(
    clippy::too_many_lines,
    reason = "placement collection and conservative resize selection form one ordered geometry pass"
)]
pub fn plan_print_downsampling_hayro(
    document: &crate::EditDocument,
    target_ppi: u32,
) -> Result<PrintPlanHayro> {
    let mut placements = HashMap::new();
    let mut direct_page_bindings = HashSet::new();
    let mut complete = true;
    for page in document.page_handles()? {
        let resources = document.inherited_page_value(page, b"Resources")?;
        let (xobjects, scope_complete) = cow_xobjects(document, resources)?;
        if !scope_complete {
            complete = false;
        }
        for handle in xobjects.values() {
            if cow_subtype(document, *handle)?.as_deref() == Some(b"Image") {
                direct_page_bindings.insert((page, *handle));
            }
        }
        let mut base_ctm = Matrix::default();
        let user_unit = document.current_owned_object(page)?.and_then(|o| {
            o.as_dictionary()
                .and_then(|d| d.get(b"UserUnit".as_slice()))
                .cloned()
        });
        let user_unit = match user_unit.as_ref() {
            Some(value) => cow_number(document, Some(value))?
                .filter(|v| v.is_finite() && *v > 0.0)
                .unwrap_or(1.0),
            None => 1.0,
        };
        base_ctm.scale(user_unit, user_unit);
        let content = cow_page_content(document, page)?;
        let mut scanner = CowPlacementScanner::new(xobjects, base_ctm, scope_complete);
        if crate::content_stream::parse_detached_content_stream(
            &content,
            "Hayro/COW page placement",
            &mut scanner,
        )
        .is_err()
            || !scanner.complete
        {
            complete = false;
        }
        let current = scanner.xobjects.clone();
        let mut form_stack = HashSet::new();
        for draw in scanner.draws {
            match cow_subtype(document, draw.target)?.as_deref() {
                Some(b"Image") => {
                    cow_record_image(document, &mut placements, draw.target, draw.ctm)?;
                }
                Some(b"Form") => {
                    let mut state = CowPlacementWalkState {
                        placements: &mut placements,
                        form_stack: &mut form_stack,
                        complete: &mut complete,
                    };
                    cow_scan_form(document, draw.target, draw.ctm, &current, &mut state, 0)?;
                }
                _ => complete = false,
            }
        }
    }

    let mut plan = PrintPlanHayro {
        stats: PrintPlacementStats {
            images_placed: placements.len(),
            geometry_complete: complete,
            ..Default::default()
        },
        resize_targets: HashMap::new(),
    };
    for (image, placement) in placements {
        plan.stats.image_uses += placement.uses;
        let (target_width, target_height) = placement.target_dimensions(target_ppi);
        let source_pixels = u64::from(placement.width_px) * u64::from(placement.height_px);
        let target_pixels = u64::from(target_width) * u64::from(target_height);
        let downsample = target_width < placement.width_px || target_height < placement.height_px;
        if downsample {
            plan.stats.downsample_candidates += 1;
        }
        plan.stats.source_pixels += source_pixels;
        plan.stats.target_pixels += target_pixels;
        let clears_gate = target_pixels.saturating_mul(100)
            <= source_pixels.saturating_mul(100 - MIN_DOWNSAMPLE_PIXEL_REDUCTION_PERCENT);
        if !downsample || !clears_gate || !complete {
            continue;
        }
        let target = if cow_image_resize_safe(document, image, true)? {
            plan.stats.existing_jpeg_resize_candidates += 1;
            Some(ImageResizeTarget::jpeg(target_width, target_height))
        } else if cow_image_resize_safe(document, image, false)? {
            plan.stats.flate_resize_candidates += 1;
            Some(ImageResizeTarget::flate(target_width, target_height))
        } else {
            None
        };
        let Some(target) = target else {
            continue;
        };
        for &(page, binding_image) in &direct_page_bindings {
            if binding_image == image {
                plan.resize_targets.insert((page, image), target);
            }
        }
    }
    Ok(plan)
}
