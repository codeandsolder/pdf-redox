use crate::{
    Config, EditDocument, GlyphCatalog, ImagePolicy, OptimizationReport, PdfAnalysis, Result,
    analyze::{OptimizationAnalysis, analyze_document_for_optimization},
    content::{PageRewriteResult, normalize_page_contents},
    dead_paint::{DeadPaintStats, eliminate_dead_image_paints},
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
        StrokeFormFactorStats, StrokeFormScanEvidence, batch_page_paints,
        coalesce_optional_content, compact_collinear_paths, factor_outlined_glyphs,
        factor_repeated_stroke_forms,
    },
    polyline_simplify::{
        MAX_POLYLINE_PAGE_ERROR_PT, PolylineSimplificationStats, simplify_page_polylines,
    },
    preservation::{
        PreservationScanHints, PreservationStats, apply_preservation_policy,
        apply_preservation_policy_with_hints, drop_page_thumbnails, drop_private_authoring_data,
    },
    print::{PrintPlanHayro, plan_print_downsampling},
    prune::{VerifiedResourceUsage, prune_resources},
    raster_layout::{RasterLayoutStats, normalize_raster_layout},
    repeated_clip::{RepeatedClipStats, hoist_repeated_clip_prefixes},
    repeated_page_objects::{
        RepeatedPageObjectStats, remove_repeated_page_objects,
        repeated_page_objects_prefix_possible,
    },
    scrub::scrub_edit_document_cos_privacy,
    stream_codec::FlateBatchEncoder,
    structure_compact::compact_structure,
    vector_compact::{
        ProcessingVectorAnalysis, VectorCompactionStats, compact_vector_paths_with_flate_cache,
        factor_size_shared_forms, processing_factor_candidate,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
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

/// Source-negative hints can skip exact dedup only while the upstream phases
/// that affect their object identities or references have not invalidated them.
#[derive(Clone, Copy)]
struct FontDedupContext<'a> {
    cfg: &'a Config,
    before: &'a PdfAnalysis,
    pages: &'a [crate::ObjectHandle],
    preservation: &'a PreservationStats,
    scrub: &'a crate::scrub::ScrubStats,
    ext_gstate_dictionary_handles: Option<&'a [crate::ObjectHandle]>,
}

struct FontDedupPhaseResult {
    font_rendering: FontOptimizationStats,
    metadata_dedup: crate::dedup::TargetedDedupStats,
    font_dedup: crate::dedup::TargetedDedupStats,
    font_dense: FontOptimizationStats,
    post_dedup_font_graph: Option<FontUsageGraph>,
    cid_cmap_source_inventory_still_valid: bool,
    to_unicode_dedup: crate::dedup::TargetedDedupStats,
    font_object_dedup: crate::dedup::ExactObjectDedupStats,
    sampled_function_dedup: crate::dedup::TargetedDedupStats,
    extgstate_object_dedup: crate::dedup::ExactObjectDedupStats,
    structure_attribute_dedup: crate::dedup::ExactObjectDedupStats,
    icc_dedup: crate::dedup::TargetedDedupStats,
    icc_alternate: IccAlternateElisionStats,
}

/// Deduplication of font-reference-bearing dictionaries observes the mutation
/// counters from prior font/metadata stages, then rechecks `ExtGState` candidates
/// against current document objects after sampled Functions are canonicalized.
struct ReferenceDedupResult {
    to_unicode_dedup: crate::dedup::TargetedDedupStats,
    font_object_dedup: crate::dedup::ExactObjectDedupStats,
    sampled_function_dedup: crate::dedup::TargetedDedupStats,
    extgstate_object_dedup: crate::dedup::ExactObjectDedupStats,
    structure_attribute_dedup: crate::dedup::ExactObjectDedupStats,
    icc_dedup: crate::dedup::TargetedDedupStats,
    icc_alternate: IccAlternateElisionStats,
}

fn run_reference_dedup_phase(
    document: &mut EditDocument,
    context: FontDedupContext<'_>,
    metadata_dedup: &crate::dedup::TargetedDedupStats,
    font_dedup: &crate::dedup::TargetedDedupStats,
    timings: &mut BTreeMap<String, f64>,
) -> Result<ReferenceDedupResult> {
    let FontDedupContext {
        cfg,
        before,
        preservation,
        scrub,
        ext_gstate_dictionary_handles,
        ..
    } = context;
    let to_unicode_dedup = timed(timings, "to-unicode-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 1 && before.to_unicode_key_count == 0
            || before.to_unicode_raw_payload_inventory_version >= 1
                && before.to_unicode_raw_duplicate_payload_groups == 0
        {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_to_unicode_cmaps(document)
        }
    })?;
    let font_object_dedup = timed(timings, "font-object-dedup", || {
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
            canonicalize_exact_font_dictionaries(document)
        }
    })?;
    // Sampled Functions can be dependencies of ExtGState, tint transforms, and
    // shadings. Intern them before higher-level exact dedup so containers that
    // differed only by duplicate Function references can converge this pass.
    let sampled_function_dedup = timed(timings, "sampled-function-dedup", || {
        canonicalize_sampled_function_streams(document)
    })?;
    let extgstate_object_dedup = timed(timings, "extgstate-object-dedup", || {
        let source_candidates_still_prove_uniqueness = match ext_gstate_dictionary_handles {
            Some(handles) => !has_exact_duplicate_extgstate_dictionaries(document, handles)?,
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
            canonicalize_exact_extgstate_dictionaries(document)
        }
    })?;
    let structure_attribute_dedup = timed(timings, "structure-attribute-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 2
            && before.structure_attribute_ref_count == 0
        {
            Ok(crate::dedup::ExactObjectDedupStats::default())
        } else {
            canonicalize_exact_structure_attribute_dictionaries(document)
        }
    })?;
    let icc_dedup = timed(timings, "icc-dedup", || {
        if before.targeted_dedup_key_inventory_version >= 2 && before.icc_based_array_count == 0 {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_icc_profiles(document)
        }
    })?;
    let icc_alternate = timed(timings, "icc-alternate-elision", || {
        if cfg.elide_icc_profiles_to_alternate {
            if before.targeted_dedup_key_inventory_version >= 2 && before.icc_based_array_count == 0
            {
                Ok(IccAlternateElisionStats::default())
            } else {
                elide_icc_profiles_to_alternates(document)
            }
        } else {
            Ok(IccAlternateElisionStats::default())
        }
    })?;
    Ok(ReferenceDedupResult {
        to_unicode_dedup,
        font_object_dedup,
        sampled_function_dedup,
        extgstate_object_dedup,
        structure_attribute_dedup,
        icc_dedup,
        icc_alternate,
    })
}

/// Keep font-table stripping, font program canonicalization, graph-dependent
/// rewrites, and exact dictionary dedup in this order. In particular, font
/// and `ToUnicode` reference mutations precede the source-uniqueness proof.
fn run_font_dedup_phase(
    document: &mut EditDocument,
    context: FontDedupContext<'_>,
    timings: &mut BTreeMap<String, f64>,
) -> Result<FontDedupPhaseResult> {
    let FontDedupContext {
        cfg, before, pages, ..
    } = context;
    let flate_level = cfg.optimization_goal.flate_level();
    // Strip rendering-irrelevant editing/layout state before font-program
    // dedup so producer subsets can converge to the same program.
    let mut font_rendering = timed(timings, "font-table-strip", || {
        if cfg.preservation.font_editing_support
            || before.font_rendering_table_inventory_complete
                && before.font_rendering_table_candidate_count == 0
        {
            Ok(FontOptimizationStats::default())
        } else {
            strip_font_editing_tables(document, flate_level)
        }
    })?;
    let metadata_dedup = timed(timings, "metadata-dedup", || {
        if before.preservation_key_inventory_complete && before.authoring_metadata_key_count == 0 {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_metadata_streams(document)
        }
    })?;
    let font_dedup = timed(timings, "font-dedup", || {
        if before.font_raw_payload_inventory_version >= 1
            && before.font_raw_duplicate_payload_groups == 0
            && font_rendering.programs_optimized == 0
        {
            Ok(crate::dedup::TargetedDedupStats::default())
        } else {
            canonicalize_font_program_streams(document)
        }
    })?;
    let FontGraphPhaseResult {
        font_dense,
        post_dedup_font_graph,
        cid_cmap_source_inventory_still_valid,
    } = run_font_graph_phase(
        document,
        pages,
        cfg,
        before,
        flate_level,
        timings,
        &mut font_rendering,
    )?;
    let ReferenceDedupResult {
        to_unicode_dedup,
        font_object_dedup,
        sampled_function_dedup,
        extgstate_object_dedup,
        structure_attribute_dedup,
        icc_dedup,
        icc_alternate,
    } = run_reference_dedup_phase(document, context, &metadata_dedup, &font_dedup, timings)?;
    Ok(FontDedupPhaseResult {
        font_rendering,
        metadata_dedup,
        font_dedup,
        font_dense,
        post_dedup_font_graph,
        cid_cmap_source_inventory_still_valid,
        to_unicode_dedup,
        font_object_dedup,
        sampled_function_dedup,
        extgstate_object_dedup,
        structure_attribute_dedup,
        icc_dedup,
        icc_alternate,
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
    if cfg
        .flate_gpu
        .as_ref()
        .is_some_and(|gpu| gpu.min_cap_per_mille > 1000)
    {
        return Err(crate::Error::Invalid(
            "GPU Flate minimum cap ratio must be between 0 and 1000 per mille".to_owned(),
        ));
    }

    #[cfg(not(all(feature = "gpu", not(target_family = "wasm"))))]
    if cfg.flate_gpu.is_some() {
        return Err(crate::Error::Invalid(
            "GPU Flate requires a native build with the gpu feature".to_owned(),
        ));
    }
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

/// Run the production pathological-microstroke detector and raster reconstruction
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
    rasterize_pathological_microstrokes(&mut document, &pages, optimization_goal)
}

/// Reusable optimizer with a fixed policy and lazily initialized compression
/// resources.
///
/// Document-local analyses and rewrite caches are never retained.
/// Reuse the session across PDFs to amortize worker and GPU initialization.
pub struct OptimizationSession {
    config: Config,
    flate_encoder: OnceLock<Result<FlateBatchEncoder>>,
}

impl OptimizationSession {
    /// Create a session with the supplied immutable optimization policy.
    ///
    /// # Errors
    /// Returns an error for an invalid or unsupported configuration.
    pub fn new(config: Config) -> Result<Self> {
        validate_config(&config)?;
        Ok(Self {
            config,
            flate_encoder: OnceLock::new(),
        })
    }

    fn flate_encoder(&self) -> Result<&FlateBatchEncoder> {
        self.flate_encoder
            .get_or_init(|| FlateBatchEncoder::from_config(&self.config))
            .as_ref()
            .map_err(|error| crate::Error::Compression(error.to_string()))
    }

    /// Optimize one document using the session's policy and compression resources.
    ///
    /// # Errors
    /// Returns an error when the PDF cannot be parsed, optimized or serialized.
    pub fn optimize(&self, input: &[u8]) -> Result<(Vec<u8>, OptimizationReport)> {
        self.run(input, None, None)
    }

    /// Optimize one document using a previously computed analysis of these exact bytes.
    ///
    /// # Errors
    /// Returns an error for mismatched analysis or when optimization fails.
    pub fn optimize_with_analysis(
        &self,
        input: &[u8],
        analysis: &PdfAnalysis,
    ) -> Result<(Vec<u8>, OptimizationReport)> {
        self.run(input, Some(analysis), None)
    }

    /// Optimize one document and collect the Type3 geometry catalog produced by
    /// the Processing goal. Collection does not change the optimized PDF.
    ///
    /// # Errors
    /// Returns an error when optimization fails.
    pub fn optimize_with_glyph_catalog(
        &self,
        input: &[u8],
    ) -> Result<(Vec<u8>, OptimizationReport, GlyphCatalog)> {
        let mut catalog = GlyphCatalog::default();
        let (output, report) = self.run(input, None, Some(&mut catalog))?;
        catalog.optimized_pdf_sha256 = crate::digest::sha256_hex(&output);
        Ok((output, report, catalog))
    }

    fn run(
        &self,
        input: &[u8],
        analysis: Option<&PdfAnalysis>,
        catalog: Option<&mut GlyphCatalog>,
    ) -> Result<(Vec<u8>, OptimizationReport)> {
        if analysis.is_some_and(|analysis| !analysis.matches_input(input)) {
            return Err(crate::Error::AnalysisInputMismatch);
        }
        let mut timings = BTreeMap::new();
        let document = timed(&mut timings, "document-open", || {
            EditDocument::from_bytes(input.to_vec())
        })?;
        let analysis = match analysis {
            Some(analysis) => PreparedAnalysis::Cached(analysis.clone()),
            None => PreparedAnalysis::Fresh(timed(&mut timings, "analysis", || {
                analyze_document_for_optimization(input, &document)
            })?),
        };
        optimize_pdf_with_document(document, self, analysis, timings, catalog)
    }
}

/// Optimize a PDF according to the supplied configuration.
///
/// # Errors
///
/// Returns an error when the configuration is invalid, the input PDF cannot be parsed,
/// an optimization pass fails, or the rewritten document cannot be serialized.
pub fn optimize_pdf(input: &[u8], cfg: &Config) -> Result<(Vec<u8>, OptimizationReport)> {
    OptimizationSession::new(cfg.clone())?.optimize(input)
}

/// Optimize a PDF and return the document-wide catalog of Type3 outline
/// components actually created by the Processing goal.
///
/// The catalog stores reusable geometry candidates and page placements, not
/// Unicode predictions: one visual component is not necessarily one character.
/// All ordinary optimization results remain identical when collection is enabled.
///
/// # Errors
///
/// Returns an error when the PDF or optimization configuration is invalid.
pub fn optimize_pdf_with_glyph_catalog(
    input: &[u8],
    cfg: &Config,
) -> Result<(Vec<u8>, OptimizationReport, GlyphCatalog)> {
    OptimizationSession::new(cfg.clone())?.optimize_with_glyph_catalog(input)
}

/// A freshly analyzed document carries source-local handles; a cached public
/// report cannot, so optimizations requiring those handles must rescan.
/// Keeping the two states distinct prevents accidentally treating cached
/// reports as complete source inventories.
enum PreparedAnalysis {
    Fresh(OptimizationAnalysis),
    Cached(PdfAnalysis),
}

impl PreparedAnalysis {
    fn into_parts(self) -> (PdfAnalysis, Option<Vec<crate::ObjectHandle>>) {
        match self {
            Self::Fresh(analysis) => (
                analysis.report,
                Some(analysis.ext_gstate_dictionary_handles),
            ),
            Self::Cached(report) => (report, None),
        }
    }
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
    OptimizationSession::new(cfg.clone())?.optimize_with_analysis(input, analysis)
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
    dead_paint: DeadPaintStats,
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
    flate_batch: crate::FlateBatchStats,
    page_content_dedup: crate::dedup::TargetedDedupStats,
    structure_compaction: crate::structure_compact::StructureCompactionStats,
    unfiltered_flate: crate::flate::FlateOptimizationStats,
    font_cid_cmap_strip: FontOptimizationStats,
}

/// Raster preflight facts are observations of a specific content generation,
/// not durable facts about page handles. Page rewrites invalidate only their
/// vector analyses; resource mutations invalidate the global resource inventory
/// independently (including mutations confined to Form `XObjects`).
struct RasterPhaseEvidence {
    vectors_by_page: BTreeMap<crate::ObjectHandle, ProcessingVectorAnalysis>,
    resource_usage: Option<VerifiedResourceUsage>,
}

impl RasterPhaseEvidence {
    fn from_layout(
        vectors_by_page: BTreeMap<crate::ObjectHandle, ProcessingVectorAnalysis>,
        layout_enabled: bool,
        layout: &mut RasterLayoutStats,
    ) -> Self {
        Self {
            vectors_by_page,
            resource_usage: VerifiedResourceUsage::take_from_layout(layout, layout_enabled),
        }
    }

    /// A page rewrite can invalidate previously parsed vector candidates.
    /// A resource effect may also originate in a Form, with no page rewrite.
    fn after_page_rewrite(
        &mut self,
        rewritten_pages: &BTreeSet<crate::ObjectHandle>,
        resource_usage_may_change: bool,
    ) {
        self.vectors_by_page
            .retain(|page, _| !rewritten_pages.contains(page));
        self.after_resource_mutation(resource_usage_may_change);
    }

    fn after_resource_mutation(&mut self, resource_usage_may_change: bool) {
        if resource_usage_may_change {
            // The inventory belongs to the content generation it describes;
            // discarding it makes stale reuse impossible after a rewrite.
            self.resource_usage = None;
        }
    }

    fn prune_resources(
        &self,
        document: &mut EditDocument,
        pages: &[crate::ObjectHandle],
        generated_page_xobjects: &BTreeMap<crate::ObjectHandle, BTreeSet<Vec<u8>>>,
    ) -> Result<crate::prune::ResourcePruneStats> {
        if let Some(verified) = &self.resource_usage {
            verified.prune(document, generated_page_xobjects)
        } else {
            prune_resources(document, pages)
        }
    }

    /// A negative candidate proof requires a current analysis for *every*
    /// page in the stable page snapshot. Matching counts alone are insufficient:
    /// an extra/stale page must not compensate for a missing current one.
    fn rules_out_processing_vector_factor(
        &self,
        page_count: usize,
        stable_pages: &[crate::ObjectHandle],
    ) -> bool {
        if self.vectors_by_page.len() != page_count || stable_pages.len() != page_count {
            return false;
        }
        // Page tree order need not match object-id order. Sorting a copy keeps
        // the original page order intact, and comparing iterators also rejects
        // duplicate page handles compensated by stale cached entries.
        let mut expected_pages = stable_pages.to_vec();
        expected_pages.sort_unstable();
        self.vectors_by_page.keys().copied().eq(expected_pages)
            && !processing_factor_candidate(&self.vectors_by_page)
    }

    fn take_vector_analysis(&mut self) -> BTreeMap<crate::ObjectHandle, ProcessingVectorAnalysis> {
        std::mem::take(&mut self.vectors_by_page)
    }
}

/// Analysis and its reusable proofs originate from the same raster pass.
/// Subsequent mutating phases must update the proof lifecycle explicitly.
struct RasterPreflight {
    layout: RasterLayoutStats,
    evidence: RasterPhaseEvidence,
}

fn prepare_raster_preflight(
    document: &mut EditDocument,
    pages: &[crate::ObjectHandle],
    cfg: &Config,
    flate_level: crate::FlateLevel,
    timings: &mut BTreeMap<String, f64>,
) -> Result<RasterPreflight> {
    let repeated_page_objects_may_rewrite = cfg.remove_repeated_page_objects
        && cfg.optimization_goal == crate::OptimizationGoal::Processing
        && repeated_page_objects_prefix_possible(document, pages)?;
    let mut raster_vector_cache = BTreeMap::new();
    let mut raster_layout = timed(timings, "raster-layout", || {
        let vector_cache = (cfg.optimization_goal == crate::OptimizationGoal::Processing
            && !repeated_page_objects_may_rewrite)
            .then_some(&mut raster_vector_cache);
        normalize_raster_layout(
            document,
            pages,
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
    let evidence = RasterPhaseEvidence::from_layout(
        raster_vector_cache,
        cfg.raster_layout.enabled,
        &mut raster_layout,
    );
    Ok(RasterPreflight {
        layout: raster_layout,
        evidence,
    })
}

/// The ordered vector/paint mutation phase consumes raster preflight facts and
/// returns its Flate cache for the later stream compression passes. Keeping the
/// transformations together preserves the dependencies between factoring,
/// subsequent paint rewrites, and resource-usage proof invalidation.
struct VectorPhaseResult {
    repeated_clips: RepeatedClipStats,
    vector_compaction: VectorCompactionStats,
    marked_content: MarkedContentCoalesceStats,
    collinear_paths: CollinearPathStats,
    outlined_glyphs: OutlinedGlyphFactorStats,
    paint_batch: PaintBatchStats,
    stroke_forms: StrokeFormFactorStats,
    polyline_simplification: PolylineSimplificationStats,
    flate_reuse_cache: FlateReuseCache,
}

fn run_vector_optimization_phase(
    document: &mut EditDocument,
    pages: &[crate::ObjectHandle],
    cfg: &Config,
    layout: &RasterLayoutStats,
    evidence: &mut RasterPhaseEvidence,
    timings: &mut BTreeMap<String, f64>,
    catalog: Option<&mut GlyphCatalog>,
) -> Result<VectorPhaseResult> {
    let flate_level = cfg.optimization_goal.flate_level();
    let mut flate_reuse_cache = FlateReuseCache::default();
    let PageRewriteResult {
        stats: repeated_clips,
        rewritten_pages: repeated_clip_rewritten_pages,
    } = timed(timings, "repeated-clip-hoist", || {
        // The clip/body rewrite is exact in either mode. Size requires a
        // strictly smaller encoded stream; Processing also accepts equal-sized
        // rewrites when they eliminate redundant clipping work.
        hoist_repeated_clip_prefixes(
            document,
            pages,
            flate_level,
            cfg.optimization_goal,
            &mut flate_reuse_cache,
        )
    })?;
    // Clip hoisting changes page operators, but not their resource references.
    evidence.after_page_rewrite(&repeated_clip_rewritten_pages, false);

    let mut vector_compaction = timed(timings, "vector-compaction", || {
        let raster_proved_no_vector_candidate = cfg.optimization_goal
            != crate::OptimizationGoal::Processing
            && cfg.raster_layout.enabled
            && layout.page_vector_inventory_complete
            && !layout.page_vector_merge_candidate;
        let processing_proved_no_vector_candidate = cfg.optimization_goal
            == crate::OptimizationGoal::Processing
            && cfg.raster_layout.enabled
            && layout.page_vector_inventory_complete
            && !layout.page_vector_merge_candidate
            && evidence.rules_out_processing_vector_factor(layout.page_count, pages);
        if !raster_proved_no_vector_candidate && !processing_proved_no_vector_candidate {
            let vector_cache = evidence.take_vector_analysis();
            compact_vector_paths_with_flate_cache(
                document,
                pages,
                flate_level,
                cfg.optimization_goal,
                vector_cache,
                &mut flate_reuse_cache,
            )
        } else {
            Ok(VectorCompactionStats::default())
        }
    })?;
    // A proof that there are no mergeable page fills says nothing about
    // identical, resource-proven q runs across different page streams.
    // Keep the cheap fill-compaction fast path while considering exact
    // shared Forms independently in Size mode. Processing already runs
    // this factor as part of its ordered structural-Form closure.
    if cfg.optimization_goal == crate::OptimizationGoal::Size {
        let forms = timed(timings, "size-shared-form-factor", || {
            factor_size_shared_forms(document, flate_level, pages)
        })?;
        vector_compaction.absorb(forms);
    }
    evidence.after_resource_mutation(vector_compaction.shared_run_forms_created != 0);

    let marked_content = timed(timings, "marked-content-coalesce", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            coalesce_optional_content(document, pages, flate_level, &mut flate_reuse_cache)
        } else {
            Ok(MarkedContentCoalesceStats::default())
        }
    })?;

    let collinear_paths = timed(timings, "collinear-path-compact", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            compact_collinear_paths(document, pages, flate_level, &mut flate_reuse_cache)
        } else {
            Ok(CollinearPathStats::default())
        }
    })?;

    let outlined_glyphs = timed(timings, "outlined-glyph-factor", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            factor_outlined_glyphs(
                document,
                pages,
                flate_level,
                catalog,
                &mut flate_reuse_cache,
            )
        } else {
            Ok(OutlinedGlyphFactorStats::default())
        }
    })?;
    evidence.after_resource_mutation(outlined_glyphs.fonts_created != 0);

    let mut stroke_scan_evidence = StrokeFormScanEvidence::default();
    let paint_batch = timed(timings, "paint-batching", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            batch_page_paints(document, pages, &mut stroke_scan_evidence)
        } else {
            Ok(PaintBatchStats::default())
        }
    })?;

    let stroke_forms = timed(timings, "stroke-form-factor", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            factor_repeated_stroke_forms(document, pages, flate_level, &mut stroke_scan_evidence)
        } else {
            Ok(StrokeFormFactorStats::default())
        }
    })?;
    evidence.after_resource_mutation(stroke_forms.forms_created != 0);

    let polyline_simplification = timed(timings, "polyline-simplify", || {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            simplify_page_polylines(document, pages, flate_level, &mut flate_reuse_cache)
        } else {
            Ok(PolylineSimplificationStats::default())
        }
    })?;

    Ok(VectorPhaseResult {
        repeated_clips,
        vector_compaction,
        marked_content,
        collinear_paths,
        outlined_glyphs,
        paint_batch,
        stroke_forms,
        polyline_simplification,
        flate_reuse_cache,
    })
}

/// Late mutations prune resources, normalize and compact the graph, then encode
/// remaining raw streams, remove safe CID `CMaps`, and serialize.
#[derive(Clone, Copy)]
struct FinalizationContext<'a> {
    session: &'a OptimizationSession,
    before: &'a PdfAnalysis,
    pages: &'a [crate::ObjectHandle],
    raster_evidence: &'a RasterPhaseEvidence,
    generated_page_xobjects: &'a BTreeMap<crate::ObjectHandle, BTreeSet<Vec<u8>>>,
    font_usage_hint: Option<&'a FontUsageGraph>,
    cid_cmap_source_inventory_still_valid: bool,
}

/// The result groups the late-stage counters with the final serialized bytes.
/// The Flate reuse cache is consumed here, after vector factoring, and cannot
/// outlive the document generation it was created for.
struct FinalizationResult {
    resource_prune: crate::prune::ResourcePruneStats,
    flate: crate::flate::FlateOptimizationStats,
    flate_batch: crate::FlateBatchStats,
    page_content_dedup: crate::dedup::TargetedDedupStats,
    structure_compaction: crate::structure_compact::StructureCompactionStats,
    unfiltered_flate: crate::flate::FlateOptimizationStats,
    font_cid_cmap_strip: FontOptimizationStats,
    output: Vec<u8>,
}

fn finalize_document(
    document: &mut EditDocument,
    mut flate_reuse_cache: FlateReuseCache,
    context: FinalizationContext<'_>,
    timings: &mut BTreeMap<String, f64>,
) -> Result<FinalizationResult> {
    let FinalizationContext {
        session,
        before,
        pages,
        raster_evidence,
        generated_page_xobjects,
        font_usage_hint,
        cid_cmap_source_inventory_still_valid,
    } = context;
    let cfg = &session.config;
    let flate_level = cfg.optimization_goal.flate_level();
    let resource_prune = timed(timings, "resource-prune", || {
        raster_evidence.prune_resources(document, pages, generated_page_xobjects)
    })?;
    let flate_encoder = timed(timings, "flate-executor-init", || {
        session.flate_encoder().map(FlateBatchEncoder::for_document)
    })?;
    timed(timings, "content-normalize", || -> Result<()> {
        if cfg.optimization_goal == crate::OptimizationGoal::Processing {
            normalize_page_contents(document, pages)?;
        }
        Ok(())
    })?;
    let mut page_content_dedup = timed(timings, "page-content-dedup-pre", || {
        canonicalize_page_contents(document, pages)
    })?;
    let structure_compaction = timed(timings, "structure-compaction", || {
        compact_structure(document)
    })?;

    // Usage-dependent cmap stripping runs on the final live graph, before
    // the final encoding pass so obsolete font bytes are not recompressed.
    let cid_cmap_possible = !cfg.preservation.font_editing_support
        && (before.font_opportunity_inventory_version < 1
            || !cid_cmap_source_inventory_still_valid
            || before.font_material_cid_cmap_decoded_bytes
                >= crate::font::MIN_CID_CMAP_DECODED_BYTES_PER_DOCUMENT);
    let font_cid_cmap_strip = if cid_cmap_possible {
        timed(timings, "font-cid-cmap-strip", || {
            strip_cidfont_type2_cmaps_after_dedup(document, flate_level, font_usage_hint)
        })?
    } else {
        timings.insert("font-cid-cmap-strip".to_owned(), 0.0);
        FontOptimizationStats::default()
    };
    let flate = timed(timings, "flate-policy", || {
        optimize_flate_streams_with_cache(
            document,
            flate_level,
            &mut flate_reuse_cache,
            &flate_encoder,
        )
    })?;
    // Match the historical writer's StreamDataMode::Compress policy explicitly
    // before handing the graph to the deliberately-simple fresh writer.
    let unfiltered_flate = timed(timings, "compress-unfiltered", || {
        compress_unfiltered_streams(
            document,
            flate_level,
            &mut flate_reuse_cache,
            &flate_encoder,
        )
    })?;
    // Canonical encodings can converge even when the original bytes differed.
    // The prepass already eliminated byte-identical content before encoding.
    let final_dedup = timed(timings, "page-content-dedup", || {
        canonicalize_page_contents(document, pages)
    })?;
    page_content_dedup.duplicate_streams_detected += final_dedup.duplicate_streams_detected;
    page_content_dedup.duplicate_raw_bytes += final_dedup.duplicate_raw_bytes;
    page_content_dedup.references_canonicalized += final_dedup.references_canonicalized;
    let output = timed(timings, "writer", || {
        crate::writer::write_pdf_with_object_streams(document, flate_level)
    })?;

    Ok(FinalizationResult {
        resource_prune,
        flate,
        flate_batch: flate_encoder.stats(),
        page_content_dedup,
        structure_compaction,
        unfiltered_flate,
        font_cid_cmap_strip,
        output,
    })
}

#[expect(
    clippy::too_many_lines,
    reason = "the optimization pipeline intentionally keeps ordered mutations and their shared document state together"
)]
fn optimize_pdf_with_document(
    mut document: EditDocument,
    session: &OptimizationSession,
    analysis: PreparedAnalysis,
    mut timings: BTreeMap<String, f64>,
    catalog: Option<&mut GlyphCatalog>,
) -> Result<(Vec<u8>, OptimizationReport)> {
    let cfg = &session.config;
    let (before, ext_gstate_handles) = analysis.into_parts();
    let ext_gstate_dictionary_handles = ext_gstate_handles.as_deref();
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

    let FontDedupPhaseResult {
        font_rendering,
        metadata_dedup,
        font_dedup,
        font_dense,
        post_dedup_font_graph,
        cid_cmap_source_inventory_still_valid,
        to_unicode_dedup,
        font_object_dedup,
        sampled_function_dedup,
        extgstate_object_dedup,
        structure_attribute_dedup,
        icc_dedup,
        icc_alternate,
    } = run_font_dedup_phase(
        &mut document,
        FontDedupContext {
            cfg,
            before: &before,
            pages: &stable_pages,
            preservation: &preservation,
            scrub: &scrub,
            ext_gstate_dictionary_handles,
        },
        &mut timings,
    )?;
    // Reconstruct recognized raster-like vector fields before any generic
    // vector factoring can obscure the original stroke geometry. This is a
    // structural normalization, independent of profile or encoded size.
    let microstroke_raster = timed(&mut timings, "microstroke-raster", || {
        rasterize_pathological_microstrokes(&mut document, &stable_pages, cfg.optimization_goal)
    })?;
    let RasterPreflight {
        layout: raster_layout,
        evidence: mut raster_evidence,
    } = prepare_raster_preflight(&mut document, &stable_pages, cfg, flate_level, &mut timings)?;
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
    raster_evidence.after_page_rewrite(
        &physically_hidden_text_rewritten_pages,
        !physically_hidden_text_rewritten_pages.is_empty(),
    );

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
    // Processing-vector analysis is page-content-local: Form rewrites do not
    // invalidate cached paths, while resource inventories cover both targets.
    raster_evidence.after_page_rewrite(
        &inline_image_rewritten_pages,
        inline_image_dedup.occurrences_externalized != 0,
    );

    // Entropy-optimize surviving/reconstructed JPEGs before exact image
    // canonicalization. This preserves quantized DCT coefficients while allowing
    // images that differed only in Huffman coding to converge.
    let jpeg_entropy = timed(&mut timings, "jpeg-entropy", || {
        optimize_jpeg_entropy(&mut document, 128, 1, session.flate_encoder()?)
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
    // Image and Form canonicalization make independently stored copies
    // converge. Remove redundant opaque image-only paint invocations before
    // downstream page-level factoring obscures the original duplicate runs.
    let PageRewriteResult {
        stats: dead_paint,
        rewritten_pages: dead_paint_rewritten_pages,
    } = timed(&mut timings, "dead-opaque-image-paints", || {
        eliminate_dead_image_paints(&mut document, &stable_pages)
    })?;
    raster_evidence.after_page_rewrite(&dead_paint_rewritten_pages, dead_paint.paints_removed != 0);
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
    raster_evidence.after_page_rewrite(
        &repeated_page_objects_rewritten_pages,
        repeated_page_objects.objects_removed != 0,
    );

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

    let VectorPhaseResult {
        repeated_clips,
        vector_compaction,
        marked_content,
        collinear_paths,
        outlined_glyphs,
        paint_batch,
        stroke_forms,
        polyline_simplification,
        flate_reuse_cache,
    } = run_vector_optimization_phase(
        &mut document,
        &stable_pages,
        cfg,
        &raster_layout,
        &mut raster_evidence,
        &mut timings,
        catalog,
    )?;

    let FinalizationResult {
        resource_prune,
        flate,
        flate_batch,
        page_content_dedup,
        structure_compaction,
        unfiltered_flate,
        font_cid_cmap_strip,
        output,
    } = finalize_document(
        &mut document,
        flate_reuse_cache,
        FinalizationContext {
            session,
            before: &before,
            pages: &stable_pages,
            raster_evidence: &raster_evidence,
            generated_page_xobjects: &vector_compaction.generated_page_xobjects,
            font_usage_hint: post_dedup_font_graph.as_ref(),
            cid_cmap_source_inventory_still_valid,
        },
        &mut timings,
    )?;

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
        dead_paint,
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
        flate_batch,
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
    fn font_dedup_proofs_preserve_fresh_and_cached_results_in_both_goals() -> Result<()> {
        for (label, input) in [
            ("font dictionaries", duplicate_font_dictionary_fixture()?),
            ("ToUnicode streams", duplicate_to_unicode_fixture()?),
            (
                "ExtGState convergence after font dedup",
                extgstate_converges_after_font_dedup_fixture()?,
            ),
        ] {
            let analysis = analyze_document_for_optimization(
                &input,
                &EditDocument::from_bytes(input.clone())?,
            )?
            .report;
            for goal in [
                crate::OptimizationGoal::Size,
                crate::OptimizationGoal::Processing,
            ] {
                let mut cfg = Config::optimize_only();
                cfg.optimization_goal = goal;
                let (fresh, fresh_report) = optimize_pdf(&input, &cfg)?;
                let (cached, cached_report) = optimize_pdf_with_analysis(&input, &cfg, &analysis)?;
                assert_eq!(
                    fresh, cached,
                    "{label}, {goal:?}: cached and fresh output differ"
                );
                for stage in [
                    "font-table-strip",
                    "metadata-dedup",
                    "font-dedup",
                    "font-sparse-union",
                    "font-outline-opportunity",
                    "font-outline-subset",
                    "font-dense-gid",
                    "to-unicode-dedup",
                    "font-object-dedup",
                    "sampled-function-dedup",
                    "extgstate-object-dedup",
                    "structure-attribute-dedup",
                    "icc-dedup",
                    "icc-alternate-elision",
                ] {
                    assert!(
                        fresh_report.stage_timings_ms.contains_key(stage)
                            && cached_report.stage_timings_ms.contains_key(stage),
                        "{label}, {goal:?}: missing font/dedup timing {stage}"
                    );
                }
                match label {
                    "font dictionaries" => {
                        assert_eq!(fresh_report.font_dictionary_references_canonicalized, 1);
                        assert_eq!(cached_report.font_dictionary_references_canonicalized, 1);
                    }
                    "ToUnicode streams" => {
                        assert_eq!(fresh_report.to_unicode_references_canonicalized, 1);
                        assert_eq!(cached_report.to_unicode_references_canonicalized, 1);
                    }
                    _ => {
                        assert_eq!(
                            fresh_report.extgstate_dictionary_references_canonicalized,
                            1
                        );
                        assert_eq!(
                            cached_report.extgstate_dictionary_references_canonicalized,
                            1
                        );
                    }
                }
            }
        }
        Ok(())
    }

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
    fn session_reuses_executor_without_retaining_document_state() -> Result<()> {
        let config = Config::optimize_only();
        let session = OptimizationSession::new(config.clone())?;
        let first = duplicate_extgstate_fixture()?;
        let second = duplicate_to_unicode_fixture()?;
        assert!(session.flate_encoder.get().is_none());
        let (first_output, _) = session.optimize(&first)?;
        let executor = session.flate_encoder()?;
        let analysis = crate::analyze_pdf(&second)?;
        assert!(session.optimize_with_analysis(&first, &analysis).is_err());
        let (second_output, _) = session.optimize_with_analysis(&second, &analysis)?;
        assert!(std::ptr::eq(executor, session.flate_encoder()?));
        assert_eq!(first_output, optimize_pdf(&first, &config)?.0);
        assert_eq!(second_output, optimize_pdf(&second, &config)?.0);
        assert_eq!(first_output, session.optimize(&first)?.0);
        Ok(())
    }

    #[test]
    fn finalization_preserves_stage_timing_contract_in_both_goals() -> Result<()> {
        let input = duplicate_extgstate_fixture()?;
        let analysis =
            analyze_document_for_optimization(&input, &EditDocument::from_bytes(input.clone())?)?
                .report;
        for goal in [
            crate::OptimizationGoal::Size,
            crate::OptimizationGoal::Processing,
        ] {
            let mut cfg = Config::optimize_only();
            cfg.optimization_goal = goal;
            let (output, report) = optimize_pdf(&input, &cfg)?;
            let (cached_output, cached_report) =
                optimize_pdf_with_analysis(&input, &cfg, &analysis)?;
            assert_eq!(
                output, cached_output,
                "{goal:?} changed with cached analysis"
            );
            // Only a fresh optimization performs the initial analysis stage.
            assert_eq!(
                report
                    .stage_timings_ms
                    .keys()
                    .filter(|stage| stage.as_str() != "analysis")
                    .collect::<Vec<_>>(),
                cached_report.stage_timings_ms.keys().collect::<Vec<_>>(),
                "{goal:?} omitted timing keys when using cached analysis"
            );
            assert_eq!(SourcePdf::from_bytes(output)?.page_count(), 1);
            for stage in [
                "resource-prune",
                "microstroke-raster",
                "flate-executor-init",
                "flate-policy",
                "content-normalize",
                "page-content-dedup",
                "structure-compaction",
                "compress-unfiltered",
                "font-cid-cmap-strip",
                "writer",
            ] {
                assert!(
                    report.stage_timings_ms.contains_key(stage),
                    "{goal:?} omitted finalization timing {stage}"
                );
            }
        }
        Ok(())
    }

    #[test]
    fn raster_phase_evidence_invalidates_only_affected_observations() -> Result<()> {
        use crate::{
            ObjectHandle, ObjectId, OwnedDictionary, vector_compact::ProcessingPageScanner,
        };

        let input = practical_cleanup_fixture()?;
        let document = EditDocument::from_bytes(input)?;
        let first = ObjectHandle::Existing(ObjectId::new(30, 0));
        let second = ObjectHandle::Existing(ObjectId::new(31, 0));
        let blank_analysis = || -> Result<ProcessingVectorAnalysis> {
            Ok(
                ProcessingPageScanner::for_resources(&document, &OwnedDictionary::new(), None)?
                    .finish(b""),
            )
        };
        let layout = RasterLayoutStats {
            resource_inventory_complete: true,
            ..RasterLayoutStats::default()
        };
        let mut layout = layout;
        let mut evidence = RasterPhaseEvidence::from_layout(
            BTreeMap::from([(first, blank_analysis()?), (second, blank_analysis()?)]),
            true,
            &mut layout,
        );
        assert!(evidence.resource_usage.is_some());
        assert!(evidence.rules_out_processing_vector_factor(2, &[first, second]));
        assert!(evidence.rules_out_processing_vector_factor(2, &[second, first]));
        assert!(!evidence.rules_out_processing_vector_factor(2, &[first, first]));
        assert!(!evidence.rules_out_processing_vector_factor(
            2,
            &[first, ObjectHandle::Existing(ObjectId::new(32, 0))]
        ));

        // A content-only rewrite must not discard the unrelated page's scan
        // or the independent resource usage proof.
        evidence.after_page_rewrite(&BTreeSet::from([first]), false);
        assert!(evidence.vectors_by_page.contains_key(&second));
        assert!(!evidence.vectors_by_page.contains_key(&first));
        assert!(!evidence.rules_out_processing_vector_factor(2, &[first, second]));
        assert!(evidence.resource_usage.is_some());

        // A Form-only edit can invalidate resource usage without rewriting any
        // page, while resource invalidation must remain sticky thereafter.
        evidence.after_page_rewrite(&BTreeSet::new(), true);
        assert!(evidence.vectors_by_page.contains_key(&second));
        assert!(evidence.resource_usage.is_none());
        evidence.after_page_rewrite(&BTreeSet::new(), false);
        assert!(evidence.resource_usage.is_none());
        assert_eq!(evidence.take_vector_analysis().len(), 1);
        assert!(evidence.vectors_by_page.is_empty());

        let disabled = RasterPhaseEvidence::from_layout(BTreeMap::new(), false, &mut layout);
        assert!(disabled.resource_usage.is_none());
        Ok(())
    }

    #[test]
    fn resource_pruning_proof_keeps_current_names_and_falls_back_after_rewrite() -> Result<()> {
        use crate::{
            ObjectHandle, ObjectId,
            content::{page_resources, replace_page_content, resolved_dictionary},
        };

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 20 20] /Resources << /XObject << /Keep 5 0 R /Drop 6 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"/Keep Do")?;
        for (id, gray) in [(5, 42), (6, 84)] {
            pdf.stream(
                id,
                b"/Type /XObject /Subtype /Image /Width 1 /Height 1 /ColorSpace /DeviceGray /BitsPerComponent 8",
                &[gray],
            )?;
        }
        let input = pdf.finish(1)?;
        let page = ObjectHandle::Existing(ObjectId::new(3, 0));
        let source_usage = || RasterLayoutStats {
            resource_inventory_complete: true,
            page_resource_names_by_type: BTreeMap::from([(
                page,
                BTreeMap::from([(b"XObject".to_vec(), BTreeSet::from([b"Keep".to_vec()]))]),
            )]),
            ..RasterLayoutStats::default()
        };
        let remaining_names = |doc: &EditDocument| -> Result<BTreeSet<Vec<u8>>> {
            let page_resources = page_resources(doc, page)?
                .ok_or_else(|| Error::Invalid("page has no resources".to_owned()))?;
            let xobjects = resolved_dictionary(doc, page_resources.get(b"XObject".as_slice()))?
                .ok_or_else(|| Error::Invalid("page has no XObjects".to_owned()))?;
            Ok(xobjects.keys().cloned().collect())
        };

        let mut fast_document = EditDocument::from_bytes(input.clone())?;
        let mut fresh_layout = source_usage();
        let evidence = RasterPhaseEvidence::from_layout(BTreeMap::new(), true, &mut fresh_layout);
        assert!(evidence.resource_usage.is_some());
        assert!(fresh_layout.page_resource_names_by_type.is_empty());
        assert!(fresh_layout.form_resource_names_by_type.is_empty());
        let stats = evidence.prune_resources(&mut fast_document, &[page], &BTreeMap::new())?;
        assert_eq!(stats.xobjects, 1);
        assert_eq!(
            remaining_names(&fast_document)?,
            BTreeSet::from([b"Keep".to_vec()])
        );

        // Form factoring can introduce a page XObject name after raster
        // preflight. Verified pruning must preserve such explicitly generated
        // names even if the original content inventory did not contain them.
        let mut generated_document = EditDocument::from_bytes(input.clone())?;
        let mut generated_layout = source_usage();
        let generated_evidence =
            RasterPhaseEvidence::from_layout(BTreeMap::new(), true, &mut generated_layout);
        let generated = BTreeMap::from([(page, BTreeSet::from([b"Drop".to_vec()]))]);
        assert_eq!(
            generated_evidence.prune_resources(&mut generated_document, &[page], &generated)?,
            crate::prune::ResourcePruneStats::default()
        );
        assert_eq!(
            remaining_names(&generated_document)?,
            BTreeSet::from([b"Keep".to_vec(), b"Drop".to_vec()])
        );

        // Replacing content with a reference to a previously unused resource
        // invalidates the original proof: the full scanner must retain /Drop.
        let mut rewritten_document = EditDocument::from_bytes(input.clone())?;
        let mut stale_layout = source_usage();
        let mut stale_evidence =
            RasterPhaseEvidence::from_layout(BTreeMap::new(), true, &mut stale_layout);
        replace_page_content(&mut rewritten_document, page, b"/Drop Do".to_vec())?;
        stale_evidence.after_page_rewrite(&BTreeSet::from([page]), true);
        assert!(stale_evidence.resource_usage.is_none());
        let fallback =
            stale_evidence.prune_resources(&mut rewritten_document, &[page], &BTreeMap::new())?;
        assert_eq!(fallback.xobjects, 1);
        assert_eq!(
            remaining_names(&rewritten_document)?,
            BTreeSet::from([b"Drop".to_vec()])
        );

        // If raster proof collection is unavailable, its maps are left
        // unconsumed and pruning likewise uses the canonical full scan.
        let mut disabled_layout = source_usage();
        let disabled =
            RasterPhaseEvidence::from_layout(BTreeMap::new(), false, &mut disabled_layout);
        assert!(disabled.resource_usage.is_none());
        assert!(!disabled_layout.page_resource_names_by_type.is_empty());
        let mut incomplete_layout = source_usage();
        incomplete_layout.resource_inventory_complete = false;
        let incomplete =
            RasterPhaseEvidence::from_layout(BTreeMap::new(), true, &mut incomplete_layout);
        assert!(incomplete.resource_usage.is_none());
        assert!(!incomplete_layout.page_resource_names_by_type.is_empty());
        let mut fallback_document = EditDocument::from_bytes(input)?;
        assert_eq!(
            disabled.prune_resources(&mut fallback_document, &[page], &BTreeMap::new())?,
            stats
        );
        assert_eq!(
            remaining_names(&fallback_document)?,
            BTreeSet::from([b"Keep".to_vec()])
        );
        Ok(())
    }

    #[test]
    fn size_hoists_large_exact_repeated_clip_without_coordinate_rounding() -> Result<()> {
        use std::io::Write as _;

        // The repeated clip exceeds Deflate's 32-KiB history window. This
        // ensures that repeating the exact path really costs encoded bytes,
        // rather than accidentally testing just the decoded-size heuristic.
        let mut clip = b"0 0 m\n".to_vec();
        for idx in 0..7_000_u32 {
            let x = idx.wrapping_mul(7_919) % 65_521;
            let y = idx.wrapping_mul(1_543) % 65_521;
            writeln!(&mut clip, "{x} {y} l")?;
        }
        clip.extend_from_slice(b"h W* n\n");

        let mut content = Vec::new();
        for row in 0..6_u32 {
            content.extend_from_slice(b"q\n");
            content.extend_from_slice(&clip);
            writeln!(&mut content, "{row} 0 2 2 re f\nQ")?;
        }
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 65521 65521] /Resources <<>> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", &content)?;
        let input = pdf.finish(1)?;
        let config = Config::optimize_only();
        assert_eq!(config.optimization_goal, crate::OptimizationGoal::Size);

        let (output, report) = optimize_pdf(&input, &config)?;
        assert!(report.repeated_clip_runs_hoisted > 0);
        assert!(report.repeated_clip_blocks_hoisted >= 6);
        assert!(report.repeated_clip_estimated_flate_bytes_saved > 0);
        assert_eq!(report.vector_path_coordinate_pages_rewritten, 0);
        let (second, replay) = optimize_pdf(&output, &config)?;
        assert_eq!(replay.repeated_clip_runs_hoisted, 0);
        assert_eq!(output, second);
        Ok(())
    }

    fn size_shared_form_fixture(shared_resource: bool, matching_boxes: bool) -> Result<Vec<u8>> {
        use std::io::Write as _;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        // Aliases differ, but sharing is safe when the underlying object is
        // identical. Distinct opacity objects must never be combined.
        for page in 0..2_u32 {
            let target = if shared_resource || page == 0 { 9 } else { 10 };
            let box_height = if matching_boxes || page == 0 {
                500
            } else {
                600
            };
            let page_object = format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 500 {box_height}] /Resources << /ExtGState << /G{page} {target} 0 R >> >> /Contents {} 0 R >>",
                page + 5
            );
            pdf.object(page + 3, page_object.as_bytes())?;
        }
        pdf.object(9, b"<< /Type /ExtGState /ca 0.5 /CA 0.5 >>")?;
        pdf.object(10, b"<< /Type /ExtGState /ca 0.7 /CA 0.7 >>")?;
        for page in 0..2_u32 {
            let mut content = Vec::new();
            writeln!(&mut content, "q /G{page} gs")?;
            for row in 0..7_000_u32 {
                let x = row.wrapping_mul(7_919) % 997;
                let y = row.wrapping_mul(3_149) % 997;
                writeln!(
                    &mut content,
                    "{x} {y} m {} {} l S",
                    (x + 17) % 997,
                    (y + 29) % 997
                )?;
            }
            content.extend_from_slice(b"Q\n");
            pdf.stream(page + 5, b"", &content)?;
        }
        pdf.finish(1)
    }

    #[test]
    fn size_factors_resource_identical_shared_forms_without_fill_candidate() -> Result<()> {
        let input = size_shared_form_fixture(true, true)?;
        let config = Config::optimize_only();
        assert_eq!(config.optimization_goal, crate::OptimizationGoal::Size);
        let (first, report) = optimize_pdf(&input, &config)?;
        assert_eq!(report.vector_shared_run_forms_created, 1);
        assert_eq!(report.vector_shared_run_pages_rewritten, 2);
        assert!(report.vector_shared_run_estimated_flate_bytes_saved > 0);
        assert_eq!(report.vector_path_coordinate_pages_rewritten, 0);
        assert_eq!(report.vector_fill_groups_batched, 0);
        let (second, replay) = optimize_pdf(&first, &config)?;
        assert_eq!(replay.vector_shared_run_forms_created, 0);
        assert_eq!(first, second);
        Ok(())
    }

    #[test]
    fn size_never_shares_forms_across_distinct_resource_targets_or_page_boxes() -> Result<()> {
        for (shared_resources, matching_boxes) in [(false, true), (true, false)] {
            let input = size_shared_form_fixture(shared_resources, matching_boxes)?;
            let (_, report) = optimize_pdf(&input, &Config::optimize_only())?;
            assert_eq!(report.vector_shared_run_forms_created, 0);
            assert_eq!(report.vector_shared_run_pages_rewritten, 0);
        }
        Ok(())
    }

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

    fn processing_coordinate_form_fixture() -> Result<Vec<u8>> {
        use std::io::Write as _;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        for page_number in 0..2_u32 {
            let page_id = 3 + page_number;
            let stream_id = 5 + page_number;
            pdf.object(
                page_id,
                format!(
                    "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 20000] /Resources <<>> /Contents {stream_id} 0 R >>"
                )
                .as_bytes(),
            )?;
        }
        for page_number in 0..2_u32 {
            let mut content = Vec::new();
            for row in 0..1500_u32 {
                let index = page_number * 1500 + row;
                // Every body is initially distinct, but its differences are
                // within the single-shot 0.0071pt coordinate error budget.
                // Two pages are intentional: structural keys match globally.
                let tiny = f64::from(100 + (index * 197 % 3000)) / 1_000_000.0;
                writeln!(&mut content, "q 1 0 0 1 0 {} cm", row * 12)?;
                for col in 0..6_u32 {
                    writeln!(
                        &mut content,
                        "{tiny:.6} {col} m 20 {col} l 20 {} l h f",
                        col + 1
                    )?;
                }
                content.extend_from_slice(b"Q\n");
            }
            pdf.stream(5 + page_number, b"", &content)?;
        }
        pdf.finish(1)
    }

    #[test]
    fn processing_factors_transformed_forms_exposed_by_coordinate_canonicalization() -> Result<()> {
        let input = processing_coordinate_form_fixture()?;
        let mut config = Config::optimize_only();
        config.optimization_goal = crate::OptimizationGoal::Processing;

        let (first, first_report) = optimize_pdf(&input, &config)?;
        assert!(first_report.vector_path_coordinate_pages_rewritten > 0);
        assert!(
            first_report.vector_transformed_forms_created > 0,
            "exact structural factoring must close after single-shot coordinate rounding"
        );
        assert_eq!(first_report.vector_transformed_form_pages_rewritten, 2);

        let (second, second_report) = optimize_pdf(&first, &config)?;
        assert_eq!(second_report.vector_transformed_forms_created, 0);
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

        let cfg = Config::optimize_only();
        let (fresh_output, fresh_report) = optimize_pdf(&input, &cfg)?;
        let (cached_output, report) = optimize_pdf_with_analysis(&input, &cfg, &analysis.report)?;
        assert_eq!(cached_output, fresh_output);
        assert_eq!(report.extgstate_duplicate_dictionaries_detected, 1);
        assert_eq!(report.extgstate_dictionary_references_canonicalized, 1);
        assert_eq!(
            report.extgstate_dictionary_references_canonicalized,
            fresh_report.extgstate_dictionary_references_canonicalized
        );
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
