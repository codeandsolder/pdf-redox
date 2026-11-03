use crate::{
    Config, EditDocument, ImagePolicy, OptimizationReport, PdfAnalysis, Result,
    analyze::analyze_document_for_optimization,
    content::{PageRewriteResult, normalize_page_contents},
    dedup::{
        canonicalize_appearance_streams, canonicalize_exact_extgstate_dictionaries,
        canonicalize_exact_font_dictionaries, canonicalize_exact_structure_attribute_dictionaries,
        canonicalize_font_program_streams, canonicalize_form_xobjects, canonicalize_icc_profiles,
        canonicalize_image_xobjects, canonicalize_metadata_streams, canonicalize_page_contents,
        canonicalize_sampled_function_streams, canonicalize_to_unicode_cmaps,
        canonicalize_type3_charprocs, factor_duplicate_pattern_payloads,
        factor_form_payloads_across_metadata, has_exact_duplicate_extgstate_dictionaries,
    },
    flate::{FlateReuseCache, compress_unfiltered_streams, optimize_flate_streams_with_cache},
    font::{
        FontGlyphUsageHints, FontOptimizationStats, FontUsageGraph, collect_font_usage_graph,
        dense_compact_cidfont_type2_programs, dense_compact_cidfont_type2_programs_with_graph,
        probe_profitable_font_outline_subset, strip_cidfont_type2_cmaps_after_dedup,
        strip_font_editing_tables, subset_font_outlines_after_dedup,
        union_sparse_cid_font_programs_after_dedup,
    },
    hidden_text::{
        HiddenTextApplyStats, apply_hidden_text_policy, prune_physically_hidden_text,
        remove_large_diagonal_text,
    },
    icc_alternate::{IccAlternateElisionStats, elide_icc_profiles_to_alternates},
    images::{
        ImageOptimizationOptions, ImageOptimizationStats, optimize_images,
        optimize_images_with_resize_targets,
    },
    inline_images::{DuplicateInlineImageStats, externalize_duplicate_inline_images},
    jpeg_optimize::optimize_jpeg_entropy,
    microstroke::{MicrostrokeRasterStats, rasterize_pathological_microstrokes},
    paint_batch::{
        CollinearPathStats, MarkedContentCoalesceStats, OutlinedGlyphFactorStats, PaintBatchStats,
        StrokeFormFactorStats, batch_page_paints, coalesce_optional_content,
        compact_collinear_paths, factor_outlined_glyphs, factor_repeated_stroke_forms,
    },
    polyline_simplify::{
        MAX_POLYLINE_PAGE_ERROR_PT, PolylineSimplificationStats, simplify_page_polylines,
    },
    preservation::{
        PreservationScanHints, PreservationStats, apply_preservation_policy,
        apply_preservation_policy_with_hints, drop_page_thumbnails, drop_private_authoring_data,
    },
    print::{PrintPlanHayro, plan_print_downsampling},
    prune::{prune_resources, prune_resources_with_usage},
    raster_layout::normalize_raster_layout,
    repeated_clip::{RepeatedClipStats, hoist_repeated_clip_prefixes},
    repeated_page_objects::{
        RepeatedPageObjectStats, remove_repeated_page_objects,
        repeated_page_objects_prefix_possible,
    },
    scrub::scrub_edit_document_cos_privacy,
    stream_codec::FlateBatchEncoder,
    structure_compact::compact_structure,
    vector_compact::{
        VectorCompactionStats, compact_vector_paths_with_flate_cache, processing_factor_candidate,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

mod reporting;

fn timed<T>(timings: &mut BTreeMap<String, f64>, name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = f();
    timings.insert(name.to_owned(), started.elapsed().as_secs_f64() * 1000.0);
    value
}

/// The standalone outline-opportunity probe is a pure encoded-font size
/// search. Processing avoids paying for that search, but still runs the normal
/// subset proof when another font stage already materialized the usage graph.
#[must_use]
const fn search_font_outline_subset(cfg: &Config) -> bool {
    !matches!(cfg.optimization_goal, crate::OptimizationGoal::Processing)
}

fn ensure_font_usage_graph<'a>(
    document: &EditDocument,
    graph: &'a mut Option<FontUsageGraph>,
    timings: &mut BTreeMap<String, f64>,
) -> Result<&'a FontUsageGraph> {
    if graph.is_none() {
        let collected = timed(timings, "font-post-dedup-usage-graph", || {
            collect_font_usage_graph(document)
        })?;
        *graph = Some(collected);
    }
    graph
        .as_ref()
        .ok_or_else(|| crate::Error::Invalid("font usage graph was not materialized".to_owned()))
}

struct FontGraphPhaseResult {
    font_dense: FontOptimizationStats,
    post_dedup_font_graph: Option<FontUsageGraph>,
    cid_cmap_source_inventory_still_valid: bool,
}

const fn add_font_rendering_stats(total: &mut FontOptimizationStats, delta: FontOptimizationStats) {
    total.programs_optimized += delta.programs_optimized;
    total.programs_glyph_subset += delta.programs_glyph_subset;
    total.original_encoded_bytes += delta.original_encoded_bytes;
    total.optimized_encoded_bytes += delta.optimized_encoded_bytes;
    total.decoded_table_bytes_removed += delta.decoded_table_bytes_removed;
    total.glyph_subset_decoded_bytes_removed += delta.glyph_subset_decoded_bytes_removed;
}

fn run_font_graph_phase(
    document: &mut EditDocument,
    pages: &[crate::ObjectHandle],
    cfg: &Config,
    before: &PdfAnalysis,
    flate_level: crate::FlateLevel,
    timings: &mut BTreeMap<String, f64>,
    font_rendering: &mut FontOptimizationStats,
) -> Result<FontGraphPhaseResult> {
    // These source inventories mirror the two extra scopes scanned by
    // `font_glyph_usage`. Earlier stages may delete or rewrite those scopes but
    // do not create new Type3 fonts or tiling Pattern streams, so a complete
    // source-negative inventory remains a valid negative hint here. Old cached
    // analyses default to version 0 and therefore take both full scans.
    let font_glyph_usage_hints = FontGlyphUsageHints {
        no_tiling_pattern_scopes: before.font_glyph_scope_inventory_version >= 1
            && before.tiling_pattern_scope_count == 0,
        no_type3_font_scopes: before.font_glyph_scope_inventory_version >= 1
            && before.type3_font_scope_count == 0,
    };
    // The authoritative font ownership/subtype graph is expensive and most
    // documents have no profitable consumer for it. Source preflight records
    // conservative negative-only opportunities; old/incomplete analyses fall
    // back to the historical eager proofs. Any possible winner still uses the
    // same graph before mutation.
    let font_opportunity_inventory_complete = before.font_opportunity_inventory_version >= 1;
    let sparse_union_possible = !cfg.preservation.font_editing_support
        && (!font_opportunity_inventory_complete
            || before.font_sparse_union_duplicate_skeleton_groups > 0);
    let dense_gid_possible =
        !font_opportunity_inventory_complete || before.font_explicit_cid_to_gid_ref_count > 0;
    let mut post_dedup_font_graph = None::<FontUsageGraph>;

    let font_sparse_union = if sparse_union_possible {
        let graph = ensure_font_usage_graph(document, &mut post_dedup_font_graph, timings)?;
        timed(timings, "font-sparse-union", || {
            union_sparse_cid_font_programs_after_dedup(
                document,
                pages,
                flate_level,
                graph,
                font_glyph_usage_hints,
            )
        })?
    } else {
        timings.insert("font-sparse-union".to_owned(), 0.0);
        FontOptimizationStats::default()
    };
    add_font_rendering_stats(font_rendering, font_sparse_union);

    let mut outline_probe = None;
    let outline_subset_possible = if cfg.preservation.font_editing_support {
        false
    } else if post_dedup_font_graph.is_some() {
        // A prior font stage already mutated the document. The ownership graph
        // can still be reused, but glyph usage must be scanned from current content.
        timings.insert("font-outline-opportunity".to_owned(), 0.0);
        true
    } else if !search_font_outline_subset(cfg) {
        timings.insert("font-outline-opportunity".to_owned(), 0.0);
        false
    } else {
        outline_probe = timed(timings, "font-outline-opportunity", || {
            probe_profitable_font_outline_subset(
                document,
                pages,
                flate_level,
                font_glyph_usage_hints,
            )
        })?;
        outline_probe.is_some()
    };
    let font_outline_subset = if outline_subset_possible {
        let graph = ensure_font_usage_graph(document, &mut post_dedup_font_graph, timings)?;
        timed(timings, "font-outline-subset", || {
            subset_font_outlines_after_dedup(
                document,
                pages,
                flate_level,
                graph,
                outline_probe,
                font_glyph_usage_hints,
            )
        })?
    } else {
        timings.insert("font-outline-subset".to_owned(), 0.0);
        FontOptimizationStats::default()
    };
    add_font_rendering_stats(font_rendering, font_outline_subset);

    let font_dense = if cfg.preservation.font_editing_support
        || cfg.optimization_goal != crate::OptimizationGoal::Processing
        || !dense_gid_possible
    {
        timings.insert("font-dense-gid".to_owned(), 0.0);
        FontOptimizationStats::default()
    } else if font_sparse_union.programs_optimized == 0 {
        let graph = ensure_font_usage_graph(document, &mut post_dedup_font_graph, timings)?;
        timed(timings, "font-dense-gid", || {
            dense_compact_cidfont_type2_programs_with_graph(document, flate_level, graph)
        })?
    } else {
        timed(timings, "font-dense-gid", || {
            dense_compact_cidfont_type2_programs(document, flate_level)
        })?
    };
    if post_dedup_font_graph.is_none() {
        timings.insert("font-post-dedup-usage-graph".to_owned(), 0.0);
    }

    Ok(FontGraphPhaseResult {
        font_dense,
        post_dedup_font_graph,
        cid_cmap_source_inventory_still_valid: font_sparse_union.programs_optimized == 0
            && font_outline_subset.programs_optimized == 0
            && font_dense.programs_optimized == 0,
    })
}

fn signed_size_delta(before: usize, after: usize) -> isize {
    if before >= after {
        isize::try_from(before - after).unwrap_or(isize::MAX)
    } else {
        isize::try_from(after - before).map_or(isize::MIN, |delta| -delta)
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the percentage is display-only; exact byte counts remain available in the report"
)]
fn savings_percent(saved_bytes: isize, input_bytes: usize) -> f64 {
    if input_bytes == 0 {
        0.0
    } else {
        saved_bytes as f64 * 100.0 / input_bytes as f64
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "stage timings are diagnostic milliseconds; exact microsecond counters are not part of the API"
)]
fn micros_to_millis(micros: u64) -> f64 {
    micros as f64 / 1000.0
}

fn validate_config(cfg: &Config) -> Result<()> {
    if let ImagePolicy::Print { target_ppi, .. } = &cfg.image_policy {
        let effective_target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
        if effective_target_ppi == 0 {
            return Err(crate::Error::Invalid(
                "print image PPI target must be greater than zero".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Run the production pathological-microstroke detector and encoded-cost gate
/// without serializing an output PDF. The temporary document may be rewritten
/// internally, but the caller receives only the would-be rasterization stats.
///
/// This support entry point is public for the separate CLI crate and intentionally
/// hidden from the normal library documentation surface.
///
/// # Errors
///
/// Returns an error when the Flate level is outside the supported range 0 through 9,
/// the input PDF cannot be parsed, or required content cannot be decoded or inspected.
#[doc(hidden)]
pub fn analyze_microstroke_rasterization(
    input: &[u8],
    optimization_goal: crate::OptimizationGoal,
) -> Result<MicrostrokeRasterStats> {
    let mut document = EditDocument::from_bytes(input.to_vec())?;
    let pages = document.page_handles()?;
    rasterize_pathological_microstrokes(&mut document, &pages, optimization_goal.flate_level())
}

/// Optimize a PDF according to the supplied configuration.
///
/// # Errors
///
/// Returns an error when the configuration is invalid, the input PDF cannot be parsed,
/// an optimization pass fails, or the rewritten document cannot be serialized.
pub fn optimize_pdf(input: &[u8], cfg: &Config) -> Result<(Vec<u8>, OptimizationReport)> {
    validate_config(cfg)?;
    let mut timings = BTreeMap::new();
    let document = timed(&mut timings, "document-open", || {
        EditDocument::from_bytes(input.to_vec())
    })?;
    let analysis = timed(&mut timings, "analysis", || {
        analyze_document_for_optimization(input, &document)
    })?;
    optimize_pdf_with_document(
        document,
        cfg,
        analysis.report,
        Some(&analysis.ext_gstate_dictionary_handles),
        timings,
    )
}

/// Optimize `input` using a previously computed analysis of these exact bytes.
///
/// # Errors
///
/// Returns an error when `analysis` does not belong to `input`, the configuration is invalid,
/// the input PDF cannot be parsed, an optimization pass fails, or output serialization fails.
pub fn optimize_pdf_with_analysis(
    input: &[u8],
    cfg: &Config,
    analysis: &PdfAnalysis,
) -> Result<(Vec<u8>, OptimizationReport)> {
    validate_config(cfg)?;
    if !analysis.matches_input(input) {
        return Err(crate::Error::AnalysisInputMismatch);
    }
    let mut timings = BTreeMap::new();
    let document = timed(&mut timings, "document-open", || {
        EditDocument::from_bytes(input.to_vec())
    })?;
    optimize_pdf_with_document(document, cfg, analysis.clone(), None, timings)
}

fn preservation_uses_fast_path(policy: &crate::PreservationConfig) -> bool {
    let mut fast_path_policy = crate::PreservationConfig::functional();
    fast_path_policy.private_authoring_data = policy.private_authoring_data;
    fast_path_policy.font_editing_support = policy.font_editing_support;
    *policy == fast_path_policy
}

struct OptimizationRunStats {
    preservation_fast_path: bool,
    preservation: PreservationStats,
    hidden_text: HiddenTextApplyStats,
    large_diagonal_text: HiddenTextApplyStats,
    scrub: crate::scrub::ScrubStats,
    font_rendering: FontOptimizationStats,
    metadata_dedup: crate::dedup::TargetedDedupStats,
    font_dedup: crate::dedup::TargetedDedupStats,
    font_dense: FontOptimizationStats,
    to_unicode_dedup: crate::dedup::TargetedDedupStats,
    font_object_dedup: crate::dedup::ExactObjectDedupStats,
    sampled_function_dedup: crate::dedup::TargetedDedupStats,
    extgstate_object_dedup: crate::dedup::ExactObjectDedupStats,
    structure_attribute_dedup: crate::dedup::ExactObjectDedupStats,
    icc_dedup: crate::dedup::TargetedDedupStats,
    icc_alternate: IccAlternateElisionStats,
    raster_layout: crate::raster_layout::RasterLayoutStats,
    physically_hidden_text: HiddenTextApplyStats,
    inline_image_dedup: DuplicateInlineImageStats,
    jpeg_entropy: crate::jpeg_optimize::JpegEntropyStats,
    image_dedup: crate::dedup::TargetedDedupStats,
    pattern_payload_factor: crate::dedup::PatternPayloadFactorStats,
    form_dedup: crate::dedup::TargetedDedupStats,
    form_metadata_payload_factor: crate::dedup::FormMetadataPayloadFactorStats,
    appearance_dedup: crate::dedup::TargetedDedupStats,
    type3_charproc_dedup: crate::dedup::TargetedDedupStats,
    repeated_page_objects: RepeatedPageObjectStats,
    print_plan: PrintPlanHayro,
    print_plan_error: Option<String>,
    raster_transform: ImageOptimizationStats,
    repeated_clips: RepeatedClipStats,
    vector_compaction: VectorCompactionStats,
    marked_content: MarkedContentCoalesceStats,
    collinear_paths: CollinearPathStats,
    outlined_glyphs: OutlinedGlyphFactorStats,
    paint_batch: PaintBatchStats,
    stroke_forms: StrokeFormFactorStats,
    polyline_simplification: PolylineSimplificationStats,
    resource_prune: crate::prune::ResourcePruneStats,
    microstroke_raster: MicrostrokeRasterStats,
    flate: crate::flate::FlateOptimizationStats,
    page_content_dedup: crate::dedup::TargetedDedupStats,
    structure_compaction: crate::structure_compact::StructureCompactionStats,
    unfiltered_flate: crate::flate::FlateOptimizationStats,
    font_cid_cmap_strip: FontOptimizationStats,
}

#[expect(
    clippy::too_many_lines,
    reason = "the optimization pipeline intentionally keeps ordered mutations and their shared document state together"
)]
fn optimize_pdf_with_document(
    mut document: EditDocument,
    cfg: &Config,
    before: PdfAnalysis,
    ext_gstate_dictionary_handles: Option<&[crate::ObjectHandle]>,
    mut timings: BTreeMap<String, f64>,
) -> Result<(Vec<u8>, OptimizationReport)> {
    let flate_level = cfg.optimization_goal.flate_level();
    let preservation_fast_path = preservation_uses_fast_path(&cfg.preservation);
    let preservation_scan_hints = PreservationScanHints {
        no_piece_info: before.piece_info_key_count == 0,
        no_authoring_metadata: before.authoring_metadata_key_count == 0,
    };
    let mut preservation = timed(&mut timings, "preservation", || {
        if preservation_fast_path {
            Ok(PreservationStats::default())
        } else if before.preservation_key_inventory_complete {
            apply_preservation_policy_with_hints(
                &mut document,
                &cfg.preservation,
                preservation_scan_hints,
            )
        } else {
            apply_preservation_policy(&mut document, &cfg.preservation)
        }
    })?;
    timed(&mut timings, "thumbnail-elide", || {
        if preservation_fast_path {
            drop_page_thumbnails(&mut document, &mut preservation)
        } else {
            Ok(())
        }
    })?;
    timed(&mut timings, "private-authoring-elide", || {
        let has_piece_info = before
            .risks
            .iter()
            .any(|risk| risk.kind == crate::RiskKind::PieceInfo);
        if preservation_fast_path && !cfg.preservation.private_authoring_data && has_piece_info {
            drop_private_authoring_data(&mut document, &mut preservation)
        } else {
            Ok(())
        }
    })?;
    // Preservation is the last stage before structure compaction that can touch
    // page-tree dictionaries. Page membership/order is stable across the remaining
    // optimization passes even though page contents/resources continue to mutate.
    // Snapshot ordered page identity once instead of repeatedly recovering /Pages.
    let stable_pages = timed(&mut timings, "stable-page-index", || {
        document.page_handles()
    })?;
    let hidden_text = timed(&mut timings, "hidden-text", || {
        apply_hidden_text_policy(&mut document, &stable_pages, &cfg.hidden_text)
    })?;
    let large_diagonal_text = timed(&mut timings, "large-diagonal-text", || {
        if cfg.remove_large_diagonal_text {
            remove_large_diagonal_text(&mut document, &stable_pages)
        } else {
            Ok(HiddenTextApplyStats::default())
        }
    })?;
    let scrub = timed(&mut timings, "privacy-scrub", || {
        scrub_edit_document_cos_privacy(&mut document, &cfg.privacy)
    })?;

    // Strip rendering-irrelevant editing/layout state before font-program
    // dedup so producer subsets can converge to the same program.
    let mut font_rendering = timed(&mut timings, "font-table-strip", || {
        if cfg.preservation.font_editing_support
            || before.font_rendering_table_inventory_complete
                && before.font_rendering_table_candidate_count == 0
        {
            Ok(FontOptimizationStats::default())
        } else {
            strip_font_editing_tables(&mut document, flate_level)
        }
    })?;
    let metadata_dedup = timed(&mut timings, "metadata-dedup", || {
        if before.preservation_key_inventory_complete && before.authoring_metadata_key_count == 0 {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_metadata_streams(&mut document)
        }
    })?;
    let font_dedup = timed(&mut timings, "font-dedup", || {
        if before.font_raw_payload_inventory_version >= 1
            && before.font_raw_duplicate_payload_groups == 0
            && font_rendering.programs_optimized == 0
        {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_font_program_streams(&mut document)
        }
    })?;
    let FontGraphPhaseResult {
        font_dense,
        post_dedup_font_graph,
        cid_cmap_source_inventory_still_valid,
    } = run_font_graph_phase(
        &mut document,
        &stable_pages,
        cfg,
        &before,
        flate_level,
        &mut timings,
        &mut font_rendering,
    )?;
    let to_unicode_dedup = timed(&mut timings, "to-unicode-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 1 && before.to_unicode_key_count == 0
            || before.to_unicode_raw_payload_inventory_version >= 1
                && before.to_unicode_raw_duplicate_payload_groups == 0
        {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_to_unicode_cmaps(&mut document)
        }
    })?;
    let font_object_dedup = timed(&mut timings, "font-object-dedup", || {
        let source_uniqueness_still_exact = before.font_dictionary_fingerprint_inventory_version
            >= 1
            && before.font_dictionary_duplicate_fingerprint_groups == 0
            && preservation.dropped_authoring_keys.is_empty()
            && scrub.removed.is_empty()
            && metadata_dedup.references_canonicalized == 0
            && font_dedup.references_canonicalized == 0
            && to_unicode_dedup.references_canonicalized == 0;
        if source_uniqueness_still_exact {
            Ok(crate::dedup::ExactObjectDedupStats::default())
        } else {
            canonicalize_exact_font_dictionaries(&mut document)
        }
    })?;
    // Sampled Functions can be dependencies of ExtGState, tint transforms, and
    // shadings. Intern them before higher-level exact dedup so containers that
    // differed only by duplicate Function references can converge this pass.
    let sampled_function_dedup = timed(&mut timings, "sampled-function-dedup", || {
        canonicalize_sampled_function_streams(&mut document)
    })?;
    let extgstate_object_dedup = timed(&mut timings, "extgstate-object-dedup", || {
        let source_candidates_still_prove_uniqueness = match ext_gstate_dictionary_handles {
            Some(handles) => !has_exact_duplicate_extgstate_dictionaries(&document, handles)?,
            None => false,
        };
        // No earlier optimizer stage synthesizes `/ExtGState` resource entries.
        // Re-checking the source candidates in their current state therefore
        // catches convergence caused by earlier reference canonicalization;
        // deletions can only leave conservative stale candidates. Cached analyses
        // deliberately lack this non-serialized handle inventory and fall back.
        if before.targeted_dedup_key_inventory_version >= 1 && before.ext_gstate_key_count == 0
            || source_candidates_still_prove_uniqueness
        {
            Ok(crate::dedup::ExactObjectDedupStats::default())
        } else {
            canonicalize_exact_extgstate_dictionaries(&mut document)
        }
    })?;
    let structure_attribute_dedup = timed(&mut timings, "structure-attribute-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 2
            && before.structure_attribute_ref_count == 0
        {
            Ok(crate::dedup::ExactObjectDedupStats::default())
        } else {
            canonicalize_exact_structure_attribute_dictionaries(&mut document)
        }
    })?;
    let icc_dedup = timed(&mut timings, "icc-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 2 && before.icc_based_array_count == 0 {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_icc_profiles(&mut document)
        }
    })?;
    let icc_alternate = timed(&mut timings, "icc-alternate-elision", || {
        if cfg.elide_icc_profiles_to_alternate {
            if before.targeted_dedup_key_inventory_version >= 2 && before.icc_based_array_count == 0
            {
                Ok(IccAlternateElisionStats::default())
            } else {
                elide_icc_profiles_to_alternates(&mut document)
            }
        } else {
            Ok(IccAlternateElisionStats::default())
        }
    })?;
    let repeated_page_objects_may_rewrite = cfg.remove_repeated_page_objects
        && cfg.optimization_goal == crate::OptimizationGoal::Processing
        && repeated_page_objects_prefix_possible(&document, &stable_pages)?;
    let mut raster_vector_cache = BTreeMap::new();
    let raster_layout = timed(&mut timings, "raster-layout", || {
        let vector_cache = (cfg.optimization_goal == crate::OptimizationGoal::Processing
            && !repeated_page_objects_may_rewrite)
            .then_some(&mut raster_vector_cache);
        normalize_raster_layout(
            &mut document,
            &stable_pages,
            &cfg.raster_layout,
            flate_level,
            cfg.optimization_goal == crate::OptimizationGoal::Processing,
            vector_cache,
        )
    })?;
    for (name, micros) in [
        ("inline-externalize", raster_layout.inline_externalize_us),
        ("target-scan", raster_layout.target_scan_us),
        ("hidden-prune", raster_layout.hidden_prune_us),
        ("hidden-state", raster_layout.hidden_state_us),
        ("hidden-visibility", raster_layout.hidden_visibility_us),
        ("hidden-coverage", raster_layout.hidden_coverage_us),
        ("hidden-rewrite", raster_layout.hidden_rewrite_us),
        ("image-materialize", raster_layout.image_materialize_us),
        ("native-plan", raster_layout.native_plan_us),
        ("stripe-plan", raster_layout.stripe_plan_us),
        ("pixel-plan", raster_layout.pixel_plan_us),
        ("apply-plans", raster_layout.apply_plans_us),
        ("staging-cleanup", raster_layout.staging_cleanup_us),
    ] {
        timings.insert(format!("raster/{name}"), micros_to_millis(micros));
    }
    let mut raster_resource_usage_exact =
        cfg.raster_layout.enabled && raster_layout.resource_inventory_complete;
    let PageRewriteResult {
        stats: physically_hidden_text,
        rewritten_pages: physically_hidden_text_rewritten_pages,
    } = timed(&mut timings, "physical-hidden-text", || {
        if !cfg.raster_layout.enabled || !cfg.raster_layout.prune_hidden_paints {
            return Ok(PageRewriteResult::default());
        }
        if raster_layout.page_hidden_text_inventory_complete {
            let fallback = raster_layout
                .page_hidden_text_candidates
                .difference(&raster_layout.page_hidden_text_shared_complete)
                .copied()
                .collect::<BTreeSet<_>>();
            if fallback.is_empty() {
                Ok(PageRewriteResult::default())
            } else {
                prune_physically_hidden_text(&mut document, &stable_pages, Some(&fallback))
            }
        } else {
            prune_physically_hidden_text(&mut document, &stable_pages, None)
        }
    })?;
    raster_vector_cache.retain(|page, _| !physically_hidden_text_rewritten_pages.contains(page));
    if !physically_hidden_text_rewritten_pages.is_empty() {
        raster_resource_usage_exact = false;
    }

    let PageRewriteResult {
        stats: inline_image_dedup,
        rewritten_pages: inline_image_rewritten_pages,
    } = timed(&mut timings, "inline-image-dedup", || {
        let raster_proved_no_inline = cfg.raster_layout.enabled
            && raster_layout.inline_inventory_complete
            && raster_layout.inline_occurrences_remaining == 0;
        if raster_proved_no_inline {
            Ok(PageRewriteResult::default())
        } else {
            externalize_duplicate_inline_images(&mut document, &stable_pages)
        }
    })?;
    raster_vector_cache.retain(|page, _| !inline_image_rewritten_pages.contains(page));
    if inline_image_dedup.occurrences_externalized != 0 {
        // Processing-vector analysis is page-content-local: Form rewrites do not
        // invalidate callers' cached path analysis, while rewritten pages do.
        // Resource-usage inventories are document-wide and therefore still lose
        // exactness for either target kind.
        raster_resource_usage_exact = false;
    }

    // Entropy-optimize surviving/reconstructed JPEGs before exact image
    // canonicalization. This preserves quantized DCT coefficients while allowing
    // images that differed only in Huffman coding to converge.
    let jpeg_entropy = timed(&mut timings, "jpeg-entropy", || {
        optimize_jpeg_entropy(&mut document, 128, 1)
    })?;

    // Exact image canonicalization follows inline-image externalization and JPEG
    // entropy normalization so both can converge with existing Image XObjects.
    let image_dedup = timed(&mut timings, "image-dedup", || {
        canonicalize_image_xobjects(&mut document)
    })?;
    let pattern_payload_factor = timed(&mut timings, "pattern-payload-factor", || {
        if before.targeted_dedup_key_inventory_version >= 3 && before.pattern_stream_count == 0 {
            Ok(crate::dedup::PatternPayloadFactorStats::default())
        } else {
            factor_duplicate_pattern_payloads(&mut document)
        }
    })?;
    let form_dedup = timed(&mut timings, "form-dedup", || {
        canonicalize_form_xobjects(&mut document)
    })?;
    let form_metadata_payload_factor = timed(&mut timings, "form-metadata-payload-factor", || {
        if cfg.preservation.metadata && !cfg.preservation.private_authoring_data {
            factor_form_payloads_across_metadata(&mut document)
        } else {
            Ok(crate::dedup::FormMetadataPayloadFactorStats::default())
        }
    })?;
    let appearance_dedup = timed(&mut timings, "appearance-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 1 && before.appearance_key_count == 0 {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_appearance_streams(&mut document)
        }
    })?;
    let type3_charproc_dedup = timed(&mut timings, "type3-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 1 && before.char_procs_key_count == 0 {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_type3_charprocs(&mut document)
        }
    })?;
    let PageRewriteResult {
        stats: repeated_page_objects,
        rewritten_pages: repeated_page_objects_rewritten_pages,
    } = timed(&mut timings, "repeated-page-objects", || {
        if cfg.remove_repeated_page_objects {
            remove_repeated_page_objects(&mut document, &stable_pages)
        } else {
            Ok(PageRewriteResult::default())
        }
    })?;
    raster_vector_cache.retain(|page, _| !repeated_page_objects_rewritten_pages.contains(page));
    if repeated_page_objects.objects_removed != 0 {
        raster_resource_usage_exact = false;
    }

    let print_plan_started = Instant::now();
    let (print_plan, print_plan_error) = match &cfg.image_policy {
        ImagePolicy::Print { target_ppi, .. } => {
            let target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
            match plan_print_downsampling(&document, &stable_pages, u32::from(target_ppi)) {
                Ok(plan) => (plan, None),
                Err(error) => (PrintPlanHayro::default(), Some(error.to_string())),
            }
        }
        _ => (PrintPlanHayro::default(), None),
    };
    timings.insert(
        "print-plan".to_owned(),
        print_plan_started.elapsed().as_secs_f64() * 1000.0,
    );
    let raster_transform_started = Instant::now();
    let raster_transform = match &cfg.image_policy {
        ImagePolicy::Preserve => ImageOptimizationStats::default(),
        ImagePolicy::Print {
            jpeg_quality,
            min_savings_percent,
            ..
        } => {
            if print_plan.resize_targets.is_empty() {
                ImageOptimizationStats::default()
            } else {
                optimize_images_with_resize_targets(
                    &mut document,
                    ImageOptimizationOptions {
                        min_width: 0,
                        min_height: 0,
                        min_area: 0,
                        keep_inline_images: true,
                        jpeg_quality: *jpeg_quality,
                        min_savings_bytes: 1,
                        min_savings_percent: *min_savings_percent,
                        ..ImageOptimizationOptions::default()
                    },
                    flate_level,
                    &print_plan.resize_targets,
                )?
            }
        }
        ImagePolicy::Perceptual {
            jpeg_quality,
            min_savings_percent,
            ..
        } => optimize_images(
            &mut document,
            &stable_pages,
            ImageOptimizationOptions {
                keep_inline_images: true,
                jpeg_quality: *jpeg_quality,
                min_savings_bytes: 1,
                min_savings_percent: *min_savings_percent,
                ..ImageOptimizationOptions::default()
            },
            flate_level,
        )?,
    };
    timings.insert(
        "raster-transform".to_owned(),
        raster_transform_started.elapsed().as_secs_f64() * 1000.0,
    );

    let PageRewriteResult {
        stats: repeated_clips,
        rewritten_pages: repeated_clip_rewritten_pages,
    } = timed(&mut timings, "repeated-clip-hoist", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            hoist_repeated_clip_prefixes(&mut document, &stable_pages, flate_level)
        } else {
            Ok(PageRewriteResult::default())
        }
    })?;
    raster_vector_cache.retain(|page, _| !repeated_clip_rewritten_pages.contains(page));

    let mut flate_reuse_cache = FlateReuseCache::default();
    let vector_compaction = timed(&mut timings, "vector-compaction", || {
        let raster_proved_no_vector_candidate = cfg.optimization_goal
            != crate::OptimizationGoal::Processing
            && cfg.raster_layout.enabled
            && raster_layout.page_vector_inventory_complete
            && !raster_layout.page_vector_merge_candidate;
        let processing_proved_no_vector_candidate = cfg.optimization_goal
            == crate::OptimizationGoal::Processing
            && cfg.raster_layout.enabled
            && raster_layout.page_vector_inventory_complete
            && !raster_layout.page_vector_merge_candidate
            && raster_vector_cache.len() == raster_layout.page_count
            && !processing_factor_candidate(&raster_vector_cache);
        if !raster_proved_no_vector_candidate && !processing_proved_no_vector_candidate {
            let vector_cache = std::mem::take(&mut raster_vector_cache);
            compact_vector_paths_with_flate_cache(
                &mut document,
                &stable_pages,
                flate_level,
                cfg.optimization_goal,
                vector_cache,
                &mut flate_reuse_cache,
            )
        } else {
            Ok(VectorCompactionStats::default())
        }
    })?;
    if vector_compaction.shared_run_forms_created != 0 {
        raster_resource_usage_exact = false;
    }

    let marked_content = timed(&mut timings, "marked-content-coalesce", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            coalesce_optional_content(&mut document, &stable_pages, flate_level)
        } else {
            Ok(MarkedContentCoalesceStats::default())
        }
    })?;

    let collinear_paths = timed(&mut timings, "collinear-path-compact", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            compact_collinear_paths(&mut document, &stable_pages, flate_level)
        } else {
            Ok(CollinearPathStats::default())
        }
    })?;

    let outlined_glyphs = timed(&mut timings, "outlined-glyph-factor", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            factor_outlined_glyphs(&mut document, &stable_pages, flate_level)
        } else {
            Ok(OutlinedGlyphFactorStats::default())
        }
    })?;
    if outlined_glyphs.fonts_created != 0 {
        raster_resource_usage_exact = false;
    }

    let paint_batch = timed(&mut timings, "paint-batching", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            batch_page_paints(&mut document, &stable_pages)
        } else {
            Ok(PaintBatchStats::default())
        }
    })?;

    let stroke_forms = timed(&mut timings, "stroke-form-factor", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            factor_repeated_stroke_forms(&mut document, &stable_pages, flate_level)
        } else {
            Ok(StrokeFormFactorStats::default())
        }
    })?;
    if stroke_forms.forms_created != 0 {
        raster_resource_usage_exact = false;
    }

    let polyline_simplification = timed(&mut timings, "polyline-simplify", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            simplify_page_polylines(&mut document, &stable_pages, flate_level)
        } else {
            Ok(PolylineSimplificationStats::default())
        }
    })?;

    let resource_prune = timed(&mut timings, "resource-prune", || {
        if raster_resource_usage_exact {
            prune_resources_with_usage(
                &mut document,
                &raster_layout.page_resource_names_by_type,
                &raster_layout.form_resource_names_by_type,
                &vector_compaction.generated_page_xobjects,
            )
        } else {
            prune_resources(&mut document, &stable_pages)
        }
    })?;
    let microstroke_raster = timed(&mut timings, "microstroke-raster", || {
        if cfg.rasterize_excessive_small_vectors {
            rasterize_pathological_microstrokes(&mut document, &stable_pages, flate_level)
        } else {
            Ok(MicrostrokeRasterStats::default())
        }
    })?;
    let flate_encoder = timed(&mut timings, "flate-executor-init", || {
        FlateBatchEncoder::new(cfg.flate_workers)
    })?;
    let flate = timed(&mut timings, "flate-policy", || {
        optimize_flate_streams_with_cache(
            &mut document,
            flate_level,
            &mut flate_reuse_cache,
            &flate_encoder,
        )
    })?;
    timed(&mut timings, "content-normalize", || -> Result<()> {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            normalize_page_contents(&mut document, &stable_pages)?;
        }
        Ok(())
    })?;
    // Content streams are canonicalized only after every pass that can mutate
    // them, including lexical normalization.
    let page_content_dedup = timed(&mut timings, "page-content-dedup", || {
        canonicalize_page_contents(&mut document, &stable_pages)
    })?;
    let structure_compaction = timed(&mut timings, "structure-compaction", || {
        compact_structure(&mut document)
    })?;

    // Match the historical writer's StreamDataMode::Compress policy explicitly
    // before handing the graph to the deliberately-simple fresh writer.
    let unfiltered_flate = timed(&mut timings, "compress-unfiltered", || {
        compress_unfiltered_streams(
            &mut document,
            flate_level,
            &mut flate_reuse_cache,
            &flate_encoder,
        )
    })?;
    // Cmap removal is deliberately the final graph transform. By this point
    // resource pruning has removed dead fonts and Flate policy has established
    // the stream bytes against which the rewrite's actual size win is gated.
    let cid_cmap_possible = !cfg.preservation.font_editing_support
        && (before.font_opportunity_inventory_version < 1
            || !cid_cmap_source_inventory_still_valid
            || before.font_material_cid_cmap_decoded_bytes
                >= crate::font::MIN_CID_CMAP_DECODED_BYTES_PER_DOCUMENT);
    let font_cid_cmap_strip = if cid_cmap_possible {
        timed(&mut timings, "font-cid-cmap-strip", || {
            strip_cidfont_type2_cmaps_after_dedup(
                &mut document,
                flate_level,
                post_dedup_font_graph.as_ref(),
            )
        })?
    } else {
        timings.insert("font-cid-cmap-strip".to_owned(), 0.0);
        FontOptimizationStats::default()
    };
    let output = timed(&mut timings, "writer", || {
        crate::writer::write_pdf_with_object_streams(&document, flate_level)
    })?;

    let run_stats = OptimizationRunStats {
        preservation_fast_path,
        preservation,
        hidden_text,
        large_diagonal_text,
        scrub,
        font_rendering,
        metadata_dedup,
        font_dedup,
        font_dense,
        to_unicode_dedup,
        font_object_dedup,
        sampled_function_dedup,
        extgstate_object_dedup,
        structure_attribute_dedup,
        icc_dedup,
        icc_alternate,
        raster_layout,
        physically_hidden_text,
        inline_image_dedup,
        jpeg_entropy,
        image_dedup,
        pattern_payload_factor,
        form_dedup,
        form_metadata_payload_factor,
        appearance_dedup,
        type3_charproc_dedup,
        repeated_page_objects,
        print_plan,
        print_plan_error,
        raster_transform,
        repeated_clips,
        vector_compaction,
        marked_content,
        collinear_paths,
        outlined_glyphs,
        paint_batch,
        stroke_forms,
        polyline_simplification,
        resource_prune,
        microstroke_raster,
        flate,
        page_content_dedup,
        structure_compaction,
        unfiltered_flate,
        font_cid_cmap_strip,
    };
    let report = run_stats.into_report(&document, cfg, before, output.len(), timings);
    Ok((output, report))
}

#[cfg(test)]
mod tests {
    #[test]
    fn font_editing_policy_is_orthogonal_to_preservation_fast_path() {
        let mut policy = crate::PreservationConfig::functional();
        assert!(super::preservation_uses_fast_path(&policy));

        policy.font_editing_support = !policy.font_editing_support;
        assert!(super::preservation_uses_fast_path(&policy));

        policy.private_authoring_data = !policy.private_authoring_data;
        assert!(super::preservation_uses_fast_path(&policy));

        policy.metadata = !policy.metadata;
        assert!(!super::preservation_uses_fast_path(&policy));
    }

    use super::*;
    use crate::{Error, SourcePdf, test_support::ClassicPdfBuilder};

    #[test]
    fn processing_skips_pure_size_outline_search() {
        let mut config = Config::optimize_only();
        assert!(search_font_outline_subset(&config));

        config.optimization_goal = crate::OptimizationGoal::Processing;
        assert!(!search_font_outline_subset(&config));
    }

    fn processing_coordinate_fill_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 37000] /Resources <<>> /Contents 4 0 R >>")?;
        let mut content = Vec::new();
        for row in 0..3000_u32 {
            use std::io::Write as _;
            let y = row * 12;
            // Keep both boundaries in the same 0.01-grid cell while varying the
            // discarded decimal tail enough that the Flate admission gate sees
            // the same material win as real CAD/datasheet content.
            let outer_micros = 3000 + (row * 7919 % 1900);
            let inner_micros = 100 + (row * 3571 % 1800);
            let outer = 10.0 + f64::from(outer_micros) / 1_000_000.0;
            let inner = 10.0 + f64::from(inner_micros) / 1_000_000.0;
            let remaining = 20.0 - inner;
            writeln!(
                &mut content,
                "0 {y} {outer:.6} 10 re f {inner:.6} {y} {remaining:.6} 10 re f"
            )?;
        }
        pdf.stream(4, b"", &content)?;
        pdf.finish(1)
    }

    #[test]
    fn processing_closes_fill_adjacency_exposed_by_coordinate_canonicalization() -> Result<()> {
        let input = processing_coordinate_fill_fixture()?;
        let mut config = Config::optimize_only();
        config.optimization_goal = crate::OptimizationGoal::Processing;

        let (first, first_report) = optimize_pdf(&input, &config)?;
        assert!(first_report.vector_path_coordinate_pages_rewritten > 0);
        assert!(first_report.vector_fill_groups_batched >= 3000);

        let (second, second_report) = optimize_pdf(&first, &config)?;
        assert_eq!(second_report.vector_fill_groups_batched, 0);
        assert_eq!(second_report.vector_path_coordinate_pages_rewritten, 0);
        assert_eq!(second, first);
        Ok(())
    }

    fn practical_cleanup_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Metadata 6 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R /Thumb 5 0 R /PieceInfo 8 0 R /LastModified (today) /UnknownPage 7 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8", &[0])?;
        pdf.stream(6, b"/Type /Metadata /Subtype /XML", b"<xmp/>")?;
        pdf.object(7, b"<< /Private true >>")?;
        pdf.object(8, b"<< /Illustrator << /Private true >> >>")?;
        pdf.finish(1)
    }

    fn form_metadata_payload_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /A 5 0 R /B 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q /A Do /B Do Q")?;
        let mut body = Vec::new();
        for _ in 0..400 {
            body.extend_from_slice(b"0 0 m 10 10 l S\n");
        }
        pdf.stream(
            5,
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources <<>> /LastModified (first)",
            &body,
        )?;
        pdf.stream(
            6,
            b"/Type /XObject /Subtype /Form /BBox [0 0 10 10] /Resources <<>> /LastModified (second)",
            &body,
        )?;
        pdf.finish(1)
    }

    #[test]
    fn default_optimize_factors_form_payloads_without_dropping_last_modified() -> Result<()> {
        let input = form_metadata_payload_fixture()?;
        let (output, report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert_eq!(report.form_metadata_payload_groups_factored, 1);
        assert_eq!(report.form_metadata_payload_wrappers_rewritten, 1);
        assert!(report.form_metadata_payload_duplicate_raw_bytes_factored >= 4096);
        let analysis = crate::analyze_pdf_preflight(&output)?;
        assert_eq!(analysis.authoring_metadata_key_count, 2);
        Ok(())
    }

    #[test]
    fn authoring_preservation_suppresses_form_metadata_payload_factoring() -> Result<()> {
        let input = form_metadata_payload_fixture()?;
        let mut config = Config::optimize_only();
        config.preservation.private_authoring_data = true;
        let (_, report) = optimize_pdf(&input, &config)?;
        assert_eq!(report.form_metadata_payload_groups_factored, 0);
        assert_eq!(report.form_metadata_payload_wrappers_rewritten, 0);
        Ok(())
    }

    fn duplicate_font_dictionary_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 5 0 R /F2 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")?;
        pdf.object(6, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")?;
        pdf.finish(1)
    }

    fn duplicate_extgstate_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ExtGState << /GS1 5 0 R /GS2 6 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /CA 0.5 /ca 0.5 >>")?;
        pdf.object(6, b"<< /CA 0.5 /ca 0.5 >>")?;
        pdf.finish(1)
    }

    fn extgstate_converges_after_font_dedup_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /ExtGState << /GS1 5 0 R /GS2 6 0 R >> /Font << /F1 7 0 R /F2 8 0 R >> >> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"<< /CA 0.5 /Font [7 0 R 12] >>")?;
        pdf.object(6, b"<< /CA 0.5 /Font [8 0 R 12] >>")?;
        pdf.object(7, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")?;
        pdf.object(8, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>")?;
        pdf.finish(1)
    }

    fn duplicate_to_unicode_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Private << /A << /ToUnicode 5 0 R >> /B << /ToUnicode 6 0 R >> >> >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.stream(5, b"", b"/CIDInit /ProcSet findresource begin")?;
        pdf.stream(6, b"", b"/CIDInit /ProcSet findresource begin")?;
        pdf.finish(1)
    }

    fn indirect_array_pieceinfo_fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R /Private 5 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 4 0 R >>")?;
        pdf.stream(4, b"", b"q Q")?;
        pdf.object(5, b"[ << /PieceInfo 6 0 R /Keep true >> ]")?;
        pdf.object(6, b"<< /Illustrator true >>")?;
        pdf.finish(1)
    }

    fn corrupt_xref_with_compressed_page_tree_fixture() -> Result<Vec<u8>> {
        fn offset_u32(offset: usize) -> Result<u32> {
            u32::try_from(offset)
                .map_err(|_| Error::Invalid("test fixture offset exceeds u32".to_owned()))
        }

        fn append_object(pdf: &mut Vec<u8>, object: &[u8]) -> usize {
            let offset = pdf.len();
            pdf.extend_from_slice(object);
            offset
        }

        fn xref_entry(out: &mut Vec<u8>, kind: u8, field2: u32, field3: u16) {
            out.push(kind);
            out.extend_from_slice(&field2.to_be_bytes());
            out.extend_from_slice(&field3.to_be_bytes());
        }

        let mut pdf = b"%PDF-1.5\n".to_vec();
        let catalog = append_object(
            &mut pdf,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );

        let object_stream_data = b"2 0 << /Type /Pages /Kids [4 0 R] /Count 1 >>";
        let object_stream_header = format!(
            "3 0 obj\n<< /Type /ObjStm /N 1 /First 4 /Length {} >>\nstream\n",
            object_stream_data.len()
        );
        let object_stream = pdf.len();
        pdf.extend_from_slice(object_stream_header.as_bytes());
        pdf.extend_from_slice(object_stream_data);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");

        let page = append_object(
            &mut pdf,
            b"4 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources <<>> /Contents 6 0 R >>\nendobj\n",
        );
        let content = append_object(
            &mut pdf,
            b"6 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );

        let xref = pdf.len();
        let mut entries = Vec::new();
        xref_entry(&mut entries, 0, 0, 65535);
        xref_entry(&mut entries, 1, offset_u32(catalog)?, 0);
        xref_entry(&mut entries, 2, 3, 0);
        xref_entry(&mut entries, 1, offset_u32(object_stream)?, 0);
        xref_entry(&mut entries, 1, offset_u32(page)?, 0);
        // Corrupt but unrelated normal entry: object 5 falsely points at object 1.
        xref_entry(&mut entries, 1, offset_u32(catalog)?, 0);
        xref_entry(&mut entries, 1, offset_u32(content)?, 0);
        xref_entry(&mut entries, 1, offset_u32(xref)?, 0);
        pdf.extend_from_slice(
            format!(
                "7 0 obj\n<< /Type /XRef /Size 8 /W [1 4 2] /Root 1 0 R /Length {} >>\nstream\n",
                entries.len()
            )
            .as_bytes(),
        );
        pdf.extend_from_slice(&entries);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        pdf.extend_from_slice(format!("startxref\n{xref}\n%%EOF\n").as_bytes());
        Ok(pdf)
    }

    #[test]
    fn default_optimize_drops_disposable_preview_and_private_authoring_state() -> Result<()> {
        let input = practical_cleanup_fixture()?;
        let (output, report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert_eq!(
            report.preservation_dropped_page_keys.get("/Thumb"),
            Some(&1)
        );
        assert_eq!(
            report.preservation_dropped_authoring_keys.get("/PieceInfo"),
            Some(&1)
        );
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.contains("page thumbnail"))
        );
        assert!(report.notes.iter().any(|note| note.contains("/PieceInfo")));

        let source = SourcePdf::from_bytes(output)?;
        let catalog = source.materialize(source.catalog_id())?;
        let catalog = catalog
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten catalog is not a dictionary".to_owned()))?;
        assert!(catalog.contains_key(b"Metadata".as_slice()));

        let page_id = source
            .page_ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("rewritten fixture lost its page".to_owned()))?;
        let page = source.materialize(page_id)?;
        let page = page
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten page is not a dictionary".to_owned()))?;
        assert!(!page.contains_key(b"Thumb".as_slice()));
        assert!(!page.contains_key(b"PieceInfo".as_slice()));
        assert!(page.contains_key(b"LastModified".as_slice()));
        assert!(page.contains_key(b"UnknownPage".as_slice()));
        Ok(())
    }

    #[test]
    fn default_optimize_drops_piece_info_inside_indirect_array() -> Result<()> {
        let input = indirect_array_pieceinfo_fixture()?;
        let (output, report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert_eq!(
            report.preservation_dropped_authoring_keys.get("/PieceInfo"),
            Some(&1)
        );
        let analysis = crate::analyze_pdf_preflight(&output)?;
        assert!(
            !analysis
                .risks
                .iter()
                .any(|finding| finding.kind == crate::RiskKind::PieceInfo)
        );
        Ok(())
    }

    #[test]
    fn preserve_authoring_data_keeps_piece_info_without_keeping_thumbnail() -> Result<()> {
        let input = practical_cleanup_fixture()?;
        let mut config = Config::optimize_only();
        config.preservation.private_authoring_data = true;
        let (output, report) = optimize_pdf(&input, &config)?;
        assert_eq!(
            report.preservation_dropped_page_keys.get("/Thumb"),
            Some(&1)
        );
        assert!(
            !report
                .preservation_dropped_authoring_keys
                .contains_key("/PieceInfo")
        );
        let source = SourcePdf::from_bytes(output)?;
        let page_id = source
            .page_ids()
            .into_iter()
            .next()
            .ok_or_else(|| Error::Invalid("rewritten fixture lost its page".to_owned()))?;
        let page = source.materialize(page_id)?;
        let page = page
            .as_dictionary()
            .ok_or_else(|| Error::Invalid("rewritten page is not a dictionary".to_owned()))?;
        assert!(!page.contains_key(b"Thumb".as_slice()));
        assert!(page.contains_key(b"PieceInfo".as_slice()));
        assert!(page.contains_key(b"LastModified".as_slice()));
        Ok(())
    }

    #[test]
    fn analysis_of_corrupt_xref_keeps_compressed_page_tree_reachable() -> Result<()> {
        let input = corrupt_xref_with_compressed_page_tree_fixture()?;
        let source = SourcePdf::from_bytes(input.clone())?;
        assert_eq!(source.page_count(), 1);

        let (output, report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert_eq!(report.before.page_count, 1);
        let rewritten = SourcePdf::from_bytes(output)?;
        assert_eq!(rewritten.page_count(), 1);
        Ok(())
    }

    #[test]
    fn complete_preservation_inventory_keeps_cleanup_for_present_keys() -> Result<()> {
        let input = practical_cleanup_fixture()?;
        let document = EditDocument::from_bytes(input.clone())?;
        let analysis = analyze_document_for_optimization(&input, &document)?.report;
        assert!(analysis.preservation_key_inventory_complete);
        assert!(analysis.piece_info_key_count > 0);
        assert!(analysis.authoring_metadata_key_count > 0);

        let mut config = Config::optimize_only();
        config.preservation = crate::PreservationConfig::known_functional();
        let (output, report) = optimize_pdf_with_analysis(&input, &config, &analysis)?;
        assert_eq!(
            report.preservation_dropped_authoring_keys.get("/PieceInfo"),
            Some(&1)
        );
        assert_eq!(
            report.preservation_dropped_authoring_keys.get("/Metadata"),
            Some(&1)
        );
        let preflight = crate::analyze_pdf_preflight(&output)?;
        assert_eq!(preflight.piece_info_key_count, 0);
        assert_eq!(preflight.authoring_metadata_key_count, 0);
        Ok(())
    }

    #[test]
    fn incomplete_cached_preservation_inventory_falls_back_to_full_cleanup() -> Result<()> {
        let input = practical_cleanup_fixture()?;
        let document = EditDocument::from_bytes(input.clone())?;
        let mut analysis = analyze_document_for_optimization(&input, &document)?.report;
        analysis.preservation_key_inventory_complete = false;
        analysis.piece_info_key_count = 0;
        analysis.authoring_metadata_key_count = 0;

        let mut config = Config::optimize_only();
        config.preservation = crate::PreservationConfig::known_functional();
        let (output, report) = optimize_pdf_with_analysis(&input, &config, &analysis)?;
        assert_eq!(
            report.preservation_dropped_authoring_keys.get("/PieceInfo"),
            Some(&1)
        );
        assert_eq!(
            report.preservation_dropped_authoring_keys.get("/Metadata"),
            Some(&1)
        );
        let preflight = crate::analyze_pdf_preflight(&output)?;
        assert_eq!(preflight.piece_info_key_count, 0);
        assert_eq!(preflight.authoring_metadata_key_count, 0);
        Ok(())
    }

    #[test]
    fn extgstate_current_state_proof_detects_convergence_after_font_dedup() -> Result<()> {
        let input = extgstate_converges_after_font_dedup_fixture()?;
        let (output, report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert!(report.font_dictionary_references_canonicalized > 0);
        assert_eq!(report.extgstate_duplicate_dictionaries_detected, 1);
        assert_eq!(report.extgstate_dictionary_references_canonicalized, 1);

        let (_, second) = optimize_pdf(&output, &Config::optimize_only())?;
        assert_eq!(second.extgstate_duplicate_dictionaries_detected, 0);
        Ok(())
    }

    #[test]
    fn cached_analysis_api_without_extgstate_handles_falls_back_to_full_dedup() -> Result<()> {
        let input = duplicate_extgstate_fixture()?;
        let document = EditDocument::from_bytes(input.clone())?;
        let analysis = analyze_document_for_optimization(&input, &document)?;
        assert_eq!(analysis.ext_gstate_dictionary_handles.len(), 2);

        let (_, report) =
            optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis.report)?;
        assert_eq!(report.extgstate_duplicate_dictionaries_detected, 1);
        assert_eq!(report.extgstate_dictionary_references_canonicalized, 1);
        Ok(())
    }

    #[test]
    fn old_cached_font_dictionary_inventory_falls_back_to_exact_dedup() -> Result<()> {
        let input = duplicate_font_dictionary_fixture()?;
        let document = EditDocument::from_bytes(input.clone())?;
        let mut analysis = analyze_document_for_optimization(&input, &document)?.report;
        assert_eq!(analysis.font_dictionary_fingerprint_inventory_version, 1);
        assert_eq!(analysis.font_dictionary_duplicate_fingerprint_groups, 1);
        analysis.font_dictionary_fingerprint_inventory_version = 0;
        analysis.font_dictionary_duplicate_fingerprint_groups = 0;

        let (_, report) = optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis)?;
        assert_eq!(report.font_duplicate_dictionaries_detected, 1);
        assert_eq!(report.font_dictionary_references_canonicalized, 1);
        Ok(())
    }

    #[test]
    fn old_cached_to_unicode_inventory_falls_back_to_exact_dedup() -> Result<()> {
        let input = duplicate_to_unicode_fixture()?;
        let document = EditDocument::from_bytes(input.clone())?;
        let mut analysis = analyze_document_for_optimization(&input, &document)?.report;
        assert_eq!(analysis.to_unicode_raw_payload_inventory_version, 1);
        assert_eq!(analysis.to_unicode_raw_duplicate_payload_groups, 1);
        analysis.to_unicode_raw_payload_inventory_version = 0;
        analysis.to_unicode_raw_duplicate_payload_groups = 0;

        let (_, report) = optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis)?;
        assert_eq!(report.to_unicode_duplicate_streams_detected, 1);
        assert_eq!(report.to_unicode_references_canonicalized, 1);
        Ok(())
    }

    #[test]
    fn matching_cached_analysis_is_reused() -> Result<()> {
        let input = corrupt_xref_with_compressed_page_tree_fixture()?;
        let mut analysis =
            analyze_document_for_optimization(&input, &EditDocument::from_bytes(input.clone())?)?
                .report;
        analysis.warnings.push("cached-analysis-marker".to_owned());
        let (_, report) = optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis)?;
        assert!(
            report
                .before
                .warnings
                .iter()
                .any(|warning| warning == "cached-analysis-marker")
        );
        Ok(())
    }

    #[test]
    fn stale_cached_analysis_is_rejected() -> Result<()> {
        let input = corrupt_xref_with_compressed_page_tree_fixture()?;
        let mut analysis =
            analyze_document_for_optimization(&input, &EditDocument::from_bytes(input.clone())?)?
                .report;
        analysis.input_sha256 = "00".repeat(32);
        assert!(matches!(
            optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis),
            Err(crate::Error::AnalysisInputMismatch)
        ));
        Ok(())
    }

    #[test]
    fn print_profile_rejects_zero_max_image_ppi() -> Result<()> {
        let input = corrupt_xref_with_compressed_page_tree_fixture()?;
        let mut config = Config::print();
        config.max_image_ppi = Some(0);
        let Err(error) = optimize_pdf(&input, &config) else {
            return Err(Error::Invalid("zero PPI was not rejected".to_owned()));
        };
        assert!(matches!(error, Error::Invalid(message) if message.contains("greater than zero")));
        Ok(())
    }
}
