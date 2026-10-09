#![doc = include_str!("../../../README.md")]
#![deny(missing_docs)]

mod analyze;
mod bilevel;
mod cff_cid;
mod config;
mod content;
pub(crate) mod content_stream;
mod dedup;
mod error;
mod flate;
mod font;
mod font_subset;
pub(crate) mod geometry;
pub(crate) mod hidden_text;
mod icc_alternate;
pub(crate) mod images;
mod inline_images;
mod jpeg;
mod jpeg_optimize;
mod microstroke;
mod optimize;
mod paint_batch;
mod polyline_simplify;
mod preservation;
mod print;
mod prune;
mod raster_layout;
mod repeated_clip;
mod repeated_page_objects;
mod report;
mod scrub;
pub(crate) mod source;
pub(crate) mod stream_codec;
mod structure_compact;
#[cfg(test)]
pub(crate) mod test_support;
mod vector_compact;
pub(crate) mod writer;

pub use analyze::{analyze_pdf, analyze_pdf_preflight};
pub(crate) use config::FlateLevel;
pub use config::{
    AnnotationPolicy, Config, HiddenTextPolicy, ImagePolicy, OptimizationGoal, PreservationConfig,
    PrivacyConfig, PrivacyLevel, RasterLayoutConfig,
};
pub use error::{Error, Result, SourceLoadError};
pub use microstroke::MicrostrokeRasterStats;
pub use optimize::{analyze_microstroke_rasterization, optimize_pdf, optimize_pdf_with_analysis};
pub use report::{
    HiddenTextAction, HiddenTextCategory, HiddenTextFinding, HiddenTextMechanism,
    OptimizationReport, PageRect, PdfAnalysis, RiskFinding, RiskKind,
};
pub use source::{
    EditDocument, ExistingObjectChange, NewObjectId, ObjectHandle, ObjectId, ObjectOverlay,
    OwnedDictionary, OwnedObject, SourcePdf, StreamData,
};
