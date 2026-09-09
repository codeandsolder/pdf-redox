use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RiskKind {
    IncrementalHistory,
    XmpMetadata,
    InfoDictionary,
    PieceInfo,
    EmbeddedFile,
    Javascript,
    AutomaticAction,
    Signature,
    FormValue,
    HiddenLayer,
    Thumbnail,
    JpegMetadata,
    SuspiciousHiddenText,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFinding {
    pub kind: RiskKind,
    pub count: usize,
    pub note: String,
}

/// Why text is not visible in the normal page appearance.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HiddenTextMechanism {
    /// PDF text rendering mode 3: neither fill nor stroke.
    RenderingModeInvisible,
    /// PDF text rendering mode 7: clipping only.
    ClipOnlyRenderingMode,
    /// The applicable fill/stroke alpha is effectively zero.
    ZeroOpacity,
    /// The text is inside optional content that is off in the default configuration.
    OptionalContentHidden,
    /// The estimated glyph bounds are outside the page crop box.
    OutsideCropBox,
    /// A rectangular clipping path excludes the estimated glyph bounds.
    ClippedOut,
    /// A later opaque filled rectangle covers the estimated text bounds.
    CoveredByOpaqueFill,
    /// A later raster image covers the estimated text bounds.
    CoveredByImage,
    /// The text transform/font size collapses to an effectively zero-sized result.
    DegenerateTransform,
}

/// Best-effort semantic classification of hidden text.
///
/// This is intentionally separate from [`HiddenTextMechanism`]: an OCR layer
/// and a censorship leak can both be invisible text, but should get opposite
/// default treatment.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HiddenTextCategory {
    /// Search/accessibility text associated with a scanned or rasterized page.
    OcrOverlay,
    /// Text that appears to have been hidden by a later opaque redaction-like box.
    LikelyRedactionLeak,
    /// Accessibility replacement text or similarly intentional semantic text.
    Accessibility,
    /// Text in a default-hidden optional-content layer.
    HiddenLayer,
    /// Text positioned outside the visible crop region.
    OutsidePage,
    /// Invisible text that does not fit a stronger category.
    OtherInvisible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HiddenTextAction {
    Keep,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PageRect {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl PageRect {
    pub fn width(self) -> f64 {
        (self.x1 - self.x0).max(0.0)
    }

    pub fn height(self) -> f64 {
        (self.y1 - self.y0).max(0.0)
    }

    pub fn area(self) -> f64 {
        self.width() * self.height()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HiddenTextFinding {
    /// Stable within a document as long as page content operator order is unchanged.
    pub id: String,
    /// One-based page number.
    pub page_number: usize,
    /// Zero-based content operator index on that page.
    pub operator_index: usize,
    pub mechanism: HiddenTextMechanism,
    pub category: HiddenTextCategory,
    pub suggested_action: HiddenTextAction,
    /// Best-effort Unicode. Empty when the font encoding cannot be decoded safely.
    pub text: String,
    /// Original PDF character-code bytes, useful when Unicode mapping is unavailable.
    pub raw_hex: String,
    /// Approximate page-space bounds. Text metrics in broken PDFs can make this approximate.
    pub bounds: Option<PageRect>,
    /// Heuristic confidence in the semantic category, not in the mechanism itself.
    pub confidence: f32,
    /// True when the content is explicitly marked as a PDF /Artifact.
    pub artifact: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PdfAnalysis {
    pub input_bytes: usize,
    /// UTF-8 view of `/Info /Producer`, when present.
    pub producer: Option<String>,
    /// UTF-8 view of `/Info /Creator`, when present.
    pub creator: Option<String>,
    pub page_count: usize,
    pub object_count: usize,
    pub stream_count: usize,
    pub stream_raw_bytes: usize,
    pub image_count: usize,
    pub image_raw_bytes: usize,
    pub form_xobject_count: usize,
    pub font_program_count: usize,
    pub font_program_bytes: usize,
    pub metadata_stream_count: usize,
    pub metadata_stream_bytes: usize,
    pub duplicate_metadata_payload_groups: usize,
    pub duplicate_metadata_payload_wasted_bytes: usize,
    pub duplicate_stream_payload_groups: usize,
    pub duplicate_stream_payload_wasted_bytes: usize,
    pub duplicate_image_payload_groups: usize,
    pub duplicate_image_payload_wasted_bytes: usize,
    pub duplicate_form_payload_groups: usize,
    pub duplicate_form_payload_wasted_bytes: usize,
    pub duplicate_font_payload_groups: usize,
    pub duplicate_font_payload_wasted_bytes: usize,
    pub inline_image_count: usize,
    pub inline_image_bytes: usize,
    pub duplicate_inline_image_payload_groups: usize,
    pub duplicate_inline_image_payload_wasted_bytes: usize,
    pub resource_pruning_auto_triggered: bool,
    pub flate_stream_count: usize,
    pub flate_recompress_candidate_count: usize,
    pub flate_recompress_potential_saving_bytes: usize,
    pub non_image_flate_stream_count: usize,
    pub non_image_flate_recompress_candidate_count: usize,
    pub non_image_flate_recompress_potential_saving_bytes: usize,
    pub incremental_update_count: usize,
    pub filter_counts: BTreeMap<String, usize>,
    pub risks: Vec<RiskFinding>,
    pub hidden_text: Vec<HiddenTextFinding>,
    /// Non-fatal analysis failures for optional/deep inspection passes.
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OptimizationReport {
    pub before: PdfAnalysis,
    pub after_bytes: usize,
    pub saved_bytes: isize,
    pub saved_percent: f64,
    pub privacy_items_removed: BTreeMap<String, usize>,
    pub jpeg_metadata_bytes_removed: usize,
    pub hidden_text_items_removed: usize,
    pub notes: Vec<String>,
}
