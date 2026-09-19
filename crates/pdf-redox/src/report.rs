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
    /// True when optional/deep analysis (duplicate payloads, trial recompression,
    /// hidden-text census, graph inventory) was performed. Optimization reports
    /// use a cheaper preflight analysis unless a full cached analysis is supplied.
    #[serde(default)]
    pub analysis_complete: bool,
    /// SHA-256 of the exact input bytes, as lowercase hexadecimal.
    #[serde(default)]
    pub input_sha256: String,
    /// UTF-8 view of `/Info /Producer`, when present.
    pub producer: Option<String>,
    /// UTF-8 view of `/Info /Creator`, when present.
    pub creator: Option<String>,
    pub page_count: usize,
    pub object_count: usize,
    /// Reachable indirect COS objects grouped by intrinsic structural role.
    #[serde(default)]
    pub object_role_counts: BTreeMap<String, usize>,
    /// Dictionary keys observed on indirect objects, keyed as `role/key`.
    #[serde(default)]
    pub object_key_counts: BTreeMap<String, usize>,
    /// Indirect-reference edges observed as `source-role/key->target-role`.
    #[serde(default)]
    pub object_reference_edge_counts: BTreeMap<String, usize>,
    pub stream_count: usize,
    pub stream_raw_bytes: usize,
    #[serde(default)]
    pub stream_role_counts: BTreeMap<String, usize>,
    #[serde(default)]
    pub stream_role_raw_bytes: BTreeMap<String, usize>,
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
    /// Exact duplicate raw-stream groups attributed to structurally proven roles.
    pub duplicate_stream_role_groups: BTreeMap<String, usize>,
    /// Wasted encoded bytes from exact duplicate raw-stream groups by role.
    pub duplicate_stream_role_wasted_bytes: BTreeMap<String, usize>,
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
    /// Wall-clock milliseconds spent in major optimizer stages. Intended for profiling, not stable API ordering.
    #[serde(default)]
    pub stage_timings_ms: BTreeMap<String, f64>,
    pub privacy_items_removed: BTreeMap<String, usize>,
    pub jpeg_metadata_bytes_removed: usize,
    pub hidden_text_items_removed: usize,
    /// Self-contained large diagonal `BT..ET` text objects removed by the explicit watermark-like cleanup.
    #[serde(default)]
    pub large_diagonal_text_objects_removed: usize,
    #[serde(default)]
    pub repeated_page_object_groups_removed: usize,
    #[serde(default)]
    pub repeated_page_objects_removed: usize,
    #[serde(default)]
    pub repeated_page_text_objects_removed: usize,
    #[serde(default)]
    pub repeated_page_xobject_paints_removed: usize,
    #[serde(default)]
    pub repeated_page_object_pages_rewritten: usize,
    #[serde(default)]
    pub raster_hidden_text_items_pruned: usize,
    #[serde(default)]
    pub vector_pages_compacted: usize,
    #[serde(default)]
    pub vector_fill_groups_batched: usize,
    #[serde(default)]
    pub vector_covered_fills_pruned: usize,
    #[serde(default)]
    pub vector_fill_paints_eliminated: usize,
    #[serde(default)]
    pub vector_decoded_bytes_removed: usize,
    #[serde(default)]
    pub vector_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    pub vector_path_forms_created: usize,
    #[serde(default)]
    pub vector_path_form_pages_rewritten: usize,
    #[serde(default)]
    pub vector_path_form_occurrences_replaced: usize,
    #[serde(default)]
    pub vector_path_form_decoded_bytes_factored: usize,
    #[serde(default)]
    pub vector_path_form_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    pub vector_transformed_forms_created: usize,
    #[serde(default)]
    pub vector_transformed_form_pages_rewritten: usize,
    #[serde(default)]
    pub vector_transformed_form_occurrences_replaced: usize,
    #[serde(default)]
    pub vector_transformed_form_operators_eliminated: usize,
    #[serde(default)]
    pub vector_transformed_form_estimated_flate_bytes_saved: usize,
    pub metadata_duplicate_streams_detected: usize,
    pub metadata_duplicate_raw_bytes: usize,
    pub metadata_references_canonicalized: usize,
    pub font_duplicate_streams_detected: usize,
    pub font_duplicate_raw_bytes: usize,
    pub font_references_canonicalized: usize,
    #[serde(default)]
    pub font_programs_rendering_optimized: usize,
    #[serde(default)]
    pub font_rendering_original_encoded_bytes: usize,
    #[serde(default)]
    pub font_rendering_optimized_encoded_bytes: usize,
    #[serde(default)]
    pub font_rendering_decoded_table_bytes_removed: usize,
    #[serde(default)]
    pub font_programs_glyph_subset: usize,
    #[serde(default)]
    pub font_glyph_outline_bytes_removed: usize,
    pub to_unicode_duplicate_streams_detected: usize,
    pub to_unicode_duplicate_raw_bytes: usize,
    pub to_unicode_references_canonicalized: usize,
    pub inline_image_fingerprints_selected: usize,
    pub inline_image_occurrences_externalized: usize,
    pub inline_image_xobjects_created: usize,
    pub inline_image_xobject_references_reused: usize,
    pub inline_image_duplicate_payload_bytes: usize,
    pub image_duplicate_streams_detected: usize,
    pub image_duplicate_raw_bytes: usize,
    pub image_references_canonicalized: usize,
    pub form_duplicate_streams_detected: usize,
    pub form_duplicate_raw_bytes: usize,
    pub form_references_canonicalized: usize,
    pub appearance_duplicate_streams_detected: usize,
    pub appearance_duplicate_raw_bytes: usize,
    pub appearance_references_canonicalized: usize,
    pub page_content_duplicate_streams_detected: usize,
    pub page_content_duplicate_raw_bytes: usize,
    pub page_content_references_canonicalized: usize,
    pub type3_charproc_duplicate_streams_detected: usize,
    pub type3_charproc_duplicate_raw_bytes: usize,
    pub type3_charproc_references_canonicalized: usize,
    pub icc_duplicate_streams_detected: usize,
    pub icc_duplicate_raw_bytes: usize,
    pub icc_references_canonicalized: usize,
    pub raster_images_transcoded: usize,
    pub raster_images_resized: usize,
    pub raster_jpeg_images_resized: usize,
    pub raster_flate_images_resized: usize,
    pub raster_references_reused: usize,
    pub raster_original_encoded_bytes: u64,
    pub raster_optimized_encoded_bytes: u64,
    pub raster_original_pixels: u64,
    pub raster_optimized_pixels: u64,
    #[serde(default)]
    pub raster_inline_fragmented_scopes_rewritten: usize,
    #[serde(default)]
    pub raster_inline_occurrences_externalized: usize,
    #[serde(default)]
    pub raster_pixel_clusters_reconstructed: usize,
    #[serde(default)]
    pub raster_pixel_paints_reconstructed: usize,
    #[serde(default)]
    pub raster_native_fragment_groups_reconstructed: usize,
    #[serde(default)]
    pub raster_native_fragment_paints_reconstructed: usize,
    #[serde(default)]
    pub raster_stripe_groups_merged: usize,
    #[serde(default)]
    pub raster_stripe_paints_merged: usize,
    #[serde(default)]
    pub raster_masks_baked: usize,
    #[serde(default)]
    pub raster_transparent_paints_pruned: usize,
    #[serde(default)]
    pub raster_occluded_paints_pruned: usize,
    #[serde(default)]
    pub raster_transparent_margins_cropped: usize,
    #[serde(default)]
    pub raster_background_margins_cropped: usize,
    #[serde(default)]
    pub raster_cropped_pixels_removed: u64,
    #[serde(default)]
    pub raster_binary_images_packed: usize,
    #[serde(default)]
    pub raster_binary_masks_packed: usize,
    #[serde(default)]
    pub raster_stencil_images_emitted: usize,
    #[serde(default)]
    pub raster_relaxed_stencil_images_emitted: usize,
    #[serde(default)]
    pub exact_raster_rendering: bool,
    #[serde(default)]
    pub raster_binary_image_encoded_bytes_saved: u64,
    #[serde(default)]
    pub raster_deferred_tile_candidates: usize,
    #[serde(default)]
    pub raster_deferred_tile_paints_consumed: usize,
    #[serde(default)]
    pub raster_staging_xobject_entries_removed: usize,
    #[serde(default)]
    pub resource_entries_pruned: usize,
    #[serde(default)]
    pub resource_font_entries_pruned: usize,
    #[serde(default)]
    pub resource_xobject_entries_pruned: usize,
    #[serde(default)]
    pub resource_ext_gstate_entries_pruned: usize,
    #[serde(default)]
    pub resource_pattern_entries_pruned: usize,
    #[serde(default)]
    pub resource_properties_entries_pruned: usize,
    #[serde(default)]
    pub resource_shading_entries_pruned: usize,
    pub print_images_placed: usize,
    pub print_image_uses: usize,
    pub print_geometry_complete: bool,
    pub print_downsample_candidates: usize,
    pub print_existing_jpeg_resize_candidates: usize,
    pub print_flate_resize_candidates: usize,
    pub print_source_pixels: u64,
    pub print_target_pixels: u64,
    pub flate_streams_selected_for_recompression: usize,
    pub flate_estimated_savings_bytes: usize,
    #[serde(default)]
    pub preservation_pages: usize,
    #[serde(default)]
    pub preservation_annotation_entries_seen: usize,
    #[serde(default)]
    pub preservation_annotation_entries_flattened: usize,
    #[serde(default)]
    pub preservation_annotation_entries_dropped_unflattened: usize,
    #[serde(default)]
    pub preservation_link_visual_shells_retained: usize,
    #[serde(default)]
    pub preservation_annotation_subtypes_seen: BTreeMap<String, usize>,
    #[serde(default)]
    pub preservation_unflattened_annotation_subtypes: BTreeMap<String, usize>,
    #[serde(default)]
    pub preservation_dropped_page_keys: BTreeMap<String, usize>,
    #[serde(default)]
    pub preservation_dropped_page_tree_keys: BTreeMap<String, usize>,
    #[serde(default)]
    pub preservation_dropped_catalog_keys: BTreeMap<String, usize>,
    #[serde(default)]
    pub preservation_spliced_unknown_wrapper_keys: BTreeMap<String, usize>,
    pub notes: Vec<String>,
}
