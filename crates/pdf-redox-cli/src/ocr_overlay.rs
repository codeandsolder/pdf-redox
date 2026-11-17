//! Apply externally supplied OCR line text without changing vector paint.
use clap::Parser;
use pdf_redox::{OcrDocument, OcrOverlayOptions, apply_ocr_overlay};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pdf-redox-ocr-overlay")]
struct Args {
    /// Input PDF, original or optimized. Must match the OCR draft's SHA-256.
    input: PathBuf,
    /// JSON OCR line detections with page-space quads and confidence.
    #[arg(long)]
    ocr: PathBuf,
    /// Resulting vector PDF with an invisible OCR text layer.
    #[arg(short, long)]
    output: PathBuf,
    /// Allow unreviewed OCR predictions meeting confidence and native-text exclusion checks.
    #[arg(long)]
    apply_unreviewed: bool,
    /// Lower confidence bound applied to unreviewed predictions.
    #[arg(long, default_value_t = 0.97)]
    min_confidence: f64,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let input = std::fs::read(&args.input)?;
    let draft: OcrDocument = serde_json::from_slice(&std::fs::read(&args.ocr)?)?;
    let (output, report) = apply_ocr_overlay(
        &input,
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
