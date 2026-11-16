//! Apply externally supplied OCR line text without changing vector paint.
use clap::Parser;
use pdf_redox::{GlyphCatalog, OcrDocument, OcrOverlayOptions, apply_ocr_overlay};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pdf-redox-ocr-overlay")]
struct Args {
    /// Input optimized PDF (not the original source).
    input: PathBuf,
    /// Matching schema-v2 vector glyph catalog.
    #[arg(long)]
    catalog: PathBuf,
    /// JSON OCR line detections with page-space quads and confidence.
    #[arg(long)]
    ocr: PathBuf,
    /// Resulting vector PDF with an invisible OCR text layer.
    #[arg(short, long)]
    output: PathBuf,
    /// Include unreviewed OCR predictions that meet confidence and vector-coverage gates.
    #[arg(long)]
    apply_unreviewed: bool,
    /// Lower confidence bound applied to unreviewed predictions.
    #[arg(long, default_value_t = 0.97)]
    min_confidence: f64,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let input = std::fs::read(&args.input)?;
    let catalog: GlyphCatalog = serde_json::from_slice(&std::fs::read(&args.catalog)?)?;
    let draft: OcrDocument = serde_json::from_slice(&std::fs::read(&args.ocr)?)?;
    let (output, report) = apply_ocr_overlay(
        &input,
        &catalog,
        &draft,
        OcrOverlayOptions {
            apply_unreviewed: args.apply_unreviewed,
            min_confidence: args.min_confidence,
        },
    )?;
    std::fs::write(&args.output, output)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
