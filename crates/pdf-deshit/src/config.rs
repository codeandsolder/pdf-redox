use crate::report::{HiddenTextAction, HiddenTextCategory, HiddenTextFinding};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputProfile {
    /// No intended visual changes. Fresh rewrite, GC, stream normalization,
    /// lossless metadata cleanup only when explicitly requested.
    OptimizeOnly,
    /// Lossy image transforms may be used when they pass the configured
    /// savings/quality gates. Spatial resolution is preserved by default.
    Perceptual,
    /// Print-oriented policy. Allows resolution-aware raster reduction and
    /// stronger photographic recompression, while keeping line art lossless.
    Print,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode")]
#[derive(Default)]
pub enum ImagePolicy {
    #[default]
    Preserve,
    Perceptual {
        jpeg_quality: u8,
        min_savings_percent: u8,
        preserve_resolution: bool,
    },
    Print {
        jpeg_quality: u8,
        target_ppi: u16,
        min_savings_percent: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "mode")]
pub enum FlatePolicy {
    /// Preserve unmodified lone-Flate streams byte-for-byte.
    Preserve,
    /// Recompress only lone-Flate streams that clear explicit size gates.
    Selective {
        min_savings_bytes: usize,
        min_savings_percent: u8,
    },
    /// Ask the writer to recompress every eligible Flate stream.
    RecompressAll,
}

impl Default for FlatePolicy {
    fn default() -> Self {
        Self::Selective {
            min_savings_bytes: 1024,
            min_savings_percent: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PrivacyLevel {
    None,
    Metadata,
    BestEffort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyConfig {
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub profile: OutputProfile,
    pub image_policy: ImagePolicy,
    pub privacy: PrivacyConfig,
    pub hidden_text: HiddenTextPolicy,
    /// Repack eligible small indirect objects into ObjStm containers.
    pub generate_object_streams: bool,
    /// Normalize lexical representation of page content streams.
    pub normalize_content_streams: bool,
    /// Policy for preserving or recompressing existing Flate streams.
    pub flate_policy: FlatePolicy,
    /// Remove unused `/Font` and `/XObject` resource entries using flpdf's
    /// qpdf-compatible parse-gated pruning pass. Experimental until corpus validation.
    pub prune_resources: bool,
    /// Canonicalize byte- and dictionary-identical `/Metadata` streams so a fresh rewrite
    /// can garbage-collect duplicate XMP objects.
    pub deduplicate_metadata_streams: bool,
    /// Canonicalize exact duplicate embedded font-program streams referenced through the
    /// same `/FontFile`, `/FontFile2`, or `/FontFile3` key kind.
    pub deduplicate_font_programs: bool,
    /// Externalize and share only exact inline images repeated across multiple mutable
    /// content scopes whose duplicated encoded payload clears the configured size gate.
    pub deduplicate_inline_images: bool,
    /// Minimum duplicated encoded payload bytes for one exact inline-image fingerprint.
    /// Inline-image header savings are deliberately ignored by this gate.
    pub inline_image_min_duplicate_payload_bytes: usize,
    /// Canonicalize byte- and dictionary-identical Image XObjects referenced from
    /// `/Resources /XObject` dictionaries.
    pub deduplicate_image_xobjects: bool,
    /// Canonicalize byte- and dictionary-identical ICC profile streams referenced from
    /// `/ICCBased` color-space arrays.
    pub deduplicate_icc_profiles: bool,
    /// zlib level used for rewritten streams. 9 is slower at ingest but cheap to decode.
    pub flate_level: i32,
}

impl Config {
    pub fn optimize_only() -> Self {
        Self {
            profile: OutputProfile::OptimizeOnly,
            image_policy: ImagePolicy::Preserve,
            privacy: PrivacyConfig::default(),
            hidden_text: HiddenTextPolicy::default(),
            generate_object_streams: true,
            normalize_content_streams: false,
            flate_policy: FlatePolicy::default(),
            prune_resources: false,
            deduplicate_metadata_streams: true,
            deduplicate_font_programs: true,
            deduplicate_inline_images: true,
            inline_image_min_duplicate_payload_bytes: 1024,
            deduplicate_image_xobjects: true,
            deduplicate_icc_profiles: true,
            flate_level: 9,
        }
    }

    pub fn perceptual() -> Self {
        Self {
            profile: OutputProfile::Perceptual,
            image_policy: ImagePolicy::Perceptual {
                jpeg_quality: 85,
                min_savings_percent: 20,
                preserve_resolution: true,
            },
            ..Self::optimize_only()
        }
    }

    pub fn print() -> Self {
        Self {
            profile: OutputProfile::Print,
            image_policy: ImagePolicy::Print {
                jpeg_quality: 85,
                target_ppi: 450,
                min_savings_percent: 20,
            },
            ..Self::optimize_only()
        }
    }
}

/// Fluent construction for [`Config`] when callers need to override several
/// independent policy knobs without mutating public fields piecemeal.
#[derive(Debug, Clone, PartialEq)]
#[must_use = "config builders do nothing unless build() is called"]
pub struct ConfigBuilder {
    config: Config,
}

impl ConfigBuilder {
    pub fn image_policy(mut self, value: ImagePolicy) -> Self {
        self.config.image_policy = value;
        self
    }

    pub fn privacy(mut self, value: PrivacyConfig) -> Self {
        self.config.privacy = value;
        self
    }

    pub fn hidden_text(mut self, value: HiddenTextPolicy) -> Self {
        self.config.hidden_text = value;
        self
    }

    pub fn generate_object_streams(mut self, value: bool) -> Self {
        self.config.generate_object_streams = value;
        self
    }

    pub fn normalize_content_streams(mut self, value: bool) -> Self {
        self.config.normalize_content_streams = value;
        self
    }

    pub fn flate_policy(mut self, value: FlatePolicy) -> Self {
        self.config.flate_policy = value;
        self
    }

    pub fn prune_resources(mut self, value: bool) -> Self {
        self.config.prune_resources = value;
        self
    }

    pub fn deduplicate_metadata_streams(mut self, value: bool) -> Self {
        self.config.deduplicate_metadata_streams = value;
        self
    }

    pub fn deduplicate_font_programs(mut self, value: bool) -> Self {
        self.config.deduplicate_font_programs = value;
        self
    }

    pub fn deduplicate_inline_images(mut self, value: bool) -> Self {
        self.config.deduplicate_inline_images = value;
        self
    }

    pub fn inline_image_min_duplicate_payload_bytes(mut self, value: usize) -> Self {
        self.config.inline_image_min_duplicate_payload_bytes = value;
        self
    }

    pub fn deduplicate_image_xobjects(mut self, value: bool) -> Self {
        self.config.deduplicate_image_xobjects = value;
        self
    }

    pub fn deduplicate_icc_profiles(mut self, value: bool) -> Self {
        self.config.deduplicate_icc_profiles = value;
        self
    }

    pub fn flate_level(mut self, value: i32) -> Self {
        self.config.flate_level = value;
        self
    }

    pub fn build(self) -> Config {
        self.config
    }
}

impl Config {
    /// Start from the canonical preset for `profile` and override only the
    /// settings the caller cares about.
    pub fn builder(profile: OutputProfile) -> ConfigBuilder {
        let config = match profile {
            OutputProfile::OptimizeOnly => Self::optimize_only(),
            OutputProfile::Perceptual => Self::perceptual(),
            OutputProfile::Print => Self::print(),
        };
        ConfigBuilder { config }
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
    fn builder_starts_from_requested_profile() {
        let perceptual = Config::builder(OutputProfile::Perceptual).build();
        assert_eq!(perceptual, Config::perceptual());

        let print = Config::builder(OutputProfile::Print).build();
        assert_eq!(print, Config::print());
    }

    #[test]
    fn builder_overrides_independent_policy_knobs() {
        let config = Config::builder(OutputProfile::OptimizeOnly)
            .generate_object_streams(false)
            .normalize_content_streams(true)
            .flate_policy(FlatePolicy::Preserve)
            .prune_resources(true)
            .deduplicate_metadata_streams(false)
            .deduplicate_font_programs(true)
            .deduplicate_inline_images(false)
            .inline_image_min_duplicate_payload_bytes(4096)
            .deduplicate_image_xobjects(false)
            .deduplicate_icc_profiles(false)
            .flate_level(6)
            .privacy(PrivacyConfig {
                level: PrivacyLevel::Metadata,
                strip_jpeg_metadata: true,
                ..PrivacyConfig::default()
            })
            .build();

        assert!(!config.generate_object_streams);
        assert!(config.normalize_content_streams);
        assert_eq!(config.flate_policy, FlatePolicy::Preserve);
        assert!(config.prune_resources);
        assert!(!config.deduplicate_metadata_streams);
        assert!(config.deduplicate_font_programs);
        assert!(!config.deduplicate_inline_images);
        assert_eq!(config.inline_image_min_duplicate_payload_bytes, 4096);
        assert!(!config.deduplicate_image_xobjects);
        assert!(!config.deduplicate_icc_profiles);
        assert_eq!(config.flate_level, 6);
        assert_eq!(config.privacy.level, PrivacyLevel::Metadata);
        assert!(config.privacy.strip_jpeg_metadata);
    }
}
