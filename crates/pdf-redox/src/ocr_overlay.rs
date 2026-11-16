//! Embed selected full OCR text lines in reading-position-matched invisible
//! PDF text scopes. The original vector pages are not rasterized.

use crate::{
    EditDocument, Error, GlyphCatalog, ObjectHandle, OwnedDictionary, OwnedObject, Result,
    content::{
        decoded_content_value, effective_page_resources, install_page_resource, resolved_dictionary,
    },
    glyph_catalog::sha256_hex,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
};

const OCR_CONFIDENCE_DEFAULT: f64 = 0.97;
const MIN_COMPONENT_HITS: usize = 2;

/// Machine-readable OCR line predictions bound to one optimized PDF.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OcrDocument {
    /// Version of the OCR draft JSON schema.
    pub schema_version: u32,
    /// SHA-256 of the exact input optimized PDF.
    pub optimized_pdf_sha256: String,
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
    /// Detections lacking source outlined components.
    pub skipped_without_vector_coverage: usize,
    /// Unreviewed predictions intentionally excluded by default.
    pub skipped_unreviewed: usize,
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
    long > 0.25 && high > 0.25 && signed_area(quad) > 0.0625
}

fn ink_centers(catalog: &GlyphCatalog) -> BTreeMap<usize, Vec<[f64; 2]>> {
    let mut out = BTreeMap::<usize, Vec<[f64; 2]>>::new();
    for template in &catalog.glyphs {
        for occ in &template.occurrences {
            let quad = &occ.page_quad;
            out.entry(occ.page).or_default().push([
                quad.iter().map(|p| p[0]).sum::<f64>() / 4.,
                quad.iter().map(|p| p[1]).sum::<f64>() / 4.,
            ]);
        }
    }
    out
}

/// Count reusable outlined components in a detected OCR line, independent
/// of skew and page rotation. This rejects most already-native PDF text.
fn coverage(quad: &[[f64; 2]; 4], centers: &[[f64; 2]]) -> usize {
    let origin = quad[0];
    let ux = [quad[1][0] - origin[0], quad[1][1] - origin[1]];
    let uy = [quad[3][0] - origin[0], quad[3][1] - origin[1]];
    let det = ux[0].mul_add(uy[1], -ux[1] * uy[0]);
    if det.abs() < 1e-8 {
        return 0;
    }
    let h = uy[0].hypot(uy[1]);
    let padding = 0.5 / h + 0.25;
    centers
        .iter()
        .filter(|p| {
            let x = p[0] - origin[0];
            let y = p[1] - origin[1];
            let along = x.mul_add(uy[1], -y * uy[0]) / det;
            let across = ux[0].mul_add(y, -ux[1] * x) / det;
            (-0.01..=1.01).contains(&along) && (-padding..=1.0 + padding).contains(&across)
        })
        .count()
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

fn overlap(a: &[[f64; 2]; 4], b: &[[f64; 2]; 4]) -> f64 {
    // Efficient AABB overlap for duplicate *OCR* detections. Structural
    // page overlap is separately verified against PDF vector components.
    let (ax0, ax1) = (
        a.iter().map(|x| x[0]).fold(f64::INFINITY, f64::min),
        a.iter().map(|x| x[0]).fold(f64::NEG_INFINITY, f64::max),
    );
    let (ay0, ay1) = (
        a.iter().map(|x| x[1]).fold(f64::INFINITY, f64::min),
        a.iter().map(|x| x[1]).fold(f64::NEG_INFINITY, f64::max),
    );
    let (bx0, bx1) = (
        b.iter().map(|x| x[0]).fold(f64::INFINITY, f64::min),
        b.iter().map(|x| x[0]).fold(f64::NEG_INFINITY, f64::max),
    );
    let (by0, by1) = (
        b.iter().map(|x| x[1]).fold(f64::INFINITY, f64::min),
        b.iter().map(|x| x[1]).fold(f64::NEG_INFINITY, f64::max),
    );
    let intersection =
        (ax1.min(bx1) - ax0.max(bx0)).max(0.) * (ay1.min(by1) - ay0.max(by0)).max(0.);
    let a_area = (ax1 - ax0) * (ay1 - ay0);
    let b_area = (bx1 - bx0) * (by1 - by0);
    if a_area > 0. && b_area > 0. {
        intersection / a_area.min(b_area)
    } else {
        0.
    }
}

fn validate(
    ocr: &OcrDocument,
    catalog: &GlyphCatalog,
    pages: usize,
    options: OcrOverlayOptions,
) -> Result<()> {
    if ocr.schema_version != 1 || catalog.schema_version != 2 {
        return Err(invalid("unsupported OCR draft or glyph catalog schema"));
    }
    if ocr.optimized_pdf_sha256 != catalog.optimized_pdf_sha256 || ocr.model.trim().is_empty() {
        return Err(invalid(
            "OCR data does not match the exact glyph catalog or has no model identifier",
        ));
    }
    if !(0.0..=1.0).contains(&options.min_confidence) {
        return Err(invalid("OCR confidence threshold is outside [0,1]"));
    }
    let mut seen = BTreeSet::new();
    for page in &ocr.pages {
        if page.page == 0 || page.page > pages || !seen.insert(page.page) {
            return Err(invalid("unknown or duplicate OCR page number"));
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
/// Only accepted lines overlapping at least two existing reusable vector
/// components are included. Predictions not explicitly reviewed require an
/// opt-in and confidence threshold.
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
    catalog: &GlyphCatalog,
    ocr: &OcrDocument,
    options: OcrOverlayOptions,
) -> Result<(Vec<u8>, OcrOverlayReport)> {
    if sha256_hex(input) != catalog.optimized_pdf_sha256 {
        return Err(invalid(
            "OCR catalog does not belong to this exact optimized PDF",
        ));
    }
    let mut doc = EditDocument::from_bytes(input.to_vec())?;
    let handles = doc.page_handles()?;
    validate(ocr, catalog, handles.len(), options)?;
    let centers = ink_centers(catalog);
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
            let found = coverage(
                &line.quad,
                centers.get(&page.page).map_or(&[], Vec::as_slice),
            );
            if found < MIN_COMPONENT_HITS {
                report.skipped_without_vector_coverage += 1;
                continue;
            }
            if selected.iter().any(|x| overlap(&x.quad, &line.quad) > 0.60) {
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
            decoded_content_value(&doc, contents, &mut decoded)?;
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
    fn rotated_geometry_coverage_and_utf16_be() {
        let b = [[0., 0.], [0., 100.], [-10., 100.], [-10., 0.]];
        assert!(valid_box(&b));
        assert_eq!(coverage(&b, &[[-5., 20.], [-5., 80.], [10., 30.]]), 2);
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
