use super::{
    BTreeMap, Config, EditDocument, ImagePolicy, MAX_POLYLINE_PAGE_ERROR_PT, OptimizationReport,
    OptimizationRunStats, PdfAnalysis, savings_percent, signed_size_delta,
};

impl OptimizationRunStats {
    #[expect(
        clippy::too_many_lines,
        reason = "human-readable optimization notes intentionally cover every independently reported transform"
    )]
    fn notes(&self, cfg: &Config, before: &PdfAnalysis) -> Vec<String> {
        let mut notes = Vec::new();
        if !self.preservation_fast_path {
            notes.push(format!(
                "Applied semantic-preservation policy: {} page(s); flattened {} of {} annotation entries into page content, retained {} inert Link visual shell(s), and dropped {} unflattened annotation entries. Annotation subtypes seen: {:?}; unflattened subtypes before visual-shell pruning: {:?}. Dropped leaf-page dictionary keys: {:?}. Dropped intermediate page-tree keys: {:?}. Dropped source Catalog keys: {:?}. Dropped authoring/metadata keys: {:?}.",
                self.preservation.pages,
                self.preservation.annotation_entries_flattened,
                self.preservation.annotation_entries_seen,
                self.preservation.link_visual_shells_retained,
                self.preservation.annotation_entries_dropped_unflattened,
                self.preservation.annotation_subtypes_seen,
                self.preservation.unflattened_annotation_subtypes,
                self.preservation.dropped_page_keys,
                self.preservation.dropped_page_tree_keys,
                self.preservation.dropped_catalog_keys,
                self.preservation.dropped_authoring_keys
            ));
            if self
                .preservation
                .dropped_catalog_keys
                .contains_key("/OCProperties")
            {
                notes.push(
                    "Optional-content configuration (/OCProperties) was discarded by preservation policy; content whose default visibility depends on OCG state may render differently."
                        .to_owned(),
                );
            }
        }
        if self.preservation_fast_path {
            let thumbnails = self
                .preservation
                .dropped_page_keys
                .get("/Thumb")
                .copied()
                .unwrap_or_default();
            if thumbnails > 0 {
                notes.push(format!(
                    "Dropped {thumbnails} embedded page thumbnail cache entr{}; viewers can regenerate previews from final page content when needed.",
                    if thumbnails == 1 { "y" } else { "ies" }
                ));
            }
            let private_authoring = self
                .preservation
                .dropped_authoring_keys
                .get("/PieceInfo")
                .copied()
                .unwrap_or_default();
            if private_authoring > 0 {
                notes.push(format!(
                    "Dropped {private_authoring} producer-private /PieceInfo entr{}; use --preserve-authoring-data when editor round-tripping is explicitly required.",
                    if private_authoring == 1 { "y" } else { "ies" }
                ));
            }
        }
        if let ImagePolicy::Print { target_ppi, .. } = &cfg.image_policy {
            let target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
            if let Some(error) = &self.print_plan_error {
                notes.push(format!(
                    "Print placement analysis failed ({error}); resolution-aware raster resizing was disabled for this document."
                ));
            } else if self.print_plan.stats.geometry_complete {
                let images_placed = self.print_plan.stats.images_placed;
                let image_uses = self.print_plan.stats.image_uses;
                let downsample_candidates = self.print_plan.stats.downsample_candidates;
                let existing_jpeg_candidates =
                    self.print_plan.stats.existing_jpeg_resize_candidates;
                let flate_candidates = self.print_plan.stats.flate_resize_candidates;
                let images_resized = self.raster_transform.images_resized;
                let jpeg_images_resized = self.raster_transform.jpeg_images_resized;
                let flate_images_resized = self.raster_transform.flate_images_resized;
                notes.push(format!(
                    "Print placement analysis found {images_placed} raster image object(s) across {image_uses} use(s); {downsample_candidates} exceed the {target_ppi} PPI target, {existing_jpeg_candidates} are conservative existing-JPEG candidates, {flate_candidates} are conservative Flate-encoded resize candidates, and {images_resized} were resized ({jpeg_images_resized} JPEG, {flate_images_resized} Flate-encoded)."
                ));
            } else {
                notes.push(format!(
                    "Print placement analysis was incomplete after finding {} raster image object(s) across {} use(s); resolution-aware raster resizing was disabled for this document.",
                    self.print_plan.stats.images_placed, self.print_plan.stats.image_uses
                ));
            }
        }
        if self.raster_transform.images_optimized > 0
            && let ImagePolicy::Perceptual { jpeg_quality, .. } = &cfg.image_policy
        {
            let images_optimized = self.raster_transform.images_optimized;
            let paper_indexed_images = self.raster_transform.paper_indexed_images;
            let layered_indexed_images = self.raster_transform.paper_indexed_layered_images;
            let residual_indexed_images = self.raster_transform.paper_indexed_residual_images;
            let jpeg_images = images_optimized.saturating_sub(paper_indexed_images);
            let saved_bytes = self.raster_transform.saved_bytes();
            notes.push(format!(
                "Transformed {images_optimized} eligible raster image(s): {jpeg_images} to JPEG at quality {jpeg_quality} and {paper_indexed_images} paper-like RGB image(s) to Indexed color ({layered_indexed_images} using source-grid local-palette layers, {residual_indexed_images} using sparse residual overlays); reduced their encoded payload by {saved_bytes} bytes."
            ));
        }
        if self.jpeg_entropy.streams_optimized > 0 {
            notes.push(format!(
                "Entropy-optimized {} JPEG stream(s) without changing quantized DCT coefficients: {} -> {} encoded bytes ({} bytes saved).",
                self.jpeg_entropy.streams_optimized,
                self.jpeg_entropy.original_encoded_bytes,
                self.jpeg_entropy.optimized_encoded_bytes,
                self.jpeg_entropy.saved_bytes()
            ));
        }
        if self.repeated_page_objects.objects_removed > 0 {
            notes.push(format!(
                "Removed {} persistent page object(s) from {} repeated group(s) across {} page(s): {} text object(s), {} Image/Form paint(s).",
                self.repeated_page_objects.objects_removed,
                self.repeated_page_objects.groups_removed,
                self.repeated_page_objects.pages_rewritten,
                self.repeated_page_objects.text_objects_removed,
                self.repeated_page_objects.xobject_paints_removed
            ));
        }
        if self.inline_image_dedup.occurrences_externalized > 0 {
            notes.push(format!(
                "Externalized {} repeated inline-image occurrence(s) from {} exact semantic fingerprint(s) into {} shared Image XObject(s), representing {} duplicated encoded payload bytes.",
                self.inline_image_dedup.occurrences_externalized,
                self.inline_image_dedup.fingerprints_selected,
                self.inline_image_dedup.xobjects_created,
                self.inline_image_dedup.duplicate_payload_bytes
            ));
        }
        if before.incremental_update_count > 0 {
            notes.push(format!(
                "Fresh rewrite discarded {} incremental revision(s) from the source byte history.",
                before.incremental_update_count
            ));
        }
        if self.metadata_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate metadata reference(s) across {} duplicate stream object(s).",
                self.metadata_dedup.references_canonicalized, self.metadata_dedup.duplicate_streams_detected
            ));
        }
        if self.font_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate embedded-font reference(s) across {} duplicate font-program stream object(s).",
                self.font_dedup.references_canonicalized, self.font_dedup.duplicate_streams_detected
            ));
        }
        if self.font_object_dedup.duplicate_objects_detected > 0 {
            notes.push(format!(
                "Interned {} exact duplicate Font dictionary object(s), canonicalizing {} reference(s) without changing font semantics.",
                self.font_object_dedup.duplicate_objects_detected,
                self.font_object_dedup.references_canonicalized
            ));
        }
        if self.extgstate_object_dedup.duplicate_objects_detected > 0 {
            notes.push(format!(
                "Interned {} exact duplicate ExtGState dictionary object(s), canonicalizing {} resource reference(s).",
                self.extgstate_object_dedup.duplicate_objects_detected,
                self.extgstate_object_dedup.references_canonicalized
            ));
        }
        if self.structure_attribute_dedup.duplicate_objects_detected > 0 {
            notes.push(format!(
                "Interned {} exact duplicate structure-attribute dictionary object(s), canonicalizing {} StructElem /A reference(s).",
                self.structure_attribute_dedup.duplicate_objects_detected,
                self.structure_attribute_dedup.references_canonicalized
            ));
        }
        if self.sampled_function_dedup.duplicate_streams_detected > 0 {
            notes.push(format!(
                "Interned {} exact duplicate sampled Function stream object(s), canonicalizing {} reference(s) and removing about {} duplicate encoded payload bytes.",
                self.sampled_function_dedup.duplicate_streams_detected,
                self.sampled_function_dedup.references_canonicalized,
                self.sampled_function_dedup.duplicate_raw_bytes
            ));
        }
        if self.to_unicode_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate ToUnicode reference(s) across {} duplicate CMap stream object(s).",
                self.to_unicode_dedup.references_canonicalized, self.to_unicode_dedup.duplicate_streams_detected
            ));
        }
        if self.image_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate Image XObject reference(s) across {} duplicate image stream object(s).",
                self.image_dedup.references_canonicalized, self.image_dedup.duplicate_streams_detected
            ));
        }
        if self.form_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate Form XObject reference(s) across {} duplicate form stream object(s).",
                self.form_dedup.references_canonicalized, self.form_dedup.duplicate_streams_detected
            ));
        }
        if self.form_metadata_payload_factor.wrappers_rewritten > 0 {
            notes.push(format!(
                "Factored {} metadata-variant Form payload group(s) behind {} metadata-preserving wrapper(s), sharing about {} encoded payload bytes before wrapper overhead.",
                self.form_metadata_payload_factor.groups_factored,
                self.form_metadata_payload_factor.wrappers_rewritten,
                self.form_metadata_payload_factor.duplicate_raw_bytes_factored
            ));
        }
        if self.appearance_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate annotation appearance reference(s) across {} duplicate Form stream object(s).",
                self.appearance_dedup.references_canonicalized,
                self.appearance_dedup.duplicate_streams_detected
            ));
        }
        if self.page_content_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate page-content reference(s) across {} duplicate content stream object(s).",
                self.page_content_dedup.references_canonicalized,
                self.page_content_dedup.duplicate_streams_detected
            ));
        }
        if self.type3_charproc_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate Type3 CharProc reference(s) across {} duplicate glyph stream object(s).",
                self.type3_charproc_dedup.references_canonicalized,
                self.type3_charproc_dedup.duplicate_streams_detected
            ));
        }
        if self.icc_dedup.references_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} duplicate ICCBased profile reference(s) across {} duplicate ICC stream object(s).",
                self.icc_dedup.references_canonicalized, self.icc_dedup.duplicate_streams_detected
            ));
        }
        if self.icc_alternate.references_rewritten > 0 {
            notes.push(format!(
                "Replaced {} ICCBased color-space value(s) with their declared Device alternate, making {} large ICC profile(s) unreachable and removing about {} encoded profile bytes. Color management is intentionally simplified.",
                self.icc_alternate.references_rewritten,
                self.icc_alternate.profiles_elided,
                self.icc_alternate.encoded_profile_bytes_elided
            ));
        }
        if self.font_rendering.programs_optimized > 0 {
            notes.push(format!(
                "Removed PDF-rendering-unused embedded-font editing/layout tables from {} font program(s): {} -> {} encoded bytes ({} decoded table bytes removed).",
                self.font_rendering.programs_optimized,
                self.font_rendering.original_encoded_bytes,
                self.font_rendering.optimized_encoded_bytes,
                self.font_rendering.decoded_table_bytes_removed
            ));
        }
        if self.font_cid_cmap_strip.cid_cmaps_removed > 0 {
            notes.push(format!(
                "Removed redundant internal cmap table(s) from {} CIDFontType2-only TrueType program(s): {} -> {} encoded bytes ({} decoded cmap bytes removed).",
                self.font_cid_cmap_strip.cid_cmaps_removed,
                self.font_cid_cmap_strip.original_encoded_bytes,
                self.font_cid_cmap_strip.optimized_encoded_bytes,
                self.font_cid_cmap_strip.cid_cmap_decoded_bytes_removed
            ));
        }
        if self.font_rendering.programs_glyph_subset > 0 {
            notes.push(format!(
                "Subset {} embedded font program(s), removing about {} decoded font-program bytes.",
                self.font_rendering.programs_glyph_subset,
                self.font_rendering.glyph_subset_decoded_bytes_removed
            ));
        }
        if self.font_dense.programs_dense_remapped > 0 {
            notes.push(format!(
                "Densely remapped {} CIDFontType2 TrueType program(s), rewriting {} explicit CIDToGIDMap stream(s), removing {} dead glyph slot(s) and about {} decoded font bytes ({} -> {} encoded bytes including maps).",
                self.font_dense.programs_dense_remapped,
                self.font_dense.cid_to_gid_maps_rewritten,
                self.font_dense.dense_glyph_slots_removed,
                self.font_dense.dense_decoded_bytes_removed,
                self.font_dense.original_encoded_bytes,
                self.font_dense.optimized_encoded_bytes
            ));
        }
        if self.vector_compaction.path_forms_created > 0 {
            notes.push(format!(
                "Factored {} repeated painted path block(s) into Form XObjects across {} page(s), replacing {} occurrence(s) and removing about {} decoded duplicate bytes.",
                self.vector_compaction.path_forms_created,
                self.vector_compaction.path_form_pages_rewritten,
                self.vector_compaction.path_form_occurrences_replaced,
                self.vector_compaction.path_form_decoded_bytes_factored
            ));
        }
        if self.vector_compaction.transformed_forms_created > 0 {
            notes.push(format!(
                "Factored {} repeated affine-placed vector block(s) into Form XObjects across {} page(s), replacing {} occurrence(s), eliminating {} repeated operators, and saving about {} encoded bytes in page/Form streams.",
                self.vector_compaction.transformed_forms_created,
                self.vector_compaction.transformed_form_pages_rewritten,
                self.vector_compaction.transformed_form_occurrences_replaced,
                self.vector_compaction.transformed_form_operators_eliminated,
                self.vector_compaction.transformed_form_estimated_flate_bytes_saved
            ));
        }
        if self.vector_compaction.shared_run_forms_created > 0 {
            notes.push(format!(
                "Factored {} shared resource-aware q-block run(s) into Form XObjects across {} page(s), replacing {} block occurrence(s), removing about {} decoded duplicate bytes, and saving about {} encoded bytes.",
                self.vector_compaction.shared_run_forms_created,
                self.vector_compaction.shared_run_pages_rewritten,
                self.vector_compaction.shared_run_blocks_replaced,
                self.vector_compaction.shared_run_decoded_bytes_factored,
                self.vector_compaction.shared_run_estimated_flate_bytes_saved
            ));
        }
        if self.repeated_clips.blocks_hoisted > 0 {
            let pages_rewritten = self.repeated_clips.pages_rewritten;
            let runs_hoisted = self.repeated_clips.runs_hoisted;
            let blocks_hoisted = self.repeated_clips.blocks_hoisted;
            let decoded_bytes_removed = self.repeated_clips.decoded_bytes_removed;
            let estimated_flate_bytes_saved = self.repeated_clips.estimated_flate_bytes_saved;
            notes.push(format!(
                "Hoisted {runs_hoisted} repeated clipping-path run(s) across {pages_rewritten} page(s), collapsing {blocks_hoisted} q-block clip prefixes, removing about {decoded_bytes_removed} decoded bytes and saving about {estimated_flate_bytes_saved} encoded bytes."
            ));
        }
        if self.vector_compaction.path_coordinates_canonicalized > 0 {
            notes.push(format!(
                "Canonicalized {} path-coordinate operand(s) across {} page(s) within a 0.0071 pt page-space error bound, removing about {} decoded bytes and saving about {} encoded bytes.",
                self.vector_compaction.path_coordinates_canonicalized,
                self.vector_compaction.path_coordinate_pages_rewritten,
                self.vector_compaction.path_coordinate_decoded_bytes_removed,
                self.vector_compaction.path_coordinate_estimated_flate_bytes_saved
            ));
        }
        if self.polyline_simplification.vertices_removed > 0 {
            let vertices_removed = self.polyline_simplification.vertices_removed;
            let pages_rewritten = self.polyline_simplification.pages_rewritten;
            let decoded_bytes_removed = self.polyline_simplification.decoded_bytes_removed;
            let estimated_flate_bytes_saved =
                self.polyline_simplification.estimated_flate_bytes_saved;
            notes.push(format!(
                "Simplified {vertices_removed} oversampled filled-polyline vertex/vertices across {pages_rewritten} page(s) within a {MAX_POLYLINE_PAGE_ERROR_PT:.3} pt page-space deviation bound, removing about {decoded_bytes_removed} decoded bytes and saving about {estimated_flate_bytes_saved} encoded bytes."
            ));
        }
        if self.marked_content.boundaries_coalesced > 0 {
            notes.push(format!(
                "Coalesced {} redundant adjacent optional-content boundary pair(s) across {} page(s), removing about {} decoded bytes and saving about {} encoded bytes while preserving the OCG layer scopes.",
                self.marked_content.boundaries_coalesced,
                self.marked_content.pages_rewritten,
                self.marked_content.decoded_bytes_removed,
                self.marked_content.estimated_flate_bytes_saved
            ));
        }
        if self.collinear_paths.vertices_removed > 0 {
            notes.push(format!(
                "Removed {} exact forward-collinear line vertex/vertices across {} page(s) and {} Form XObject(s), removing about {} decoded bytes and saving about {} encoded bytes without changing path geometry.",
                self.collinear_paths.vertices_removed,
                self.collinear_paths.pages_rewritten,
                self.collinear_paths.forms_rewritten,
                self.collinear_paths.decoded_bytes_removed,
                self.collinear_paths.estimated_flate_bytes_saved
            ));
        }
        if self.outlined_glyphs.occurrences_replaced > 0 {
            notes.push(format!(
                "Recovered {} reusable outlined glyph shape(s) in {} synthetic Type3 font(s), replacing {} glyph occurrence(s) spanning {} source paint operation(s), removing about {} decoded bytes and saving about {} encoded bytes.",
                self.outlined_glyphs.glyphs_created,
                self.outlined_glyphs.fonts_created,
                self.outlined_glyphs.occurrences_replaced,
                self.outlined_glyphs.source_paints_replaced,
                self.outlined_glyphs.decoded_bytes_removed,
                self.outlined_glyphs.estimated_flate_bytes_saved
            ));
        }
        if self.paint_batch.paints_eliminated > 0 {
            notes.push(format!(
                "Collapsed {} source paint operation(s) into {} bounded compound paint group(s) across {} page(s), eliminating {} paint operations.",
                self.paint_batch.source_paints_batched,
                self.paint_batch.groups_created,
                self.paint_batch.pages_rewritten,
                self.paint_batch.paints_eliminated
            ));
        }
        if self.stroke_forms.forms_created > 0 {
            notes.push(format!(
                "Factored {} repeated independently-stroked path block occurrence(s) into {} translation-reused Form XObject(s) across {} page(s), factoring about {} decoded bytes and saving about {} encoded bytes.",
                self.stroke_forms.occurrences_replaced,
                self.stroke_forms.forms_created,
                self.stroke_forms.pages_rewritten,
                self.stroke_forms.decoded_bytes_factored,
                self.stroke_forms.estimated_flate_bytes_saved
            ));
        }
        if self.microstroke_raster.runs_rasterized > 0 {
            notes.push(format!(
                "Rasterized {} pathological micro-stroke run(s) across {} page(s), replacing {} individually painted strokes with compact binary image masks and saving about {} encoded bytes.",
                self.microstroke_raster.runs_rasterized,
                self.microstroke_raster.pages_rewritten,
                self.microstroke_raster.strokes_rasterized,
                self.microstroke_raster.estimated_flate_bytes_saved
            ));
        }
        if self.raster_layout.constant_color_mask_stencils_emitted > 0 {
            notes.push(format!(
                "Collapsed {} constant-color Image+binary-SMask pair(s) into single native-resolution stencil image(s).",
                self.raster_layout.constant_color_mask_stencils_emitted
            ));
        }
        if self.structure_compaction.page_tree_nodes_removed > 0
            || self
                .structure_compaction
                .shadowed_page_tree_inherited_entries_removed
                > 0
            || self.structure_compaction.name_tree_nodes_removed > 0
            || self.structure_compaction.named_destination_wrappers_inlined > 0
            || self.structure_compaction.named_destination_arrays_inlined > 0
            || self
                .structure_compaction
                .stale_page_destination_references_rewritten
                > 0
        {
            notes.push(format!(
                "Canonicalized document trees: page-tree nodes {} -> {}, removed {} shadowed inherited page-tree entry/entries, name-tree nodes {} -> {}, inlined {} trivial named-destination wrapper object(s), inlined {} indirect destination array(s), and retargeted {} stale page destination reference(s).",
                self.structure_compaction.page_tree_nodes_before,
                self.structure_compaction.page_tree_nodes_after,
                self.structure_compaction.shadowed_page_tree_inherited_entries_removed,
                self.structure_compaction.name_tree_nodes_before,
                self.structure_compaction.name_tree_nodes_after,
                self.structure_compaction.named_destination_wrappers_inlined,
                self.structure_compaction.named_destination_arrays_inlined,
                self.structure_compaction.stale_page_destination_references_rewritten
            ));
        }
        if self.pattern_payload_factor.groups_factored > 0 {
            notes.push(format!(
                "Factored {} duplicate coloured tiling-pattern payload group(s) across {} Pattern stream(s) into {} shared Form XObject(s), eliminating {} bytes of duplicate encoded cell payload before wrapper overhead.",
                self.pattern_payload_factor.groups_factored,
                self.pattern_payload_factor.patterns_rewritten,
                self.pattern_payload_factor.forms_created,
                self.pattern_payload_factor.duplicate_raw_bytes_factored
            ));
        }
        if self.flate.streams_selected > 0 {
            notes.push(format!(
                "Selected {} lone-Flate stream(s) for recompression after measuring about {} bytes of encoded savings.",
                self.flate.streams_selected, self.flate.estimated_savings_bytes
            ));
        }

        notes
    }

    #[expect(
        clippy::too_many_lines,
        reason = "public report fields are populated explicitly so accounting remains auditable"
    )]
    pub(super) fn into_report(
        self,
        document: &EditDocument,
        cfg: &Config,
        before: PdfAnalysis,
        output_len: usize,
        timings: BTreeMap<String, f64>,
    ) -> OptimizationReport {
        let mut notes = self.notes(cfg, &before);
        let flate_level = cfg.optimization_goal.flate_level();
        let adaptive_flate_streams_tested = self
            .flate
            .high_effort_streams_tested
            .saturating_add(self.unfiltered_flate.high_effort_streams_tested);
        let adaptive_flate_streams_selected = self
            .flate
            .high_effort_streams_selected
            .saturating_add(self.unfiltered_flate.high_effort_streams_selected);
        let adaptive_flate_extra_savings = self
            .flate
            .high_effort_extra_savings_bytes
            .saturating_add(self.unfiltered_flate.high_effort_extra_savings_bytes);
        if adaptive_flate_streams_selected > 0 {
            notes.push(format!(
                "Selected high-effort Flate for {adaptive_flate_streams_selected} of {adaptive_flate_streams_tested} highly-compressible large stream(s), saving about {adaptive_flate_extra_savings} additional encoded bytes beyond level {flate_level}."
            ));
        }

        let input_bytes = before.input_bytes;
        let saved_bytes = signed_size_delta(input_bytes, output_len);
        let saved_percent = savings_percent(saved_bytes, input_bytes);
        let reachability_cache = document.reachability_cache_stats();
        OptimizationReport {
            before,
            after_bytes: output_len,
            saved_bytes,
            saved_percent,
            stage_timings_ms: timings,
            reachability_queries: reachability_cache.queries,
            reachability_rebuilds: reachability_cache.rebuilds,
            reachability_cache_hits: reachability_cache.cache_hits,
            reachability_edge_checks: reachability_cache.edge_checks,
            reachability_edge_stable_reuses: reachability_cache.edge_stable_reuses,
            privacy_items_removed: self.scrub.removed,
            jpeg_metadata_bytes_removed: self.scrub.jpeg_metadata_bytes_removed,
            jpeg_entropy_streams_considered: self.jpeg_entropy.streams_considered,
            jpeg_entropy_streams_optimized: self.jpeg_entropy.streams_optimized,
            jpeg_entropy_original_encoded_bytes: self.jpeg_entropy.original_encoded_bytes,
            jpeg_entropy_optimized_encoded_bytes: self.jpeg_entropy.optimized_encoded_bytes,
            hidden_text_items_removed: self.hidden_text.removed,
            large_diagonal_text_objects_removed: self.large_diagonal_text.removed,
            repeated_page_object_groups_removed: self.repeated_page_objects.groups_removed,
            repeated_page_objects_removed: self.repeated_page_objects.objects_removed,
            repeated_page_text_objects_removed: self.repeated_page_objects.text_objects_removed,
            repeated_page_xobject_paints_removed: self.repeated_page_objects.xobject_paints_removed,
            repeated_page_object_pages_rewritten: self.repeated_page_objects.pages_rewritten,
            raster_hidden_text_items_pruned: self
                .physically_hidden_text
                .removed
                .saturating_add(self.raster_layout.shared_hidden_text_paints_pruned),
            vector_pages_compacted: self.vector_compaction.pages_compacted,
            vector_fill_groups_batched: self.vector_compaction.fill_groups_batched,
            vector_covered_fills_pruned: self.vector_compaction.covered_fills_pruned,
            vector_fill_paints_eliminated: self.vector_compaction.fill_paints_eliminated,
            vector_decoded_bytes_removed: self.vector_compaction.decoded_bytes_removed,
            vector_estimated_flate_bytes_saved: self.vector_compaction.estimated_flate_bytes_saved,
            vector_path_forms_created: self.vector_compaction.path_forms_created,
            vector_path_form_pages_rewritten: self.vector_compaction.path_form_pages_rewritten,
            vector_path_form_occurrences_replaced: self
                .vector_compaction
                .path_form_occurrences_replaced,
            vector_path_form_decoded_bytes_factored: self
                .vector_compaction
                .path_form_decoded_bytes_factored,
            vector_path_form_estimated_flate_bytes_saved: self
                .vector_compaction
                .path_form_estimated_flate_bytes_saved,
            vector_transformed_forms_created: self.vector_compaction.transformed_forms_created,
            vector_transformed_form_pages_rewritten: self
                .vector_compaction
                .transformed_form_pages_rewritten,
            vector_transformed_form_occurrences_replaced: self
                .vector_compaction
                .transformed_form_occurrences_replaced,
            vector_transformed_form_operators_eliminated: self
                .vector_compaction
                .transformed_form_operators_eliminated,
            vector_transformed_form_estimated_flate_bytes_saved: self
                .vector_compaction
                .transformed_form_estimated_flate_bytes_saved,
            vector_shared_run_forms_created: self.vector_compaction.shared_run_forms_created,
            vector_shared_run_pages_rewritten: self.vector_compaction.shared_run_pages_rewritten,
            vector_shared_run_blocks_replaced: self.vector_compaction.shared_run_blocks_replaced,
            vector_shared_run_decoded_bytes_factored: self
                .vector_compaction
                .shared_run_decoded_bytes_factored,
            vector_shared_run_estimated_flate_bytes_saved: self
                .vector_compaction
                .shared_run_estimated_flate_bytes_saved,
            repeated_clip_pages_rewritten: self.repeated_clips.pages_rewritten,
            repeated_clip_runs_hoisted: self.repeated_clips.runs_hoisted,
            repeated_clip_blocks_hoisted: self.repeated_clips.blocks_hoisted,
            repeated_clip_decoded_bytes_removed: self.repeated_clips.decoded_bytes_removed,
            repeated_clip_estimated_flate_bytes_saved: self
                .repeated_clips
                .estimated_flate_bytes_saved,
            vector_path_coordinates_canonicalized: self
                .vector_compaction
                .path_coordinates_canonicalized,
            vector_path_coordinate_pages_rewritten: self
                .vector_compaction
                .path_coordinate_pages_rewritten,
            vector_path_coordinate_decoded_bytes_removed: self
                .vector_compaction
                .path_coordinate_decoded_bytes_removed,
            vector_path_coordinate_estimated_flate_bytes_saved: self
                .vector_compaction
                .path_coordinate_estimated_flate_bytes_saved,
            marked_content_pages_rewritten: self.marked_content.pages_rewritten,
            marked_content_boundaries_coalesced: self.marked_content.boundaries_coalesced,
            marked_content_decoded_bytes_removed: self.marked_content.decoded_bytes_removed,
            marked_content_estimated_flate_bytes_saved: self
                .marked_content
                .estimated_flate_bytes_saved,
            polyline_simplification_pages_rewritten: self.polyline_simplification.pages_rewritten,
            polyline_simplification_vertices_removed: self.polyline_simplification.vertices_removed,
            polyline_simplification_decoded_bytes_removed: self
                .polyline_simplification
                .decoded_bytes_removed,
            polyline_simplification_estimated_flate_bytes_saved: self
                .polyline_simplification
                .estimated_flate_bytes_saved,
            collinear_path_pages_rewritten: self.collinear_paths.pages_rewritten,
            collinear_path_forms_rewritten: self.collinear_paths.forms_rewritten,
            collinear_path_vertices_removed: self.collinear_paths.vertices_removed,
            collinear_path_decoded_bytes_removed: self.collinear_paths.decoded_bytes_removed,
            collinear_path_estimated_flate_bytes_saved: self
                .collinear_paths
                .estimated_flate_bytes_saved,
            outlined_glyph_pages_rewritten: self.outlined_glyphs.pages_rewritten,
            outlined_glyph_fonts_created: self.outlined_glyphs.fonts_created,
            outlined_glyph_shapes_created: self.outlined_glyphs.glyphs_created,
            outlined_glyph_occurrences_replaced: self.outlined_glyphs.occurrences_replaced,
            outlined_glyph_source_paints_replaced: self.outlined_glyphs.source_paints_replaced,
            outlined_glyph_decoded_bytes_removed: self.outlined_glyphs.decoded_bytes_removed,
            outlined_glyph_estimated_flate_bytes_saved: self
                .outlined_glyphs
                .estimated_flate_bytes_saved,
            paint_batch_pages_rewritten: self.paint_batch.pages_rewritten,
            paint_batch_groups_created: self.paint_batch.groups_created,
            paint_batch_source_paints_batched: self.paint_batch.source_paints_batched,
            paint_batch_paints_eliminated: self.paint_batch.paints_eliminated,
            stroke_form_pages_rewritten: self.stroke_forms.pages_rewritten,
            stroke_forms_created: self.stroke_forms.forms_created,
            stroke_form_occurrences_replaced: self.stroke_forms.occurrences_replaced,
            stroke_form_decoded_bytes_factored: self.stroke_forms.decoded_bytes_factored,
            stroke_form_estimated_flate_bytes_saved: self.stroke_forms.estimated_flate_bytes_saved,
            metadata_duplicate_streams_detected: self.metadata_dedup.duplicate_streams_detected,
            metadata_duplicate_raw_bytes: self.metadata_dedup.duplicate_raw_bytes,
            metadata_references_canonicalized: self.metadata_dedup.references_canonicalized,
            font_duplicate_streams_detected: self.font_dedup.duplicate_streams_detected,
            font_duplicate_raw_bytes: self.font_dedup.duplicate_raw_bytes,
            font_references_canonicalized: self.font_dedup.references_canonicalized,
            font_duplicate_dictionaries_detected: self.font_object_dedup.duplicate_objects_detected,
            font_dictionary_references_canonicalized: self
                .font_object_dedup
                .references_canonicalized,
            extgstate_duplicate_dictionaries_detected: self
                .extgstate_object_dedup
                .duplicate_objects_detected,
            extgstate_dictionary_references_canonicalized: self
                .extgstate_object_dedup
                .references_canonicalized,
            structure_attribute_duplicate_dictionaries_detected: self
                .structure_attribute_dedup
                .duplicate_objects_detected,
            structure_attribute_references_canonicalized: self
                .structure_attribute_dedup
                .references_canonicalized,
            sampled_function_duplicate_streams_detected: self
                .sampled_function_dedup
                .duplicate_streams_detected,
            sampled_function_duplicate_raw_bytes: self.sampled_function_dedup.duplicate_raw_bytes,
            sampled_function_references_canonicalized: self
                .sampled_function_dedup
                .references_canonicalized,
            font_programs_rendering_optimized: self.font_rendering.programs_optimized,
            font_rendering_original_encoded_bytes: self.font_rendering.original_encoded_bytes,
            font_rendering_optimized_encoded_bytes: self.font_rendering.optimized_encoded_bytes,
            font_rendering_decoded_table_bytes_removed: self
                .font_rendering
                .decoded_table_bytes_removed,
            font_programs_glyph_subset: self.font_rendering.programs_glyph_subset,
            font_glyph_subset_decoded_bytes_removed: self
                .font_rendering
                .glyph_subset_decoded_bytes_removed,
            font_cid_cmaps_removed: self.font_cid_cmap_strip.cid_cmaps_removed,
            font_cid_cmap_decoded_bytes_removed: self
                .font_cid_cmap_strip
                .cid_cmap_decoded_bytes_removed,
            font_cid_cmap_original_encoded_bytes: self.font_cid_cmap_strip.original_encoded_bytes,
            font_cid_cmap_optimized_encoded_bytes: self.font_cid_cmap_strip.optimized_encoded_bytes,
            font_dense_programs_remapped: self.font_dense.programs_dense_remapped,
            font_dense_cid_to_gid_maps_rewritten: self.font_dense.cid_to_gid_maps_rewritten,
            font_dense_glyph_slots_removed: self.font_dense.dense_glyph_slots_removed,
            font_dense_decoded_bytes_removed: self.font_dense.dense_decoded_bytes_removed,
            font_dense_original_encoded_bytes: self.font_dense.original_encoded_bytes,
            font_dense_optimized_encoded_bytes: self.font_dense.optimized_encoded_bytes,
            to_unicode_duplicate_streams_detected: self.to_unicode_dedup.duplicate_streams_detected,
            to_unicode_duplicate_raw_bytes: self.to_unicode_dedup.duplicate_raw_bytes,
            to_unicode_references_canonicalized: self.to_unicode_dedup.references_canonicalized,
            inline_image_fingerprints_selected: self.inline_image_dedup.fingerprints_selected,
            inline_image_occurrences_externalized: self.inline_image_dedup.occurrences_externalized,
            inline_image_xobjects_created: self.inline_image_dedup.xobjects_created,
            inline_image_xobject_references_reused: self
                .inline_image_dedup
                .xobject_references_reused,
            inline_image_duplicate_payload_bytes: self.inline_image_dedup.duplicate_payload_bytes,
            image_duplicate_streams_detected: self.image_dedup.duplicate_streams_detected,
            image_duplicate_raw_bytes: self.image_dedup.duplicate_raw_bytes,
            image_references_canonicalized: self.image_dedup.references_canonicalized,
            pattern_payload_groups_factored: self.pattern_payload_factor.groups_factored,
            pattern_payload_patterns_rewritten: self.pattern_payload_factor.patterns_rewritten,
            pattern_payload_forms_created: self.pattern_payload_factor.forms_created,
            pattern_payload_duplicate_raw_bytes_factored: self
                .pattern_payload_factor
                .duplicate_raw_bytes_factored,
            form_metadata_payload_groups_factored: self
                .form_metadata_payload_factor
                .groups_factored,
            form_metadata_payload_wrappers_rewritten: self
                .form_metadata_payload_factor
                .wrappers_rewritten,
            form_metadata_payload_duplicate_raw_bytes_factored: self
                .form_metadata_payload_factor
                .duplicate_raw_bytes_factored,
            form_duplicate_streams_detected: self.form_dedup.duplicate_streams_detected,
            form_duplicate_raw_bytes: self.form_dedup.duplicate_raw_bytes,
            form_references_canonicalized: self.form_dedup.references_canonicalized,
            appearance_duplicate_streams_detected: self.appearance_dedup.duplicate_streams_detected,
            appearance_duplicate_raw_bytes: self.appearance_dedup.duplicate_raw_bytes,
            appearance_references_canonicalized: self.appearance_dedup.references_canonicalized,
            page_content_duplicate_streams_detected: self
                .page_content_dedup
                .duplicate_streams_detected,
            page_content_duplicate_raw_bytes: self.page_content_dedup.duplicate_raw_bytes,
            page_content_references_canonicalized: self.page_content_dedup.references_canonicalized,
            type3_charproc_duplicate_streams_detected: self
                .type3_charproc_dedup
                .duplicate_streams_detected,
            type3_charproc_duplicate_raw_bytes: self.type3_charproc_dedup.duplicate_raw_bytes,
            type3_charproc_references_canonicalized: self
                .type3_charproc_dedup
                .references_canonicalized,
            icc_duplicate_streams_detected: self.icc_dedup.duplicate_streams_detected,
            icc_duplicate_raw_bytes: self.icc_dedup.duplicate_raw_bytes,
            icc_references_canonicalized: self.icc_dedup.references_canonicalized,
            icc_alternate_profiles_eligible: self.icc_alternate.profiles_eligible,
            icc_alternate_profiles_elided: self.icc_alternate.profiles_elided,
            icc_alternate_references_rewritten: self.icc_alternate.references_rewritten,
            icc_alternate_encoded_bytes_elided: self.icc_alternate.encoded_profile_bytes_elided,
            raster_images_transcoded: self.raster_transform.images_optimized,
            raster_paper_indexed_images: self.raster_transform.paper_indexed_images,
            raster_paper_indexed_layered_images: self.raster_transform.paper_indexed_layered_images,
            raster_paper_indexed_residual_images: self
                .raster_transform
                .paper_indexed_residual_images,
            raster_images_resized: self.raster_transform.images_resized,
            raster_jpeg_images_resized: self.raster_transform.jpeg_images_resized,
            raster_flate_images_resized: self.raster_transform.flate_images_resized,
            raster_references_reused: self.raster_transform.references_reused,
            raster_original_encoded_bytes: self.raster_transform.original_encoded_bytes,
            raster_optimized_encoded_bytes: self.raster_transform.optimized_encoded_bytes,
            raster_original_pixels: self.raster_transform.original_pixels,
            raster_optimized_pixels: self.raster_transform.optimized_pixels,
            raster_inline_fragmented_scopes_rewritten: self.raster_layout.inline.scopes_rewritten,
            raster_inline_occurrences_externalized: self
                .raster_layout
                .inline
                .occurrences_externalized,
            raster_pixel_clusters_reconstructed: self.raster_layout.pixel_clusters_reconstructed,
            raster_pixel_paints_reconstructed: self.raster_layout.pixel_paints_reconstructed,
            raster_native_fragment_groups_reconstructed: self
                .raster_layout
                .native_fragment_groups_reconstructed,
            raster_native_fragment_paints_reconstructed: self
                .raster_layout
                .native_fragment_paints_reconstructed,
            raster_stripe_groups_merged: self.raster_layout.stripe_groups_merged,
            raster_stripe_paints_merged: self.raster_layout.stripe_paints_merged,
            raster_masks_baked: self.raster_layout.masks_baked,
            raster_transparent_paints_pruned: self.raster_layout.transparent_paints_pruned,
            raster_occluded_paints_pruned: self.raster_layout.occluded_raster_paints_pruned,
            raster_transparent_margins_cropped: self.raster_layout.transparent_margins_cropped,
            raster_background_margins_cropped: self.raster_layout.background_margins_cropped,
            raster_cropped_pixels_removed: self.raster_layout.cropped_pixels_removed,
            raster_binary_images_packed: self.raster_layout.binary_images_packed,
            raster_binary_masks_packed: self.raster_layout.binary_masks_packed,
            raster_stencil_images_emitted: self.raster_layout.stencil_images_emitted,
            raster_constant_color_mask_stencils_emitted: self
                .raster_layout
                .constant_color_mask_stencils_emitted,
            raster_relaxed_stencil_images_emitted: self
                .raster_layout
                .relaxed_stencil_images_emitted,
            raster_bilevel_ccitt_images_emitted: self.raster_layout.bilevel_ccitt_images_emitted,
            raster_bilevel_flate_images_emitted: self.raster_layout.bilevel_flate_images_emitted,
            exact_raster_rendering: cfg.raster_layout.exact_raster_rendering,
            raster_binary_image_encoded_bytes_saved: self
                .raster_layout
                .binary_image_encoded_bytes_saved,
            raster_deferred_tile_candidates: self.raster_layout.deferred_tile_candidates,
            raster_deferred_tile_paints_consumed: self.raster_layout.deferred_tile_paints_consumed,
            raster_staging_xobject_entries_removed: self
                .raster_layout
                .staging_xobject_entries_removed,
            raster_inline_reconstruction_payload_rejections: self
                .raster_layout
                .inline_reconstruction_payload_rejections,
            raster_rewritten_xobject_entries_removed: self
                .raster_layout
                .rewritten_xobject_entries_removed,
            resource_entries_pruned: self.resource_prune.entries_removed,
            resource_font_entries_pruned: self.resource_prune.fonts,
            resource_xobject_entries_pruned: self.resource_prune.xobjects,
            resource_ext_gstate_entries_pruned: self.resource_prune.ext_gstates,
            resource_pattern_entries_pruned: self.resource_prune.patterns,
            resource_properties_entries_pruned: self.resource_prune.properties,
            resource_shading_entries_pruned: self.resource_prune.shadings,
            page_tree_nodes_before: self.structure_compaction.page_tree_nodes_before,
            page_tree_nodes_after: self.structure_compaction.page_tree_nodes_after,
            page_tree_nodes_removed: self.structure_compaction.page_tree_nodes_removed,
            page_tree_pages_reparented: self.structure_compaction.page_tree_pages_reparented,
            shadowed_page_tree_inherited_entries_removed: self
                .structure_compaction
                .shadowed_page_tree_inherited_entries_removed,
            name_trees_repacked: self.structure_compaction.name_trees_repacked,
            name_tree_nodes_before: self.structure_compaction.name_tree_nodes_before,
            name_tree_nodes_after: self.structure_compaction.name_tree_nodes_after,
            name_tree_nodes_removed: self.structure_compaction.name_tree_nodes_removed,
            named_destination_wrappers_inlined: self
                .structure_compaction
                .named_destination_wrappers_inlined,
            named_destination_arrays_inlined: self
                .structure_compaction
                .named_destination_arrays_inlined,
            stale_page_destination_references_rewritten: self
                .structure_compaction
                .stale_page_destination_references_rewritten,
            microstroke_pages_rasterized: self.microstroke_raster.pages_rewritten,
            microstroke_runs_rasterized: self.microstroke_raster.runs_rasterized,
            microstroke_strokes_rasterized: self.microstroke_raster.strokes_rasterized,
            microstroke_image_payload_bytes: self.microstroke_raster.image_payload_bytes,
            microstroke_ccitt_images: self.microstroke_raster.ccitt_images,
            microstroke_flate_images: self.microstroke_raster.flate_images,
            microstroke_estimated_flate_bytes_saved: self
                .microstroke_raster
                .estimated_flate_bytes_saved,
            print_images_placed: self.print_plan.stats.images_placed,
            print_image_uses: self.print_plan.stats.image_uses,
            print_geometry_complete: self.print_plan.stats.geometry_complete,
            print_downsample_candidates: self.print_plan.stats.downsample_candidates,
            print_existing_jpeg_resize_candidates: self
                .print_plan
                .stats
                .existing_jpeg_resize_candidates,
            print_flate_resize_candidates: self.print_plan.stats.flate_resize_candidates,
            print_source_pixels: self.print_plan.stats.source_pixels,
            print_target_pixels: self.print_plan.stats.target_pixels,
            flate_streams_selected_for_recompression: self.flate.streams_selected,
            flate_estimated_savings_bytes: self.flate.estimated_savings_bytes,
            flate_high_effort_streams_tested: adaptive_flate_streams_tested,
            flate_high_effort_streams_selected: adaptive_flate_streams_selected,
            flate_high_effort_extra_savings_bytes: adaptive_flate_extra_savings,
            preservation_pages: self.preservation.pages,
            preservation_annotation_entries_seen: self.preservation.annotation_entries_seen,
            preservation_annotation_entries_flattened: self
                .preservation
                .annotation_entries_flattened,
            preservation_annotation_entries_dropped_unflattened: self
                .preservation
                .annotation_entries_dropped_unflattened,
            preservation_link_visual_shells_retained: self.preservation.link_visual_shells_retained,
            preservation_annotation_subtypes_seen: self.preservation.annotation_subtypes_seen,
            preservation_unflattened_annotation_subtypes: self
                .preservation
                .unflattened_annotation_subtypes,
            preservation_dropped_page_keys: self.preservation.dropped_page_keys,
            preservation_dropped_page_tree_keys: self.preservation.dropped_page_tree_keys,
            preservation_dropped_catalog_keys: self.preservation.dropped_catalog_keys,
            preservation_dropped_authoring_keys: self.preservation.dropped_authoring_keys,
            preservation_spliced_unknown_wrapper_keys: self
                .preservation
                .spliced_unknown_wrapper_keys,
            notes,
        }
    }
}
