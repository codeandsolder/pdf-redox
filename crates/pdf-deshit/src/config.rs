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
    /// Re-run DEFLATE on already-Flate streams. Important for malformed exporters.
    pub recompress_flate: bool,
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
            normalize_content_streams: true,
            recompress_flate: true,
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

impl Default for Config {
    fn default() -> Self {
        Self::optimize_only()
    }
}
