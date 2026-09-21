#[cfg(test)]
use crate::analyze::analyze_pdf;
use crate::{
    Config, EditDocument, ImagePolicy, OptimizationReport, PdfAnalysis, Result,
    analyze::{analyze_document_for_optimization, input_sha256},
    content::normalize_page_contents_hayro,
    dedup::{
        canonicalize_appearance_streams_hayro, canonicalize_font_program_streams_hayro,
        canonicalize_form_xobjects_hayro, canonicalize_icc_profiles_hayro,
        canonicalize_image_xobjects_hayro, canonicalize_metadata_streams_hayro,
        canonicalize_page_contents_hayro, canonicalize_to_unicode_cmaps_hayro,
        canonicalize_type3_charprocs_hayro,
    },
    flate::{apply_flate_policy_hayro, compress_unfiltered_streams_hayro},
    font::{strip_font_editing_tables_hayro, union_sparse_cid_font_programs_after_dedup_hayro},
    hidden_text::{
        apply_hidden_text_policy_hayro, prune_physically_hidden_text_hayro,
        remove_large_diagonal_text_hayro,
    },
    images::{optimize_images_hayro, optimize_images_with_resize_targets_hayro},
    inline_images::externalize_duplicate_inline_images_hayro,
    microstroke::{MicrostrokeRasterStats, rasterize_pathological_microstrokes_hayro},
    preservation::{PreservationStats, apply_preservation_policy_hayro},
    print::{PrintPlanHayro, plan_print_downsampling_hayro},
    prune::{prune_resources_hayro, prune_resources_with_usage_hayro},
    raster_layout::normalize_raster_layout_hayro,
    repeated_page_objects::{
        remove_repeated_page_objects_hayro, repeated_page_objects_prefix_possible_hayro,
    },
    scrub::scrub_edit_document_cos_privacy,
    vector_compact::{compact_vector_paths_hayro, processing_factor_candidate},
};
use flpdf::{ImageOptimizationOptions, ImageOptimizationStats};
#[cfg(test)]
use flpdf::{ObjectStreamMode, Pdf, PdfWriter};
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
#[doc(hidden)]
pub fn analyze_microstroke_rasterization(
    input: &[u8],
    flate_level: i32,
) -> Result<MicrostrokeRasterStats> {
    if !(0..=9).contains(&flate_level) {
        return Err(crate::Error::Invalid(
            "Flate level must be an integer from 0 through 9".to_owned(),
        ));
    }
    let mut document = EditDocument::from_bytes(input.to_vec())?;
    rasterize_pathological_microstrokes_hayro(&mut document, flate_level)
}

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

/// Optimize `input`, reusing a previously computed analysis when it belongs
/// to these exact bytes. A missing/legacy digest or any byte-length/digest
/// mismatch falls back to a fresh analysis.
///
/// The supplied analysis is treated as trusted cached application state once
/// its input identity matches. Callers that accept analysis objects from an
/// untrusted boundary should keep their own trusted cached copy rather than
/// round-tripping mutable user data into this function.
pub fn optimize_pdf_with_analysis(
    input: &[u8],
    cfg: &Config,
    analysis: &PdfAnalysis,
) -> Result<(Vec<u8>, OptimizationReport)> {
    validate_config(cfg)?;
    let matches = analysis.input_bytes == input.len()
        && !analysis.input_sha256.is_empty()
        && analysis.input_sha256 == input_sha256(input);
    let mut timings = BTreeMap::new();
    let document = timed(&mut timings, "document-open", || {
        EditDocument::from_bytes(input.to_vec())
    })?;
    let before = if matches {
        analysis.clone()
    } else {
        timed(&mut timings, "analysis", || {
            analyze_document_for_optimization(input, &document)
        })?
    };
    optimize_pdf_with_document(document, cfg, before, timings)
}

fn optimize_pdf_with_document(
    mut document: EditDocument,
    cfg: &Config,
    before: PdfAnalysis,
    mut timings: BTreeMap<String, f64>,
) -> Result<(Vec<u8>, OptimizationReport)> {
    let preservation = timed(&mut timings, "preservation", || {
        if cfg.preservation == crate::PreservationConfig::functional() {
            Ok(PreservationStats::default())
        } else {
            apply_preservation_policy_hayro(&mut document, &cfg.preservation)
        }
    })?;
    let hidden_text = timed(&mut timings, "hidden-text", || {
        apply_hidden_text_policy_hayro(&mut document, &cfg.hidden_text)
    })?;
    let large_diagonal_text = timed(&mut timings, "large-diagonal-text", || {
        if cfg.remove_large_diagonal_text {
            remove_large_diagonal_text_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let scrub = timed(&mut timings, "privacy-scrub", || {
        scrub_edit_document_cos_privacy(&mut document, &cfg.privacy)
    })?;

    // Strip rendering-irrelevant editing/layout state before font-program
    // dedup so producer subsets can converge to the same program.
    let mut font_rendering = timed(&mut timings, "font-table-strip", || {
        if cfg.preservation.font_editing_support {
            Ok(Default::default())
        } else {
            strip_font_editing_tables_hayro(&mut document, cfg.flate_level)
        }
    })?;
    let metadata_dedup = timed(&mut timings, "metadata-dedup", || {
        if cfg.deduplicate_metadata_streams {
            canonicalize_metadata_streams_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let font_dedup = timed(&mut timings, "font-dedup", || {
        if cfg.deduplicate_font_programs {
            canonicalize_font_program_streams_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let font_sparse_union = timed(&mut timings, "font-sparse-union", || {
        if cfg.preservation.font_editing_support {
            Ok(Default::default())
        } else {
            union_sparse_cid_font_programs_after_dedup_hayro(&mut document, cfg.flate_level)
        }
    })?;
    font_rendering.programs_optimized += font_sparse_union.programs_optimized;
    font_rendering.programs_glyph_subset += font_sparse_union.programs_glyph_subset;
    font_rendering.original_encoded_bytes += font_sparse_union.original_encoded_bytes;
    font_rendering.optimized_encoded_bytes += font_sparse_union.optimized_encoded_bytes;
    font_rendering.decoded_table_bytes_removed += font_sparse_union.decoded_table_bytes_removed;
    font_rendering.glyph_outline_bytes_removed += font_sparse_union.glyph_outline_bytes_removed;
    let to_unicode_dedup = timed(&mut timings, "to-unicode-dedup", || {
        if cfg.deduplicate_to_unicode_cmaps {
            canonicalize_to_unicode_cmaps_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let icc_dedup = timed(&mut timings, "icc-dedup", || {
        if cfg.deduplicate_icc_profiles {
            canonicalize_icc_profiles_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let repeated_page_objects_may_rewrite = if cfg.remove_repeated_page_objects
        && cfg.optimization_goal == crate::OptimizationGoal::Processing
    {
        repeated_page_objects_prefix_possible_hayro(&document)?
    } else {
        false
    };
    let mut raster_vector_cache = BTreeMap::new();
    let raster_layout = timed(&mut timings, "raster-layout", || {
        let vector_cache = (cfg.optimization_goal == crate::OptimizationGoal::Processing
            && !repeated_page_objects_may_rewrite)
            .then_some(&mut raster_vector_cache);
        normalize_raster_layout_hayro(
            &mut document,
            &cfg.raster_layout,
            cfg.flate_level,
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
        timings.insert(format!("raster/{name}"), micros as f64 / 1000.0);
    }
    let physically_hidden_text = timed(&mut timings, "physical-hidden-text", || {
        if !cfg.raster_layout.enabled || !cfg.raster_layout.prune_hidden_paints {
            return Ok(Default::default());
        }
        if raster_layout.page_hidden_text_inventory_complete {
            let fallback = raster_layout
                .page_hidden_text_candidates
                .difference(&raster_layout.page_hidden_text_shared_complete)
                .copied()
                .collect::<BTreeSet<_>>();
            if fallback.is_empty() {
                Ok(Default::default())
            } else {
                prune_physically_hidden_text_hayro(&mut document, Some(&fallback))
            }
        } else {
            prune_physically_hidden_text_hayro(&mut document, None)
        }
    })?;

    let inline_image_dedup = timed(&mut timings, "inline-image-dedup", || {
        let raster_proved_no_inline = cfg.raster_layout.enabled
            && raster_layout.inline_inventory_complete
            && raster_layout.inline_occurrences_remaining == 0;
        if cfg.deduplicate_inline_images && !raster_proved_no_inline {
            externalize_duplicate_inline_images_hayro(
                &mut document,
                0,
                cfg.inline_image_min_duplicate_payload_bytes,
            )
        } else {
            Ok(Default::default())
        }
    })?;

    // Exact image canonicalization follows inline-image externalization so the
    // latter can converge with already-existing Image XObjects.
    let image_dedup = timed(&mut timings, "image-dedup", || {
        if cfg.deduplicate_image_xobjects {
            canonicalize_image_xobjects_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let form_dedup = timed(&mut timings, "form-dedup", || {
        if cfg.deduplicate_form_xobjects {
            canonicalize_form_xobjects_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let appearance_dedup = timed(&mut timings, "appearance-dedup", || {
        if cfg.deduplicate_appearance_streams {
            canonicalize_appearance_streams_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let type3_charproc_dedup = timed(&mut timings, "type3-dedup", || {
        if cfg.deduplicate_type3_charprocs {
            canonicalize_type3_charprocs_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;
    let repeated_page_objects = timed(&mut timings, "repeated-page-objects", || {
        if cfg.remove_repeated_page_objects {
            remove_repeated_page_objects_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;

    let print_plan_started = Instant::now();
    let (print_plan, print_plan_error) = match &cfg.image_policy {
        ImagePolicy::Print { target_ppi, .. } => {
            let target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
            match plan_print_downsampling_hayro(&document, u32::from(target_ppi)) {
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
                optimize_images_with_resize_targets_hayro(
                    &mut document,
                    ImageOptimizationOptions {
                        min_width: 0,
                        min_height: 0,
                        min_area: 0,
                        keep_inline_images: true,
                        jpeg_quality: *jpeg_quality,
                        flate_level: cfg.flate_level,
                        min_savings_bytes: 1,
                        min_savings_percent: *min_savings_percent,
                        ..ImageOptimizationOptions::default()
                    },
                    &print_plan.resize_targets,
                )?
            }
        }
        ImagePolicy::Perceptual {
            jpeg_quality,
            min_savings_percent,
            ..
        } => optimize_images_hayro(
            &mut document,
            ImageOptimizationOptions {
                keep_inline_images: true,
                jpeg_quality: *jpeg_quality,
                min_savings_bytes: 1,
                min_savings_percent: *min_savings_percent,
                ..ImageOptimizationOptions::default()
            },
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
            compact_vector_paths_hayro(
                &mut document,
                cfg.flate_level,
                cfg.optimization_goal,
                vector_cache,
            )
        } else {
            Ok(Default::default())
        }
    })?;

    let resource_prune = timed(&mut timings, "resource-prune", || {
        if !cfg.prune_resources {
            return Ok(Default::default());
        }
        let shared_usage_is_exact = cfg.raster_layout.enabled
            && raster_layout.resource_inventory_complete
            && physically_hidden_text.removed == 0
            && inline_image_dedup.occurrences_externalized == 0
            && repeated_page_objects.objects_removed == 0;
        if shared_usage_is_exact {
            prune_resources_with_usage_hayro(
                &mut document,
                &cfg.keep_unused_resources,
                &raster_layout.page_resource_names,
                &raster_layout.form_resource_names,
                &raster_layout.page_resource_names_by_type,
                &raster_layout.form_resource_names_by_type,
                &vector_compaction.generated_page_xobjects,
            )
        } else {
            prune_resources_hayro(&mut document, &cfg.keep_unused_resources)
        }
    })?;
    let microstroke_raster = timed(&mut timings, "microstroke-raster", || {
        if cfg.rasterize_excessive_small_vectors {
            rasterize_pathological_microstrokes_hayro(&mut document, cfg.flate_level)
        } else {
            Ok(Default::default())
        }
    })?;
    let flate = timed(&mut timings, "flate-policy", || {
        apply_flate_policy_hayro(&mut document, cfg.flate_policy, cfg.flate_level)
    })?;
    timed(&mut timings, "content-normalize", || -> Result<()> {
        if cfg.normalize_content_streams {
            normalize_page_contents_hayro(&mut document)?;
        }
        Ok(())
    })?;
    // Content streams are canonicalized only after every pass that can mutate
    // them, including lexical normalization.
    let page_content_dedup = timed(&mut timings, "page-content-dedup", || {
        if cfg.deduplicate_page_contents {
            canonicalize_page_contents_hayro(&mut document)
        } else {
            Ok(Default::default())
        }
    })?;

    // Match the historical writer's StreamDataMode::Compress policy explicitly
    // before handing the graph to the deliberately-simple fresh writer.
    timed(&mut timings, "compress-unfiltered", || {
        compress_unfiltered_streams_hayro(&mut document, cfg.flate_level)
    })?;
    let output = timed(&mut timings, "writer", || {
        crate::writer::write_pdf_with_options(
            &document,
            cfg.generate_object_streams,
            cfg.flate_level,
        )
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
            notes.push(format!(
                "Print placement analysis found {} raster image object(s) across {} use(s); {} exceed the {} PPI target, {} are conservative existing-JPEG candidates, {} are conservative Flate-encoded resize candidates, and {} were resized ({} JPEG, {} Flate-encoded).",
                print_plan.stats.images_placed,
                print_plan.stats.image_uses,
                print_plan.stats.downsample_candidates,
                target_ppi,
                print_plan.stats.existing_jpeg_resize_candidates,
                print_plan.stats.flate_resize_candidates,
                raster_transform.images_resized,
                raster_transform.jpeg_images_resized,
                raster_transform.flate_images_resized
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
        notes.push(format!(
            "Transcoded {} eligible raster image(s) to JPEG at quality {} and reduced their encoded payload by {} bytes.",
            raster_transform.images_optimized,
            jpeg_quality,
            raster_transform.saved_bytes()
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
            "Retain-GID subset {} CID TrueType font program(s), removing {} decoded glyph-outline bytes.",
            font_rendering.programs_glyph_subset,
            font_rendering.glyph_outline_bytes_removed
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
    if microstroke_raster.runs_rasterized > 0 {
        notes.push(format!(
            "Rasterized {} pathological micro-stroke run(s) across {} page(s), replacing {} individually painted strokes with compact binary image masks and saving about {} encoded bytes.",
            microstroke_raster.runs_rasterized,
            microstroke_raster.pages_rewritten,
            microstroke_raster.strokes_rasterized,
            microstroke_raster.estimated_flate_bytes_saved
        ));
    }
    if flate.streams_selected > 0 {
        notes.push(format!(
            "Selected {} lone-Flate stream(s) for recompression after measuring about {} bytes of encoded savings.",
            flate.streams_selected, flate.estimated_savings_bytes
        ));
    }

    let input_bytes = before.input_bytes;
    let saved_bytes = input_bytes as isize - output.len() as isize;
    let saved_percent = if input_bytes == 0 {
        0.0
    } else {
        saved_bytes as f64 * 100.0 / input_bytes as f64
    };
    let report = OptimizationReport {
        before,
        after_bytes: output.len(),
        saved_bytes,
        saved_percent,
        stage_timings_ms: timings,
        privacy_items_removed: scrub.removed,
        jpeg_metadata_bytes_removed: scrub.jpeg_metadata_bytes_removed,
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
        metadata_duplicate_streams_detected: metadata_dedup.duplicate_streams_detected,
        metadata_duplicate_raw_bytes: metadata_dedup.duplicate_raw_bytes,
        metadata_references_canonicalized: metadata_dedup.references_canonicalized,
        font_duplicate_streams_detected: font_dedup.duplicate_streams_detected,
        font_duplicate_raw_bytes: font_dedup.duplicate_raw_bytes,
        font_references_canonicalized: font_dedup.references_canonicalized,
        font_programs_rendering_optimized: font_rendering.programs_optimized,
        font_rendering_original_encoded_bytes: font_rendering.original_encoded_bytes,
        font_rendering_optimized_encoded_bytes: font_rendering.optimized_encoded_bytes,
        font_rendering_decoded_table_bytes_removed: font_rendering.decoded_table_bytes_removed,
        font_programs_glyph_subset: font_rendering.programs_glyph_subset,
        font_glyph_outline_bytes_removed: font_rendering.glyph_outline_bytes_removed,
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
        raster_relaxed_stencil_images_emitted: raster_layout.relaxed_stencil_images_emitted,
        exact_raster_rendering: cfg.raster_layout.exact_raster_rendering,
        raster_binary_image_encoded_bytes_saved: raster_layout.binary_image_encoded_bytes_saved,
        raster_deferred_tile_candidates: raster_layout.deferred_tile_candidates,
        raster_deferred_tile_paints_consumed: raster_layout.deferred_tile_paints_consumed,
        raster_staging_xobject_entries_removed: raster_layout.staging_xobject_entries_removed,
        resource_entries_pruned: resource_prune.entries_removed,
        resource_font_entries_pruned: resource_prune.font_entries_removed,
        resource_xobject_entries_pruned: resource_prune.xobject_entries_removed,
        resource_ext_gstate_entries_pruned: resource_prune.ext_gstate_entries_removed,
        resource_pattern_entries_pruned: resource_prune.pattern_entries_removed,
        resource_properties_entries_pruned: resource_prune.properties_entries_removed,
        resource_shading_entries_pruned: resource_prune.shading_entries_removed,
        microstroke_pages_rasterized: microstroke_raster.pages_rewritten,
        microstroke_runs_rasterized: microstroke_raster.runs_rasterized,
        microstroke_strokes_rasterized: microstroke_raster.strokes_rasterized,
        microstroke_image_payload_bytes: microstroke_raster.image_payload_bytes,
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
    use crate::Error;
    use flate2::{Compression, write::ZlibEncoder};
    use flpdf::ObjectHandle;
    use std::{io::Write, rc::Rc};

    fn perceptual_image_fixture() -> Result<Vec<u8>> {
        let width = 200_usize;
        let height = 200_usize;
        let mut state = 0x1234_5678_u32;
        let mut pixels = Vec::with_capacity(width * height);
        for _ in 0..width * height {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            pixels.push((state >> 24) as u8);
        }

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
        encoder.write_all(&pixels)?;
        let compressed = encoder.finish()?;

        let mut pdf = Pdf::empty()?;
        let catalog = pdf.root_handle()?;
        let pages = catalog.try_get_key(b"/Pages")?;
        pdf.resolve(&pages)?;

        let image = pdf.new_stream_with_data(Rc::new(compressed))?;
        let image_dict = image
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("fixture image has no stream dictionary".to_owned()))?;
        for (key, value) in [
            (b"/Type".as_slice(), ObjectHandle::name(b"XObject".to_vec())),
            (
                b"/Subtype".as_slice(),
                ObjectHandle::name(b"Image".to_vec()),
            ),
            (b"/Width".as_slice(), ObjectHandle::integer(width as i64)),
            (b"/Height".as_slice(), ObjectHandle::integer(height as i64)),
            (
                b"/ColorSpace".as_slice(),
                ObjectHandle::name(b"DeviceGray".to_vec()),
            ),
            (b"/BitsPerComponent".as_slice(), ObjectHandle::integer(8)),
            (
                b"/Filter".as_slice(),
                ObjectHandle::name(b"FlateDecode".to_vec()),
            ),
        ] {
            image_dict.replace_key(key, value)?;
        }
        pdf.mark_object_handle_dirty(&image_dict)?;

        let mut page_handles = Vec::new();
        for _ in 0..2 {
            let content =
                pdf.new_stream_with_data(Rc::new(b"q 24 0 0 24 0 0 cm /Im0 Do Q\n".to_vec()))?;
            let resources = ObjectHandle::dictionary(vec![(
                b"/XObject".to_vec(),
                ObjectHandle::dictionary(vec![(b"/Im0".to_vec(), image.clone())]),
            )]);
            let page = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
                (b"/Parent".to_vec(), pages.clone()),
                (
                    b"/MediaBox".to_vec(),
                    ObjectHandle::array(vec![
                        ObjectHandle::integer(0),
                        ObjectHandle::integer(0),
                        ObjectHandle::integer(width as i64),
                        ObjectHandle::integer(height as i64),
                    ]),
                ),
                (b"/Resources".to_vec(), resources),
                (b"/Contents".to_vec(), content),
            ]))?;
            pdf.mark_object_handle_dirty(&page)?;
            page_handles.push(page);
        }
        pages.replace_key(b"/Kids", ObjectHandle::array(page_handles))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(2))?;
        pdf.mark_object_handle_dirty(&pages)?;

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.set_object_stream_mode(ObjectStreamMode::Preserve);
        writer.write()?;
        Ok(writer.get_buffer()?)
    }

    fn repeated_inline_image_fixture() -> Result<Vec<u8>> {
        let mut pdf = Pdf::empty()?;
        let catalog = pdf.root_handle()?;
        let pages = catalog.try_get_key(b"/Pages")?;
        pdf.resolve(&pages)?;

        let payload = vec![b'A'; 600];
        let make_content = |copies: usize| {
            let mut bytes = Vec::new();
            for _ in 0..copies {
                bytes.extend_from_slice(b"q BI /W 600 /H 1 /BPC 8 /CS /G ID ");
                bytes.extend_from_slice(&payload);
                bytes.extend_from_slice(b" EI Q\n");
            }
            bytes
        };

        let mut page_handles = Vec::new();
        for copies in [2, 1] {
            let content = pdf.new_stream_with_data(Rc::new(make_content(copies)))?;
            let page = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
                (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
                (b"/Parent".to_vec(), pages.clone()),
                (
                    b"/MediaBox".to_vec(),
                    ObjectHandle::array(vec![
                        ObjectHandle::integer(0),
                        ObjectHandle::integer(0),
                        ObjectHandle::integer(600),
                        ObjectHandle::integer(100),
                    ]),
                ),
                (b"/Resources".to_vec(), ObjectHandle::dictionary(Vec::new())),
                (b"/Contents".to_vec(), content),
            ]))?;
            pdf.mark_object_handle_dirty(&page)?;
            page_handles.push(page);
        }
        pages.replace_key(b"/Kids", ObjectHandle::array(page_handles))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(2))?;
        pdf.mark_object_handle_dirty(&pages)?;

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.set_object_stream_mode(ObjectStreamMode::Preserve);
        writer.write()?;
        Ok(writer.get_buffer()?)
    }

    fn empty_then_drawing_fixture() -> Result<Vec<u8>> {
        let mut pdf = Pdf::empty()?;
        let catalog = pdf.root_handle()?;
        let pages = catalog.try_get_key(b"/Pages")?;
        pdf.resolve(&pages)?;

        let empty = pdf.new_stream_with_data(Rc::new(Vec::new()))?;
        let drawing = pdf.new_stream_with_data(Rc::new(b"0 0 m 10 10 l S\n".to_vec()))?;
        let page = pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![
            (b"/Type".to_vec(), ObjectHandle::name(b"Page".to_vec())),
            (b"/Parent".to_vec(), pages.clone()),
            (
                b"/MediaBox".to_vec(),
                ObjectHandle::array(vec![
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(100),
                    ObjectHandle::integer(100),
                ]),
            ),
            (b"/Resources".to_vec(), ObjectHandle::dictionary(Vec::new())),
            (
                b"/Contents".to_vec(),
                ObjectHandle::array(vec![empty, drawing]),
            ),
        ]))?;
        pdf.mark_object_handle_dirty(&page)?;
        pages.replace_key(b"/Kids", ObjectHandle::array(vec![page]))?;
        pages.replace_key(b"/Count", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&pages)?;

        let mut writer = PdfWriter::new(&mut pdf);
        writer.set_output_memory()?;
        writer.set_preserve_unreferenced_objects(false);
        writer.set_object_stream_mode(ObjectStreamMode::Preserve);
        writer.write()?;
        Ok(writer.get_buffer()?)
    }

    #[test]
    fn optimize_only_keeps_content_after_an_empty_page_stream() -> Result<()> {
        let input = empty_then_drawing_fixture()?;
        let (output, _) = optimize_pdf(&input, &Config::optimize_only())?;
        let mut pdf = Pdf::open_mem_owned(output)?;
        let page_ref = flpdf::pages::page_refs(&mut pdf)?[0];
        let page = pdf.get_object_handle(page_ref);
        pdf.resolve(&page)?;
        let contents = page.try_get_key(b"/Contents")?;
        pdf.resolve(&contents)?;
        let streams = contents.as_array().ok_or_else(|| {
            Error::Invalid("rewritten fixture /Contents is not an array".to_owned())
        })?;

        let mut saw_empty = false;
        let mut saw_drawing = false;
        for stream in streams {
            pdf.resolve(&stream)?;
            let dictionary = stream.as_stream_dict().ok_or_else(|| {
                Error::Invalid("rewritten fixture content item is not a stream".to_owned())
            })?;
            let raw = stream.get_raw_stream_data()?;
            let decoded = flpdf::filters::decode_stream_data(&dictionary, raw.as_ref())?;
            if decoded.is_empty() {
                saw_empty = true;
                assert!(dictionary.try_get_key(b"/Filter")?.is_null());
            }
            if decoded == b"0 0 m 10 10 l S\n" {
                saw_drawing = true;
            }
        }
        assert!(saw_empty);
        assert!(saw_drawing);
        Ok(())
    }

    #[test]
    fn matching_cached_analysis_is_reused() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let mut analysis = analyze_pdf(&input)?;
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
    fn mismatched_cached_analysis_falls_back_to_fresh_analysis() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let mut analysis = analyze_pdf(&input)?;
        analysis.input_sha256 = "00".repeat(32);
        analysis.warnings.push("stale-analysis-marker".to_owned());

        let (_, report) = optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis)?;
        assert_eq!(report.before.input_sha256, input_sha256(&input));
        assert!(
            !report
                .before
                .warnings
                .iter()
                .any(|warning| warning == "stale-analysis-marker")
        );
        Ok(())
    }

    #[test]
    fn legacy_cached_analysis_without_digest_falls_back_to_fresh_analysis() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let mut analysis = analyze_pdf(&input)?;
        analysis.input_sha256.clear();
        analysis.warnings.push("legacy-analysis-marker".to_owned());

        let (_, report) = optimize_pdf_with_analysis(&input, &Config::optimize_only(), &analysis)?;
        assert_eq!(report.before.input_sha256, input_sha256(&input));
        assert!(
            !report
                .before
                .warnings
                .iter()
                .any(|warning| warning == "legacy-analysis-marker")
        );
        Ok(())
    }

    #[test]
    fn optimize_only_externalizes_and_reuses_large_duplicate_inline_images() -> Result<()> {
        let input = repeated_inline_image_fixture()?;
        let before = analyze_pdf(&input)?;
        assert_eq!(before.inline_image_count, 3);
        assert!(before.duplicate_inline_image_payload_wasted_bytes >= 1024);

        let (output, report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert_eq!(report.inline_image_fingerprints_selected, 1);
        assert_eq!(report.inline_image_occurrences_externalized, 3);
        assert_eq!(report.inline_image_xobjects_created, 1);
        assert!(report.inline_image_duplicate_payload_bytes >= 1024);

        let after = analyze_pdf(&output)?;
        assert_eq!(after.inline_image_count, 0);
        assert_eq!(after.image_count, 1);
        assert_eq!(after.duplicate_image_payload_wasted_bytes, 0);
        Ok(())
    }

    #[test]
    fn perceptual_profile_transcodes_an_eligible_lossless_image() -> Result<()> {
        let input = perceptual_image_fixture()?;

        let (_, lossless_report) = optimize_pdf(&input, &Config::optimize_only())?;
        assert_eq!(lossless_report.raster_images_transcoded, 0);

        let (output, perceptual_report) = optimize_pdf(&input, &Config::perceptual())?;
        assert_eq!(perceptual_report.raster_images_transcoded, 1);
        assert_eq!(perceptual_report.raster_references_reused, 1);
        assert!(
            perceptual_report.raster_optimized_encoded_bytes
                < perceptual_report.raster_original_encoded_bytes
        );
        assert!(
            perceptual_report.raster_original_encoded_bytes
                - perceptual_report.raster_optimized_encoded_bytes
                >= perceptual_report.raster_original_encoded_bytes / 5
        );

        let analysis = analyze_pdf(&output)?;
        assert!(
            analysis
                .filter_counts
                .get("/DCTDecode")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        Ok(())
    }

    #[test]
    fn print_profile_downsamples_a_simple_flate_image_and_keeps_flate_encoding() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let mut config = Config::print();
        config.max_image_ppi = Some(450);
        let (output, report) = optimize_pdf(&input, &config)?;

        assert_eq!(report.print_images_placed, 1);
        assert_eq!(report.print_image_uses, 2);
        assert!(report.print_geometry_complete);
        assert_eq!(report.print_downsample_candidates, 1);
        assert_eq!(report.print_existing_jpeg_resize_candidates, 0);
        assert_eq!(report.print_flate_resize_candidates, 1);
        assert_eq!(report.raster_images_resized, 1);
        assert_eq!(report.raster_jpeg_images_resized, 0);
        assert_eq!(report.raster_flate_images_resized, 1);
        assert_eq!(report.raster_references_reused, 1);
        assert_eq!(report.raster_original_pixels, 200 * 200);
        assert_eq!(report.raster_optimized_pixels, 150 * 150);
        assert!(report.raster_optimized_encoded_bytes < report.raster_original_encoded_bytes);

        let analysis = analyze_pdf(&output)?;
        assert!(
            analysis
                .filter_counts
                .get("/FlateDecode")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        assert_eq!(
            analysis
                .filter_counts
                .get("/DCTDecode")
                .copied()
                .unwrap_or(0),
            0
        );

        let mut rewritten = Pdf::open_mem_owned(output)?;
        let page_ref = flpdf::pages::page_refs(&mut rewritten)?[0];
        let mut images = Vec::new();
        flpdf::PageObjectHelper::new(page_ref, &mut rewritten).for_each_image(
            false,
            |image, _, _| {
                images.push(image);
                Ok(())
            },
        )?;
        assert_eq!(images.len(), 1);
        let image = images.pop().ok_or_else(|| {
            Error::Invalid("rewritten Print fixture has no page image".to_owned())
        })?;
        let dictionary = image.as_stream_dict().ok_or_else(|| {
            Error::Invalid("rewritten Print image has no stream dictionary".to_owned())
        })?;
        assert_eq!(dictionary.try_get_key(b"/Width")?.as_integer(), Some(150));
        assert_eq!(dictionary.try_get_key(b"/Height")?.as_integer(), Some(150));
        assert!(
            dictionary
                .try_get_key(b"/Filter")?
                .try_is_name_and_equals(b"FlateDecode")?
        );
        let decode_params = dictionary.try_get_key(b"/DecodeParms")?;
        assert_eq!(
            decode_params.try_get_key(b"/Predictor")?.as_integer(),
            Some(12)
        );
        assert_eq!(
            decode_params.try_get_key(b"/Columns")?.as_integer(),
            Some(150)
        );
        let raw = image.get_raw_stream_data()?;
        assert_eq!(raw.len() as u64, report.raster_optimized_encoded_bytes);
        let decoded = flpdf::filters::decode_stream_data(&dictionary, raw.as_ref())?;
        assert_eq!(decoded.len(), 150 * 150);
        Ok(())
    }

    #[test]
    fn print_profile_rejects_zero_max_image_ppi() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let mut config = Config::print();
        config.max_image_ppi = Some(0);
        let error = match optimize_pdf(&input, &config) {
            Ok(_) => return Err(Error::Invalid("zero PPI was not rejected".to_owned())),
            Err(error) => error,
        };
        assert!(matches!(error, Error::Invalid(message) if message.contains("greater than zero")));
        Ok(())
    }

    #[test]
    fn print_profile_honors_max_image_ppi_override() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let mut config = Config::print();
        config.max_image_ppi = Some(300);
        let (_output, report) = optimize_pdf(&input, &config)?;

        assert_eq!(report.print_downsample_candidates, 1);
        assert_eq!(report.raster_images_resized, 1);
        assert_eq!(report.raster_original_pixels, 200 * 200);
        assert_eq!(report.raster_optimized_pixels, 100 * 100);
        Ok(())
    }

    #[test]
    fn print_profile_downsamples_an_oversampled_existing_jpeg_once() -> Result<()> {
        let input = perceptual_image_fixture()?;
        let (jpeg_input, _) = optimize_pdf(&input, &Config::perceptual())?;

        let mut config = Config::print();
        config.max_image_ppi = Some(450);
        let (output, report) = optimize_pdf(&jpeg_input, &config)?;
        assert_eq!(report.print_images_placed, 1);
        assert_eq!(report.print_image_uses, 2);
        assert!(report.print_geometry_complete);
        assert_eq!(report.print_downsample_candidates, 1);
        assert_eq!(report.print_existing_jpeg_resize_candidates, 1);
        assert_eq!(report.raster_images_resized, 1);
        assert_eq!(report.raster_references_reused, 1);
        assert_eq!(report.raster_original_pixels, 200 * 200);
        assert_eq!(report.raster_optimized_pixels, 150 * 150);
        assert!(report.raster_optimized_encoded_bytes < report.raster_original_encoded_bytes);

        let analysis = analyze_pdf(&output)?;
        assert!(
            analysis
                .filter_counts
                .get("/DCTDecode")
                .copied()
                .unwrap_or(0)
                >= 1
        );
        Ok(())
    }
}
