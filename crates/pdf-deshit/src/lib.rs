mod analyze;
mod config;
mod error;
mod hidden_text;
mod jpeg;
mod optimize;
mod report;
mod scrub;

pub use analyze::analyze_pdf;
pub use config::{
    Config, ConfigBuilder, HiddenTextPolicy, ImagePolicy, OutputProfile, PrivacyConfig,
    PrivacyLevel,
};
pub use error::{Error, Result};
pub use optimize::optimize_pdf;
pub use report::{
    HiddenTextAction, HiddenTextCategory, HiddenTextFinding, HiddenTextMechanism,
    OptimizationReport, PageRect, PdfAnalysis, RiskFinding, RiskKind,
};
