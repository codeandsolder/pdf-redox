mod analyze;
mod config;
mod dedup;
mod error;
mod flate;
mod hidden_text;
mod jpeg;
mod optimize;
mod print;
mod report;
mod scrub;

pub use analyze::analyze_pdf;
pub use config::{
    Config, ConfigBuilder, FlatePolicy, HiddenTextPolicy, ImagePolicy, OutputProfile,
    PrivacyConfig, PrivacyLevel,
};
pub use error::{Error, Result};
pub use optimize::{optimize_pdf, optimize_pdf_with_analysis};
pub use report::{
    HiddenTextAction, HiddenTextCategory, HiddenTextFinding, HiddenTextMechanism,
    OptimizationReport, PageRect, PdfAnalysis, RiskFinding, RiskKind,
};
