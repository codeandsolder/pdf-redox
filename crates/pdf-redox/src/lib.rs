mod analyze;
mod config;
mod content;
mod dedup;
mod error;
mod flate;
mod font;
mod hidden_text;
mod images;
mod inline_images;
mod jpeg;
mod optimize;
mod preservation;
mod print;
mod prune;
mod report;
mod scrub;
mod source;
mod writer;

pub use analyze::analyze_pdf;
pub use config::{
    AnnotationPolicy, Config, ConfigBuilder, FlatePolicy, HiddenTextPolicy, ImagePolicy,
    OutputProfile, PreservationConfig, PrivacyConfig, PrivacyLevel,
};
pub use error::{Error, Result, SourceLoadError};
pub use optimize::{optimize_pdf, optimize_pdf_with_analysis};
pub use report::{
    HiddenTextAction, HiddenTextCategory, HiddenTextFinding, HiddenTextMechanism,
    OptimizationReport, PageRect, PdfAnalysis, RiskFinding, RiskKind,
};
pub use source::{
    EditDocument, ExistingObjectChange, NewObjectId, ObjectHandle, ObjectId, ObjectOverlay,
    OwnedDictionary, OwnedObject, SourcePdf, StreamData,
};
