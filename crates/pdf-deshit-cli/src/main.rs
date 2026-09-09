mod corpus;

use clap::{Parser, ValueEnum};
use pdf_deshit::{Config, OutputProfile, PrivacyLevel, analyze_pdf, optimize_pdf};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProfileArg {
    Optimize,
    Perceptual,
    Print,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
enum PrivacyArg {
    None,
    Metadata,
    BestEffort,
}

#[derive(Debug, Parser)]
#[command(
    name = "pdf-deshit",
    about = "Pure-Rust PDF normalization and deshittification"
)]
struct Args {
    input: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "optimize")]
    profile: ProfileArg,
    #[arg(long, value_enum, default_value = "none")]
    privacy: PrivacyArg,
    #[arg(long)]
    scrub_jpeg_metadata: bool,
    #[arg(long)]
    remove_attachments: bool,
    #[arg(long)]
    remove_active_content: bool,
    #[arg(long)]
    remove_signatures: bool,
    /// Experimentally remove unused Font/XObject resource entries.
    #[arg(long)]
    prune_resources: bool,
    /// Preserve separate byte-identical metadata stream objects instead of canonicalizing them.
    #[arg(long)]
    no_metadata_dedup: bool,
    /// Recursively analyze every PDF below INPUT and emit one aggregate JSON report.
    #[arg(long)]
    corpus: bool,
    /// Number of per-file worst cases retained in each corpus ranking.
    #[arg(long, default_value_t = 20)]
    corpus_top: usize,
    #[arg(long)]
    analyze_only: bool,
    #[arg(long)]
    json: bool,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    if a.corpus {
        let report = corpus::analyze_corpus(&a.input, a.corpus_top)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let input = std::fs::read(&a.input)?;
    if a.analyze_only {
        let r = analyze_pdf(&input)?;
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    let mut cfg = match a.profile {
        ProfileArg::Optimize => Config::optimize_only(),
        ProfileArg::Perceptual => Config::perceptual(),
        ProfileArg::Print => Config::print(),
    };
    cfg.profile = match a.profile {
        ProfileArg::Optimize => OutputProfile::OptimizeOnly,
        ProfileArg::Perceptual => OutputProfile::Perceptual,
        ProfileArg::Print => OutputProfile::Print,
    };
    cfg.privacy.level = match a.privacy {
        PrivacyArg::None => PrivacyLevel::None,
        PrivacyArg::Metadata => PrivacyLevel::Metadata,
        PrivacyArg::BestEffort => PrivacyLevel::BestEffort,
    };
    cfg.privacy.strip_jpeg_metadata =
        a.scrub_jpeg_metadata || cfg.privacy.level != PrivacyLevel::None;
    cfg.privacy.remove_attachments = a.remove_attachments;
    cfg.privacy.remove_active_content = a.remove_active_content;
    cfg.privacy.remove_signatures = a.remove_signatures;
    cfg.prune_resources = a.prune_resources;
    cfg.deduplicate_metadata_streams = !a.no_metadata_dedup;

    let (output, report) = optimize_pdf(&input, &cfg)?;
    let out_path = a.output.unwrap_or_else(|| {
        let stem = a
            .input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        a.input.with_file_name(format!("{stem}.deshit.pdf"))
    });
    std::fs::write(&out_path, output)?;
    if a.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        eprintln!(
            "{} -> {} bytes ({:+.2}%)",
            report.before.input_bytes, report.after_bytes, -report.saved_percent
        );
        eprintln!("wrote {}", out_path.display());
    }
    Ok(())
}
