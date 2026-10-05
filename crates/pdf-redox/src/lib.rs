#![doc = include_str!("../../../README.md")]
#![deny(missing_docs)]

mod analyze;
mod bilevel;
mod cff_cid;
mod config;
mod content;
mod content_stream;
mod dedup;
mod error;
mod flate;
mod font;
mod geometry;
mod hidden_text;
mod icc_alternate;
mod images;
mod inline_images;
mod jpeg;
mod jpeg_optimize;
mod microstroke;
mod optimize;
mod paint_batch;
mod preservation;
mod print;
mod prune;
mod raster_layout;
mod repeated_page_objects;
mod report;
mod scrub;
mod sfnt_bitmap;
mod source;
mod stream_codec;
mod structure_compact;
mod vector_compact;
mod writer;

pub use analyze::analyze_pdf;
pub use config::{
    AnnotationPolicy, Config, ConfigBuilder, FlatePolicy, HiddenTextPolicy, ImagePolicy,
    OptimizationGoal, OutputProfile, PreservationConfig, PrivacyConfig, PrivacyLevel,
    RasterLayoutConfig,
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
