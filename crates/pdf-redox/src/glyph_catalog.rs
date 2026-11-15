//! Document-wide candidates for recognizing text represented by vector outlines.
//!
//! One geometrically reusable component is not necessarily one Unicode character:
//! combining marks, i-dots, and complex glyphs may occupy multiple components.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;

/// Exportable glyph-candidate dictionary from an actual Processing rewrite.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlyphCatalog {
    /// Sidecar format version.
    pub schema_version: u32,
    /// SHA-256 of the exact serialized optimized PDF that owns these resources.
    pub optimized_pdf_sha256: String,
    /// Canonical reusable vector shapes with all occurrence placements.
    pub glyphs: Vec<GlyphTemplate>,
}

/// A single accepted Type3 shape, which may only be part of a text character.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlyphTemplate {
    /// SHA-256 of the exact normalized Type3 `CharProc`, independent of resource name.
    pub id: String,
    /// Resource key used for this glyph in the resulting PDF page resource dictionaries.
    pub font_resource: String,
    /// The one-byte code of this glyph in the generated Type3 font.
    pub code: u8,
    /// The exact reusable Type3 `CharProc` PDF drawing instructions.
    pub charproc: String,
    /// Unit-square SVG path data in PDF local coordinates (Y positive upward).
    pub svg_path: String,
    /// SVG fill-rule value (nonzero or evenodd).
    pub fill_rule: String,
    /// Original painted occurrences of this template.
    pub occurrences: Vec<GlyphOccurrence>,
}

/// The placed bounding-box estimate of one component in the source content.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlyphOccurrence {
    /// One-based PDF page index.
    pub page: usize,
    /// Zero-based ordinal of this same glyph within the PDF page content.
    pub index_on_page: usize,
    /// Component bounds before the PDF current transformation matrix.
    pub local_bbox: [f64; 4],
    /// The effective affine PDF matrix at the component's original paint instruction.
    pub ctm: [f64; 6],
    /// Transformed local bounding-box corners, ordered bottom-left, bottom-right,
    /// top-right, top-left, in PDF user-space coordinates.
    pub page_quad: [[f64; 2]; 4],
}

impl Default for GlyphCatalog {
    fn default() -> Self {
        Self {
            schema_version: 2,
            optimized_pdf_sha256: String::new(),
            glyphs: Vec::new(),
        }
    }
}

/// A verified semantic label set for the exact PDF described by a catalog.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlyphLabels {
    /// Label-set format version.
    pub schema_version: u32,
    /// Character strings applying to all appearances of a particular shape.
    pub labels: BTreeMap<String, String>,
    /// Per-occurrence corrections for contextual ambiguities.
    #[serde(default)]
    pub overrides: Vec<GlyphLabelOverride>,
}

/// Override one appearance without changing the label of the underlying shape.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GlyphLabelOverride {
    /// Canonical template ID.
    pub glyph_id: String,
    /// One-based PDF page index.
    pub page: usize,
    /// Zero-based ordinal among same-template appearances on that page.
    pub index_on_page: usize,
    /// Unicode replacement text, or empty to suppress a template-level label.
    pub text: String,
}

/// Summary of Unicode labels applied to a processed PDF.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GlyphLabelReport {
    /// Number of Type3 occurrences matched to the catalog.
    pub matched_occurrences: usize,
    /// Number of occurrences given nonempty searchable text.
    pub labeled_occurrences: usize,
    /// Number of pages with at least one newly inserted semantic label.
    pub rewritten_pages: usize,
}

/// Lowercase SHA-256 of exact bytes, used to tie semantic edits to a PDF.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let digest = Sha256::digest(bytes);
    let mut text = String::with_capacity(64);
    for byte in digest {
        let _ = write!(&mut text, "{byte:02x}");
    }
    text
}
