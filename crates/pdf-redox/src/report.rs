use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Kind of privacy or structural risk reported by analysis.
pub enum RiskKind {
    /// The input contains incremental-update history that a fresh rewrite can discard.
    IncrementalHistory,
    /// The document contains XMP metadata.
    XmpMetadata,
    /// The document contains a trailer Info dictionary.
    InfoDictionary,
    /// The document contains producer-specific `PieceInfo` authoring data.
    PieceInfo,
    /// The document contains an embedded or associated file.
    EmbeddedFile,
    /// The document contains JavaScript.
    Javascript,
    /// The document contains an automatic document or page action.
    AutomaticAction,
    /// The document contains a digital-signature value or certificate data.
    Signature,
    /// The document contains interactive-form values.
    FormValue,
    /// The document contains optional content hidden in the default configuration.
    HiddenLayer,
    /// The document contains page-thumbnail data.
    Thumbnail,
    /// A JPEG stream contains removable metadata markers.
    JpegMetadata,
    /// The document contains hidden text requiring semantic review.
    SuspiciousHiddenText,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// One summarized risk category found during analysis.
pub struct RiskFinding {
    /// Risk category.
    pub kind: RiskKind,
    /// Number of count.
    pub count: usize,
    /// Human-readable context for the finding.
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
/// Requested action for one hidden-text finding.
pub enum HiddenTextAction {
    /// Keep the finding unchanged.
    Keep,
    /// Remove the finding when rewriting the affected content stream.
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
/// Approximate axis-aligned page-space rectangle, in PDF user-space units.
pub struct PageRect {
    /// Minimum horizontal page-space coordinate.
    pub x0: f64,
    /// Minimum vertical page-space coordinate.
    pub y0: f64,
    /// Maximum horizontal page-space coordinate.
    pub x1: f64,
    /// Maximum vertical page-space coordinate.
    pub y1: f64,
}

impl PageRect {
    #[must_use]
    /// Returns the non-negative rectangle width in PDF user-space units.
    pub fn width(self) -> f64 {
        (self.x1 - self.x0).max(0.0)
    }

    #[must_use]
    /// Returns the non-negative rectangle height in PDF user-space units.
    pub fn height(self) -> f64 {
        (self.y1 - self.y0).max(0.0)
    }

    #[must_use]
    /// Returns the non-negative rectangle area in squared PDF user-space units.
    pub fn area(self) -> f64 {
        self.width() * self.height()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
/// One text paint classified as invisible in the default page appearance.
pub struct HiddenTextFinding {
    /// Stable within a document as long as page content operator order is unchanged.
    pub id: String,
    /// One-based page number.
    pub page_number: usize,
    /// Zero-based content operator index on that page.
    pub operator_index: usize,
    /// Rendering mechanism that makes the text invisible.
    pub mechanism: HiddenTextMechanism,
    /// Best-effort semantic category assigned to the hidden text.
    pub category: HiddenTextCategory,
    /// Default keep/remove recommendation inferred from the semantic category.
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
/// Structural, compression, privacy, and hidden-content analysis of an input PDF.
pub struct PdfAnalysis {
    /// Exact input PDF size in bytes.
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
    /// Number of page entries.
    pub page_count: usize,
    /// Number of object entries.
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
    /// Number of stream entries.
    pub stream_count: usize,
    /// Total raw encoded bytes attributed to stream.
    pub stream_raw_bytes: usize,
    #[serde(default)]
    /// Counts of stream role, keyed by category or role.
    pub stream_role_counts: BTreeMap<String, usize>,
    #[serde(default)]
    /// Total raw encoded bytes attributed to stream role.
    pub stream_role_raw_bytes: BTreeMap<String, usize>,
    /// Number of image entries.
    pub image_count: usize,
    /// Total raw encoded bytes attributed to image.
    pub image_raw_bytes: usize,
    /// Number of Form `XObject` entries.
    pub form_xobject_count: usize,
    /// Number of font program entries.
    pub font_program_count: usize,
    /// Total bytes represented by font program.
    pub font_program_bytes: usize,
    /// Number of metadata stream entries.
    pub metadata_stream_count: usize,
    /// Total bytes represented by metadata stream.
    pub metadata_stream_bytes: usize,
    /// Number of duplicate metadata payload groups.
    pub duplicate_metadata_payload_groups: usize,
    /// Encoded metadata bytes wasted by exact duplicate payloads.
    pub duplicate_metadata_payload_wasted_bytes: usize,
    /// Number of duplicate stream payload groups.
    pub duplicate_stream_payload_groups: usize,
    /// Encoded stream bytes wasted by exact duplicate payloads.
    pub duplicate_stream_payload_wasted_bytes: usize,
    /// Exact duplicate raw-stream groups attributed to structurally proven roles.
    pub duplicate_stream_role_groups: BTreeMap<String, usize>,
    /// Wasted encoded bytes from exact duplicate raw-stream groups by role.
    pub duplicate_stream_role_wasted_bytes: BTreeMap<String, usize>,
    /// Number of duplicate image payload groups.
    pub duplicate_image_payload_groups: usize,
    /// Encoded image bytes wasted by exact duplicate payloads.
    pub duplicate_image_payload_wasted_bytes: usize,
    /// Number of duplicate form payload groups.
    pub duplicate_form_payload_groups: usize,
    /// Encoded Form `XObject` bytes wasted by exact duplicate payloads.
    pub duplicate_form_payload_wasted_bytes: usize,
    /// Number of duplicate font payload groups.
    pub duplicate_font_payload_groups: usize,
    /// Embedded font-program bytes wasted by exact duplicate payloads.
    pub duplicate_font_payload_wasted_bytes: usize,
    /// Total number of parsed instructions in page content streams.
    #[serde(default)]
    pub page_content_instruction_count: usize,
    /// Maximum parsed instruction count on any single page.
    #[serde(default)]
    pub page_content_max_instructions_per_page: usize,
    /// Total number of path-construction operators in page content streams.
    #[serde(default)]
    pub page_content_path_construction_operator_count: usize,
    /// Maximum path-construction operator count on any single page.
    #[serde(default)]
    pub page_content_max_path_construction_operators_per_page: usize,
    /// Total number of painting/showing operators in page content streams.
    #[serde(default)]
    pub page_content_paint_operator_count: usize,
    /// Maximum painting/showing operator count on any single page.
    #[serde(default)]
    pub page_content_max_paint_operators_per_page: usize,
    /// Total number of `Do` `XObject` painting operators in page content streams.
    #[serde(default)]
    pub page_content_xobject_paint_operator_count: usize,
    /// Maximum `Do` operator count on any single page.
    #[serde(default)]
    pub page_content_max_xobject_paints_per_page: usize,
    /// Total number of text-show operators (`Tj`, `TJ`, `'`, `"`) in page content streams.
    #[serde(default)]
    pub page_content_text_show_operator_count: usize,
    /// Maximum text-show operator count on any single page.
    #[serde(default)]
    pub page_content_max_text_show_operators_per_page: usize,
    /// Pages whose content parser stopped before the physical end of the stream.
    #[serde(default)]
    pub page_content_incomplete_parse_pages: usize,
    /// Total number of parsed instructions in Form `XObject` content streams.
    #[serde(default)]
    pub form_content_instruction_count: usize,
    /// Maximum parsed instruction count in any single Form `XObject`.
    #[serde(default)]
    pub form_content_max_instructions_per_form: usize,
    /// Total number of path-construction operators in Form `XObject` streams.
    #[serde(default)]
    pub form_content_path_construction_operator_count: usize,
    /// Maximum path-construction operator count in any single Form `XObject`.
    #[serde(default)]
    pub form_content_max_path_construction_operators_per_form: usize,
    /// Total number of painting/showing operators in Form `XObject` streams.
    #[serde(default)]
    pub form_content_paint_operator_count: usize,
    /// Maximum painting/showing operator count in any single Form `XObject`.
    #[serde(default)]
    pub form_content_max_paint_operators_per_form: usize,
    /// Form `XObjects` whose content parser stopped before the physical end of the stream.
    #[serde(default)]
    pub form_content_incomplete_parse_forms: usize,
    /// Number of inline image entries.
    pub inline_image_count: usize,
    /// Total bytes represented by inline image.
    pub inline_image_bytes: usize,
    /// Number of duplicate inline image payload groups.
    pub duplicate_inline_image_payload_groups: usize,
    /// Inline-image payload bytes wasted by exact duplicate occurrences.
    pub duplicate_inline_image_payload_wasted_bytes: usize,
    /// Number of Flate stream entries.
    pub flate_stream_count: usize,
    /// Number of Flate recompress candidate entries.
    pub flate_recompress_candidate_count: usize,
    /// Potential encoded-byte saving from recompressing eligible Flate streams.
    pub flate_recompress_potential_saving_bytes: usize,
    /// Number of non image Flate stream entries.
    pub non_image_flate_stream_count: usize,
    /// Number of non image Flate recompress candidate entries.
    pub non_image_flate_recompress_candidate_count: usize,
    /// Potential encoded-byte saving from recompressing eligible non-image Flate streams.
    pub non_image_flate_recompress_potential_saving_bytes: usize,
    /// Number of incremental update entries.
    pub incremental_update_count: usize,
    /// Counts of filter, keyed by category or role.
    pub filter_counts: BTreeMap<String, usize>,
    /// Privacy and structural risk findings collected during analysis.
    pub risks: Vec<RiskFinding>,
    /// Hidden-text findings collected during analysis.
    pub hidden_text: Vec<HiddenTextFinding>,
    /// Non-fatal analysis failures for optional/deep inspection passes.
    pub warnings: Vec<String>,
}

impl PdfAnalysis {
    /// Returns whether this analysis belongs to these exact input bytes.
    #[must_use]
    pub fn matches_input(&self, input: &[u8]) -> bool {
        self.input_bytes == input.len() && self.input_sha256 == crate::analyze::input_sha256(input)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
/// Metrics and diagnostics produced by a complete optimization run.
pub struct OptimizationReport {
    /// Analysis of the exact input bytes used for the optimization run.
    pub before: PdfAnalysis,
    /// Final rewritten PDF size in bytes.
    pub after_bytes: usize,
    /// Signed byte difference between the input and rewritten PDF; negative values indicate growth.
    pub saved_bytes: isize,
    /// Signed percentage size reduction relative to the input PDF.
    pub saved_percent: f64,
    /// Wall-clock milliseconds spent in major optimizer stages. Intended for profiling, not stable API ordering.
    #[serde(default)]
    pub stage_timings_ms: BTreeMap<String, f64>,
    #[serde(default)]
    /// Number of reachability queries.
    pub reachability_queries: u64,
    #[serde(default)]
    /// Number of reachability rebuilds.
    pub reachability_rebuilds: u64,
    #[serde(default)]
    /// Number of reachability cache hits.
    pub reachability_cache_hits: u64,
    #[serde(default)]
    /// Number of reachability edge checks.
    pub reachability_edge_checks: u64,
    #[serde(default)]
    /// Number of reachability edge stable reuses.
    pub reachability_edge_stable_reuses: u64,
    /// Removed privacy-sensitive items keyed by removal category.
    pub privacy_items_removed: BTreeMap<String, usize>,
    /// Bytes removed by JPEG metadata.
    pub jpeg_metadata_bytes_removed: usize,
    #[serde(default)]
    /// Number of JPEG entropy streams considered.
    pub jpeg_entropy_streams_considered: usize,
    #[serde(default)]
    /// Number of JPEG entropy streams optimized.
    pub jpeg_entropy_streams_optimized: usize,
    #[serde(default)]
    /// Original encoded byte size attributed to JPEG entropy.
    pub jpeg_entropy_original_encoded_bytes: usize,
    #[serde(default)]
    /// Optimized encoded byte size attributed to JPEG entropy.
    pub jpeg_entropy_optimized_encoded_bytes: usize,
    /// Number of hidden text items removed.
    pub hidden_text_items_removed: usize,
    /// Self-contained large diagonal `BT..ET` text objects removed by the explicit watermark-like cleanup.
    #[serde(default)]
    pub large_diagonal_text_objects_removed: usize,
    #[serde(default)]
    /// Number of repeated page object groups removed.
    pub repeated_page_object_groups_removed: usize,
    #[serde(default)]
    /// Number of repeated page objects removed.
    pub repeated_page_objects_removed: usize,
    #[serde(default)]
    /// Number of repeated page text objects removed.
    pub repeated_page_text_objects_removed: usize,
    #[serde(default)]
    /// Number of repeated page `XObject` paints removed.
    pub repeated_page_xobject_paints_removed: usize,
    #[serde(default)]
    /// Number of repeated page object pages rewritten.
    pub repeated_page_object_pages_rewritten: usize,
    #[serde(default)]
    /// Number of raster hidden text items pruned.
    pub raster_hidden_text_items_pruned: usize,
    #[serde(default)]
    /// Number of vector pages compacted.
    pub vector_pages_compacted: usize,
    #[serde(default)]
    /// Number of vector fill groups batched.
    pub vector_fill_groups_batched: usize,
    #[serde(default)]
    /// Number of vector covered fills pruned.
    pub vector_covered_fills_pruned: usize,
    #[serde(default)]
    /// Number of vector fill paints eliminated.
    pub vector_fill_paints_eliminated: usize,
    #[serde(default)]
    /// Bytes removed by vector decoded.
    pub vector_decoded_bytes_removed: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by vector.
    pub vector_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    /// Number of vector path forms created.
    pub vector_path_forms_created: usize,
    #[serde(default)]
    /// Number of vector path form pages rewritten.
    pub vector_path_form_pages_rewritten: usize,
    #[serde(default)]
    /// Number of vector path form occurrences replaced.
    pub vector_path_form_occurrences_replaced: usize,
    #[serde(default)]
    /// Decoded bytes factored out by vector path form decoded.
    pub vector_path_form_decoded_bytes_factored: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by vector path form.
    pub vector_path_form_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    /// Number of vector transformed forms created.
    pub vector_transformed_forms_created: usize,
    #[serde(default)]
    /// Number of vector transformed form pages rewritten.
    pub vector_transformed_form_pages_rewritten: usize,
    #[serde(default)]
    /// Number of vector transformed form occurrences replaced.
    pub vector_transformed_form_occurrences_replaced: usize,
    #[serde(default)]
    /// Number of vector transformed form operators eliminated.
    pub vector_transformed_form_operators_eliminated: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by vector transformed form.
    pub vector_transformed_form_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    /// Number of shared resource-aware q-block run Forms created.
    pub vector_shared_run_forms_created: usize,
    #[serde(default)]
    /// Number of pages rewritten by shared q-block run factoring.
    pub vector_shared_run_pages_rewritten: usize,
    #[serde(default)]
    /// Number of q-block occurrences replaced by shared run Forms.
    pub vector_shared_run_blocks_replaced: usize,
    #[serde(default)]
    /// Decoded duplicate bytes factored into shared run Forms.
    pub vector_shared_run_decoded_bytes_factored: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by shared q-block run factoring.
    pub vector_shared_run_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    /// Number of pages rewritten by repeated clipping-path hoisting.
    pub repeated_clip_pages_rewritten: usize,
    #[serde(default)]
    /// Number of adjacent repeated clipping-path runs hoisted.
    pub repeated_clip_runs_hoisted: usize,
    #[serde(default)]
    /// Number of original q-blocks covered by repeated clipping-path hoisting.
    pub repeated_clip_blocks_hoisted: usize,
    #[serde(default)]
    /// Decoded page-content bytes removed by repeated clipping-path hoisting.
    pub repeated_clip_decoded_bytes_removed: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by repeated clipping-path hoisting.
    pub repeated_clip_estimated_flate_bytes_saved: usize,
    #[serde(default)]
    /// Number of path coordinate operands shortened within the physical-error gate.
    pub vector_path_coordinates_canonicalized: usize,
    #[serde(default)]
    /// Number of pages rewritten by path-coordinate canonicalization.
    pub vector_path_coordinate_pages_rewritten: usize,
    #[serde(default)]
    /// Decoded bytes removed by path-coordinate canonicalization.
    pub vector_path_coordinate_decoded_bytes_removed: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by path-coordinate canonicalization.
    pub vector_path_coordinate_estimated_flate_bytes_saved: usize,
    /// Number of pages rewritten by optional-content boundary coalescing.
    #[serde(default)]
    pub marked_content_pages_rewritten: usize,
    /// Number of redundant adjacent optional-content boundary pairs coalesced.
    #[serde(default)]
    pub marked_content_boundaries_coalesced: usize,
    /// Decoded page-content bytes removed by optional-content boundary coalescing.
    #[serde(default)]
    pub marked_content_decoded_bytes_removed: usize,
    /// Estimated encoded bytes saved by optional-content boundary coalescing.
    #[serde(default)]
    pub marked_content_estimated_flate_bytes_saved: usize,
    /// Number of pages rewritten by bounded Processing-mode polyline simplification.
    #[serde(default)]
    pub polyline_simplification_pages_rewritten: usize,
    /// Number of line vertices removed within the bounded page-space deviation.
    #[serde(default)]
    pub polyline_simplification_vertices_removed: usize,
    /// Decoded page-content bytes removed by bounded polyline simplification.
    #[serde(default)]
    pub polyline_simplification_decoded_bytes_removed: usize,
    /// Estimated encoded bytes saved by bounded polyline simplification.
    #[serde(default)]
    pub polyline_simplification_estimated_flate_bytes_saved: usize,
    /// Number of pages rewritten by exact collinear path compaction.
    #[serde(default)]
    pub collinear_path_pages_rewritten: usize,
    /// Number of exact forward-collinear line vertices removed.
    #[serde(default)]
    pub collinear_path_vertices_removed: usize,
    /// Decoded page-content bytes removed by exact collinear path compaction.
    #[serde(default)]
    pub collinear_path_decoded_bytes_removed: usize,
    /// Estimated encoded bytes saved by exact collinear path compaction.
    #[serde(default)]
    pub collinear_path_estimated_flate_bytes_saved: usize,
    /// Number of pages rewritten by outlined-glyph Type3 factoring.
    #[serde(default)]
    pub outlined_glyph_pages_rewritten: usize,
    /// Number of synthetic Type3 fonts created from outlined glyphs.
    #[serde(default)]
    pub outlined_glyph_fonts_created: usize,
    /// Number of reusable outlined glyph shapes created.
    #[serde(default)]
    pub outlined_glyph_shapes_created: usize,
    /// Number of outlined glyph occurrences replaced with Type3 text shows.
    #[serde(default)]
    pub outlined_glyph_occurrences_replaced: usize,
    /// Number of source paint operations covered by outlined glyph factoring.
    #[serde(default)]
    pub outlined_glyph_source_paints_replaced: usize,
    /// Decoded page-content bytes removed by outlined glyph factoring.
    #[serde(default)]
    pub outlined_glyph_decoded_bytes_removed: usize,
    /// Estimated encoded bytes saved by outlined glyph factoring.
    #[serde(default)]
    pub outlined_glyph_estimated_flate_bytes_saved: usize,
    /// Number of pages rewritten by bounded compound paint batching.
    #[serde(default)]
    pub paint_batch_pages_rewritten: usize,
    #[serde(default)]
    /// Number of bounded compound paint groups created.
    pub paint_batch_groups_created: usize,
    #[serde(default)]
    /// Number of source paint operations participating in batching.
    pub paint_batch_source_paints_batched: usize,
    #[serde(default)]
    /// Number of paint operations eliminated by batching.
    pub paint_batch_paints_eliminated: usize,
    #[serde(default)]
    /// Decoded content bytes removed by paint batching.
    pub paint_batch_decoded_bytes_removed: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by paint batching.
    pub paint_batch_estimated_flate_bytes_saved: usize,
    /// Number of pages rewritten by repeated fenced-stroke Form factoring.
    #[serde(default)]
    pub stroke_form_pages_rewritten: usize,
    /// Number of reusable Form `XObjects` created for repeated independently-stroked paths.
    #[serde(default)]
    pub stroke_forms_created: usize,
    /// Number of fenced stroke occurrences replaced by Form invocations.
    #[serde(default)]
    pub stroke_form_occurrences_replaced: usize,
    /// Decoded source bytes covered by repeated fenced-stroke Form factoring.
    #[serde(default)]
    pub stroke_form_decoded_bytes_factored: usize,
    /// Estimated encoded bytes saved by repeated fenced-stroke Form factoring.
    #[serde(default)]
    pub stroke_form_estimated_flate_bytes_saved: usize,
    /// Number of metadata duplicate streams detected.
    pub metadata_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to metadata duplicate.
    pub metadata_duplicate_raw_bytes: usize,
    /// Number of metadata references canonicalized.
    pub metadata_references_canonicalized: usize,
    /// Number of font duplicate streams detected.
    pub font_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to font duplicate.
    pub font_duplicate_raw_bytes: usize,
    /// Number of font references canonicalized.
    pub font_references_canonicalized: usize,
    #[serde(default)]
    /// Number of font duplicate dictionaries detected.
    pub font_duplicate_dictionaries_detected: usize,
    #[serde(default)]
    /// Number of font dictionary references canonicalized.
    pub font_dictionary_references_canonicalized: usize,
    #[serde(default)]
    /// Number of `ExtGState` duplicate dictionaries detected.
    pub extgstate_duplicate_dictionaries_detected: usize,
    #[serde(default)]
    /// Number of `ExtGState` dictionary references canonicalized.
    pub extgstate_dictionary_references_canonicalized: usize,
    #[serde(default)]
    /// Number of structure attribute duplicate dictionaries detected.
    pub structure_attribute_duplicate_dictionaries_detected: usize,
    #[serde(default)]
    /// Number of structure attribute references canonicalized.
    pub structure_attribute_references_canonicalized: usize,
    #[serde(default)]
    /// Number of font programs rendering optimized.
    pub font_programs_rendering_optimized: usize,
    #[serde(default)]
    /// Original encoded byte size attributed to font rendering.
    pub font_rendering_original_encoded_bytes: usize,
    #[serde(default)]
    /// Optimized encoded byte size attributed to font rendering.
    pub font_rendering_optimized_encoded_bytes: usize,
    #[serde(default)]
    /// Bytes removed by font rendering decoded table.
    pub font_rendering_decoded_table_bytes_removed: usize,
    #[serde(default)]
    /// Number of font programs glyph subset.
    pub font_programs_glyph_subset: usize,
    #[serde(default)]
    /// Decoded bytes removed by embedded-font glyph subsetting.
    pub font_glyph_subset_decoded_bytes_removed: usize,
    /// Number of `CIDFontType2` TrueType programs densely GID-remapped.
    #[serde(default)]
    pub font_dense_programs_remapped: usize,
    /// Number of explicit `CIDToGIDMap` streams rewritten for dense GIDs.
    #[serde(default)]
    pub font_dense_cid_to_gid_maps_rewritten: usize,
    /// Number of dead TrueType glyph slots removed by dense GID compaction.
    #[serde(default)]
    pub font_dense_glyph_slots_removed: usize,
    /// Decoded font-program bytes removed by dense GID compaction.
    #[serde(default)]
    pub font_dense_decoded_bytes_removed: usize,
    /// Original encoded bytes for dense-remapped font programs plus maps.
    #[serde(default)]
    pub font_dense_original_encoded_bytes: usize,
    /// Optimized encoded bytes for dense-remapped font programs plus maps.
    #[serde(default)]
    pub font_dense_optimized_encoded_bytes: usize,
    /// Number of to unicode duplicate streams detected.
    pub to_unicode_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to to unicode duplicate.
    pub to_unicode_duplicate_raw_bytes: usize,
    /// Number of to unicode references canonicalized.
    pub to_unicode_references_canonicalized: usize,
    /// Number of inline image fingerprints selected.
    pub inline_image_fingerprints_selected: usize,
    /// Number of inline image occurrences externalized.
    pub inline_image_occurrences_externalized: usize,
    /// Number of inline-image `XObjects` created.
    pub inline_image_xobjects_created: usize,
    /// Number of inline-image `XObject` references reused.
    pub inline_image_xobject_references_reused: usize,
    /// Total encoded payload bytes attributed to inline image duplicate.
    pub inline_image_duplicate_payload_bytes: usize,
    /// Number of image duplicate streams detected.
    pub image_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to image duplicate.
    pub image_duplicate_raw_bytes: usize,
    /// Number of image references canonicalized.
    pub image_references_canonicalized: usize,
    /// Number of exact duplicate coloured tiling-pattern cell payload groups factored.
    #[serde(default)]
    pub pattern_payload_groups_factored: usize,
    /// Number of Pattern streams rewritten as wrappers around shared cell Forms.
    #[serde(default)]
    pub pattern_payload_patterns_rewritten: usize,
    /// Number of shared Form `XObjects` created for duplicate Pattern cell payloads.
    #[serde(default)]
    pub pattern_payload_forms_created: usize,
    /// Duplicate encoded Pattern cell payload bytes moved behind shared Forms, before wrapper overhead.
    #[serde(default)]
    pub pattern_payload_duplicate_raw_bytes_factored: usize,
    /// Number of form duplicate streams detected.
    pub form_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to form duplicate.
    pub form_duplicate_raw_bytes: usize,
    /// Number of form references canonicalized.
    pub form_references_canonicalized: usize,
    /// Number of appearance duplicate streams detected.
    pub appearance_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to appearance duplicate.
    pub appearance_duplicate_raw_bytes: usize,
    /// Number of appearance references canonicalized.
    pub appearance_references_canonicalized: usize,
    /// Number of page content duplicate streams detected.
    pub page_content_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to page content duplicate.
    pub page_content_duplicate_raw_bytes: usize,
    /// Number of page content references canonicalized.
    pub page_content_references_canonicalized: usize,
    /// Number of Type3 charproc duplicate streams detected.
    pub type3_charproc_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to Type3 charproc duplicate.
    pub type3_charproc_duplicate_raw_bytes: usize,
    /// Number of Type3 charproc references canonicalized.
    pub type3_charproc_references_canonicalized: usize,
    /// Number of ICC duplicate streams detected.
    pub icc_duplicate_streams_detected: usize,
    /// Total raw encoded bytes attributed to ICC duplicate.
    pub icc_duplicate_raw_bytes: usize,
    /// Number of ICC references canonicalized.
    pub icc_references_canonicalized: usize,
    #[serde(default)]
    /// Number of large ICC profiles eligible for alternate-space elision.
    pub icc_alternate_profiles_eligible: usize,
    #[serde(default)]
    /// Number of ICC profiles made unreachable by alternate-space elision.
    pub icc_alternate_profiles_elided: usize,
    #[serde(default)]
    /// Number of `ICCBased` color-space values replaced by their declared alternate.
    pub icc_alternate_references_rewritten: usize,
    #[serde(default)]
    /// Encoded ICC profile bytes made unreachable by alternate-space elision.
    pub icc_alternate_encoded_bytes_elided: usize,
    /// Number of raster images transcoded.
    pub raster_images_transcoded: usize,
    /// Number of raster images resized.
    pub raster_images_resized: usize,
    /// Number of raster JPEG images resized.
    pub raster_jpeg_images_resized: usize,
    /// Number of raster Flate images resized.
    pub raster_flate_images_resized: usize,
    /// Number of raster references reused.
    pub raster_references_reused: usize,
    /// Original encoded byte size attributed to raster.
    pub raster_original_encoded_bytes: u64,
    /// Optimized encoded byte size attributed to raster.
    pub raster_optimized_encoded_bytes: u64,
    /// Total pixels represented by raster original.
    pub raster_original_pixels: u64,
    /// Total pixels represented by raster optimized.
    pub raster_optimized_pixels: u64,
    #[serde(default)]
    /// Number of raster inline fragmented scopes rewritten.
    pub raster_inline_fragmented_scopes_rewritten: usize,
    #[serde(default)]
    /// Number of raster inline occurrences externalized.
    pub raster_inline_occurrences_externalized: usize,
    #[serde(default)]
    /// Number of raster pixel clusters reconstructed.
    pub raster_pixel_clusters_reconstructed: usize,
    #[serde(default)]
    /// Number of raster pixel paints reconstructed.
    pub raster_pixel_paints_reconstructed: usize,
    #[serde(default)]
    /// Number of raster native fragment groups reconstructed.
    pub raster_native_fragment_groups_reconstructed: usize,
    #[serde(default)]
    /// Number of raster native fragment paints reconstructed.
    pub raster_native_fragment_paints_reconstructed: usize,
    #[serde(default)]
    /// Number of raster stripe groups merged.
    pub raster_stripe_groups_merged: usize,
    #[serde(default)]
    /// Number of raster stripe paints merged.
    pub raster_stripe_paints_merged: usize,
    #[serde(default)]
    /// Number of raster masks baked.
    pub raster_masks_baked: usize,
    #[serde(default)]
    /// Number of raster transparent paints pruned.
    pub raster_transparent_paints_pruned: usize,
    #[serde(default)]
    /// Number of raster occluded paints pruned.
    pub raster_occluded_paints_pruned: usize,
    #[serde(default)]
    /// Number of raster transparent margins cropped.
    pub raster_transparent_margins_cropped: usize,
    #[serde(default)]
    /// Number of raster background margins cropped.
    pub raster_background_margins_cropped: usize,
    #[serde(default)]
    /// Total pixels represented by raster cropped removed.
    pub raster_cropped_pixels_removed: u64,
    #[serde(default)]
    /// Number of raster binary images packed.
    pub raster_binary_images_packed: usize,
    #[serde(default)]
    /// Number of raster binary masks packed.
    pub raster_binary_masks_packed: usize,
    #[serde(default)]
    /// Number of raster stencil images emitted.
    pub raster_stencil_images_emitted: usize,
    #[serde(default)]
    /// Number of raster constant color mask stencils emitted.
    pub raster_constant_color_mask_stencils_emitted: usize,
    #[serde(default)]
    /// Number of raster relaxed stencil images emitted.
    pub raster_relaxed_stencil_images_emitted: usize,
    #[serde(default)]
    /// Number of raster bilevel ccitt images emitted.
    pub raster_bilevel_ccitt_images_emitted: usize,
    #[serde(default)]
    /// Number of raster bilevel Flate images emitted.
    pub raster_bilevel_flate_images_emitted: usize,
    #[serde(default)]
    /// Whether exact raster rendering.
    pub exact_raster_rendering: bool,
    #[serde(default)]
    /// Estimated encoded bytes saved by raster binary image encoded.
    pub raster_binary_image_encoded_bytes_saved: u64,
    #[serde(default)]
    /// Number of raster deferred tile candidates.
    pub raster_deferred_tile_candidates: usize,
    #[serde(default)]
    /// Number of raster deferred tile paints consumed.
    pub raster_deferred_tile_paints_consumed: usize,
    #[serde(default)]
    /// Number of raster staging `XObject` entries removed.
    pub raster_staging_xobject_entries_removed: usize,
    #[serde(default)]
    /// Number of raster rewritten `XObject` entries removed.
    pub raster_rewritten_xobject_entries_removed: usize,
    #[serde(default)]
    /// Number of resource entries pruned.
    pub resource_entries_pruned: usize,
    #[serde(default)]
    /// Number of resource font entries pruned.
    pub resource_font_entries_pruned: usize,
    #[serde(default)]
    /// Number of resource `XObject` entries pruned.
    pub resource_xobject_entries_pruned: usize,
    #[serde(default)]
    /// Number of resource ext gstate entries pruned.
    pub resource_ext_gstate_entries_pruned: usize,
    #[serde(default)]
    /// Number of resource pattern entries pruned.
    pub resource_pattern_entries_pruned: usize,
    #[serde(default)]
    /// Number of resource properties entries pruned.
    pub resource_properties_entries_pruned: usize,
    #[serde(default)]
    /// Number of resource shading entries pruned.
    pub resource_shading_entries_pruned: usize,
    #[serde(default)]
    /// Number of page tree nodes before.
    pub page_tree_nodes_before: usize,
    #[serde(default)]
    /// Number of page tree nodes after.
    pub page_tree_nodes_after: usize,
    #[serde(default)]
    /// Number of page tree nodes removed.
    pub page_tree_nodes_removed: usize,
    #[serde(default)]
    /// Number of page tree pages reparented.
    pub page_tree_pages_reparented: usize,
    #[serde(default)]
    /// Number of name trees repacked.
    pub name_trees_repacked: usize,
    #[serde(default)]
    /// Number of name tree nodes before.
    pub name_tree_nodes_before: usize,
    #[serde(default)]
    /// Number of name tree nodes after.
    pub name_tree_nodes_after: usize,
    #[serde(default)]
    /// Number of name tree nodes removed.
    pub name_tree_nodes_removed: usize,
    #[serde(default)]
    /// Number of named destination wrappers inlined.
    pub named_destination_wrappers_inlined: usize,
    #[serde(default)]
    /// Number of indirect named-destination arrays inlined into name-tree leaves.
    pub named_destination_arrays_inlined: usize,
    #[serde(default)]
    /// Number of microstroke pages rasterized.
    pub microstroke_pages_rasterized: usize,
    #[serde(default)]
    /// Number of microstroke runs rasterized.
    pub microstroke_runs_rasterized: usize,
    #[serde(default)]
    /// Number of microstroke strokes rasterized.
    pub microstroke_strokes_rasterized: usize,
    #[serde(default)]
    /// Total encoded payload bytes attributed to microstroke image.
    pub microstroke_image_payload_bytes: usize,
    #[serde(default)]
    /// Number of microstroke ccitt images.
    pub microstroke_ccitt_images: usize,
    #[serde(default)]
    /// Number of microstroke Flate images.
    pub microstroke_flate_images: usize,
    #[serde(default)]
    /// Estimated encoded bytes saved by microstroke.
    pub microstroke_estimated_flate_bytes_saved: usize,
    /// Number of print images placed.
    pub print_images_placed: usize,
    /// Number of print image uses.
    pub print_image_uses: usize,
    /// Whether print geometry complete.
    pub print_geometry_complete: bool,
    /// Number of print downsample candidates.
    pub print_downsample_candidates: usize,
    /// Number of print existing JPEG resize candidates.
    pub print_existing_jpeg_resize_candidates: usize,
    /// Number of print Flate resize candidates.
    pub print_flate_resize_candidates: usize,
    /// Total pixels represented by print source.
    pub print_source_pixels: u64,
    /// Total pixels represented by print target.
    pub print_target_pixels: u64,
    /// Number of Flate streams selected for recompression.
    pub flate_streams_selected_for_recompression: usize,
    /// Estimated encoded-byte saving from the Flate streams selected for recompression.
    pub flate_estimated_savings_bytes: usize,
    /// Number of large highly-compressible streams that received a high-effort Flate trial.
    #[serde(default)]
    pub flate_high_effort_streams_tested: usize,
    /// Number of streams for which high-effort Flate beat the configured baseline and was installed.
    #[serde(default)]
    pub flate_high_effort_streams_selected: usize,
    /// Additional encoded bytes saved by selected high-effort Flate beyond the configured baseline.
    #[serde(default)]
    pub flate_high_effort_extra_savings_bytes: usize,
    #[serde(default)]
    /// Number of preservation pages.
    pub preservation_pages: usize,
    #[serde(default)]
    /// Number of preservation annotation entries seen.
    pub preservation_annotation_entries_seen: usize,
    #[serde(default)]
    /// Number of preservation annotation entries flattened.
    pub preservation_annotation_entries_flattened: usize,
    #[serde(default)]
    /// Number of preservation annotation entries dropped unflattened.
    pub preservation_annotation_entries_dropped_unflattened: usize,
    #[serde(default)]
    /// Number of preservation link visual shells retained.
    pub preservation_link_visual_shells_retained: usize,
    #[serde(default)]
    /// Annotation subtype counts observed while applying preservation policy.
    pub preservation_annotation_subtypes_seen: BTreeMap<String, usize>,
    #[serde(default)]
    /// Unflattened annotation counts keyed by subtype.
    pub preservation_unflattened_annotation_subtypes: BTreeMap<String, usize>,
    #[serde(default)]
    /// Dropped page-dictionary keys and their occurrence counts.
    pub preservation_dropped_page_keys: BTreeMap<String, usize>,
    #[serde(default)]
    /// Dropped page-tree-node keys and their occurrence counts.
    pub preservation_dropped_page_tree_keys: BTreeMap<String, usize>,
    #[serde(default)]
    /// Dropped catalog keys and their occurrence counts.
    pub preservation_dropped_catalog_keys: BTreeMap<String, usize>,
    #[serde(default)]
    /// Unknown-wrapper keys promoted to a recognized parent and their occurrence counts.
    pub preservation_spliced_unknown_wrapper_keys: BTreeMap<String, usize>,
    /// Human-readable non-fatal notes produced during optimization.
    pub notes: Vec<String>,
}
