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

fn parse_jpeg_quality(value: &str) -> Result<u8, String> {
    let quality = value
        .parse::<u8>()
        .map_err(|_| "JPEG quality must be an integer from 1 through 100".to_owned())?;
    (1..=100)
        .contains(&quality)
        .then_some(quality)
        .ok_or_else(|| "JPEG quality must be an integer from 1 through 100".to_owned())
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
#[expect(
    clippy::struct_excessive_bools,
    reason = "CLI switch fields are intentionally independent boolean flags generated directly by clap"
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
    /// Drop color-management output intents. Enabled automatically by processing mode.
    #[arg(long, conflicts_with = "keep_output_intents")]
    drop_output_intents: bool,
    /// Preserve color-management output intents even in processing mode.
    #[arg(long, conflicts_with = "drop_output_intents")]
    keep_output_intents: bool,
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
    /// Override JPEG quality (1-100) for Perceptual/Print image re-encoding.
    /// The lossless JPEG entropy pass does not use this value.
    #[arg(long, value_parser = parse_jpeg_quality)]
    jpeg_quality: Option<u8>,
    /// Override the maximum effective image PPI used by the Print profile.
    /// Perceptual preserves source pixel dimensions instead.
    #[arg(long)]
    max_image_ppi: Option<u16>,
    #[arg(long, value_enum, default_value = "none")]
    privacy: PrivacyArg,
    /// Existing Flate stream policy. Selective is the optimize-only default.
    #[arg(long, value_enum, default_value = "selective")]
    flate_policy: FlatePolicyArg,
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
    /// Minimum duplicated encoded payload bytes for one cross-scope inline-image fingerprint.
    #[arg(long, default_value_t = 1024)]
    inline_image_dedup_min_waste: usize,
    /// Replace eligible large `ICCBased` color spaces with their declared Device alternate.
    /// Enabled automatically by --optimize-for processing.
    #[arg(long, conflicts_with = "keep_icc_color_management")]
    elide_icc_to_alternate: bool,
    /// Keep embedded ICC color-management transforms even in processing mode.
    #[arg(long, conflicts_with = "elide_icc_to_alternate")]
    keep_icc_color_management: bool,
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

#[expect(
    clippy::too_many_lines,
    reason = "keeping the one-to-one CLI flag-to-config mapping linear makes omissions and precedence easier to audit"
)]
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
    cfg.preservation.output_intents = args.keep_output_intents
        || (!args.drop_output_intents && !matches!(args.optimize_for, OptimizeForArg::Processing));
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
    if let Some(jpeg_quality) = args.jpeg_quality {
        match &mut cfg.image_policy {
            pdf_redox::ImagePolicy::Perceptual {
                jpeg_quality: quality,
                ..
            }
            | pdf_redox::ImagePolicy::Print {
                jpeg_quality: quality,
                ..
            } => *quality = jpeg_quality,
            pdf_redox::ImagePolicy::Preserve => {}
        }
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
    cfg.prune_resources =
        args.prune_resources || matches!(args.optimize_for, OptimizeForArg::Processing);
    cfg.inline_image_min_duplicate_payload_bytes = args.inline_image_dedup_min_waste;
    cfg.elide_icc_profiles_to_alternate = args.elide_icc_to_alternate
        || (matches!(args.optimize_for, OptimizeForArg::Processing)
            && !args.keep_icc_color_management);

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
        let stats = analyze_microstroke_rasterization(&input, cfg.optimization_goal)?;
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
        assert_eq!(
            config_from_args(&args).optimization_goal,
            OptimizationGoal::Processing
        );
        Ok(())
    }

    #[test]
    fn processing_drops_output_intents_by_default() -> Result<(), clap::Error> {
        let args =
            Args::try_parse_from(["pdf-redox", "input.pdf", "--optimize-for", "processing"])?;
        assert!(!config_from_args(&args).preservation.output_intents);

        let keep = Args::try_parse_from([
            "pdf-redox",
            "input.pdf",
            "--optimize-for",
            "processing",
            "--keep-output-intents",
        ])?;
        assert!(config_from_args(&keep).preservation.output_intents);
        Ok(())
    }

    #[test]
    fn processing_elides_eligible_icc_profiles_by_default() -> Result<(), clap::Error> {
        let args =
            Args::try_parse_from(["pdf-redox", "input.pdf", "--optimize-for", "processing"])?;
        assert!(config_from_args(&args).elide_icc_profiles_to_alternate);

        let keep = Args::try_parse_from([
            "pdf-redox",
            "input.pdf",
            "--optimize-for",
            "processing",
            "--keep-icc-color-management",
        ])?;
        assert!(!config_from_args(&keep).elide_icc_profiles_to_alternate);
        Ok(())
    }

    #[test]
    fn size_mode_keeps_size_focused_flate_default() -> Result<(), clap::Error> {
        let args = Args::try_parse_from(["pdf-redox", "input.pdf"])?;
        assert_eq!(
            config_from_args(&args).optimization_goal,
            OptimizationGoal::Size
        );
        Ok(())
    }

    #[test]
    fn removed_flate_level_override_is_rejected() {
        assert!(Args::try_parse_from(["pdf-redox", "input.pdf", "--flate-level", "7",]).is_err());
    }

    #[test]
    fn removed_keep_unused_resources_override_is_rejected() {
        assert!(
            Args::try_parse_from(["pdf-redox", "input.pdf", "--keep-unused-resources", "*"])
                .is_err()
        );
    }

    #[test]
    fn removed_dedup_overrides_are_rejected() {
        for flag in [
            "--no-metadata-dedup",
            "--no-inline-image-dedup",
            "--no-font-program-dedup",
            "--no-to-unicode-dedup",
            "--no-image-dedup",
            "--no-form-dedup",
            "--no-appearance-dedup",
            "--no-page-content-dedup",
            "--no-type3-charproc-dedup",
            "--no-icc-dedup",
        ] {
            assert!(Args::try_parse_from(["pdf-redox", "input.pdf", flag]).is_err());
        }
    }

    #[test]
    fn jpeg_quality_overrides_perceptual_and_print_profiles()
    -> Result<(), Box<dyn std::error::Error>> {
        for profile in ["perceptual", "print"] {
            let args = Args::try_parse_from([
                "pdf-redox",
                "input.pdf",
                "--profile",
                profile,
                "--jpeg-quality",
                "73",
            ])?;
            let cfg = config_from_args(&args);
            let quality = match cfg.image_policy {
                pdf_redox::ImagePolicy::Perceptual { jpeg_quality, .. }
                | pdf_redox::ImagePolicy::Print { jpeg_quality, .. } => jpeg_quality,
                pdf_redox::ImagePolicy::Preserve => {
                    return Err(std::io::Error::other(format!(
                        "{profile} unexpectedly preserves images"
                    ))
                    .into());
                }
            };
            assert_eq!(quality, 73);
        }
        Ok(())
    }

    #[test]
    fn jpeg_quality_rejects_out_of_range_values() {
        assert!(Args::try_parse_from(["pdf-redox", "input.pdf", "--jpeg-quality", "0"]).is_err());
        assert!(Args::try_parse_from(["pdf-redox", "input.pdf", "--jpeg-quality", "101"]).is_err());
    }

    #[test]
    fn max_image_ppi_overrides_print_cap() -> Result<(), clap::Error> {
        let args = Args::try_parse_from([
            "pdf-redox",
            "input.pdf",
            "--profile",
            "print",
            "--max-image-ppi",
            "300",
        ])?;
        assert_eq!(config_from_args(&args).max_image_ppi, Some(300));
        Ok(())
    }
}
