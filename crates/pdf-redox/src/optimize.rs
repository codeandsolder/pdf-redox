use crate::{
    Config, EditDocument, ImagePolicy, OptimizationReport, PdfAnalysis, Result,
    analyze::analyze_document_for_optimization,
    content::normalize_page_contents,
    dedup::{
        canonicalize_appearance_streams, canonicalize_exact_extgstate_dictionaries,
        canonicalize_exact_font_dictionaries, canonicalize_exact_structure_attribute_dictionaries,
        canonicalize_font_program_streams, canonicalize_form_xobjects, canonicalize_icc_profiles,
        canonicalize_image_xobjects, canonicalize_metadata_streams, canonicalize_page_contents,
        canonicalize_to_unicode_cmaps, canonicalize_type3_charprocs,
    },
    flate::{apply_flate_policy, compress_unfiltered_streams},
    font::{
        FontOptimizationStats, dense_compact_cidfont_type2_programs, strip_font_editing_tables,
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
    preservation::{PreservationStats, apply_preservation_policy},
    print::{PrintPlanHayro, plan_print_downsampling},
    prune::{ResourcePruneStats, prune_resources, prune_resources_with_usage},
    raster_layout::normalize_raster_layout,
    repeated_page_objects::{
        RepeatedPageObjectStats, remove_repeated_page_objects,
        repeated_page_objects_prefix_possible,
    },
    scrub::scrub_edit_document_cos_privacy,
    structure_compact::compact_structure,
    vector_compact::{VectorCompactionStats, compact_vector_paths, processing_factor_candidate},
};
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Instant,
};

fn timed<T>(timings: &mut BTreeMap<String, f64>, name: &str, f: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let value = f();
    timings.insert(name.to_owned(), started.elapsed().as_secs_f64() * 1000.0);
    value
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
    rasterize_pathological_microstrokes(&mut document, optimization_goal.flate_level())
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
    let before = timed(&mut timings, "analysis", || {
        analyze_document_for_optimization(input, &document)
    })?;
    optimize_pdf_with_document(document, cfg, before, timings)
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
    optimize_pdf_with_document(document, cfg, analysis.clone(), timings)
}

#[expect(
    clippy::too_many_lines,
    reason = "the optimization pipeline intentionally keeps stage ordering, shared document state, timings, and report accounting together"
)]
fn optimize_pdf_with_document(
    mut document: EditDocument,
    cfg: &Config,
    before: PdfAnalysis,
    mut timings: BTreeMap<String, f64>,
) -> Result<(Vec<u8>, OptimizationReport)> {
    let flate_level = cfg.optimization_goal.flate_level();
    let preservation = timed(&mut timings, "preservation", || {
        if cfg.preservation == crate::PreservationConfig::functional() {
            Ok(PreservationStats::default())
        } else {
            apply_preservation_policy(&mut document, &cfg.preservation)
        }
    })?;
    let hidden_text = timed(&mut timings, "hidden-text", || {
        apply_hidden_text_policy(&mut document, &cfg.hidden_text)
    })?;
    let large_diagonal_text = timed(&mut timings, "large-diagonal-text", || {
        if cfg.remove_large_diagonal_text {
            remove_large_diagonal_text(&mut document)
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
        if cfg.preservation.font_editing_support {
            Ok(FontOptimizationStats::default())
        } else {
            strip_font_editing_tables(&mut document, flate_level)
        }
    })?;
    let metadata_dedup = timed(&mut timings, "metadata-dedup", || {
        canonicalize_metadata_streams(&mut document)
    })?;
    let font_dedup = timed(&mut timings, "font-dedup", || {
        canonicalize_font_program_streams(&mut document)
    })?;
    let font_sparse_union = timed(&mut timings, "font-sparse-union", || {
        if cfg.preservation.font_editing_support {
            Ok(FontOptimizationStats::default())
        } else {
            union_sparse_cid_font_programs_after_dedup(&mut document, flate_level)
        }
    })?;
    font_rendering.programs_optimized += font_sparse_union.programs_optimized;
    font_rendering.programs_glyph_subset += font_sparse_union.programs_glyph_subset;
    font_rendering.original_encoded_bytes += font_sparse_union.original_encoded_bytes;
    font_rendering.optimized_encoded_bytes += font_sparse_union.optimized_encoded_bytes;
    font_rendering.decoded_table_bytes_removed += font_sparse_union.decoded_table_bytes_removed;
    font_rendering.glyph_subset_decoded_bytes_removed +=
        font_sparse_union.glyph_subset_decoded_bytes_removed;
    let font_dense = timed(&mut timings, "font-dense-gid", || {
        if cfg.preservation.font_editing_support
            || cfg.optimization_goal != crate::OptimizationGoal::Processing
        {
            Ok(FontOptimizationStats::default())
        } else {
            dense_compact_cidfont_type2_programs(&mut document, flate_level)
        }
    })?;
    let to_unicode_dedup = timed(&mut timings, "to-unicode-dedup", || {
        canonicalize_to_unicode_cmaps(&mut document)
    })?;
    let font_object_dedup = timed(&mut timings, "font-object-dedup", || {
        canonicalize_exact_font_dictionaries(&mut document)
    })?;
    let extgstate_object_dedup = timed(&mut timings, "extgstate-object-dedup", || {
        canonicalize_exact_extgstate_dictionaries(&mut document)
    })?;
    let structure_attribute_dedup = timed(&mut timings, "structure-attribute-dedup", || {
        canonicalize_exact_structure_attribute_dictionaries(&mut document)
    })?;
    let icc_dedup = timed(&mut timings, "icc-dedup", || {
        canonicalize_icc_profiles(&mut document)
    })?;
    let icc_alternate = timed(&mut timings, "icc-alternate-elision", || {
        if cfg.elide_icc_profiles_to_alternate {
            elide_icc_profiles_to_alternates(&mut document)
        } else {
            Ok(IccAlternateElisionStats::default())
        }
    })?;
    let repeated_page_objects_may_rewrite = if cfg.remove_repeated_page_objects
        && cfg.optimization_goal == crate::OptimizationGoal::Processing
    {
        repeated_page_objects_prefix_possible(&document)?
    } else {
        false
    };
    let mut raster_vector_cache = BTreeMap::new();
    let raster_layout = timed(&mut timings, "raster-layout", || {
        let vector_cache = (cfg.optimization_goal == crate::OptimizationGoal::Processing
            && !repeated_page_objects_may_rewrite)
            .then_some(&mut raster_vector_cache);
        normalize_raster_layout(&mut document, &cfg.raster_layout, flate_level, vector_cache)
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
    let physically_hidden_text = timed(&mut timings, "physical-hidden-text", || {
        if !cfg.raster_layout.enabled || !cfg.raster_layout.prune_hidden_paints {
            return Ok(HiddenTextApplyStats::default());
        }
        if raster_layout.page_hidden_text_inventory_complete {
            let fallback = raster_layout
                .page_hidden_text_candidates
                .difference(&raster_layout.page_hidden_text_shared_complete)
                .copied()
                .collect::<BTreeSet<_>>();
            if fallback.is_empty() {
                Ok(HiddenTextApplyStats::default())
            } else {
                prune_physically_hidden_text(&mut document, Some(&fallback))
            }
        } else {
            prune_physically_hidden_text(&mut document, None)
        }
    })?;

    let inline_image_dedup = timed(&mut timings, "inline-image-dedup", || {
        let raster_proved_no_inline = cfg.raster_layout.enabled
            && raster_layout.inline_inventory_complete
            && raster_layout.inline_occurrences_remaining == 0;
        if raster_proved_no_inline {
            Ok(DuplicateInlineImageStats::default())
        } else {
            externalize_duplicate_inline_images(
                &mut document,
                0,
                cfg.inline_image_min_duplicate_payload_bytes,
            )
        }
    })?;

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
    let form_dedup = timed(&mut timings, "form-dedup", || {
        canonicalize_form_xobjects(&mut document)
    })?;
    let appearance_dedup = timed(&mut timings, "appearance-dedup", || {
        canonicalize_appearance_streams(&mut document)
    })?;
    let type3_charproc_dedup = timed(&mut timings, "type3-dedup", || {
        canonicalize_type3_charprocs(&mut document)
    })?;
    let repeated_page_objects = timed(&mut timings, "repeated-page-objects", || {
        if cfg.remove_repeated_page_objects {
            remove_repeated_page_objects(&mut document)
        } else {
            Ok(RepeatedPageObjectStats::default())
        }
    })?;

    let print_plan_started = Instant::now();
    let (print_plan, print_plan_error) = match &cfg.image_policy {
        ImagePolicy::Print { target_ppi, .. } => {
            let target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
            match plan_print_downsampling(&document, u32::from(target_ppi)) {
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

    let vector_compaction = timed(&mut timings, "vector-compaction", || {
        let raster_proved_no_vector_candidate = cfg.optimization_goal
            != crate::OptimizationGoal::Processing
            && cfg.raster_layout.enabled
            && raster_layout.page_vector_inventory_complete
            && !raster_layout.page_vector_merge_candidate;
        let raster_cache_is_exact =
            physically_hidden_text.removed == 0 && inline_image_dedup.occurrences_externalized == 0;
        if raster_cache_is_exact {
            for page in &repeated_page_objects.rewritten_pages {
                raster_vector_cache.remove(page);
            }
        }
        let processing_proved_no_vector_candidate = if cfg.optimization_goal
            == crate::OptimizationGoal::Processing
            && cfg.raster_layout.enabled
            && raster_layout.page_vector_inventory_complete
            && raster_cache_is_exact
            && !raster_layout.page_vector_merge_candidate
        {
            raster_vector_cache.len() == raster_layout.page_count
                && !processing_factor_candidate(&raster_vector_cache)
        } else {
            false
        };
        if cfg.compact_vector_paths
            && !raster_proved_no_vector_candidate
            && !processing_proved_no_vector_candidate
        {
            let vector_cache = if raster_cache_is_exact {
                std::mem::take(&mut raster_vector_cache)
            } else {
                BTreeMap::new()
            };
            compact_vector_paths(
                &mut document,
                flate_level,
                cfg.optimization_goal,
                vector_cache,
            )
        } else {
            Ok(VectorCompactionStats::default())
        }
    })?;

    let marked_content = timed(&mut timings, "marked-content-coalesce", || {
        if cfg.compact_vector_paths && cfg.optimization_goal == crate::OptimizationGoal::Processing
        {
            coalesce_optional_content(&mut document, flate_level)
        } else {
            Ok(MarkedContentCoalesceStats::default())
        }
    })?;

    let collinear_paths = timed(&mut timings, "collinear-path-compact", || {
        if cfg.compact_vector_paths && cfg.optimization_goal == crate::OptimizationGoal::Processing
        {
            compact_collinear_paths(&mut document, flate_level)
        } else {
            Ok(CollinearPathStats::default())
        }
    })?;

    let outlined_glyphs = timed(&mut timings, "outlined-glyph-factor", || {
        if cfg.compact_vector_paths && cfg.optimization_goal == crate::OptimizationGoal::Processing
        {
            factor_outlined_glyphs(&mut document, flate_level)
        } else {
            Ok(OutlinedGlyphFactorStats::default())
        }
    })?;

    let paint_batch = timed(&mut timings, "paint-batching", || {
        if cfg.compact_vector_paths && cfg.optimization_goal == crate::OptimizationGoal::Processing
        {
            batch_page_paints(&mut document, flate_level)
        } else {
            Ok(PaintBatchStats::default())
        }
    })?;

    let stroke_forms = timed(&mut timings, "stroke-form-factor", || {
        if cfg.compact_vector_paths && cfg.optimization_goal == crate::OptimizationGoal::Processing
        {
            factor_repeated_stroke_forms(&mut document, flate_level)
        } else {
            Ok(StrokeFormFactorStats::default())
        }
    })?;

    let resource_prune = timed(&mut timings, "resource-prune", || {
        if !cfg.prune_resources {
            return Ok(ResourcePruneStats::default());
        }
        let shared_usage_is_exact = cfg.raster_layout.enabled
            && raster_layout.resource_inventory_complete
            && physically_hidden_text.removed == 0
            && inline_image_dedup.occurrences_externalized == 0
            && repeated_page_objects.objects_removed == 0
            && vector_compaction.shared_run_forms_created == 0
            && outlined_glyphs.fonts_created == 0
            && stroke_forms.forms_created == 0;
        if shared_usage_is_exact {
            prune_resources_with_usage(
                &mut document,
                &raster_layout.page_resource_names_by_type,
                &raster_layout.form_resource_names_by_type,
                &vector_compaction.generated_page_xobjects,
            )
        } else {
            prune_resources(&mut document)
        }
    })?;
    let microstroke_raster = timed(&mut timings, "microstroke-raster", || {
        if cfg.rasterize_excessive_small_vectors {
            rasterize_pathological_microstrokes(&mut document, flate_level)
        } else {
            Ok(MicrostrokeRasterStats::default())
        }
    })?;
    let flate = timed(&mut timings, "flate-policy", || {
        apply_flate_policy(&mut document, cfg.flate_policy, flate_level)
    })?;
    timed(&mut timings, "content-normalize", || -> Result<()> {
        if cfg.normalize_content_streams {
            normalize_page_contents(&mut document)?;
        }
        Ok(())
    })?;
    // Content streams are canonicalized only after every pass that can mutate
    // them, including lexical normalization.
    let page_content_dedup = timed(&mut timings, "page-content-dedup", || {
        canonicalize_page_contents(&mut document)
    })?;
    let structure_compaction = timed(&mut timings, "structure-compaction", || {
        compact_structure(&mut document)
    })?;

    // Match the historical writer's StreamDataMode::Compress policy explicitly
    // before handing the graph to the deliberately-simple fresh writer.
    let unfiltered_flate = timed(&mut timings, "compress-unfiltered", || {
        compress_unfiltered_streams(&mut document, flate_level)
    })?;
    let output = timed(&mut timings, "writer", || {
        crate::writer::write_pdf_with_object_streams(&document, flate_level)
    })?;

    let mut notes = Vec::new();
    if cfg.preservation != crate::PreservationConfig::functional() {
        notes.push(format!(
            "Applied semantic-preservation policy: {} page(s); flattened {} of {} annotation entries into page content, retained {} inert Link visual shell(s), and dropped {} unflattened annotation entries. Annotation subtypes seen: {:?}; unflattened subtypes before visual-shell pruning: {:?}. Dropped leaf-page dictionary keys: {:?}. Dropped intermediate page-tree keys: {:?}. Dropped source Catalog keys: {:?}. Dropped authoring metadata keys: {:?}.",
            preservation.pages,
            preservation.annotation_entries_flattened,
            preservation.annotation_entries_seen,
            preservation.link_visual_shells_retained,
            preservation.annotation_entries_dropped_unflattened,
            preservation.annotation_subtypes_seen,
            preservation.unflattened_annotation_subtypes,
            preservation.dropped_page_keys,
            preservation.dropped_page_tree_keys,
            preservation.dropped_catalog_keys,
            preservation.dropped_authoring_metadata_keys
        ));
        if preservation
            .dropped_catalog_keys
            .contains_key("/OCProperties")
        {
            notes.push(
                "Optional-content configuration (/OCProperties) was discarded by preservation policy; content whose default visibility depends on OCG state may render differently."
                    .to_owned(),
            );
        }
    }
    if let ImagePolicy::Print { target_ppi, .. } = &cfg.image_policy {
        let target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
        if let Some(error) = &print_plan_error {
            notes.push(format!(
                "Print placement analysis failed ({error}); resolution-aware raster resizing was disabled for this document."
            ));
        } else if print_plan.stats.geometry_complete {
            let images_placed = print_plan.stats.images_placed;
            let image_uses = print_plan.stats.image_uses;
            let downsample_candidates = print_plan.stats.downsample_candidates;
            let existing_jpeg_candidates = print_plan.stats.existing_jpeg_resize_candidates;
            let flate_candidates = print_plan.stats.flate_resize_candidates;
            let images_resized = raster_transform.images_resized;
            let jpeg_images_resized = raster_transform.jpeg_images_resized;
            let flate_images_resized = raster_transform.flate_images_resized;
            notes.push(format!(
                "Print placement analysis found {images_placed} raster image object(s) across {image_uses} use(s); {downsample_candidates} exceed the {target_ppi} PPI target, {existing_jpeg_candidates} are conservative existing-JPEG candidates, {flate_candidates} are conservative Flate-encoded resize candidates, and {images_resized} were resized ({jpeg_images_resized} JPEG, {flate_images_resized} Flate-encoded)."
            ));
        } else {
            notes.push(format!(
                "Print placement analysis was incomplete after finding {} raster image object(s) across {} use(s); resolution-aware raster resizing was disabled for this document.",
                print_plan.stats.images_placed, print_plan.stats.image_uses
            ));
        }
    }
    if raster_transform.images_optimized > 0
        && let ImagePolicy::Perceptual { jpeg_quality, .. } = &cfg.image_policy
    {
        let images_optimized = raster_transform.images_optimized;
        let saved_bytes = raster_transform.saved_bytes();
        notes.push(format!(
            "Transcoded {images_optimized} eligible raster image(s) to JPEG at quality {jpeg_quality} and reduced their encoded payload by {saved_bytes} bytes."
        ));
    }
    if jpeg_entropy.streams_optimized > 0 {
        notes.push(format!(
            "Entropy-optimized {} JPEG stream(s) without changing quantized DCT coefficients: {} -> {} encoded bytes ({} bytes saved).",
            jpeg_entropy.streams_optimized,
            jpeg_entropy.original_encoded_bytes,
            jpeg_entropy.optimized_encoded_bytes,
            jpeg_entropy.saved_bytes()
        ));
    }
    if repeated_page_objects.objects_removed > 0 {
        notes.push(format!(
            "Removed {} persistent page object(s) from {} repeated group(s) across {} page(s): {} text object(s), {} Image/Form paint(s).",
            repeated_page_objects.objects_removed,
            repeated_page_objects.groups_removed,
            repeated_page_objects.pages_rewritten,
            repeated_page_objects.text_objects_removed,
            repeated_page_objects.xobject_paints_removed
        ));
    }
    if inline_image_dedup.occurrences_externalized > 0 {
        notes.push(format!(
            "Externalized {} repeated inline-image occurrence(s) from {} exact semantic fingerprint(s) into {} shared Image XObject(s), representing {} duplicated encoded payload bytes.",
            inline_image_dedup.occurrences_externalized,
            inline_image_dedup.fingerprints_selected,
            inline_image_dedup.xobjects_created,
            inline_image_dedup.duplicate_payload_bytes
        ));
    }
    if before.incremental_update_count > 0 {
        notes.push(format!(
            "Fresh rewrite discarded {} incremental revision(s) from the source byte history.",
            before.incremental_update_count
        ));
    }
    if metadata_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate metadata reference(s) across {} duplicate stream object(s).",
            metadata_dedup.references_canonicalized, metadata_dedup.duplicate_streams_detected
        ));
    }
    if font_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate embedded-font reference(s) across {} duplicate font-program stream object(s).",
            font_dedup.references_canonicalized, font_dedup.duplicate_streams_detected
        ));
    }
    if font_object_dedup.duplicate_objects_detected > 0 {
        notes.push(format!(
            "Interned {} exact duplicate Font dictionary object(s), canonicalizing {} reference(s) without changing font semantics.",
            font_object_dedup.duplicate_objects_detected,
            font_object_dedup.references_canonicalized
        ));
    }
    if extgstate_object_dedup.duplicate_objects_detected > 0 {
        notes.push(format!(
            "Interned {} exact duplicate ExtGState dictionary object(s), canonicalizing {} resource reference(s).",
            extgstate_object_dedup.duplicate_objects_detected,
            extgstate_object_dedup.references_canonicalized
        ));
    }
    if structure_attribute_dedup.duplicate_objects_detected > 0 {
        notes.push(format!(
            "Interned {} exact duplicate structure-attribute dictionary object(s), canonicalizing {} StructElem /A reference(s).",
            structure_attribute_dedup.duplicate_objects_detected,
            structure_attribute_dedup.references_canonicalized
        ));
    }
    if to_unicode_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate ToUnicode reference(s) across {} duplicate CMap stream object(s).",
            to_unicode_dedup.references_canonicalized, to_unicode_dedup.duplicate_streams_detected
        ));
    }
    if image_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate Image XObject reference(s) across {} duplicate image stream object(s).",
            image_dedup.references_canonicalized, image_dedup.duplicate_streams_detected
        ));
    }
    if form_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate Form XObject reference(s) across {} duplicate form stream object(s).",
            form_dedup.references_canonicalized, form_dedup.duplicate_streams_detected
        ));
    }
    if appearance_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate annotation appearance reference(s) across {} duplicate Form stream object(s).",
            appearance_dedup.references_canonicalized,
            appearance_dedup.duplicate_streams_detected
        ));
    }
    if page_content_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate page-content reference(s) across {} duplicate content stream object(s).",
            page_content_dedup.references_canonicalized,
            page_content_dedup.duplicate_streams_detected
        ));
    }
    if type3_charproc_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate Type3 CharProc reference(s) across {} duplicate glyph stream object(s).",
            type3_charproc_dedup.references_canonicalized,
            type3_charproc_dedup.duplicate_streams_detected
        ));
    }
    if icc_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate ICCBased profile reference(s) across {} duplicate ICC stream object(s).",
            icc_dedup.references_canonicalized, icc_dedup.duplicate_streams_detected
        ));
    }
    if icc_alternate.references_rewritten > 0 {
        notes.push(format!(
            "Replaced {} ICCBased color-space value(s) with their declared Device alternate, making {} large ICC profile(s) unreachable and removing about {} encoded profile bytes. Color management is intentionally simplified.",
            icc_alternate.references_rewritten,
            icc_alternate.profiles_elided,
            icc_alternate.encoded_profile_bytes_elided
        ));
    }
    if font_rendering.programs_optimized > 0 {
        notes.push(format!(
            "Removed PDF-rendering-unused embedded-font editing/layout tables from {} font program(s): {} -> {} encoded bytes ({} decoded table bytes removed).",
            font_rendering.programs_optimized,
            font_rendering.original_encoded_bytes,
            font_rendering.optimized_encoded_bytes,
            font_rendering.decoded_table_bytes_removed
        ));
    }
    if font_rendering.programs_glyph_subset > 0 {
        notes.push(format!(
            "Subset {} embedded font program(s), removing about {} decoded font-program bytes.",
            font_rendering.programs_glyph_subset, font_rendering.glyph_subset_decoded_bytes_removed
        ));
    }
    if font_dense.programs_dense_remapped > 0 {
        notes.push(format!(
            "Densely remapped {} CIDFontType2 TrueType program(s), rewriting {} explicit CIDToGIDMap stream(s), removing {} dead glyph slot(s) and about {} decoded font bytes ({} -> {} encoded bytes including maps).",
            font_dense.programs_dense_remapped,
            font_dense.cid_to_gid_maps_rewritten,
            font_dense.dense_glyph_slots_removed,
            font_dense.dense_decoded_bytes_removed,
            font_dense.original_encoded_bytes,
            font_dense.optimized_encoded_bytes
        ));
    }
    if vector_compaction.path_forms_created > 0 {
        notes.push(format!(
            "Factored {} repeated painted path block(s) into Form XObjects across {} page(s), replacing {} occurrence(s) and removing about {} decoded duplicate bytes.",
            vector_compaction.path_forms_created,
            vector_compaction.path_form_pages_rewritten,
            vector_compaction.path_form_occurrences_replaced,
            vector_compaction.path_form_decoded_bytes_factored
        ));
    }
    if vector_compaction.transformed_forms_created > 0 {
        notes.push(format!(
            "Factored {} repeated affine-placed vector block(s) into Form XObjects across {} page(s), replacing {} occurrence(s), eliminating {} repeated operators, and saving about {} encoded bytes in page/Form streams.",
            vector_compaction.transformed_forms_created,
            vector_compaction.transformed_form_pages_rewritten,
            vector_compaction.transformed_form_occurrences_replaced,
            vector_compaction.transformed_form_operators_eliminated,
            vector_compaction.transformed_form_estimated_flate_bytes_saved
        ));
    }
    if vector_compaction.shared_run_forms_created > 0 {
        notes.push(format!(
            "Factored {} shared resource-aware q-block run(s) into Form XObjects across {} page(s), replacing {} block occurrence(s), removing about {} decoded duplicate bytes, and saving about {} encoded bytes.",
            vector_compaction.shared_run_forms_created,
            vector_compaction.shared_run_pages_rewritten,
            vector_compaction.shared_run_blocks_replaced,
            vector_compaction.shared_run_decoded_bytes_factored,
            vector_compaction.shared_run_estimated_flate_bytes_saved
        ));
    }
    if vector_compaction.path_coordinates_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} path-coordinate operand(s) across {} page(s) within a 0.0071 pt page-space error bound, removing about {} decoded bytes and saving about {} encoded bytes.",
            vector_compaction.path_coordinates_canonicalized,
            vector_compaction.path_coordinate_pages_rewritten,
            vector_compaction.path_coordinate_decoded_bytes_removed,
            vector_compaction.path_coordinate_estimated_flate_bytes_saved
        ));
    }
    if marked_content.boundaries_coalesced > 0 {
        notes.push(format!(
            "Coalesced {} redundant adjacent optional-content boundary pair(s) across {} page(s), removing about {} decoded bytes and saving about {} encoded bytes while preserving the OCG layer scopes.",
            marked_content.boundaries_coalesced,
            marked_content.pages_rewritten,
            marked_content.decoded_bytes_removed,
            marked_content.estimated_flate_bytes_saved
        ));
    }
    if collinear_paths.vertices_removed > 0 {
        notes.push(format!(
            "Removed {} exact forward-collinear line vertex/vertices across {} page(s), removing about {} decoded bytes and saving about {} encoded bytes without changing path geometry.",
            collinear_paths.vertices_removed,
            collinear_paths.pages_rewritten,
            collinear_paths.decoded_bytes_removed,
            collinear_paths.estimated_flate_bytes_saved
        ));
    }
    if outlined_glyphs.occurrences_replaced > 0 {
        notes.push(format!(
            "Recovered {} reusable outlined glyph shape(s) in {} synthetic Type3 font(s), replacing {} glyph occurrence(s) spanning {} source paint operation(s), removing about {} decoded bytes and saving about {} encoded bytes.",
            outlined_glyphs.glyphs_created,
            outlined_glyphs.fonts_created,
            outlined_glyphs.occurrences_replaced,
            outlined_glyphs.source_paints_replaced,
            outlined_glyphs.decoded_bytes_removed,
            outlined_glyphs.estimated_flate_bytes_saved
        ));
    }
    if paint_batch.paints_eliminated > 0 {
        notes.push(format!(
            "Collapsed {} source paint operation(s) into {} bounded compound paint group(s) across {} page(s), eliminating {} paint operations, removing about {} decoded bytes, and saving about {} encoded bytes.",
            paint_batch.source_paints_batched,
            paint_batch.groups_created,
            paint_batch.pages_rewritten,
            paint_batch.paints_eliminated,
            paint_batch.decoded_bytes_removed,
            paint_batch.estimated_flate_bytes_saved
        ));
    }
    if stroke_forms.forms_created > 0 {
        notes.push(format!(
            "Factored {} repeated independently-stroked path block occurrence(s) into {} translation-reused Form XObject(s) across {} page(s), factoring about {} decoded bytes and saving about {} encoded bytes.",
            stroke_forms.occurrences_replaced,
            stroke_forms.forms_created,
            stroke_forms.pages_rewritten,
            stroke_forms.decoded_bytes_factored,
            stroke_forms.estimated_flate_bytes_saved
        ));
    }
    if microstroke_raster.runs_rasterized > 0 {
        notes.push(format!(
            "Rasterized {} pathological micro-stroke run(s) across {} page(s), replacing {} individually painted strokes with compact binary image masks and saving about {} encoded bytes.",
            microstroke_raster.runs_rasterized,
            microstroke_raster.pages_rewritten,
            microstroke_raster.strokes_rasterized,
            microstroke_raster.estimated_flate_bytes_saved
        ));
    }
    if raster_layout.constant_color_mask_stencils_emitted > 0 {
        notes.push(format!(
            "Collapsed {} constant-color Image+binary-SMask pair(s) into single native-resolution stencil image(s).",
            raster_layout.constant_color_mask_stencils_emitted
        ));
    }
    if structure_compaction.page_tree_nodes_removed > 0
        || structure_compaction.name_tree_nodes_removed > 0
        || structure_compaction.named_destination_wrappers_inlined > 0
    {
        notes.push(format!(
            "Canonicalized document trees: page-tree nodes {} -> {}, name-tree nodes {} -> {}, and inlined {} trivial named-destination wrapper object(s).",
            structure_compaction.page_tree_nodes_before,
            structure_compaction.page_tree_nodes_after,
            structure_compaction.name_tree_nodes_before,
            structure_compaction.name_tree_nodes_after,
            structure_compaction.named_destination_wrappers_inlined
        ));
    }
    if flate.streams_selected > 0 {
        notes.push(format!(
            "Selected {} lone-Flate stream(s) for recompression after measuring about {} bytes of encoded savings.",
            flate.streams_selected, flate.estimated_savings_bytes
        ));
    }

    let adaptive_flate_streams_tested = flate
        .high_effort_streams_tested
        .saturating_add(unfiltered_flate.high_effort_streams_tested);
    let adaptive_flate_streams_selected = flate
        .high_effort_streams_selected
        .saturating_add(unfiltered_flate.high_effort_streams_selected);
    let adaptive_flate_extra_savings = flate
        .high_effort_extra_savings_bytes
        .saturating_add(unfiltered_flate.high_effort_extra_savings_bytes);
    if adaptive_flate_streams_selected > 0 {
        notes.push(format!(
            "Selected high-effort Flate for {adaptive_flate_streams_selected} of {adaptive_flate_streams_tested} highly-compressible large stream(s), saving about {adaptive_flate_extra_savings} additional encoded bytes beyond level {flate_level}."
        ));
    }

    let input_bytes = before.input_bytes;
    let saved_bytes = signed_size_delta(input_bytes, output.len());
    let saved_percent = savings_percent(saved_bytes, input_bytes);
    let reachability_cache = document.reachability_cache_stats();
    let report = OptimizationReport {
        before,
        after_bytes: output.len(),
        saved_bytes,
        saved_percent,
        stage_timings_ms: timings,
        reachability_queries: reachability_cache.queries,
        reachability_rebuilds: reachability_cache.rebuilds,
        reachability_cache_hits: reachability_cache.cache_hits,
        reachability_edge_checks: reachability_cache.edge_checks,
        reachability_edge_stable_reuses: reachability_cache.edge_stable_reuses,
        privacy_items_removed: scrub.removed,
        jpeg_metadata_bytes_removed: scrub.jpeg_metadata_bytes_removed,
        jpeg_entropy_streams_considered: jpeg_entropy.streams_considered,
        jpeg_entropy_streams_optimized: jpeg_entropy.streams_optimized,
        jpeg_entropy_original_encoded_bytes: jpeg_entropy.original_encoded_bytes,
        jpeg_entropy_optimized_encoded_bytes: jpeg_entropy.optimized_encoded_bytes,
        hidden_text_items_removed: hidden_text.removed,
        large_diagonal_text_objects_removed: large_diagonal_text.removed,
        repeated_page_object_groups_removed: repeated_page_objects.groups_removed,
        repeated_page_objects_removed: repeated_page_objects.objects_removed,
        repeated_page_text_objects_removed: repeated_page_objects.text_objects_removed,
        repeated_page_xobject_paints_removed: repeated_page_objects.xobject_paints_removed,
        repeated_page_object_pages_rewritten: repeated_page_objects.pages_rewritten,
        raster_hidden_text_items_pruned: physically_hidden_text
            .removed
            .saturating_add(raster_layout.shared_hidden_text_paints_pruned),
        vector_pages_compacted: vector_compaction.pages_compacted,
        vector_fill_groups_batched: vector_compaction.fill_groups_batched,
        vector_covered_fills_pruned: vector_compaction.covered_fills_pruned,
        vector_fill_paints_eliminated: vector_compaction.fill_paints_eliminated,
        vector_decoded_bytes_removed: vector_compaction.decoded_bytes_removed,
        vector_estimated_flate_bytes_saved: vector_compaction.estimated_flate_bytes_saved,
        vector_path_forms_created: vector_compaction.path_forms_created,
        vector_path_form_pages_rewritten: vector_compaction.path_form_pages_rewritten,
        vector_path_form_occurrences_replaced: vector_compaction.path_form_occurrences_replaced,
        vector_path_form_decoded_bytes_factored: vector_compaction.path_form_decoded_bytes_factored,
        vector_path_form_estimated_flate_bytes_saved: vector_compaction
            .path_form_estimated_flate_bytes_saved,
        vector_transformed_forms_created: vector_compaction.transformed_forms_created,
        vector_transformed_form_pages_rewritten: vector_compaction.transformed_form_pages_rewritten,
        vector_transformed_form_occurrences_replaced: vector_compaction
            .transformed_form_occurrences_replaced,
        vector_transformed_form_operators_eliminated: vector_compaction
            .transformed_form_operators_eliminated,
        vector_transformed_form_estimated_flate_bytes_saved: vector_compaction
            .transformed_form_estimated_flate_bytes_saved,
        vector_shared_run_forms_created: vector_compaction.shared_run_forms_created,
        vector_shared_run_pages_rewritten: vector_compaction.shared_run_pages_rewritten,
        vector_shared_run_blocks_replaced: vector_compaction.shared_run_blocks_replaced,
        vector_shared_run_decoded_bytes_factored: vector_compaction
            .shared_run_decoded_bytes_factored,
        vector_shared_run_estimated_flate_bytes_saved: vector_compaction
            .shared_run_estimated_flate_bytes_saved,
        vector_path_coordinates_canonicalized: vector_compaction.path_coordinates_canonicalized,
        vector_path_coordinate_pages_rewritten: vector_compaction.path_coordinate_pages_rewritten,
        vector_path_coordinate_decoded_bytes_removed: vector_compaction
            .path_coordinate_decoded_bytes_removed,
        vector_path_coordinate_estimated_flate_bytes_saved: vector_compaction
            .path_coordinate_estimated_flate_bytes_saved,
        marked_content_pages_rewritten: marked_content.pages_rewritten,
        marked_content_boundaries_coalesced: marked_content.boundaries_coalesced,
        marked_content_decoded_bytes_removed: marked_content.decoded_bytes_removed,
        marked_content_estimated_flate_bytes_saved: marked_content.estimated_flate_bytes_saved,
        collinear_path_pages_rewritten: collinear_paths.pages_rewritten,
        collinear_path_vertices_removed: collinear_paths.vertices_removed,
        collinear_path_decoded_bytes_removed: collinear_paths.decoded_bytes_removed,
        collinear_path_estimated_flate_bytes_saved: collinear_paths.estimated_flate_bytes_saved,
        outlined_glyph_pages_rewritten: outlined_glyphs.pages_rewritten,
        outlined_glyph_fonts_created: outlined_glyphs.fonts_created,
        outlined_glyph_shapes_created: outlined_glyphs.glyphs_created,
        outlined_glyph_occurrences_replaced: outlined_glyphs.occurrences_replaced,
        outlined_glyph_source_paints_replaced: outlined_glyphs.source_paints_replaced,
        outlined_glyph_decoded_bytes_removed: outlined_glyphs.decoded_bytes_removed,
        outlined_glyph_estimated_flate_bytes_saved: outlined_glyphs.estimated_flate_bytes_saved,
        paint_batch_pages_rewritten: paint_batch.pages_rewritten,
        paint_batch_groups_created: paint_batch.groups_created,
        paint_batch_source_paints_batched: paint_batch.source_paints_batched,
        paint_batch_paints_eliminated: paint_batch.paints_eliminated,
        paint_batch_decoded_bytes_removed: paint_batch.decoded_bytes_removed,
        paint_batch_estimated_flate_bytes_saved: paint_batch.estimated_flate_bytes_saved,
        stroke_form_pages_rewritten: stroke_forms.pages_rewritten,
        stroke_forms_created: stroke_forms.forms_created,
        stroke_form_occurrences_replaced: stroke_forms.occurrences_replaced,
        stroke_form_decoded_bytes_factored: stroke_forms.decoded_bytes_factored,
        stroke_form_estimated_flate_bytes_saved: stroke_forms.estimated_flate_bytes_saved,
        metadata_duplicate_streams_detected: metadata_dedup.duplicate_streams_detected,
        metadata_duplicate_raw_bytes: metadata_dedup.duplicate_raw_bytes,
        metadata_references_canonicalized: metadata_dedup.references_canonicalized,
        font_duplicate_streams_detected: font_dedup.duplicate_streams_detected,
        font_duplicate_raw_bytes: font_dedup.duplicate_raw_bytes,
        font_references_canonicalized: font_dedup.references_canonicalized,
        font_duplicate_dictionaries_detected: font_object_dedup.duplicate_objects_detected,
        font_dictionary_references_canonicalized: font_object_dedup.references_canonicalized,
        extgstate_duplicate_dictionaries_detected: extgstate_object_dedup
            .duplicate_objects_detected,
        extgstate_dictionary_references_canonicalized: extgstate_object_dedup
            .references_canonicalized,
        structure_attribute_duplicate_dictionaries_detected: structure_attribute_dedup
            .duplicate_objects_detected,
        structure_attribute_references_canonicalized: structure_attribute_dedup
            .references_canonicalized,
        font_programs_rendering_optimized: font_rendering.programs_optimized,
        font_rendering_original_encoded_bytes: font_rendering.original_encoded_bytes,
        font_rendering_optimized_encoded_bytes: font_rendering.optimized_encoded_bytes,
        font_rendering_decoded_table_bytes_removed: font_rendering.decoded_table_bytes_removed,
        font_programs_glyph_subset: font_rendering.programs_glyph_subset,
        font_glyph_subset_decoded_bytes_removed: font_rendering.glyph_subset_decoded_bytes_removed,
        font_dense_programs_remapped: font_dense.programs_dense_remapped,
        font_dense_cid_to_gid_maps_rewritten: font_dense.cid_to_gid_maps_rewritten,
        font_dense_glyph_slots_removed: font_dense.dense_glyph_slots_removed,
        font_dense_decoded_bytes_removed: font_dense.dense_decoded_bytes_removed,
        font_dense_original_encoded_bytes: font_dense.original_encoded_bytes,
        font_dense_optimized_encoded_bytes: font_dense.optimized_encoded_bytes,
        to_unicode_duplicate_streams_detected: to_unicode_dedup.duplicate_streams_detected,
        to_unicode_duplicate_raw_bytes: to_unicode_dedup.duplicate_raw_bytes,
        to_unicode_references_canonicalized: to_unicode_dedup.references_canonicalized,
        inline_image_fingerprints_selected: inline_image_dedup.fingerprints_selected,
        inline_image_occurrences_externalized: inline_image_dedup.occurrences_externalized,
        inline_image_xobjects_created: inline_image_dedup.xobjects_created,
        inline_image_xobject_references_reused: inline_image_dedup.xobject_references_reused,
        inline_image_duplicate_payload_bytes: inline_image_dedup.duplicate_payload_bytes,
        image_duplicate_streams_detected: image_dedup.duplicate_streams_detected,
        image_duplicate_raw_bytes: image_dedup.duplicate_raw_bytes,
        image_references_canonicalized: image_dedup.references_canonicalized,
        form_duplicate_streams_detected: form_dedup.duplicate_streams_detected,
        form_duplicate_raw_bytes: form_dedup.duplicate_raw_bytes,
        form_references_canonicalized: form_dedup.references_canonicalized,
        appearance_duplicate_streams_detected: appearance_dedup.duplicate_streams_detected,
        appearance_duplicate_raw_bytes: appearance_dedup.duplicate_raw_bytes,
        appearance_references_canonicalized: appearance_dedup.references_canonicalized,
        page_content_duplicate_streams_detected: page_content_dedup.duplicate_streams_detected,
        page_content_duplicate_raw_bytes: page_content_dedup.duplicate_raw_bytes,
        page_content_references_canonicalized: page_content_dedup.references_canonicalized,
        type3_charproc_duplicate_streams_detected: type3_charproc_dedup.duplicate_streams_detected,
        type3_charproc_duplicate_raw_bytes: type3_charproc_dedup.duplicate_raw_bytes,
        type3_charproc_references_canonicalized: type3_charproc_dedup.references_canonicalized,
        icc_duplicate_streams_detected: icc_dedup.duplicate_streams_detected,
        icc_duplicate_raw_bytes: icc_dedup.duplicate_raw_bytes,
        icc_references_canonicalized: icc_dedup.references_canonicalized,
        icc_alternate_profiles_eligible: icc_alternate.profiles_eligible,
        icc_alternate_profiles_elided: icc_alternate.profiles_elided,
        icc_alternate_references_rewritten: icc_alternate.references_rewritten,
        icc_alternate_encoded_bytes_elided: icc_alternate.encoded_profile_bytes_elided,
        raster_images_transcoded: raster_transform.images_optimized,
        raster_images_resized: raster_transform.images_resized,
        raster_jpeg_images_resized: raster_transform.jpeg_images_resized,
        raster_flate_images_resized: raster_transform.flate_images_resized,
        raster_references_reused: raster_transform.references_reused,
        raster_original_encoded_bytes: raster_transform.original_encoded_bytes,
        raster_optimized_encoded_bytes: raster_transform.optimized_encoded_bytes,
        raster_original_pixels: raster_transform.original_pixels,
        raster_optimized_pixels: raster_transform.optimized_pixels,
        raster_inline_fragmented_scopes_rewritten: raster_layout.inline.scopes_rewritten,
        raster_inline_occurrences_externalized: raster_layout.inline.occurrences_externalized,
        raster_pixel_clusters_reconstructed: raster_layout.pixel_clusters_reconstructed,
        raster_pixel_paints_reconstructed: raster_layout.pixel_paints_reconstructed,
        raster_native_fragment_groups_reconstructed: raster_layout
            .native_fragment_groups_reconstructed,
        raster_native_fragment_paints_reconstructed: raster_layout
            .native_fragment_paints_reconstructed,
        raster_stripe_groups_merged: raster_layout.stripe_groups_merged,
        raster_stripe_paints_merged: raster_layout.stripe_paints_merged,
        raster_masks_baked: raster_layout.masks_baked,
        raster_transparent_paints_pruned: raster_layout.transparent_paints_pruned,
        raster_occluded_paints_pruned: raster_layout.occluded_raster_paints_pruned,
        raster_transparent_margins_cropped: raster_layout.transparent_margins_cropped,
        raster_background_margins_cropped: raster_layout.background_margins_cropped,
        raster_cropped_pixels_removed: raster_layout.cropped_pixels_removed,
        raster_binary_images_packed: raster_layout.binary_images_packed,
        raster_binary_masks_packed: raster_layout.binary_masks_packed,
        raster_stencil_images_emitted: raster_layout.stencil_images_emitted,
        raster_constant_color_mask_stencils_emitted: raster_layout
            .constant_color_mask_stencils_emitted,
        raster_relaxed_stencil_images_emitted: raster_layout.relaxed_stencil_images_emitted,
        raster_bilevel_ccitt_images_emitted: raster_layout.bilevel_ccitt_images_emitted,
        raster_bilevel_flate_images_emitted: raster_layout.bilevel_flate_images_emitted,
        exact_raster_rendering: cfg.raster_layout.exact_raster_rendering,
        raster_binary_image_encoded_bytes_saved: raster_layout.binary_image_encoded_bytes_saved,
        raster_deferred_tile_candidates: raster_layout.deferred_tile_candidates,
        raster_deferred_tile_paints_consumed: raster_layout.deferred_tile_paints_consumed,
        raster_staging_xobject_entries_removed: raster_layout.staging_xobject_entries_removed,
        raster_rewritten_xobject_entries_removed: raster_layout.rewritten_xobject_entries_removed,
        resource_entries_pruned: resource_prune.entries_removed,
        resource_font_entries_pruned: resource_prune.font_entries_removed,
        resource_xobject_entries_pruned: resource_prune.xobject_entries_removed,
        resource_ext_gstate_entries_pruned: resource_prune.ext_gstate_entries_removed,
        resource_pattern_entries_pruned: resource_prune.pattern_entries_removed,
        resource_properties_entries_pruned: resource_prune.properties_entries_removed,
        resource_shading_entries_pruned: resource_prune.shading_entries_removed,
        page_tree_nodes_before: structure_compaction.page_tree_nodes_before,
        page_tree_nodes_after: structure_compaction.page_tree_nodes_after,
        page_tree_nodes_removed: structure_compaction.page_tree_nodes_removed,
        page_tree_pages_reparented: structure_compaction.page_tree_pages_reparented,
        name_trees_repacked: structure_compaction.name_trees_repacked,
        name_tree_nodes_before: structure_compaction.name_tree_nodes_before,
        name_tree_nodes_after: structure_compaction.name_tree_nodes_after,
        name_tree_nodes_removed: structure_compaction.name_tree_nodes_removed,
        named_destination_wrappers_inlined: structure_compaction.named_destination_wrappers_inlined,
        microstroke_pages_rasterized: microstroke_raster.pages_rewritten,
        microstroke_runs_rasterized: microstroke_raster.runs_rasterized,
        microstroke_strokes_rasterized: microstroke_raster.strokes_rasterized,
        microstroke_image_payload_bytes: microstroke_raster.image_payload_bytes,
        microstroke_ccitt_images: microstroke_raster.ccitt_images,
        microstroke_flate_images: microstroke_raster.flate_images,
        microstroke_estimated_flate_bytes_saved: microstroke_raster.estimated_flate_bytes_saved,
        print_images_placed: print_plan.stats.images_placed,
        print_image_uses: print_plan.stats.image_uses,
        print_geometry_complete: print_plan.stats.geometry_complete,
        print_downsample_candidates: print_plan.stats.downsample_candidates,
        print_existing_jpeg_resize_candidates: print_plan.stats.existing_jpeg_resize_candidates,
        print_flate_resize_candidates: print_plan.stats.flate_resize_candidates,
        print_source_pixels: print_plan.stats.source_pixels,
        print_target_pixels: print_plan.stats.target_pixels,
        flate_streams_selected_for_recompression: flate.streams_selected,
        flate_estimated_savings_bytes: flate.estimated_savings_bytes,
        flate_high_effort_streams_tested: adaptive_flate_streams_tested,
        flate_high_effort_streams_selected: adaptive_flate_streams_selected,
        flate_high_effort_extra_savings_bytes: adaptive_flate_extra_savings,
        preservation_pages: preservation.pages,
        preservation_annotation_entries_seen: preservation.annotation_entries_seen,
        preservation_annotation_entries_flattened: preservation.annotation_entries_flattened,
        preservation_annotation_entries_dropped_unflattened: preservation
            .annotation_entries_dropped_unflattened,
        preservation_link_visual_shells_retained: preservation.link_visual_shells_retained,
        preservation_annotation_subtypes_seen: preservation.annotation_subtypes_seen,
        preservation_unflattened_annotation_subtypes: preservation.unflattened_annotation_subtypes,
        preservation_dropped_page_keys: preservation.dropped_page_keys,
        preservation_dropped_page_tree_keys: preservation.dropped_page_tree_keys,
        preservation_dropped_catalog_keys: preservation.dropped_catalog_keys,
        preservation_spliced_unknown_wrapper_keys: preservation.spliced_unknown_wrapper_keys,
        notes,
    };
    Ok((output, report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, SourcePdf};

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
    fn matching_cached_analysis_is_reused() -> Result<()> {
        let input = corrupt_xref_with_compressed_page_tree_fixture()?;
        let mut analysis =
            analyze_document_for_optimization(&input, &EditDocument::from_bytes(input.clone())?)?;
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
            analyze_document_for_optimization(&input, &EditDocument::from_bytes(input.clone())?)?;
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
