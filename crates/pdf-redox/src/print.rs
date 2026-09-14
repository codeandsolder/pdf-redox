use crate::Result;
use flpdf::{
    ImageResizeTarget, Matrix, ObjectHandle, ObjectHandleParserCallbacks, ObjectRef,
    PageObjectHelper, ParseControl, Pdf,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Seek};

const MAX_FORM_DEPTH: usize = 64;
const MIN_PLACEMENT_POINTS: f64 = 1.0e-9;
const MIN_DOWNSAMPLE_PIXEL_REDUCTION_PERCENT: u64 = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ImagePlacement {
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
            (desired as u64).min(u64::from(pixels)) as u32
        }

        (
            target_axis(self.max_width_points, self.width_px, target_ppi),
            target_axis(self.max_height_points, self.height_px, target_ppi),
        )
    }
}

#[derive(Debug, Clone)]
struct DrawEvent {
    target: ObjectHandle,
    ctm: Matrix,
}

#[derive(Debug)]
struct PlacementScanner {
    xobjects: BTreeMap<Vec<u8>, ObjectHandle>,
    ctm: Matrix,
    stack: Vec<Matrix>,
    operands: Vec<ObjectHandle>,
    draws: Vec<DrawEvent>,
    complete: bool,
}

impl PlacementScanner {
    fn new(xobjects: BTreeMap<Vec<u8>, ObjectHandle>, base_ctm: Matrix, complete: bool) -> Self {
        Self {
            xobjects,
            ctm: base_ctm,
            stack: Vec::new(),
            operands: Vec::new(),
            draws: Vec::new(),
            complete,
        }
    }

    fn number(object: &ObjectHandle) -> Option<f64> {
        object
            .as_integer()
            .map(|value| value as f64)
            .or_else(|| object.as_real())
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
                    let values: Option<Vec<f64>> = self.operands.iter().map(Self::number).collect();
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
                }
                if self.operands.len() == 1
                    && let Some(name) = self.operands[0].as_name()
                {
                    if let Some(target) = self.xobjects.get(&name) {
                        self.draws.push(DrawEvent {
                            target: target.clone(),
                            ctm: self.ctm,
                        });
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

impl ObjectHandleParserCallbacks for PlacementScanner {
    fn handle_object(
        &mut self,
        object: ObjectHandle,
        _offset: usize,
        _length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.operator(&operator);
            self.operands.clear();
        } else if object.as_inline_image().is_none() {
            self.operands.push(object);
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        if !self.stack.is_empty() {
            self.complete = false;
        }
        Ok(())
    }
}

fn normalized_resource_key(key: &[u8]) -> Vec<u8> {
    key.strip_prefix(b"/").unwrap_or(key).to_vec()
}

#[derive(Debug)]
enum XObjectScope {
    Inherit,
    Local(BTreeMap<Vec<u8>, ObjectHandle>),
    Malformed,
}

fn xobject_scope(resources: &ObjectHandle) -> Result<XObjectScope> {
    if resources.is_null() {
        return Ok(XObjectScope::Inherit);
    }
    if !resources.try_is_dictionary()? {
        return Ok(XObjectScope::Malformed);
    }
    let xobjects = resources.try_get_key(b"/XObject")?;
    if xobjects.is_null() {
        return Ok(XObjectScope::Local(BTreeMap::new()));
    }
    if !xobjects.try_is_dictionary()? {
        return Ok(XObjectScope::Malformed);
    }

    let mut out = BTreeMap::new();
    for key in xobjects.try_get_keys()? {
        out.insert(normalized_resource_key(&key), xobjects.try_get_key(&key)?);
    }
    Ok(XObjectScope::Local(out))
}

fn form_matrix(dict: &ObjectHandle) -> Result<Matrix> {
    let matrix = dict.try_get_key(b"/Matrix")?;
    if !matrix.try_is_matrix()? {
        return Ok(Matrix::default());
    }
    let matrix = matrix.try_get_array_as_matrix()?;
    Ok(Matrix::new(
        matrix.a, matrix.b, matrix.c, matrix.d, matrix.e, matrix.f,
    ))
}

fn page_user_unit(page: &ObjectHandle) -> Result<f64> {
    let value = page.try_get_key(b"/UserUnit")?;
    if !value.try_is_number()? {
        return Ok(1.0);
    }
    let value = value.try_get_numeric_value()?;
    if value.is_finite() && value > 0.0 {
        Ok(value)
    } else {
        Ok(1.0)
    }
}

fn image_dimensions(dict: &ObjectHandle) -> Result<Option<(u32, u32)>> {
    let width = dict.try_get_key(b"/Width")?;
    let height = dict.try_get_key(b"/Height")?;
    if !width.try_is_integer()? || !height.try_is_integer()? {
        return Ok(None);
    }
    let width = width.try_get_int_value()?;
    let height = height.try_get_int_value()?;
    let (Ok(width), Ok(height)) = (u32::try_from(width), u32::try_from(height)) else {
        return Ok(None);
    };
    if width == 0 || height == 0 {
        return Ok(None);
    }
    Ok(Some((width, height)))
}

fn record_image(
    placements: &mut HashMap<ObjectRef, ImagePlacement>,
    object_ref: ObjectRef,
    dict: &ObjectHandle,
    ctm: Matrix,
) -> Result<()> {
    let Some((width_px, height_px)) = image_dimensions(dict)? else {
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
        .entry(object_ref)
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

struct PlacementWalkState<'a> {
    placements: &'a mut HashMap<ObjectRef, ImagePlacement>,
    used_images_on_page: &'a mut HashSet<ObjectRef>,
    form_stack: &'a mut HashSet<ObjectRef>,
    complete: &'a mut bool,
}

fn scan_form<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    form: ObjectHandle,
    outer_ctm: Matrix,
    inherited_xobjects: &BTreeMap<Vec<u8>, ObjectHandle>,
    state: &mut PlacementWalkState<'_>,
    depth: usize,
) -> Result<()> {
    if depth >= MAX_FORM_DEPTH {
        *state.complete = false;
        return Ok(());
    }
    pdf.resolve(&form)?;
    let Some(dict) = form.as_stream_dict() else {
        *state.complete = false;
        return Ok(());
    };
    if !dict
        .try_get_key(b"/Subtype")?
        .try_is_name_and_equals(b"Form")?
    {
        *state.complete = false;
        return Ok(());
    }

    let form_ref = form.object_ref();
    if let Some(form_ref) = form_ref
        && !state.form_stack.insert(form_ref)
    {
        *state.complete = false;
        return Ok(());
    }

    let result = (|| {
        let mut base_ctm = outer_ctm;
        base_ctm.concat(form_matrix(&dict)?);
        let resources = dict.try_get_key(b"/Resources")?;
        let (xobjects, scope_complete) = match xobject_scope(&resources)? {
            XObjectScope::Inherit => (inherited_xobjects.clone(), true),
            XObjectScope::Local(xobjects) => (xobjects, true),
            XObjectScope::Malformed => (BTreeMap::new(), false),
        };

        let diagnostics_before = pdf.num_warnings();
        let mut scanner = PlacementScanner::new(xobjects, base_ctm, scope_complete);
        let parse_ok = form.parse_as_contents(&mut scanner).is_ok();
        if !parse_ok || pdf.num_warnings() > diagnostics_before || !scanner.complete {
            *state.complete = false;
        }
        let current_xobjects = scanner.xobjects.clone();
        process_draws(pdf, scanner.draws, &current_xobjects, state, depth + 1)
    })();

    if let Some(form_ref) = form_ref {
        state.form_stack.remove(&form_ref);
    }
    result
}

fn process_draws<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    draws: Vec<DrawEvent>,
    current_xobjects: &BTreeMap<Vec<u8>, ObjectHandle>,
    state: &mut PlacementWalkState<'_>,
    depth: usize,
) -> Result<()> {
    for draw in draws {
        pdf.resolve(&draw.target)?;
        let Some(dict) = draw.target.as_stream_dict() else {
            *state.complete = false;
            continue;
        };
        let subtype = dict.try_get_key(b"/Subtype")?;
        if subtype.try_is_name_and_equals(b"Image")? {
            if let Some(object_ref) = draw.target.object_ref() {
                state.used_images_on_page.insert(object_ref);
                record_image(state.placements, object_ref, &dict, draw.ctm)?;
            } else {
                *state.complete = false;
            }
        } else if subtype.try_is_name_and_equals(b"Form")? {
            scan_form(pdf, draw.target, draw.ctm, current_xobjects, state, depth)?;
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct PlacementCollection {
    placements: HashMap<ObjectRef, ImagePlacement>,
    page_bindings: HashSet<(ObjectRef, ObjectRef)>,
    complete: bool,
}

fn collect_image_placements<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<PlacementCollection> {
    let mut placements = HashMap::new();
    let mut page_image_bindings = HashSet::new();
    let mut complete = true;
    for page_ref in flpdf::pages::page_refs(pdf)? {
        let resources = PageObjectHelper::new(page_ref, pdf).get_resources(false)?;
        let page = pdf.get_object_handle(page_ref);
        pdf.resolve(&page)?;

        let mut base_ctm = Matrix::default();
        let user_unit = page_user_unit(&page)?;
        base_ctm.scale(user_unit, user_unit);

        let (xobjects, scope_complete) = match xobject_scope(&resources)? {
            XObjectScope::Inherit => (BTreeMap::new(), true),
            XObjectScope::Local(xobjects) => (xobjects, true),
            XObjectScope::Malformed => (BTreeMap::new(), false),
        };
        let mut direct_images_on_page = HashSet::new();
        for object in xobjects.values() {
            let object = object.clone();
            pdf.resolve(&object)?;
            if object.is_image(true)?
                && let Some(object_ref) = object.object_ref()
            {
                direct_images_on_page.insert(object_ref);
            }
        }
        let diagnostics_before = pdf.num_warnings();
        let mut scanner = PlacementScanner::new(xobjects, base_ctm, scope_complete);
        let parse_ok = page.parse_page_contents(&mut scanner).is_ok();
        if !parse_ok || pdf.num_warnings() > diagnostics_before || !scanner.complete {
            complete = false;
        }
        let current_xobjects = scanner.xobjects.clone();
        let mut form_stack = HashSet::new();
        let mut used_images_on_page = HashSet::new();
        let mut state = PlacementWalkState {
            placements: &mut placements,
            used_images_on_page: &mut used_images_on_page,
            form_stack: &mut form_stack,
            complete: &mut complete,
        };
        process_draws(pdf, scanner.draws, &current_xobjects, &mut state, 0)?;
        page_image_bindings.extend(
            direct_images_on_page
                .intersection(&used_images_on_page)
                .copied()
                .map(|image_ref| (page_ref, image_ref)),
        );
    }
    Ok(PlacementCollection {
        placements,
        page_bindings: page_image_bindings,
        complete,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct PrintPlacementStats {
    pub images_placed: usize,
    pub image_uses: usize,
    pub downsample_candidates: usize,
    pub existing_jpeg_resize_candidates: usize,
    pub flate_resize_candidates: usize,
    pub source_pixels: u64,
    pub target_pixels: u64,
    pub geometry_complete: bool,
}

#[derive(Debug, Default)]
pub(crate) struct PrintPlan {
    pub stats: PrintPlacementStats,
    pub resize_targets: HashMap<(ObjectRef, ObjectRef), ImageResizeTarget>,
}

fn is_single_dct_filter(dict: &ObjectHandle) -> bool {
    dict.try_get_key(b"/Filter").is_ok_and(|filter| {
        matches!(filter.try_is_name_and_equals(b"DCTDecode"), Ok(true))
            || matches!(filter.try_is_name_and_equals(b"DCT"), Ok(true))
    })
}

fn is_single_flate_filter(dict: &ObjectHandle) -> bool {
    dict.try_get_key(b"/Filter").is_ok_and(|filter| {
        matches!(filter.try_is_name_and_equals(b"FlateDecode"), Ok(true))
            || matches!(filter.try_is_name_and_equals(b"Fl"), Ok(true))
    })
}

fn is_resize_safe_existing_jpeg<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    object_ref: ObjectRef,
) -> bool {
    let image = pdf.get_object_handle(object_ref);
    if pdf.resolve(&image).is_err() {
        return false;
    }
    let Some(dict) = image.as_stream_dict() else {
        return false;
    };
    if !dict
        .try_get_key(b"/SMask")
        .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/Mask")
            .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/Decode")
            .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/DecodeParms")
            .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/BitsPerComponent")
            .is_ok_and(|value| value.as_integer() == Some(8))
        || !is_single_dct_filter(&dict)
    {
        return false;
    }

    let Ok(color_space) = dict.try_get_key(b"/ColorSpace") else {
        return false;
    };
    if pdf.resolve(&color_space).is_err() {
        return false;
    }
    matches!(color_space.try_is_name_and_equals(b"DeviceGray"), Ok(true))
        || matches!(color_space.try_is_name_and_equals(b"DeviceRGB"), Ok(true))
}

fn is_resize_safe_flate<R: Read + Seek + 'static>(pdf: &mut Pdf<R>, object_ref: ObjectRef) -> bool {
    let image = pdf.get_object_handle(object_ref);
    if pdf.resolve(&image).is_err() {
        return false;
    }
    let Some(dict) = image.as_stream_dict() else {
        return false;
    };
    if !dict
        .try_get_key(b"/SMask")
        .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/Mask")
            .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/Decode")
            .is_ok_and(|value| value.is_null())
        || !dict
            .try_get_key(b"/BitsPerComponent")
            .is_ok_and(|value| value.as_integer() == Some(8))
        || !is_single_flate_filter(&dict)
    {
        return false;
    }

    let Ok(color_space) = dict.try_get_key(b"/ColorSpace") else {
        return false;
    };
    if pdf.resolve(&color_space).is_err() {
        return false;
    }
    matches!(color_space.try_is_name_and_equals(b"DeviceGray"), Ok(true))
        || matches!(color_space.try_is_name_and_equals(b"DeviceRGB"), Ok(true))
}

pub(crate) fn plan_print_downsampling<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    target_ppi: u32,
) -> Result<PrintPlan> {
    let placement_collection = collect_image_placements(pdf)?;
    let geometry_complete = placement_collection.complete;
    let page_image_bindings = placement_collection.page_bindings;
    let placements = placement_collection.placements;
    let mut plan = PrintPlan {
        stats: PrintPlacementStats {
            images_placed: placements.len(),
            geometry_complete,
            ..PrintPlacementStats::default()
        },
        ..PrintPlan::default()
    };
    for (object_ref, placement) in placements {
        plan.stats.image_uses += placement.uses;
        let (target_width, target_height) = placement.target_dimensions(target_ppi);
        let source_pixels = u64::from(placement.width_px) * u64::from(placement.height_px);
        let target_pixels = u64::from(target_width) * u64::from(target_height);
        let is_downsample =
            target_width < placement.width_px || target_height < placement.height_px;
        if is_downsample {
            plan.stats.downsample_candidates += 1;
        }
        plan.stats.source_pixels += source_pixels;
        plan.stats.target_pixels += target_pixels;

        let clears_pixel_reduction_gate = target_pixels.saturating_mul(100)
            <= source_pixels.saturating_mul(100 - MIN_DOWNSAMPLE_PIXEL_REDUCTION_PERCENT);
        if is_downsample && clears_pixel_reduction_gate && geometry_complete {
            let target = if is_resize_safe_existing_jpeg(pdf, object_ref) {
                Some(ImageResizeTarget::jpeg(target_width, target_height))
            } else if is_resize_safe_flate(pdf, object_ref) {
                Some(ImageResizeTarget::flate(target_width, target_height))
            } else {
                None
            };
            if let Some(target) = target {
                let binding_pages = page_image_bindings
                    .iter()
                    .filter_map(|(page_ref, image_ref)| {
                        (*image_ref == object_ref).then_some(*page_ref)
                    })
                    .collect::<Vec<_>>();
                if !binding_pages.is_empty() {
                    match target.encoding {
                        flpdf::ImageResizeEncoding::Jpeg => {
                            plan.stats.existing_jpeg_resize_candidates += 1;
                        }
                        flpdf::ImageResizeEncoding::Flate => {
                            plan.stats.flate_resize_candidates += 1;
                        }
                    }
                    for page_ref in binding_pages {
                        plan.resize_targets.insert((page_ref, object_ref), target);
                    }
                }
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use flpdf::ObjectHandle;
    use std::{io::Cursor, rc::Rc};

    fn stream(pdf: &mut Pdf<Cursor<Vec<u8>>>, bytes: &[u8]) -> Result<ObjectHandle> {
        pdf.new_stream_with_data(Rc::new(bytes.to_vec()))
            .map_err(Into::into)
    }

    fn image(pdf: &mut Pdf<Cursor<Vec<u8>>>, width: i64, height: i64) -> Result<ObjectHandle> {
        let image = stream(pdf, b"pixels")?;
        let dict = image
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture image has no stream dictionary".to_owned()))?;
        for (key, value) in [
            (b"/Type".as_slice(), ObjectHandle::name(b"XObject".to_vec())),
            (
                b"/Subtype".as_slice(),
                ObjectHandle::name(b"Image".to_vec()),
            ),
            (b"/Width".as_slice(), ObjectHandle::integer(width)),
            (b"/Height".as_slice(), ObjectHandle::integer(height)),
            (
                b"/ColorSpace".as_slice(),
                ObjectHandle::name(b"DeviceGray".to_vec()),
            ),
            (b"/BitsPerComponent".as_slice(), ObjectHandle::integer(8)),
        ] {
            dict.replace_key(key, value)?;
        }
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(image)
    }

    fn resources(entries: Vec<(&[u8], ObjectHandle)>) -> ObjectHandle {
        ObjectHandle::dictionary(vec![(
            b"/XObject".to_vec(),
            ObjectHandle::dictionary(
                entries
                    .into_iter()
                    .map(|(name, object)| (name.to_vec(), object))
                    .collect(),
            ),
        )])
    }

    fn add_page(
        pdf: &mut Pdf<Cursor<Vec<u8>>>,
        content: &[u8],
        resources: ObjectHandle,
        user_unit: Option<f64>,
    ) -> Result<()> {
        let catalog = pdf.root_handle()?;
        let pages = catalog.try_get_key(b"/Pages")?;
        pdf.resolve(&pages)?;
        let contents = stream(pdf, content)?;
        let mut entries = vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
            (b"/Parent".to_vec(), pages.clone()),
            (
                b"/MediaBox".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(612),
                    ObjectHandle::integer(792),
                ]),
            ),
            (b"/Resources".to_vec(), resources),
            (b"/Contents".to_vec(), contents),
        ];
        if let Some(user_unit) = user_unit {
            entries.push((b"/UserUnit".to_vec(), ObjectHandle::real(user_unit)));
        }
        let page = pdf.make_indirect_object_handle(ObjectHandle::dictionary(entries))?;
        let kids = pages.try_get_key(b"/Kids")?;
        let mut page_handles = if kids.try_is_array()? {
            kids.try_get_array_as_vector()?
        } else {
            Vec::new()
        };
        page_handles.push(page);
        let count = page_handles.len() as i64;
        pages.replace_key(b"/Kids", ObjectHandle::array(page_handles))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(count))?;
        pdf.mark_object_handle_dirty(&pages)?;
        Ok(())
    }

    #[test]
    fn print_resize_eligibility_rejects_ambiguous_jpeg_color_semantics() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1200, 600)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;
        let dict = image
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture image has no stream dictionary".to_owned()))?;
        dict.replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;
        assert!(is_resize_safe_existing_jpeg(&mut pdf, image_ref));

        dict.replace_key(b"/ColorSpace", ObjectHandle::name(b"DeviceCMYK".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;
        assert!(!is_resize_safe_existing_jpeg(&mut pdf, image_ref));

        dict.replace_key(b"/ColorSpace", ObjectHandle::name(b"DeviceRGB".to_vec()))?;
        dict.replace_key(
            b"/Decode",
            ObjectHandle::array(vec![
                ObjectHandle::integer(1),
                ObjectHandle::integer(0),
                ObjectHandle::integer(1),
                ObjectHandle::integer(0),
                ObjectHandle::integer(1),
                ObjectHandle::integer(0),
            ]),
        )?;
        pdf.mark_object_handle_dirty(&dict)?;
        assert!(!is_resize_safe_existing_jpeg(&mut pdf, image_ref));

        dict.replace_key(b"/Decode", ObjectHandle::null())?;
        dict.replace_key(
            b"/DecodeParms",
            ObjectHandle::dictionary(vec![(
                b"/ColorTransform".to_vec(),
                ObjectHandle::integer(0),
            )]),
        )?;
        pdf.mark_object_handle_dirty(&dict)?;
        assert!(!is_resize_safe_existing_jpeg(&mut pdf, image_ref));

        dict.replace_key(b"/DecodeParms", ObjectHandle::null())?;
        dict.replace_key(b"/Mask", ObjectHandle::array(vec![]))?;
        pdf.mark_object_handle_dirty(&dict)?;
        assert!(!is_resize_safe_existing_jpeg(&mut pdf, image_ref));

        dict.replace_key(b"/Mask", ObjectHandle::null())?;
        dict.replace_key(b"/BitsPerComponent", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&dict)?;
        assert!(!is_resize_safe_existing_jpeg(&mut pdf, image_ref));
        Ok(())
    }

    #[test]
    fn shared_image_uses_largest_physical_placement() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1200, 600)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;
        add_page(
            &mut pdf,
            b"q 300 0 0 150 0 0 cm /Im Do Q q 72 0 0 36 0 0 cm /Im Do Q\n",
            resources(vec![(b"/Im", image)]),
            None,
        )?;

        let collection = collect_image_placements(&mut pdf)?;
        let placements = collection.placements;
        let complete = collection.complete;
        assert!(complete);
        let placement = placements
            .get(&image_ref)
            .ok_or_else(|| Error::Invalid("image placement not collected".to_owned()))?;
        assert_eq!(placement.uses, 2);
        assert_eq!(placement.max_width_points, 300.0);
        assert_eq!(placement.max_height_points, 150.0);
        assert_eq!(placement.target_dimensions(144), (600, 300));
        Ok(())
    }

    #[test]
    fn resource_less_form_inherits_page_xobjects_for_placement() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1200, 600)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;

        let form = stream(&mut pdf, b"q 300 0 0 150 0 0 cm /Im Do Q\n")?;
        let form_dict = form
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture form has no stream dictionary".to_owned()))?;
        form_dict.replace_key(b"/Type", ObjectHandle::name(b"XObject".to_vec()))?;
        form_dict.replace_key(b"/Subtype", ObjectHandle::name(b"Form".to_vec()))?;
        pdf.mark_object_handle_dirty(&form_dict)?;

        add_page(
            &mut pdf,
            b"q 72 0 0 36 0 0 cm /Im Do Q /Fm Do\n",
            resources(vec![(b"/Im", image), (b"/Fm", form)]),
            None,
        )?;

        let collection = collect_image_placements(&mut pdf)?;
        let placements = collection.placements;
        let complete = collection.complete;
        assert!(complete);
        let placement = placements
            .get(&image_ref)
            .ok_or_else(|| Error::Invalid("inherited image placement not collected".to_owned()))?;
        assert_eq!(placement.uses, 2);
        assert_eq!(placement.max_width_points, 300.0);
        assert_eq!(placement.max_height_points, 150.0);
        assert_eq!(placement.target_dimensions(144), (600, 300));
        Ok(())
    }

    #[test]
    fn resize_targets_are_scoped_to_pages_that_actually_use_the_image() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1200, 600)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;
        let dict = image
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture image has no stream dictionary".to_owned()))?;
        dict.replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;

        add_page(
            &mut pdf,
            b"q 72 0 0 36 0 0 cm /Im Do Q\n",
            resources(vec![(b"/Im", image.clone())]),
            None,
        )?;
        add_page(&mut pdf, b"q Q\n", resources(vec![(b"/Im", image)]), None)?;
        let page_refs = flpdf::pages::page_refs(&mut pdf)?;
        let plan = plan_print_downsampling(&mut pdf, 450)?;

        assert!(plan.stats.geometry_complete);
        assert_eq!(plan.stats.existing_jpeg_resize_candidates, 1);
        assert!(plan.resize_targets.contains_key(&(page_refs[0], image_ref)));
        assert!(!plan.resize_targets.contains_key(&(page_refs[1], image_ref)));
        Ok(())
    }

    #[test]
    fn form_local_image_is_measured_but_not_targeted_for_resize() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1200, 600)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;
        let dict = image
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture image has no stream dictionary".to_owned()))?;
        dict.replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;

        let form = stream(&mut pdf, b"q 72 0 0 36 0 0 cm /Im Do Q\n")?;
        let form_dict = form
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture form has no stream dictionary".to_owned()))?;
        form_dict.replace_key(b"/Type", ObjectHandle::name(b"XObject".to_vec()))?;
        form_dict.replace_key(b"/Subtype", ObjectHandle::name(b"Form".to_vec()))?;
        form_dict.replace_key(b"/Resources", resources(vec![(b"/Im", image)]))?;
        pdf.mark_object_handle_dirty(&form_dict)?;

        add_page(&mut pdf, b"/Fm Do\n", resources(vec![(b"/Fm", form)]), None)?;

        let plan = plan_print_downsampling(&mut pdf, 450)?;
        assert!(plan.stats.geometry_complete);
        assert_eq!(plan.stats.downsample_candidates, 1);
        assert_eq!(plan.stats.existing_jpeg_resize_candidates, 0);
        assert!(
            !plan
                .resize_targets
                .keys()
                .any(|(_, candidate)| *candidate == image_ref)
        );
        Ok(())
    }

    #[test]
    fn incomplete_geometry_vetoes_all_resize_targets() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1200, 600)?;
        let dict = image
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture image has no stream dictionary".to_owned()))?;
        dict.replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;

        add_page(
            &mut pdf,
            b"q 72 0 0 36 0 0 cm /Im Do Q /Missing Do\n",
            resources(vec![(b"/Im", image)]),
            None,
        )?;

        let plan = plan_print_downsampling(&mut pdf, 450)?;
        assert!(!plan.stats.geometry_complete);
        assert_eq!(plan.stats.downsample_candidates, 1);
        assert_eq!(plan.stats.existing_jpeg_resize_candidates, 0);
        assert!(plan.resize_targets.is_empty());
        Ok(())
    }

    #[test]
    fn nested_form_matrix_contributes_to_image_placement() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1000, 500)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;

        let form = stream(&mut pdf, b"q 2 0 0 1 0 0 cm /Im Do Q\n")?;
        let form_dict = form
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture form has no stream dictionary".to_owned()))?;
        form_dict.replace_key(b"/Type", ObjectHandle::name(b"XObject".to_vec()))?;
        form_dict.replace_key(b"/Subtype", ObjectHandle::name(b"Form".to_vec()))?;
        form_dict.replace_key(
            b"/BBox",
            ObjectHandle::array(vec![
                ObjectHandle::integer(0),
                ObjectHandle::integer(0),
                ObjectHandle::integer(1),
                ObjectHandle::integer(1),
            ]),
        )?;
        form_dict.replace_key(
            b"/Matrix",
            ObjectHandle::array(vec![
                ObjectHandle::real(0.5),
                ObjectHandle::integer(0),
                ObjectHandle::integer(0),
                ObjectHandle::real(0.5),
                ObjectHandle::integer(0),
                ObjectHandle::integer(0),
            ]),
        )?;
        form_dict.replace_key(b"/Resources", resources(vec![(b"/Im", image)]))?;
        pdf.mark_object_handle_dirty(&form_dict)?;

        add_page(
            &mut pdf,
            b"q 100 0 0 100 0 0 cm /Fm Do Q\n",
            resources(vec![(b"/Fm", form)]),
            None,
        )?;

        let collection = collect_image_placements(&mut pdf)?;
        let placements = collection.placements;
        let complete = collection.complete;
        assert!(complete);
        let placement = placements
            .get(&image_ref)
            .ok_or_else(|| Error::Invalid("nested image placement not collected".to_owned()))?;
        assert_eq!(placement.uses, 1);
        assert_eq!(placement.max_width_points, 100.0);
        assert_eq!(placement.max_height_points, 50.0);
        assert_eq!(placement.target_dimensions(450), (625, 313));
        Ok(())
    }

    #[test]
    fn page_user_unit_scales_physical_placement() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let image = image(&mut pdf, 1000, 1000)?;
        let image_ref = image
            .object_ref()
            .ok_or_else(|| Error::Invalid("fixture image is not indirect".to_owned()))?;
        add_page(
            &mut pdf,
            b"q 72 0 0 72 0 0 cm /Im Do Q\n",
            resources(vec![(b"/Im", image)]),
            Some(2.0),
        )?;

        let collection = collect_image_placements(&mut pdf)?;
        let placements = collection.placements;
        let complete = collection.complete;
        assert!(complete);
        let placement = placements
            .get(&image_ref)
            .ok_or_else(|| Error::Invalid("UserUnit image placement not collected".to_owned()))?;
        assert_eq!(placement.max_width_points, 144.0);
        assert_eq!(placement.max_height_points, 144.0);
        assert_eq!(placement.target_dimensions(300), (600, 600));
        Ok(())
    }
}
