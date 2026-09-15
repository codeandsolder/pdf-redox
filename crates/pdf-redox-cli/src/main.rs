mod corpus;

use clap::{Parser, ValueEnum};
use pdf_redox::{
    AnnotationPolicy, Config, EditDocument, FlatePolicy, PrivacyConfig, PrivacyLevel, analyze_pdf,
    optimize_pdf,
};
use std::{
    io::{self, Write},
    path::PathBuf,
};

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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum FlatePolicyArg {
    Preserve,
    Selective,
    RecompressAll,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AnnotationPolicyArg {
    Preserve,
    AppearanceOnly,
    Discard,
}

#[derive(Debug, Parser)]
#[command(
    name = "pdf-redox",
    about = "Pure-Rust PDF normalization, optimization, and cleanup"
)]
struct Args {
    input: PathBuf,
    #[arg(short, long)]
    output: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "optimize")]
    profile: ProfileArg,
    /// Drop Link annotation navigation/actions.
    #[arg(long)]
    drop_links: bool,
    /// Drop interactive form state and Widget annotations.
    #[arg(long)]
    drop_forms: bool,
    /// Drop outlines, named destinations, page labels, threads, and open actions.
    #[arg(long)]
    drop_navigation: bool,
    /// Drop optional-content/layer configuration.
    #[arg(long)]
    drop_optional_content: bool,
    /// Drop tagged-PDF/accessibility structure.
    #[arg(long)]
    drop_structure: bool,
    /// Drop color-management output intents.
    #[arg(long)]
    drop_output_intents: bool,
    /// Drop viewer layout/mode/preferences.
    #[arg(long)]
    drop_viewer_preferences: bool,
    /// Drop Catalog/page metadata-like auxiliary objects before privacy scrubbing.
    #[arg(long)]
    drop_document_metadata: bool,
    /// Drop unrecognized Catalog/page entries and objects reachable only through them.
    #[arg(long)]
    drop_unknown_objects: bool,
    /// Drop embedded-font tables useful for later editing/reflow but unused by PDF page rendering.
    #[arg(long)]
    drop_font_editing_support: bool,
    /// Override handling of non-Link/non-Widget annotations.
    #[arg(long, value_enum)]
    annotations: Option<AnnotationPolicyArg>,
    /// Override the effective-PPI target used by the print image policy.
    #[arg(long)]
    max_image_ppi: Option<u16>,
    #[arg(long, value_enum, default_value = "none")]
    privacy: PrivacyArg,
    /// Existing Flate stream policy. Selective is the optimize-only default.
    #[arg(long, value_enum, default_value = "selective")]
    flate_policy: FlatePolicyArg,
    /// Normalize page-content token syntax. This can increase file size.
    #[arg(long)]
    normalize_content: bool,
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
    /// Preserve repeated inline-image syntax instead of externalizing exact duplicates.
    #[arg(long)]
    no_inline_image_dedup: bool,
    /// Minimum duplicated encoded payload bytes for one cross-scope inline-image fingerprint.
    #[arg(long, default_value_t = 1024)]
    inline_image_dedup_min_waste: usize,
    /// Preserve separate exact duplicate embedded font-program stream objects.
    #[arg(long)]
    no_font_program_dedup: bool,
    /// Preserve separate exact duplicate ToUnicode CMap stream objects.
    #[arg(long)]
    no_to_unicode_dedup: bool,
    /// Preserve separate exact duplicate Image XObjects instead of canonicalizing them.
    #[arg(long)]
    no_image_dedup: bool,
    /// Preserve separate exact duplicate Form XObjects instead of canonicalizing them.
    #[arg(long)]
    no_form_dedup: bool,
    /// Preserve separate exact duplicate annotation appearance streams instead of canonicalizing them.
    #[arg(long)]
    no_appearance_dedup: bool,
    /// Preserve separate exact duplicate page content streams instead of canonicalizing them.
    #[arg(long)]
    no_page_content_dedup: bool,
    /// Preserve separate exact duplicate Type3 CharProc streams instead of canonicalizing them.
    #[arg(long)]
    no_type3_charproc_dedup: bool,
    /// Preserve separate exact duplicate ICC profile streams instead of canonicalizing them.
    #[arg(long)]
    no_icc_dedup: bool,
    /// Recursively analyze every PDF below INPUT and emit one aggregate JSON report.
    #[arg(long)]
    corpus: bool,
    /// Number of per-file worst cases retained in each corpus ranking.
    #[arg(long, default_value_t = 20)]
    corpus_top: usize,
    #[arg(long)]
    analyze_only: bool,
    /// Migration-only fresh rewrite through the Hayro/COW backend.
    #[arg(long, hide = true)]
    hayro_rewrite_experimental: bool,
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
    if a.hayro_rewrite_experimental {
        if a.scrub_jpeg_metadata || a.remove_attachments || a.remove_signatures {
            return Err(
                "Hayro experimental mode has not migrated JPEG, attachment, or signature privacy operations yet"
                    .into(),
            );
        }
        let input_bytes = input.len();
        let mut document = EditDocument::from_bytes(input)?;
        let privacy_level = match a.privacy {
            PrivacyArg::None => PrivacyLevel::None,
            PrivacyArg::Metadata => PrivacyLevel::Metadata,
            PrivacyArg::BestEffort => PrivacyLevel::BestEffort,
        };
        let privacy_items_removed = document.scrub_cos_privacy_experimental(&PrivacyConfig {
            level: privacy_level,
            remove_active_content: a.remove_active_content,
            ..PrivacyConfig::default()
        })?;
        let font_optimization = if a.drop_font_editing_support {
            document.strip_font_editing_tables_experimental(Config::optimize_only().flate_level)?
        } else {
            Default::default()
        };
        let font_program_dedup = if a.no_font_program_dedup {
            Default::default()
        } else {
            document.canonicalize_font_programs_experimental()?
        };
        let to_unicode_dedup = if a.no_to_unicode_dedup {
            Default::default()
        } else {
            document.canonicalize_to_unicode_experimental()?
        };
        let page_count = document.source().page_count();
        let source_objects = document.source().object_count();
        let output = document.write_compact_experimental()?;
        let output_bytes = output.len();
        let out_path = a.output.unwrap_or_else(|| {
            let stem = a
                .input
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("output");
            a.input.with_file_name(format!("{stem}.redox.pdf"))
        });
        if out_path.as_os_str() == "-" {
            let stdout = io::stdout();
            let mut lock = stdout.lock();
            lock.write_all(&output)?;
            lock.flush()?;
        } else {
            std::fs::write(&out_path, output)?;
        }
        if a.json {
            let summary = serde_json::json!({
                "input_bytes": input_bytes,
                "output_bytes": output_bytes,
                "page_count": page_count,
                "source_objects": source_objects,
                "privacy_items_removed": privacy_items_removed,
                "font_optimization": font_optimization,
                "font_program_dedup": font_program_dedup,
                "to_unicode_dedup": to_unicode_dedup,
            });
            if out_path.as_os_str() == "-" {
                eprintln!("{}", serde_json::to_string(&summary)?);
            } else {
                println!("{}", serde_json::to_string_pretty(&summary)?);
            }
        }
        return Ok(());
    }
    let mut cfg = match a.profile {
        ProfileArg::Optimize => Config::optimize_only(),
        ProfileArg::Perceptual => Config::perceptual(),
        ProfileArg::Print => Config::print(),
    };
    if a.drop_links {
        cfg.preservation.links = false;
    }
    if a.drop_forms {
        cfg.preservation.forms = false;
    }
    if a.drop_navigation {
        cfg.preservation.navigation = false;
    }
    if a.drop_optional_content {
        cfg.preservation.optional_content = false;
    }
    if a.drop_structure {
        cfg.preservation.structure = false;
    }
    if a.drop_output_intents {
        cfg.preservation.output_intents = false;
    }
    if a.drop_viewer_preferences {
        cfg.preservation.viewer_preferences = false;
    }
    if a.drop_document_metadata {
        cfg.preservation.metadata = false;
    }
    if a.drop_unknown_objects {
        cfg.preservation.unknown_objects = false;
    }
    if a.drop_font_editing_support {
        cfg.preservation.font_editing_support = false;
    }
    if let Some(policy) = a.annotations {
        cfg.preservation.annotations = match policy {
            AnnotationPolicyArg::Preserve => AnnotationPolicy::Preserve,
            AnnotationPolicyArg::AppearanceOnly => AnnotationPolicy::AppearanceOnly,
            AnnotationPolicyArg::Discard => AnnotationPolicy::Discard,
        };
    }
    if let Some(max_image_ppi) = a.max_image_ppi {
        cfg.max_image_ppi = Some(max_image_ppi);
    }
    cfg.flate_policy = match a.flate_policy {
        FlatePolicyArg::Preserve => FlatePolicy::Preserve,
        FlatePolicyArg::Selective => FlatePolicy::default(),
        FlatePolicyArg::RecompressAll => FlatePolicy::RecompressAll,
    };
    cfg.normalize_content_streams = a.normalize_content;
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
    cfg.deduplicate_font_programs = !a.no_font_program_dedup;
    cfg.deduplicate_to_unicode_cmaps = !a.no_to_unicode_dedup;
    cfg.deduplicate_inline_images = !a.no_inline_image_dedup;
    cfg.inline_image_min_duplicate_payload_bytes = a.inline_image_dedup_min_waste;
    cfg.deduplicate_image_xobjects = !a.no_image_dedup;
    cfg.deduplicate_form_xobjects = !a.no_form_dedup;
    cfg.deduplicate_appearance_streams = !a.no_appearance_dedup;
    cfg.deduplicate_page_contents = !a.no_page_content_dedup;
    cfg.deduplicate_type3_charprocs = !a.no_type3_charproc_dedup;
    cfg.deduplicate_icc_profiles = !a.no_icc_dedup;

    let (output, report) = optimize_pdf(&input, &cfg)?;
    let out_path = a.output.unwrap_or_else(|| {
        let stem = a
            .input
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("output");
        a.input.with_file_name(format!("{stem}.redox.pdf"))
    });
    let output_to_stdout = out_path.as_os_str() == "-";
    if output_to_stdout {
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(&output)?;
        lock.flush()?;
        if a.json {
            eprintln!("{}", serde_json::to_string(&report)?);
        } else {
            eprintln!(
                "{} -> {} bytes ({:+.2}%)",
                report.before.input_bytes, report.after_bytes, -report.saved_percent
            );
        }
    } else {
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
    }
    Ok(())
}
