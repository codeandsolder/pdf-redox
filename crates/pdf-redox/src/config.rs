use crate::report::{HiddenTextAction, HiddenTextCategory, HiddenTextFinding};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Policy for annotations that are not independently preserved as links or forms.
pub enum AnnotationPolicy {
    /// Preserve annotation dictionaries and their interactive semantics.
    Preserve,
    /// Burn usable visible appearances into page content, retain only inert
    /// visual shells where viewers synthesize visible ink, and discard the
    /// remaining annotation semantics.
    AppearanceOnly,
    /// Remove annotations rather than preserving or flattening them.
    Discard,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "public preservation switches are independent serialized policy fields"
)]
/// Controls which interactive, structural, and authoring semantics survive a rewrite.
pub struct PreservationConfig {
    /// Preserve Link annotation navigation/actions. Independent of whether
    /// other annotations are preserved, flattened, or discarded.
    pub links: bool,
    /// Preserve interactive form state and Widget annotations.
    pub forms: bool,
    /// Preserve outlines, names/destinations, page labels, article threads,
    /// and document open actions.
    pub navigation: bool,
    /// Preserve optional-content configuration so default layer visibility is
    /// interpreted correctly.
    pub optional_content: bool,
    /// Preserve tagged-PDF/accessibility structure and language metadata.
    pub structure: bool,
    /// Preserve color-management output intents.
    pub output_intents: bool,
    /// Preserve viewer layout/mode/preferences.
    pub viewer_preferences: bool,
    /// Preserve authoring/document metadata attached throughout the known
    /// object graph, including XMP `/Metadata`, Form `/PieceInfo` and `/LastModified`,
    /// and page/catalog metadata-like auxiliary entries. Privacy scrubbing is a
    /// separate policy and may remove metadata afterward.
    pub metadata: bool,
    /// Preserve embedded-font tables used for later text editing/reflow but not
    /// for rendering already-positioned PDF text. Visible-surface mode can drop
    /// OpenType layout tables and sfnt vertical metrics that PDF consumers do
    /// not use for page rendering.
    pub font_editing_support: bool,
    /// Handling for annotations not protected by `links` or `forms`.
    pub annotations: AnnotationPolicy,
    /// Keep unrecognized Catalog/page entries and everything reachable only
    /// from them. Turning this off is the main "known semantics only" switch.
    pub unknown_objects: bool,
    /// Experimental: when an unknown Catalog/Page/PageTree wrapper is dropped, promote direct
    /// child keys that are themselves valid for the wrapper's parent role.
    #[serde(default)]
    pub splice_unknown_wrappers: bool,
}

impl PreservationConfig {
    #[must_use]
    /// Returns the preservation policy that retains normal document functionality and authoring semantics.
    pub const fn functional() -> Self {
        Self {
            links: true,
            forms: true,
            navigation: true,
            optional_content: true,
            structure: true,
            output_intents: true,
            viewer_preferences: true,
            metadata: true,
            font_editing_support: true,
            annotations: AnnotationPolicy::Preserve,
            unknown_objects: true,
            splice_unknown_wrappers: false,
        }
    }

    /// Preserve standardized interactive/document semantics while dropping
    /// authoring-only metadata, editing support, and unclassified extensions.
    /// This is the processing-oriented baseline for a known-semantics graph.
    #[must_use]
    pub const fn known_functional() -> Self {
        Self {
            links: true,
            forms: true,
            navigation: true,
            optional_content: true,
            structure: true,
            output_intents: true,
            viewer_preferences: true,
            metadata: false,
            font_editing_support: false,
            annotations: AnnotationPolicy::Preserve,
            unknown_objects: false,
            splice_unknown_wrappers: false,
        }
    }

    #[must_use]
    /// Returns the policy that retains what can affect the default visible page surface while dropping non-visual semantics.
    pub const fn visible_surface() -> Self {
        Self {
            links: false,
            forms: false,
            navigation: false,
            // OCG state and output intents can change what "visible" means;
            // preserve them until we explicitly flatten those semantics.
            optional_content: true,
            structure: false,
            output_intents: true,
            viewer_preferences: false,
            metadata: false,
            font_editing_support: false,
            annotations: AnnotationPolicy::AppearanceOnly,
            unknown_objects: false,
            splice_unknown_wrappers: false,
        }
    }
}

impl Default for PreservationConfig {
    fn default() -> Self {
        Self::functional()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode")]
#[derive(Default)]
/// Raster-image optimization policy for the selected output profile.
pub enum ImagePolicy {
    #[default]
    /// Preserve eligible raster payloads without lossy transcoding or downsampling.
    Preserve,
    /// Permit size-gated lossy image transcoding while retaining the configured resolution policy.
    Perceptual {
        /// JPEG quality used when encoding eligible photographic raster data.
        jpeg_quality: u8,
        /// Minimum encoded-size reduction, as a percentage, required to accept the transform.
        min_savings_percent: u8,
        /// Whether perceptual optimization must retain the original pixel dimensions.
        preserve_resolution: bool,
    },
    /// Permit print-oriented raster downsampling and recompression.
    Print {
        /// JPEG quality used when encoding eligible photographic raster data.
        jpeg_quality: u8,
        /// Target effective pixels per inch for eligible print-oriented downsampling.
        target_ppi: u16,
        /// Minimum encoded-size reduction, as a percentage, required to accept the transform.
        min_savings_percent: u8,
    },
}

/// Structural raster-layout normalization independent of codec/downsampling policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "public raster-layout switches are independent serialized policy fields"
)]
pub struct RasterLayoutConfig {
    /// Enable raster crop/reconstruction passes.
    pub enabled: bool,
    /// Remove fully transparent outer pixel margins when mask semantics can be preserved.
    pub crop_transparent: bool,
    /// Replace uniform opaque outer backgrounds with an equivalent compact background paint
    /// plus the cropped foreground raster when doing so cannot reveal retained content.
    pub crop_background: bool,
    /// Remove paint conservatively classified as invisible in the default appearance: fully
    /// transparent raster paints and high-confidence removable hidden text. This also permits
    /// opaque-background cropping to discard such content instead of exposing it.
    pub prune_hidden_paints: bool,
    /// Additionally remove raster image paints inferred to be fully occluded by later opaque
    /// paint. Disabled by default because geometric coverage is not a proof of renderer-exact
    /// equivalence for all PDF transparency/antialiasing combinations.
    pub prune_occluded_raster_paints: bool,
    /// Materialize `/SMask`, explicit `/Mask`, and color-key masks into a normalized alpha
    /// plane when rebuilding a raster. The emitted PDF still uses a standard soft-mask image.
    pub bake_masks: bool,
    /// Preserve renderer-exact color/alpha resampling for binary-alpha images. When false,
    /// transparent color samples may be elided by replacing a constant visible color plus a
    /// binary soft mask with a stencil. This is visually conservative but can change subpixel
    /// antialiasing when a viewer downsamples color and alpha separately.
    pub exact_raster_rendering: bool,
    /// Maximum exact-sample distance from the detected border color when considering a pixel
    /// background. Zero is byte-exact and is the lossless default.
    pub background_tolerance: u8,
    /// Reconstruct clusters of tiny images that are being used as scalar raster pixels/sprites.
    pub reconstruct_pixel_clusters: bool,
    /// Maximum page-space gap between tiny-image paint rectangles in one connected component.
    /// Five millimetres is deliberately generous enough to bridge sparse plot/image pixels
    /// while still keeping unrelated figures apart in ordinary datasheet layouts.
    pub pixel_cluster_max_gap_mm: f32,
    /// Maximum source-image width or height treated as a pixel/sprite primitive.
    pub pixel_cluster_max_source_dimension: u8,
    /// Minimum number of tiny-image paints required before a component is reconstructed.
    pub pixel_cluster_min_paints: usize,
    /// Maximum long-axis source dimension admitted as a thin native-resolution fragment.
    /// These larger fragments are reconstructed only when a compatible continuous raster
    /// anchor establishes the native pixel pitch for their spatial component.
    pub native_fragment_max_long_dimension: u16,
    /// Minimum long-axis source dimension considered a continuous native-resolution anchor.
    pub native_anchor_min_long_dimension: u16,
    /// Merge source rasters that were needlessly split into adjacent strips.
    pub merge_stripes: bool,
    /// Minimum number of compatible strips required before replacing them with one image.
    pub stripe_min_paints: usize,
    /// Maximum placement error, measured in source pixels along the strip-join axis.
    pub stripe_max_gap_pixels: f32,
    /// Inline images are first externalized only in scopes this fragmented, so normal PDFs do
    /// not gain thousands of transient `XObjects` merely to discover there is nothing to merge.
    pub fragmented_paint_threshold: usize,
    /// Hard memory/size guard for a reconstructed raster.
    pub max_reconstructed_pixels: u64,
}

impl Default for RasterLayoutConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            crop_transparent: true,
            crop_background: true,
            prune_hidden_paints: true,
            prune_occluded_raster_paints: false,
            bake_masks: false,
            exact_raster_rendering: false,
            background_tolerance: 0,
            reconstruct_pixel_clusters: true,
            pixel_cluster_max_gap_mm: 5.0,
            pixel_cluster_max_source_dimension: 5,
            pixel_cluster_min_paints: 16,
            native_fragment_max_long_dimension: 32,
            native_anchor_min_long_dimension: 64,
            merge_stripes: true,
            stripe_min_paints: 3,
            stripe_max_gap_pixels: 0.35,
            fragmented_paint_threshold: 64,
            max_reconstructed_pixels: 32_000_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
/// Requested depth of privacy-oriented metadata and active-content cleanup.
pub enum PrivacyLevel {
    /// Perform no privacy-specific cleanup.
    None,
    /// Remove authoring and document metadata without enabling the full best-effort scrub.
    Metadata,
    /// Apply all enabled privacy cleanup, including active-content and attachment policies.
    BestEffort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "public privacy switches are independent serialized policy fields"
)]
/// Controls privacy-oriented cleanup independently from compression and preservation policy.
pub struct PrivacyConfig {
    /// Requested overall privacy-cleanup level.
    pub level: PrivacyLevel,
    /// Remove JPEG APP1/APP13/COM payloads without touching entropy-coded data.
    pub strip_jpeg_metadata: bool,
    /// Under best-effort mode, also remove most non-essential JPEG APP markers.
    pub aggressive_jpeg_app_scrub: bool,
    /// Remove embedded files / associated files.
    pub remove_attachments: bool,
    /// Remove document JavaScript and dangerous automatic actions.
    pub remove_active_content: bool,
    /// Remove signature values/certificates. Rewriting invalidates signatures anyway.
    pub remove_signatures: bool,
    /// Potentially changes interactive form state; off by default.
    pub remove_form_values: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            level: PrivacyLevel::None,
            strip_jpeg_metadata: false,
            aggressive_jpeg_app_scrub: false,
            remove_attachments: false,
            remove_active_content: false,
            remove_signatures: false,
            remove_form_values: false,
        }
    }
}

/// User-selectable handling for text that is present in PDF content streams
/// but not visible in the default page appearance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct HiddenTextPolicy {
    /// Remove every finding in these semantic categories unless an explicit
    /// per-finding override says to keep it.
    #[serde(default)]
    pub remove_categories: BTreeSet<HiddenTextCategory>,
    /// Per-finding decisions keyed by [`HiddenTextFinding::id`]. These take
    /// precedence over category defaults.
    #[serde(default)]
    pub overrides: BTreeMap<String, HiddenTextAction>,
}

impl HiddenTextPolicy {
    pub(crate) fn should_remove(&self, finding: &HiddenTextFinding) -> bool {
        match self.overrides.get(&finding.id) {
            Some(HiddenTextAction::Keep) => false,
            Some(HiddenTextAction::Remove) => true,
            None => self.remove_categories.contains(&finding.category),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
/// Primary objective used when encoded size and display-list simplicity conflict.
pub enum OptimizationGoal {
    /// Prefer the smallest encoded PDF; candidate structural rewrites may be
    /// rejected when they make the compressed representation larger.
    #[default]
    Size,
    /// Prefer a simpler display list / fewer paint operations, even when that
    /// costs some encoded bytes. Geometry and semantic-preservation gates still apply.
    Processing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlateLevel(u32);

impl FlateLevel {
    pub const PROCESSING: Self = Self(6);
    pub const SIZE: Self = Self(9);

    pub const fn value(self) -> u32 {
        self.0
    }
}

impl std::fmt::Display for FlateLevel {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl OptimizationGoal {
    pub(crate) const fn flate_level(self) -> FlateLevel {
        match self {
            Self::Size => FlateLevel::SIZE,
            Self::Processing => FlateLevel::PROCESSING,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "top-level configuration exposes independent user-selectable feature switches"
)]
/// Complete configuration for one optimization run.
pub struct Config {
    #[serde(default)]
    /// Semantic-preservation policy applied before privacy cleanup.
    pub preservation: PreservationConfig,
    /// Optional override for the effective-PPI limit used by print raster
    /// downsampling. `None` uses the profile's image-policy default.
    #[serde(default)]
    pub max_image_ppi: Option<u16>,
    /// Raster-image optimization policy.
    pub image_policy: ImagePolicy,
    /// Optional structural raster-layout normalization (crop/join/fragment reconstruction).
    #[serde(default)]
    pub raster_layout: RasterLayoutConfig,
    /// Privacy-cleanup policy applied after preservation transforms.
    pub privacy: PrivacyConfig,
    /// Policy for text that is present in content streams but invisible in the default appearance.
    pub hidden_text: HiddenTextPolicy,
    /// Remove self-contained diagonal watermark text. Text at least 24 pt is removed directly;
    /// 20–24 pt text is removed only when the same payload repeats at least three times on a page.
    #[serde(default)]
    pub remove_large_diagonal_text: bool,
    /// Remove exact repeated text/XObject paints that occur at roughly the same page position
    /// on every page, or on every page after the first. Explicit semantic cleanup; off by default.
    #[serde(default)]
    pub remove_repeated_page_objects: bool,
    /// Primary optimization objective used when size and display-list simplicity conflict.
    #[serde(default)]
    pub optimization_goal: OptimizationGoal,
    /// Rasterize only pathological fields of hundreds of tiny opaque vector strokes.
    /// This is intentionally lossy at the vector/semantic level, so it is off for the
    /// normal optimize/processing policies and enabled by default only for Print.
    #[serde(default)]
    pub rasterize_excessive_small_vectors: bool,
    /// Replace eligible large `/ICCBased` color spaces with their declared Device alternate.
    /// This intentionally drops embedded color-management transforms and is therefore lossy.
    #[serde(default)]
    pub elide_icc_profiles_to_alternate: bool,
}

impl Config {
    #[must_use]
    /// Returns the lossless structural-optimization baseline configuration.
    pub fn optimize_only() -> Self {
        Self {
            preservation: PreservationConfig::functional(),
            max_image_ppi: None,
            image_policy: ImagePolicy::Preserve,
            raster_layout: RasterLayoutConfig::default(),
            privacy: PrivacyConfig::default(),
            hidden_text: HiddenTextPolicy::default(),
            remove_large_diagonal_text: false,
            remove_repeated_page_objects: false,
            optimization_goal: OptimizationGoal::Size,
            rasterize_excessive_small_vectors: false,
            elide_icc_profiles_to_alternate: false,
        }
    }

    #[must_use]
    /// Returns the perceptual preset with size-gated JPEG optimization enabled.
    pub fn perceptual() -> Self {
        Self {
            image_policy: ImagePolicy::Perceptual {
                jpeg_quality: 85,
                min_savings_percent: 20,
                preserve_resolution: true,
            },
            ..Self::optimize_only()
        }
    }

    #[must_use]
    /// Returns the print preset with resolution-aware raster optimization enabled.
    pub fn print() -> Self {
        Self {
            max_image_ppi: Some(600),
            image_policy: ImagePolicy::Print {
                jpeg_quality: 85,
                target_ppi: 600,
                min_savings_percent: 20,
            },
            rasterize_excessive_small_vectors: true,
            ..Self::optimize_only()
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::optimize_only()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_config_fields_are_rejected() {
        for stale in [
            r#"{"keep_unused_resources":[]}"#,
            r#"{"deduplicate_metadata_streams":false}"#,
            r#"{"deduplicate_inline_images":false}"#,
            r#"{"generate_object_streams":false}"#,
            r#"{"normalize_content_streams":true}"#,
            r#"{"flate_policy":{"mode":"preserve"}}"#,
            r#"{"prune_resources":false}"#,
            r#"{"inline_image_min_duplicate_payload_bytes":4096}"#,
        ] {
            assert!(serde_json::from_str::<Config>(stale).is_err());
        }
    }

    #[test]
    fn optimization_goal_selects_compression_policy() {
        assert_eq!(OptimizationGoal::Processing.flate_level().value(), 6);
        assert_eq!(OptimizationGoal::Size.flate_level().value(), 9);
    }

    #[test]
    fn raster_occlusion_pruning_is_explicit_opt_in() {
        let raster = RasterLayoutConfig::default();
        assert!(raster.prune_hidden_paints);
        assert!(!raster.prune_occluded_raster_paints);
        assert!(!raster.exact_raster_rendering);
    }

    #[test]
    fn print_profile_controls_microstroke_rasterization_explicitly() {
        assert!(!Config::optimize_only().rasterize_excessive_small_vectors);
        assert!(!Config::perceptual().rasterize_excessive_small_vectors);
        assert!(Config::print().rasterize_excessive_small_vectors);
    }
}
