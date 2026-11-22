//! Embed selected full OCR text lines in reading-position-matched invisible
//! PDF text scopes. The original vector pages are not rasterized.

use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result,
    content::{
        decoded_content_value, effective_page_resources, install_page_resource, resolved_dictionary,
    },
    digest::sha256_hex,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

const OCR_CONFIDENCE_DEFAULT: f64 = 0.97;

/// Machine-readable OCR line predictions bound to one optimized PDF.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrDocument {
    /// Version of the OCR draft JSON schema.
    pub schema_version: u32,
    /// SHA-256 of the exact PDF to annotate, whether optimized or original.
    pub pdf_sha256: String,
    /// Provenance identifier of the OCR detection and recognition model.
    pub model: String,
    /// Page-specific OCR predictions.
    pub pages: Vec<OcrPage>,
}

/// OCR predictions on one one-based PDF page.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrPage {
    /// One-based PDF page index, not a raster image number.
    pub page: usize,
    /// Predictions on this page, with individual review and confidence status.
    pub lines: Vec<OcrLine>,
    /// Existing native searchable words, in PDF user-space BL/BR/TR/TL
    /// quads. Lines overlapping these words are excluded to avoid
    /// duplicating existing text, including on mixed native/vector pages.
    pub native_text_regions: Vec<[[f64; 2]; 4]>,
}

/// One OCR-detected line with text and its original vector-page placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrLine {
    /// Recognized text in source reading order within this line.
    pub text: String,
    /// Model confidence between zero and one.
    pub confidence: f64,
    /// PDF-user-space corners of the OCR text baseline box:
    /// bottom-left, bottom-right, top-right, top-left.
    pub quad: [[f64; 2]; 4],
    #[serde(default)]
    /// True only when this particular prediction has been reviewed.
    pub reviewed: bool,
}

/// Counts of semantic OCR predictions accepted and skipped on a document.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OcrOverlayReport {
    /// Total supplied OCR lines.
    pub scanned_lines: usize,
    /// Lines selected to become searchable text.
    pub accepted_lines: usize,
    /// Unreviewed predictions below the configured confidence.
    pub skipped_low_confidence: usize,
    /// Unreviewed predictions intentionally excluded by default.
    pub skipped_unreviewed: usize,
    /// Lines overlapping existing searchable PDF words (including mixed lines).
    pub skipped_native_text_overlap: usize,
    /// Overlapping OCR detections suppressed after higher-confidence selections.
    pub skipped_duplicate_lines: usize,
    /// Pages receiving a text overlay.
    pub pages_rewritten: usize,
    /// Unicode scalar values inserted in accepted lines.
    pub searchable_characters: usize,
}

/// Controls whether reviewed and high-confidence automatic lines are inserted.
#[derive(Debug, Clone, Copy)]
pub struct OcrOverlayOptions {
    /// Permit unreviewed model predictions above `min_confidence`.
    pub apply_unreviewed: bool,
    /// Minimum required confidence for automatic unreviewed lines.
    pub min_confidence: f64,
}

impl Default for OcrOverlayOptions {
    fn default() -> Self {
        Self {
            apply_unreviewed: false,
            min_confidence: OCR_CONFIDENCE_DEFAULT,
        }
    }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

fn signed_area(points: &[[f64; 2]; 4]) -> f64 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(4)
        .map(|(a, b)| a[0].mul_add(b[1], -b[0] * a[1]))
        .sum::<f64>()
        / 2.0
}

fn valid_box(quad: &[[f64; 2]; 4]) -> bool {
    if quad
        .iter()
        .flatten()
        .any(|v| !v.is_finite() || v.abs() > 1.0e8)
    {
        return false;
    }
    let v = [quad[1][0] - quad[0][0], quad[1][1] - quad[0][1]];
    let h = [quad[3][0] - quad[0][0], quad[3][1] - quad[0][1]];
    let long = v[0].hypot(v[1]);
    let high = h[0].hypot(h[1]);
    long > 0.25
        && high > 0.25
        && signed_area(quad) > 0.0625
        // Every visible OCR text region must be a strictly convex,
        // counter-clockwise quadrilateral. A self-crossing or concave box
        // cannot be used for meaningful polygon intersection.
        && (0..4).all(|i| cross(quad[i], quad[(i + 1) % 4], quad[(i + 2) % 4]) > 1e-9)
}

fn actual_text_hex(value: &str) -> String {
    let mut out = String::from("<FEFF");
    for code in value.encode_utf16() {
        let _ = write!(&mut out, "{code:04X}");
    }
    out.push('>');
    out
}

fn render_invisible_line(out: &mut String, font: &str, line: &OcrLine) {
    let p = &line.quad;
    let xx = (p[1][0] - p[0][0]) / 0.667;
    let yx = (p[1][1] - p[0][1]) / 0.667;
    let xy = p[3][0] - p[0][0];
    let yy = p[3][1] - p[0][1];
    let text = actual_text_hex(&line.text);
    let _ = writeln!(
        out,
        "q\n/Span << /ActualText {text} >> BDC\nBT /{font} 1 Tf 3 Tr 0 Tc 0 Tw 0 Ts 100 Tz {xx:.8} {yx:.8} {xy:.8} {yy:.8} {:.8} {:.8} Tm (X) Tj ET\nEMC\nQ",
        p[0][0], p[0][1]
    );
}

fn cross(a: [f64; 2], b: [f64; 2], p: [f64; 2]) -> f64 {
    let dx = b[0] - a[0];
    let dy = b[1] - a[1];
    dx.mul_add(p[1] - a[1], -dy * (p[0] - a[0]))
}

fn polygon_area(points: &[[f64; 2]]) -> f64 {
    if points.len() < 3 {
        return 0.0;
    }
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| a[0].mul_add(b[1], -a[1] * b[0]))
        .sum::<f64>()
        .abs()
        / 2.0
}

fn overlap(a: &[[f64; 2]; 4], b: &[[f64; 2]; 4]) -> f64 {
    // The AABB is ONLY a cheap rejection test. Using AABB *area* as an
    // overlap score incorrectly suppresses neighboring rotated labels:
    // their bounding boxes intersect even if their glyph baselines do not.
    for axis in 0..2 {
        let a0 = a.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min);
        let a1 = a.iter().map(|p| p[axis]).fold(f64::NEG_INFINITY, f64::max);
        let b0 = b.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min);
        let b1 = b.iter().map(|p| p[axis]).fold(f64::NEG_INFINITY, f64::max);
        if a1.min(b1) <= a0.max(b0) {
            return 0.0;
        }
    }

    // Sutherland-Hodgman clipping of convex counter-clockwise source
    // quadrilaterals (guaranteed by valid_box). Compute the exact polygon
    // intersection area, normalized to the smaller source region.
    let mut polygon = a.to_vec();
    for i in 0..4 {
        if polygon.is_empty() {
            return 0.0;
        }
        let left = b[i];
        let right = b[(i + 1) % 4];
        let mut clipped = Vec::with_capacity(polygon.len() + 2);
        let Some(&mut_previous) = polygon.last() else {
            return 0.0;
        };
        let mut previous = mut_previous;
        let mut previous_cross = cross(left, right, previous);
        for &point in &polygon {
            let current_cross = cross(left, right, point);
            let previous_inside = previous_cross >= 0.0;
            let current_inside = current_cross >= 0.0;
            if previous_inside != current_inside {
                let t = previous_cross / (previous_cross - current_cross);
                clipped.push([
                    previous[0].mul_add(1.0 - t, point[0] * t),
                    previous[1].mul_add(1.0 - t, point[1] * t),
                ]);
            }
            if current_inside {
                clipped.push(point);
            }
            previous = point;
            previous_cross = current_cross;
        }
        polygon = clipped;
    }
    let intersection = polygon_area(&polygon);
    let min_area = signed_area(a).min(signed_area(b));
    if min_area > 0.0 {
        (intersection / min_area).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// Decide whether two source-space convex quadrilaterals overlap by a
/// meaningful amount, regardless of PDF rotation or line skew.
fn native_intersection(a: &[[f64; 2]; 4], b: &[[f64; 2]; 4]) -> bool {
    // Cheap broadphase for typical horizontal layouts.
    for axis in 0..2 {
        let a_min = a.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min);
        let a_max = a.iter().map(|p| p[axis]).fold(f64::NEG_INFINITY, f64::max);
        let b_min = b.iter().map(|p| p[axis]).fold(f64::INFINITY, f64::min);
        let b_max = b.iter().map(|p| p[axis]).fold(f64::NEG_INFINITY, f64::max);
        if a_max.min(b_max) - a_min.max(b_min) <= 0.2 {
            return false;
        }
    }
    // Separating-axis theorem. All OCR and text quads are convex.
    for polygon in [a, b] {
        for i in 0..4 {
            let j = (i + 1) % 4;
            let dx = polygon[j][0] - polygon[i][0];
            let dy = polygon[j][1] - polygon[i][1];
            let length = dx.hypot(dy);
            if length <= 1e-12 {
                return false;
            }
            let normal = [-dy / length, dx / length];
            let extremes = |poly: &[[f64; 2]; 4]| -> (f64, f64) {
                poly.iter()
                    .map(|p| p[0].mul_add(normal[0], p[1] * normal[1]))
                    .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), value| {
                        (lo.min(value), hi.max(value))
                    })
            };
            let (a0, a1) = extremes(a);
            let (b0, b1) = extremes(b);
            if a1.min(b1) - a0.max(b0) <= 0.2 {
                return false;
            }
        }
    }
    true
}

fn validate(ocr: &OcrDocument, pages: usize, options: OcrOverlayOptions) -> Result<()> {
    if ocr.schema_version != 3 {
        return Err(invalid("unsupported OCR detection draft schema"));
    }
    if ocr.model.trim().is_empty() {
        return Err(invalid("OCR model provenance must not be empty"));
    }
    if !(0.0..=1.0).contains(&options.min_confidence) {
        return Err(invalid("OCR confidence threshold is outside [0,1]"));
    }
    let mut seen = BTreeSet::new();
    for page in &ocr.pages {
        if page.page == 0 || page.page > pages || !seen.insert(page.page) {
            return Err(invalid("unknown or duplicate OCR page number"));
        }
        if page.native_text_regions.iter().any(|quad| !valid_box(quad)) {
            return Err(invalid(format!(
                "invalid native PDF word geometry on page {}",
                page.page
            )));
        }
        for line in &page.lines {
            if !valid_box(&line.quad)
                || !(0.0..=1.0).contains(&line.confidence)
                || line.text.chars().any(char::is_control)
                || line.text.chars().count() > 4096
            {
                return Err(invalid(format!(
                    "invalid OCR text or geometry on page {}",
                    page.page
                )));
            }
        }
    }
    Ok(())
}

/// Add OCR text lines to the exact optimized PDF without altering its painting.
///
/// Recognition is independent of vector grouping, PDF font types and image
/// compression. OCR text regions can describe ungrouped outlines, images, or
/// one-off labels. All lines are subject to the native-searchable-text overlap
/// check; unreviewed model predictions additionally need explicit opt-in
/// and a confidence threshold.
///
/// # Errors
///
/// Rejects stale documents, invalid OCR geometry/labels, and malformed PDFs.
#[expect(
    clippy::too_many_lines,
    reason = "validated OCR line selection and PDF font/resource writes form one document transaction"
)]
pub fn apply_ocr_overlay(
    input: &[u8],
    ocr: &OcrDocument,
    options: OcrOverlayOptions,
) -> Result<(Vec<u8>, OcrOverlayReport)> {
    if sha256_hex(input) != ocr.pdf_sha256 {
        return Err(invalid("OCR draft belongs to a different PDF"));
    }
    let mut doc = EditDocument::from_bytes(input.to_vec())?;
    let handles = doc.page_handles()?;
    validate(ocr, handles.len(), options)?;
    let mut report = OcrOverlayReport::default();
    let mut accepted = BTreeMap::<usize, Vec<&OcrLine>>::new();
    for page in &ocr.pages {
        let mut candidates = page.lines.iter().collect::<Vec<_>>();
        candidates.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
        let mut selected = Vec::<&OcrLine>::new();
        for line in candidates {
            report.scanned_lines += 1;
            if line.text.trim().is_empty() {
                continue;
            }
            if !line.reviewed && !options.apply_unreviewed {
                report.skipped_unreviewed += 1;
                continue;
            }
            if !line.reviewed && line.confidence < options.min_confidence {
                report.skipped_low_confidence += 1;
                continue;
            }
            if page
                .native_text_regions
                .iter()
                .any(|native| native_intersection(&line.quad, native))
            {
                report.skipped_native_text_overlap += 1;
                continue;
            }
            if selected.iter().any(|x| overlap(&x.quad, &line.quad) > 0.60) {
                report.skipped_duplicate_lines += 1;
                continue;
            }
            selected.push(line);
        }
        if !selected.is_empty() {
            selected.sort_by(|a, b| {
                let top_a = a.quad[2][1].max(a.quad[3][1]);
                let top_b = b.quad[2][1].max(b.quad[3][1]);
                top_b
                    .total_cmp(&top_a)
                    .then_with(|| a.quad[0][0].total_cmp(&b.quad[0][0]))
            });
            report.accepted_lines += selected.len();
            report.searchable_characters += selected
                .iter()
                .map(|x| x.text.chars().count())
                .sum::<usize>();
            accepted.insert(page.page, selected);
        }
    }
    if accepted.is_empty() {
        return Ok((input.to_vec(), report));
    }

    // One fresh Helvetica resource, only installed on pages receiving OCR.
    let mut occupied = BTreeSet::new();
    for &page in &handles {
        let resources = effective_page_resources(&doc, page)?;
        if let Some(fonts) = resolved_dictionary(&doc, resources.get(b"Font".as_slice()))? {
            occupied.extend(fonts.keys().cloned());
        }
    }
    let name = (0..1_000_000)
        .map(|n| format!("PdfRedoxOcr{n}"))
        .find(|s| !occupied.contains(s.as_bytes()))
        .ok_or_else(|| invalid("no free OCR font resource name"))?;
    let mut font = OwnedDictionary::new();
    font.insert(b"Type".to_vec(), OwnedObject::Name(b"Font".to_vec()));
    font.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Type1".to_vec()));
    font.insert(
        b"BaseFont".to_vec(),
        OwnedObject::Name(b"Helvetica".to_vec()),
    );
    font.insert(
        b"Encoding".to_vec(),
        OwnedObject::Name(b"WinAnsiEncoding".to_vec()),
    );
    let font_handle = ObjectHandle::New(doc.add_object(OwnedObject::Dictionary(font)));
    for (number, lines) in &accepted {
        let handle = handles[number - 1];
        let mut overlay = String::new();
        for line in lines {
            render_invisible_line(&mut overlay, &name, line);
        }
        let Some(page_object) = doc.current_owned_object(handle)? else {
            return Err(invalid("page not available"));
        };
        let Some(dict) = page_object.as_dictionary() else {
            return Err(invalid("invalid page"));
        };
        let mut decoded = Vec::new();
        if let Some(contents) = dict.get(b"Contents".as_slice()) {
            decoded_content_value(
                &doc,
                crate::source::CurrentValue::Owned(contents),
                &mut decoded,
            )?;
        }
        // Insert before original painting so overlays inherit neither the
        // source's potentially unbalanced CTM nor its text rendering mode.
        let mut new_bytes = overlay.into_bytes();
        new_bytes.extend_from_slice(b"\n");
        new_bytes.extend(decoded);
        crate::content::replace_page_content_flate(
            &mut doc,
            handle,
            &new_bytes,
            crate::FlateLevel::PROCESSING,
        )?;
        install_page_resource(
            &mut doc,
            handle,
            b"Font",
            name.as_bytes().to_vec(),
            font_handle,
        )?;
        report.pages_rewritten += 1;
    }
    Ok((
        crate::writer::write_pdf_with_object_streams(&doc, crate::FlateLevel::PROCESSING)?,
        report,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_overlap_uses_rotated_polygons_not_axis_aligned_boxes() {
        // Three adjacent lines on an actual rotated product photograph.
        // Their AABBs overlap substantially, but their glyph polygons do
        // not: rejecting two of them made genuine raster text disappear.
        let texts = [
            [
                [148.904_439_606_741_56, 682.442_421_629_213_5],
                [190.742_592_977_528_1, 712.258_117_134_831_4],
                [184.971_813_202_247_18, 720.433_388_483_146],
                [143.133_659_831_460_68, 690.617_692_977_528_1],
            ],
            [
                [159.724_651_685_393_27, 677.152_540_168_539_3],
                [201.562_805_056_179_8, 707.208_684_831_460_7],
                [196.272_923_595_505_6, 714.662_608_707_865_2],
                [154.434_770_224_719_1, 684.606_464_044_943_8],
            ],
            [
                [175.353_846_910_112_37, 674.267_150_280_898_9],
                [212.623_466_292_134_82, 700.957_006_741_573_1],
                [207.814_483_146_067_4, 707.930_032_303_370_8],
                [170.544_863_764_044_95, 681.240_175_842_696_7],
            ],
        ];
        for quad in &texts {
            assert!(valid_box(quad));
        }
        for i in 0..texts.len() {
            for j in i + 1..texts.len() {
                assert!(
                    overlap(&texts[i], &texts[j]) < 0.60,
                    "adjacent rotated labels {i} and {j} were mistaken for duplicates"
                );
            }
        }

        let horizontal = [[0., 0.], [100., 0.], [100., 20.], [0., 20.]];
        let shifted = [[50., 0.], [150., 0.], [150., 20.], [50., 20.]];
        let remote = [[200., 0.], [300., 0.], [300., 20.], [200., 20.]];
        assert!((overlap(&horizontal, &horizontal) - 1.).abs() < 1e-9);
        assert!((overlap(&horizontal, &shifted) - 0.5).abs() < 1e-9);
        assert!(overlap(&horizontal, &remote).abs() < 1e-9);
        let contained = [[25., 4.], [75., 4.], [75., 16.], [25., 16.]];
        assert!((overlap(&horizontal, &contained) - 1.).abs() < 1e-9);

        let concave = [[0., 0.], [10., 0.], [3., 1.], [0., 8.]];
        assert!(!valid_box(&concave));
        let bow_tie = [[0., 0.], [10., 0.], [0., 10.], [10., 10.]];
        assert!(!valid_box(&bow_tie));
    }

    #[test]
    fn applies_only_non_native_lines_on_multiple_pages() -> Result<()> {
        use crate::{content::page_content, digest::sha256_hex, test_support::ClassicPdfBuilder};
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        for page in 3..=4 {
            let body = format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents {} 0 R >>",
                page + 2
            );
            pdf.object(page, body.as_bytes())?;
        }
        // Neither page has fonts, images or reusable glyphs. The OCR document
        // is independently authenticated by its source-PDF hash.
        let ink = b"q 0 0 5 8 re 20 0 5 8 re f Q";
        pdf.stream(5, b"", ink)?;
        pdf.stream(6, b"", ink)?;
        let input = pdf.finish(1)?;
        let same_line = OcrLine {
            text: "你好 ABC".to_owned(),
            confidence: 0.98,
            quad: [[0., 0.], [35., 0.], [35., 10.], [0., 10.]],
            reviewed: false,
        };
        let mut ocr = OcrDocument {
            schema_version: 3,
            pdf_sha256: sha256_hex(&input),
            model: "test model".to_owned(),
            pages: vec![
                OcrPage {
                    page: 1,
                    lines: vec![same_line.clone()],
                    native_text_regions: vec![[[8., 1.], [18., 1.], [18., 9.], [8., 9.]]],
                },
                OcrPage {
                    page: 2,
                    lines: vec![same_line],
                    native_text_regions: Vec::new(),
                },
            ],
        };
        let defaults = apply_ocr_overlay(&input, &ocr, OcrOverlayOptions::default())?;
        assert_eq!(defaults.0, input);
        assert_eq!(defaults.1.skipped_unreviewed, 2);
        let options = OcrOverlayOptions {
            apply_unreviewed: true,
            min_confidence: 0.95,
        };
        let (annotated, stats) = apply_ocr_overlay(&input, &ocr, options)?;
        assert_eq!(stats.accepted_lines, 1);
        assert_eq!(stats.skipped_native_text_overlap, 1);
        assert_eq!(stats.pages_rewritten, 1);
        assert_eq!(stats.searchable_characters, 6);
        let output = EditDocument::from_bytes(annotated)?;
        let pages = output.page_handles()?;
        assert_eq!(page_content(&output, pages[0])?, ink);
        let changed = page_content(&output, pages[1])?;
        assert!(
            changed
                .windows(b"/ActualText <FEFF".len())
                .any(|part| part == b"/ActualText <FEFF")
        );
        assert!(changed.ends_with(ink));
        ocr.pdf_sha256 = "wrong digest".to_owned();
        assert!(apply_ocr_overlay(&input, &ocr, options).is_err());
        Ok(())
    }

    #[test]
    fn native_text_overlap_rejects_mixed_lines_but_not_nearby_columns() {
        let line = [[40., 100.], [210., 100.], [210., 115.], [40., 115.]];
        let native = [[110., 101.], [140., 101.], [140., 113.], [110., 113.]];
        let adjacent = [[216., 101.], [290., 101.], [290., 113.], [216., 113.]];
        let touching = [[210., 101.], [215., 101.], [215., 113.], [210., 113.]];
        assert!(native_intersection(&line, &native));
        assert!(!native_intersection(&line, &adjacent));
        assert!(!native_intersection(&line, &touching));
        let rotated = [[80., 80.], [80., 180.], [65., 180.], [65., 80.]];
        let native_rotated = [[65., 125.], [80., 125.], [80., 145.], [65., 145.]];
        assert!(native_intersection(&rotated, &native_rotated));
    }

    #[test]
    fn rotated_geometry_and_utf16_be() {
        let b = [[0., 0.], [0., 100.], [-10., 100.], [-10., 0.]];
        assert!(valid_box(&b));
        assert_eq!(actual_text_hex("中A"), "<FEFF4E2D0041>");
        let mut s = String::new();
        render_invisible_line(
            &mut s,
            "PdfRedoxOcr0",
            &OcrLine {
                text: "中A".into(),
                confidence: 1.,
                quad: b,
                reviewed: true,
            },
        );
        assert!(s.contains("3 Tr"));
        assert!(s.contains("/ActualText <FEFF4E2D0041>"));
    }
}
