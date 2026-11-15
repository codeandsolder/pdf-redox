//! Apply verified Unicode labels to an existing optimized PDF.
use clap::Parser;
use pdf_redox::{GlyphCatalog, GlyphLabels, apply_glyph_labels};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "pdf-redox-glyph-labels")]
struct Args {
    /// Input PDF produced by pdf-redox Processing.
    input: PathBuf,
    /// JSON glyph catalog exported for these exact PDF bytes.
    #[arg(long)]
    catalog: PathBuf,
    /// Reviewed semantic template labels and per-occurrence overrides.
    #[arg(long)]
    labels: PathBuf,
    /// PDF with Unicode `ActualText` added to selected glyphs.
    #[arg(short, long)]
    output: PathBuf,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let pdf = std::fs::read(&args.input)?;
    let catalog: GlyphCatalog = serde_json::from_slice(&std::fs::read(&args.catalog)?)?;
    let labels: GlyphLabels = serde_json::from_slice(&std::fs::read(&args.labels)?)?;
    let (output, report) = apply_glyph_labels(&pdf, &catalog, &labels)?;
    std::fs::write(&args.output, output)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
