use crate::geometry::Matrix;
use crate::images::ImageResizeTarget;
use crate::{
    Result,
    content::{
        object_subtype, page_content, page_user_unit, page_xobject_bindings, resolved_number_array,
        resolved_number_value, resource_xobject_bindings,
    },
};
use num_traits::ToPrimitive;
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
            if !desired.is_finite() || desired >= f64::from(pixels) {
                return pixels;
            }
            if desired <= 1.0 {
                return 1.min(pixels);
            }
            desired.to_u32().unwrap_or(pixels)
        }

        (
            target_axis(self.max_width_points, self.width_px, target_ppi),
            target_axis(self.max_height_points, self.height_px, target_ppi),
        )
    }
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
            draws: Vec::new(),
            complete,
        }
    }

    fn instruction(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let operand_count = instruction.operand_spans().len();
        match &instruction.operator[..] {
            b"q" => {
                if operand_count != 0 {
                    self.complete = false;
                }
                self.stack.push(self.ctm);
            }
            b"Q" => {
                if operand_count != 0 {
                    self.complete = false;
                }
                if let Some(ctm) = self.stack.pop() {
                    self.ctm = ctm;
                } else {
                    self.complete = false;
                }
            }
            b"cm" => {
                if let Some(values) =
                    crate::content_stream::instruction_number_array::<6>(instruction)
                {
                    self.ctm.concat(Matrix::from(values));
                } else {
                    self.complete = false;
                }
            }
            b"Do" => {
                if operand_count != 1 {
                    self.complete = false;
                    return;
                }
                let Some(name) = instruction
                    .operands()
                    .next()
                    .and_then(crate::content_stream::operand_name)
                else {
                    self.complete = false;
                    return;
                };
                if let Some(target) = self.xobjects.get(name).copied() {
                    self.draws.push(CowDrawEvent {
                        target,
                        ctm: self.ctm,
                    });
                } else {
                    self.complete = false;
                }
            }
            _ => {}
        }
    }

    fn scan(&mut self, content: &[u8]) -> crate::Result<()> {
        let incomplete = crate::content_stream::visit_instructions(content, |instruction| {
            self.instruction(instruction);
            Ok(())
        })?;
        if incomplete || !self.stack.is_empty() {
            self.complete = false;
        }
        Ok(())
    }
}

fn cow_form_matrix(
    document: &crate::EditDocument,
    value: Option<crate::source::CurrentValue<'_>>,
) -> Result<Option<Matrix>> {
    let Some(value) = value else {
        return Ok(Some(Matrix::default()));
    };
    let Some(values) = resolved_number_array::<6>(document, value)? else {
        return Ok(None);
    };
    Ok(values
        .into_iter()
        .all(f64::is_finite)
        .then(|| Matrix::from(values)))
}

fn cow_image_dimensions(
    document: &crate::EditDocument,
    object: &crate::source::CurrentValue<'_>,
) -> Result<Option<(u32, u32)>> {
    let Some(width) = object.dictionary_entry(b"Width") else {
        return Ok(None);
    };
    let Some(height) = object.dictionary_entry(b"Height") else {
        return Ok(None);
    };
    let Some(width) = resolved_number_value(document, width)? else {
        return Ok(None);
    };
    let Some(height) = resolved_number_value(document, height)? else {
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
    Ok(width.to_u32().zip(height.to_u32()))
}

fn cow_record_image(
    document: &crate::EditDocument,
    placements: &mut HashMap<crate::ObjectHandle, ImagePlacement>,
    handle: crate::ObjectHandle,
    ctm: Matrix,
    complete: &mut bool,
) -> Result<()> {
    let width_points = ctm.a.hypot(ctm.b);
    let height_points = ctm.c.hypot(ctm.d);
    if !width_points.is_finite() || !height_points.is_finite() {
        *complete = false;
        return Ok(());
    }
    if width_points <= MIN_PLACEMENT_POINTS || height_points <= MIN_PLACEMENT_POINTS {
        return Ok(());
    }
    if let Some(placement) = placements.get_mut(&handle) {
        placement.max_width_points = placement.max_width_points.max(width_points);
        placement.max_height_points = placement.max_height_points.max(height_points);
        placement.uses = placement.uses.saturating_add(1);
        return Ok(());
    }
    let Some(object) = document.current_value(handle)? else {
        return Ok(());
    };
    let Some((width_px, height_px)) = cow_image_dimensions(document, &object)? else {
        return Ok(());
    };
    placements.insert(
        handle,
        ImagePlacement {
            width_px,
            height_px,
            max_width_points: width_points,
            max_height_points: height_points,
            uses: 1,
        },
    );
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
        let Some(object) = document.current_value(form)? else {
            *state.complete = false;
            return Ok(());
        };
        if !object.has_dictionary() {
            *state.complete = false;
            return Ok(());
        }
        let mut base_ctm = outer_ctm;
        let Some(matrix) = cow_form_matrix(document, object.dictionary_entry(b"Matrix"))? else {
            *state.complete = false;
            return Ok(());
        };
        base_ctm.concat(matrix);
        let (xobjects, scope_complete) =
            if let Some(resources) = object.dictionary_entry(b"Resources") {
                let bindings = resource_xobject_bindings(document, Some(resources))?;
                (bindings.handles, bindings.complete)
            } else {
                (inherited_xobjects.clone(), true)
            };
        if !scope_complete {
            *state.complete = false;
        }
        let content = document.decoded_content_stream_data(form)?;
        let mut scanner = CowPlacementScanner::new(xobjects, base_ctm, scope_complete);
        if scanner.scan(&content).is_err() || !scanner.complete {
            *state.complete = false;
        }
        let current = scanner.xobjects.clone();
        for draw in scanner.draws {
            match object_subtype(document, draw.target)?.as_deref() {
                Some(b"Image") => {
                    cow_record_image(
                        document,
                        state.placements,
                        draw.target,
                        draw.ctm,
                        state.complete,
                    )?;
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
    let Some(object) = document.current_owned_object_cow(image)? else {
        return Ok(false);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(false);
    };
    let encoding = if jpeg {
        crate::images::ImageResizeEncoding::Jpeg
    } else {
        crate::images::ImageResizeEncoding::Flate
    };
    crate::images::resize_source_safe(document, dictionary, encoding)
}

pub fn plan_print_downsampling(
    document: &crate::EditDocument,
    pages: &[crate::ObjectHandle],
    target_ppi: u32,
) -> Result<PrintPlanHayro> {
    let mut placements = HashMap::new();
    let mut direct_page_bindings =
        HashMap::<crate::ObjectHandle, HashSet<crate::ObjectHandle>>::new();
    let mut complete = true;
    for &page in pages {
        let bindings = page_xobject_bindings(document, page)?;
        let (xobjects, scope_complete) = (bindings.handles, bindings.complete);
        if !scope_complete {
            complete = false;
        }
        for handle in xobjects.values() {
            if object_subtype(document, *handle)?.as_deref() == Some(b"Image") {
                direct_page_bindings
                    .entry(*handle)
                    .or_default()
                    .insert(page);
            }
        }
        let mut base_ctm = Matrix::default();
        let user_unit = page_user_unit(document, page)?.unwrap_or_else(|| {
            complete = false;
            1.0
        });
        base_ctm.scale(user_unit, user_unit);
        let content = page_content(document, page)?;
        let mut scanner = CowPlacementScanner::new(xobjects, base_ctm, scope_complete);
        if scanner.scan(&content).is_err() || !scanner.complete {
            complete = false;
        }
        let current = scanner.xobjects.clone();
        let mut form_stack = HashSet::new();
        for draw in scanner.draws {
            match object_subtype(document, draw.target)?.as_deref() {
                Some(b"Image") => {
                    cow_record_image(
                        document,
                        &mut placements,
                        draw.target,
                        draw.ctm,
                        &mut complete,
                    )?;
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
        plan.stats.image_uses = plan.stats.image_uses.saturating_add(placement.uses);
        let (target_width, target_height) = placement.target_dimensions(target_ppi);
        let source_pixels = u64::from(placement.width_px) * u64::from(placement.height_px);
        let target_pixels = u64::from(target_width) * u64::from(target_height);
        let downsample = target_width < placement.width_px || target_height < placement.height_px;
        if downsample {
            plan.stats.downsample_candidates += 1;
        }
        plan.stats.source_pixels = plan.stats.source_pixels.saturating_add(source_pixels);
        plan.stats.target_pixels = plan.stats.target_pixels.saturating_add(target_pixels);
        let clears_gate = u128::from(target_pixels) * 100
            <= u128::from(source_pixels) * u128::from(100 - MIN_DOWNSAMPLE_PIXEL_REDUCTION_PERCENT);
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
        if let Some(pages) = direct_page_bindings.get(&image) {
            for &page in pages {
                plan.resize_targets.insert((page, image), target);
            }
        }
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nonfinite_scaled_extent_never_downsamples_to_one_pixel() {
        let placement = ImagePlacement {
            width_px: 1000,
            height_px: 500,
            max_width_points: f64::MAX,
            max_height_points: 72.0,
            uses: 1,
        };
        assert_eq!(placement.target_dimensions(300), (1000, 300));
    }

    fn fixture(user_unit: &[u8], form_matrix: &[u8]) -> Result<Vec<u8>> {
        let mut pdf = crate::test_support::ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        let mut page = b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 300] /Resources << /XObject << /Im0 5 0 R /Fm0 6 0 R >> >> /Contents 4 0 R ".to_vec();
        page.extend_from_slice(user_unit);
        page.extend_from_slice(b" >>");
        pdf.object(3, &page)?;
        pdf.stream(
            4,
            b"",
            b"q 12 0 0 12 0 0 cm /Im0 Do Q q 24 0 0 24 0 0 cm /Fm0 Do Q",
        )?;
        pdf.stream(5, b"/Type /XObject /Subtype /Image /Width 1000 /Height 1000 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode", b"test")?;
        let mut form = b"/Type /XObject /Subtype /Form /BBox [0 0 1 1] ".to_vec();
        form.extend_from_slice(form_matrix);
        pdf.stream(6, &form, b"/Im0 Do")?;
        pdf.finish(1)
    }

    #[test]
    fn print_plan_tracks_maximum_shared_image_placement_and_invalid_geometry() -> Result<()> {
        let document =
            crate::EditDocument::from_bytes(fixture(b"/UserUnit 2", b"/Matrix [1 0 0 1 0 0]")?)?;
        let pages = document.page_handles()?;
        let plan = plan_print_downsampling(&document, &pages, 600)?;
        assert!(plan.stats.geometry_complete);
        assert_eq!(plan.stats.image_uses, 2);
        assert_eq!(plan.resize_targets.len(), 1);
        assert_eq!(
            plan.resize_targets.values().next(),
            Some(&ImageResizeTarget::flate(400, 400))
        );
        for (user_unit, form_matrix) in [
            (
                b"/UserUnit 0".as_slice(),
                b"/Matrix [1 0 0 1 0 0]".as_slice(),
            ),
            (b"/UserUnit 2".as_slice(), b"/Matrix [1 0]".as_slice()),
        ] {
            let document = crate::EditDocument::from_bytes(fixture(user_unit, form_matrix)?)?;
            let plan = plan_print_downsampling(&document, &document.page_handles()?, 600)?;
            assert!(!plan.stats.geometry_complete);
            assert!(plan.resize_targets.is_empty());
        }
        Ok(())
    }
}
