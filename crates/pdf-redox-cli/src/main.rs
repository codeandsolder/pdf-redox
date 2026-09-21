mod corpus;

use clap::{Parser, ValueEnum};
use pdf_redox::{
    AnnotationPolicy, Config, FlatePolicy, OptimizationGoal, PrivacyLevel,
    analyze_microstroke_rasterization, analyze_pdf, optimize_pdf,
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
enum OptimizeForArg {
    Size,
    Processing,
}

fn parse_flate_level(value: &str) -> Result<i32, String> {
    let level = value
        .parse::<i32>()
        .map_err(|_| "Flate level must be an integer from 0 through 9".to_owned())?;
    (0..=9)
        .contains(&level)
        .then_some(level)
        .ok_or_else(|| "Flate level must be an integer from 0 through 9".to_owned())
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
    /// Drop authoring/document metadata throughout the known object graph.
    /// Processing mode already does this unless --preserve-authoring-data is set.
    #[arg(long)]
    drop_document_metadata: bool,
    /// Drop unrecognized entries under the currently classified known-semantics graph.
    #[arg(long)]
    drop_unknown_objects: bool,
    /// Experimentally promote recognized child keys out of dropped unknown wrapper dictionaries.
    #[arg(long, requires = "drop_unknown_objects")]
    splice_unknown_wrappers: bool,
    /// In processing mode, retain authoring metadata/editing support and unknown extensions.
    #[arg(long)]
    preserve_authoring_data: bool,
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
    /// zlib compression level for rewritten Flate streams (0-9).
    #[arg(long, value_parser = parse_flate_level)]
    flate_level: Option<i32>,
    /// Choose whether conflicting rewrites favor encoded bytes or a simpler display list.
    #[arg(long, value_enum, default_value = "size")]
    optimize_for: OptimizeForArg,
    /// Normalize page-content token syntax. This can increase file size.
    #[arg(long)]
    normalize_content: bool,
    /// Compact compatible vector path paints without rasterizing them.
    #[arg(long)]
    compact_vector_paths: bool,
    /// Rasterize pathological fields of hundreds of tiny opaque vector strokes.
    /// Enabled automatically by the Print profile.
    #[arg(long, conflicts_with = "keep_excessive_small_vectors")]
    rasterize_excessive_small_vectors: bool,
    /// Preserve pathological tiny-vector fields even when the Print profile would rasterize them.
    #[arg(long, conflicts_with = "rasterize_excessive_small_vectors")]
    keep_excessive_small_vectors: bool,
    /// Reconstruct pathological fragmented raster layouts (pixel sprites and split stripes).
    /// Enabled automatically with `--optimize-for processing`.
    #[arg(long)]
    normalize_raster_layout: bool,
    /// Materialize image masks into normalized alpha planes while reconstructing rasters.
    /// Enabled automatically with `--optimize-for processing`.
    #[arg(long)]
    bake_image_masks: bool,
    /// Preserve exact color/alpha resampling for rebuilt binary-alpha rasters. Without this,
    /// normal mode may replace constant visible color + binary alpha with a smaller stencil,
    /// allowing tiny antialiasing differences when viewers downsample the image.
    #[arg(long)]
    exact_raster_rendering: bool,
    /// Remove large diagonal watermark text; repeated 20–24 pt tiled stamps are also recognized.
    #[arg(long)]
    remove_large_diagonal_text: bool,
    /// Remove identical text/Image/Form paints that recur at roughly the same position on every
    /// page, or every page after the first. Requires at least three supporting pages.
    #[arg(long)]
    remove_repeated_page_objects: bool,
    /// Keep paint that raster normalization would otherwise classify as safely removable.
    #[arg(long)]
    keep_hidden_paints: bool,
    /// Also remove raster paints inferred to be fully occluded by later opaque paint. This is
    /// experimental and is disabled by default because coverage is not always renderer-exact.
    #[arg(long)]
    prune_occluded_raster_paints: bool,
    /// Page-space connectivity radius for tiny-image pixel clusters, in millimetres.
    #[arg(long)]
    pixel_cluster_gap_mm: Option<f32>,
    #[arg(long)]
    scrub_jpeg_metadata: bool,
    #[arg(long)]
    remove_attachments: bool,
    #[arg(long)]
    remove_active_content: bool,
    #[arg(long)]
    remove_signatures: bool,
    /// Remove unused Font/XObject and typed ExtGState/Pattern/Properties/Shading resource entries. Enabled automatically in processing mode.
    #[arg(long)]
    prune_resources: bool,
    /// Keep selected unused resources while pruning. With no value, keep all unused resources.
    /// Selectors: `*`, `<Category>:*`, `<Category>:<Name>`, or a bare name; categories include `Font`, `XObject`, `ExtGState`, `Pattern`, `Properties`, and `Shading`.
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "*",
        value_delimiter = ',',
        action = clap::ArgAction::Append
    )]
    keep_unused_resources: Vec<String>,
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
    /// Preserve separate exact duplicate `ToUnicode` `CMap` stream objects.
    #[arg(long)]
    no_to_unicode_dedup: bool,
    /// Preserve separate exact duplicate Image `XObjects` instead of canonicalizing them.
    #[arg(long)]
    no_image_dedup: bool,
    /// Preserve separate exact duplicate Form `XObjects` instead of canonicalizing them.
    #[arg(long)]
    no_form_dedup: bool,
    /// Preserve separate exact duplicate annotation appearance streams instead of canonicalizing them.
    #[arg(long)]
    no_appearance_dedup: bool,
    /// Preserve separate exact duplicate page content streams instead of canonicalizing them.
    #[arg(long)]
    no_page_content_dedup: bool,
    /// Preserve separate exact duplicate Type3 `CharProc` streams instead of canonicalizing them.
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
    /// Run only the production pathological-microstroke detector/cost gate.
    #[arg(long, hide = true)]
    analyze_microstrokes: bool,
    #[arg(long)]
    analyze_only: bool,
    #[arg(long)]
    json: bool,
}

fn config_from_args(args: &Args) -> Config {
    let mut cfg = match args.profile {
        ProfileArg::Optimize => Config::optimize_only(),
        ProfileArg::Perceptual => Config::perceptual(),
        ProfileArg::Print => Config::print(),
    };
    if matches!(args.optimize_for, OptimizeForArg::Processing) && !args.preserve_authoring_data {
        cfg.preservation = pdf_redox::PreservationConfig::known_functional();
    }
    if args.drop_links {
        cfg.preservation.links = false;
    }
    if args.drop_forms {
        cfg.preservation.forms = false;
    }
    if args.drop_navigation {
        cfg.preservation.navigation = false;
    }
    if args.drop_optional_content {
        cfg.preservation.optional_content = false;
    }
    if args.drop_structure {
        cfg.preservation.structure = false;
    }
    if args.drop_output_intents {
        cfg.preservation.output_intents = false;
    }
    if args.drop_viewer_preferences {
        cfg.preservation.viewer_preferences = false;
    }
    if args.drop_document_metadata {
        cfg.preservation.metadata = false;
    }
    if args.drop_unknown_objects {
        cfg.preservation.unknown_objects = false;
    }
    cfg.preservation.splice_unknown_wrappers = args.splice_unknown_wrappers;
    if args.drop_font_editing_support {
        cfg.preservation.font_editing_support = false;
    }
    if let Some(policy) = args.annotations {
        cfg.preservation.annotations = match policy {
            AnnotationPolicyArg::Preserve => AnnotationPolicy::Preserve,
            AnnotationPolicyArg::AppearanceOnly => AnnotationPolicy::AppearanceOnly,
            AnnotationPolicyArg::Discard => AnnotationPolicy::Discard,
        };
    }
    if let Some(max_image_ppi) = args.max_image_ppi {
        cfg.max_image_ppi = Some(max_image_ppi);
    }
    cfg.flate_policy = match args.flate_policy {
        FlatePolicyArg::Preserve => FlatePolicy::Preserve,
        FlatePolicyArg::Selective => FlatePolicy::default(),
        FlatePolicyArg::RecompressAll => FlatePolicy::RecompressAll,
    };
    cfg.optimization_goal = match args.optimize_for {
        OptimizeForArg::Size => OptimizationGoal::Size,
        OptimizeForArg::Processing => OptimizationGoal::Processing,
    };
    cfg.flate_level = args.flate_level.unwrap_or_else(|| {
        if cfg.optimization_goal == OptimizationGoal::Processing {
            5
        } else {
            cfg.flate_level
        }
    });
    cfg.normalize_content_streams = args.normalize_content;
    cfg.compact_vector_paths =
        args.compact_vector_paths || matches!(args.optimize_for, OptimizeForArg::Processing);
    cfg.rasterize_excessive_small_vectors = !args.keep_excessive_small_vectors
        && (cfg.rasterize_excessive_small_vectors || args.rasterize_excessive_small_vectors);
    cfg.raster_layout.enabled =
        args.normalize_raster_layout || matches!(args.optimize_for, OptimizeForArg::Processing);
    cfg.raster_layout.bake_masks =
        args.bake_image_masks || matches!(args.optimize_for, OptimizeForArg::Processing);
    cfg.raster_layout.exact_raster_rendering = args.exact_raster_rendering;
    cfg.remove_large_diagonal_text = args.remove_large_diagonal_text;
    cfg.remove_repeated_page_objects = args.remove_repeated_page_objects;
    cfg.raster_layout.prune_hidden_paints = !args.keep_hidden_paints;
    cfg.raster_layout.prune_occluded_raster_paints = args.prune_occluded_raster_paints;
    if let Some(gap_mm) = args.pixel_cluster_gap_mm {
        cfg.raster_layout.pixel_cluster_max_gap_mm = gap_mm;
    }
    cfg.privacy.level = match args.privacy {
        PrivacyArg::None => PrivacyLevel::None,
        PrivacyArg::Metadata => PrivacyLevel::Metadata,
        PrivacyArg::BestEffort => PrivacyLevel::BestEffort,
    };
    cfg.privacy.strip_jpeg_metadata =
        args.scrub_jpeg_metadata || cfg.privacy.level != PrivacyLevel::None;
    cfg.privacy.remove_attachments = args.remove_attachments;
    cfg.privacy.remove_active_content = args.remove_active_content;
    cfg.privacy.remove_signatures = args.remove_signatures;
    let keep_all_unused = args
        .keep_unused_resources
        .iter()
        .any(|selector| selector == "*");
    cfg.prune_resources = args.prune_resources
        || (!keep_all_unused
            && (matches!(args.optimize_for, OptimizeForArg::Processing)
                || !args.keep_unused_resources.is_empty()));
    cfg.keep_unused_resources = args.keep_unused_resources.iter().cloned().collect();
    cfg.deduplicate_metadata_streams = !args.no_metadata_dedup;
    cfg.deduplicate_font_programs = !args.no_font_program_dedup;
    cfg.deduplicate_to_unicode_cmaps = !args.no_to_unicode_dedup;
    cfg.deduplicate_inline_images = !args.no_inline_image_dedup;
    cfg.inline_image_min_duplicate_payload_bytes = args.inline_image_dedup_min_waste;
    cfg.deduplicate_image_xobjects = !args.no_image_dedup;
    cfg.deduplicate_form_xobjects = !args.no_form_dedup;
    cfg.deduplicate_appearance_streams = !args.no_appearance_dedup;
    cfg.deduplicate_page_contents = !args.no_page_content_dedup;
    cfg.deduplicate_type3_charprocs = !args.no_type3_charproc_dedup;
    cfg.deduplicate_icc_profiles = !args.no_icc_dedup;

    cfg
}

fn output_path(args: &Args) -> PathBuf {
    args.output.clone().unwrap_or_else(|| {
        let stem = args
            .input
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("output");
        args.input.with_file_name(format!("{stem}.redox.pdf"))
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let a = Args::parse();
    if a.corpus {
        let report = corpus::analyze_corpus(&a.input, a.corpus_top)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }

    let input = std::fs::read(&a.input)?;
    if a.analyze_microstrokes {
        let cfg = config_from_args(&a);
        let stats = analyze_microstroke_rasterization(&input, cfg.flate_level)?;
        println!("{}", serde_json::to_string_pretty(&stats)?);
        return Ok(());
    }
    if a.analyze_only {
        let r = analyze_pdf(&input)?;
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(());
    }
    let cfg = config_from_args(&a);
    let (output, report) = optimize_pdf(&input, &cfg)?;
    let out_path = output_path(&a);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn processing_uses_faster_flate_default() -> Result<(), clap::Error> {
        let args =
            Args::try_parse_from(["pdf-redox", "input.pdf", "--optimize-for", "processing"])?;
        assert_eq!(config_from_args(&args).flate_level, 5);
        Ok(())
    }

    #[test]
    fn size_mode_keeps_size_focused_flate_default() -> Result<(), clap::Error> {
        let args = Args::try_parse_from(["pdf-redox", "input.pdf"])?;
        assert_eq!(config_from_args(&args).flate_level, 9);
        Ok(())
    }

    #[test]
    fn explicit_flate_level_overrides_processing_default() -> Result<(), clap::Error> {
        let args = Args::try_parse_from([
            "pdf-redox",
            "input.pdf",
            "--optimize-for",
            "processing",
            "--flate-level",
            "7",
        ])?;
        assert_eq!(config_from_args(&args).flate_level, 7);
        Ok(())
    }

    #[test]
    fn flate_level_rejects_out_of_range_values() {
        assert!(Args::try_parse_from(["pdf-redox", "input.pdf", "--flate-level", "10"]).is_err());
        assert!(Args::try_parse_from(["pdf-redox", "input.pdf", "--flate-level", "-1"]).is_err());
    }
}
