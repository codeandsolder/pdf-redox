//! Document-wide candidates for recognizing text represented by vector outlines.
//!
//! One geometrically reusable component is not necessarily one Unicode character:
//! combining marks, i-dots, and complex glyphs may occupy multiple components.

use serde::Serialize;

/// Exportable glyph-candidate dictionary from an actual Processing rewrite.
#[derive(Debug, Clone, Serialize)]
pub struct GlyphCatalog {
    /// Sidecar format version.
    pub schema_version: u32,
    /// Canonical reusable vector shapes with all occurrence placements.
    pub glyphs: Vec<GlyphTemplate>,
}

/// A single accepted Type3 shape, which may only be part of a text character.
#[derive(Debug, Clone, Serialize)]
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
#[derive(Debug, Clone, Serialize)]
pub struct GlyphOccurrence {
    /// One-based PDF page index.
    pub page: usize,
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
            schema_version: 1,
            glyphs: Vec::new(),
        }
    }
}
