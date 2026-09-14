use crate::{
    Config, FlatePolicy, ImagePolicy, OptimizationReport, PdfAnalysis, Result,
    analyze::{analyze_pdf, input_sha256},
    dedup::{
        canonicalize_appearance_streams, canonicalize_font_program_streams,
        canonicalize_form_xobjects, canonicalize_icc_profiles, canonicalize_image_xobjects,
        canonicalize_metadata_streams, canonicalize_page_contents, canonicalize_to_unicode_cmaps,
        canonicalize_type3_charprocs,
    },
    flate::apply_flate_policy,
    font::strip_font_editing_tables,
    hidden_text::apply_hidden_text_policy,
    preservation::{PreservationStats, apply_preservation_policy},
    print::{PrintPlan, plan_print_downsampling},
    scrub::scrub_pdf,
};
use flpdf::{
    ImageOptimizationOptions, ImageOptimizationStats, ObjectStreamMode, PageDocumentHelper, Pdf,
    PdfWriter, QPDFLogger, StreamDataMode, externalize_duplicate_inline_images,
    optimize_images_with_resize_targets, optimize_images_with_stats,
};
use std::io::Cursor;

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

pub fn optimize_pdf(input: &[u8], cfg: &Config) -> Result<(Vec<u8>, OptimizationReport)> {
    validate_config(cfg)?;
    let before = analyze_pdf(input)?;
    optimize_pdf_with_before(input, cfg, before)
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
    let before = if matches {
        analysis.clone()
    } else {
        analyze_pdf(input)?
    };
    optimize_pdf_with_before(input, cfg, before)
}

fn optimize_pdf_with_before(
    input: &[u8],
    cfg: &Config,
    before: PdfAnalysis,
) -> Result<(Vec<u8>, OptimizationReport)> {
    let mut pdf = Pdf::open(Cursor::new(input.to_vec()))?;
    let preservation = if cfg.preservation == crate::PreservationConfig::functional() {
        PreservationStats::default()
    } else {
        apply_preservation_policy(&mut pdf, &cfg.preservation)?
    };
    let hidden_text = apply_hidden_text_policy(&mut pdf, &cfg.hidden_text)?;
    let scrub = scrub_pdf(&mut pdf, &cfg.privacy)?;
    // Strip rendering-irrelevant embedded-font editing/layout tables before
    // font-program dedup. Different producer subsets can become identical once
    // non-rendering font state is removed, increasing the later dedup win.
    let font_rendering = if cfg.preservation.font_editing_support {
        Default::default()
    } else {
        strip_font_editing_tables(&mut pdf, cfg.flate_level)?
    };
    let metadata_dedup = if cfg.deduplicate_metadata_streams {
        canonicalize_metadata_streams(&mut pdf)?
    } else {
        Default::default()
    };
    let font_dedup = if cfg.deduplicate_font_programs {
        canonicalize_font_program_streams(&mut pdf)?
    } else {
        Default::default()
    };
    let to_unicode_dedup = if cfg.deduplicate_to_unicode_cmaps {
        canonicalize_to_unicode_cmaps(&mut pdf)?
    } else {
        Default::default()
    };
    let icc_dedup = if cfg.deduplicate_icc_profiles {
        canonicalize_icc_profiles(&mut pdf)?
    } else {
        Default::default()
    };
    let inline_image_dedup = if cfg.deduplicate_inline_images
        && before.duplicate_inline_image_payload_wasted_bytes
            >= cfg.inline_image_min_duplicate_payload_bytes
    {
        externalize_duplicate_inline_images(
            &mut pdf,
            0,
            cfg.inline_image_min_duplicate_payload_bytes,
        )?
    } else {
        Default::default()
    };
    // Canonicalize exact source images after duplicate inline-image
    // externalization and before any raster transform. This lets newly
    // externalized images share existing byte-identical Image XObjects and
    // gives lossy transforms one canonical source identity, avoiding
    // duplicate decode/resample/re-encode work.
    let image_dedup = if cfg.deduplicate_image_xobjects {
        canonicalize_image_xobjects(&mut pdf)?
    } else {
        Default::default()
    };
    let form_dedup = if cfg.deduplicate_form_xobjects {
        canonicalize_form_xobjects(&mut pdf)?
    } else {
        Default::default()
    };
    let appearance_dedup = if cfg.deduplicate_appearance_streams {
        canonicalize_appearance_streams(&mut pdf)?
    } else {
        Default::default()
    };
    let type3_charproc_dedup = if cfg.deduplicate_type3_charprocs {
        canonicalize_type3_charprocs(&mut pdf)?
    } else {
        Default::default()
    };
    let (print_plan, print_plan_error) = match &cfg.image_policy {
        ImagePolicy::Print { target_ppi, .. } => {
            let target_ppi = cfg.max_image_ppi.unwrap_or(*target_ppi);
            match plan_print_downsampling(&mut pdf, u32::from(target_ppi)) {
                Ok(plan) => (plan, None),
                Err(error) => (PrintPlan::default(), Some(error.to_string())),
            }
        }
        _ => (PrintPlan::default(), None),
    };
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
                let logger = QPDFLogger::create();
                optimize_images_with_resize_targets(
                    &mut pdf,
                    &logger,
                    "pdf-deshit",
                    false,
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
        } => {
            let logger = QPDFLogger::create();
            optimize_images_with_stats(
                &mut pdf,
                &logger,
                "pdf-deshit",
                false,
                ImageOptimizationOptions {
                    keep_inline_images: true,
                    jpeg_quality: *jpeg_quality,
                    min_savings_bytes: 1,
                    min_savings_percent: *min_savings_percent,
                    ..ImageOptimizationOptions::default()
                },
            )?
        }
    };
    if cfg.prune_resources {
        PageDocumentHelper::new(&mut pdf).remove_unreferenced_resources()?;
    }
    let flate = apply_flate_policy(&mut pdf, cfg.flate_policy, cfg.flate_level)?;
    // Content streams must be canonicalized after every transform that can
    // mutate them (hidden-text removal, inline-image externalization, and
    // Flate recompression). Sharing them earlier would couple later writes
    // across pages that originally had independent stream objects.
    let page_content_dedup = if cfg.deduplicate_page_contents {
        canonicalize_page_contents(&mut pdf)?
    } else {
        Default::default()
    };

    let mut writer = PdfWriter::new(&mut pdf);
    writer.set_output_memory()?;
    writer.set_preserve_unreferenced_objects(false);
    writer.set_stream_data_mode(StreamDataMode::Compress);
    writer.set_recompress_flate(matches!(cfg.flate_policy, FlatePolicy::RecompressAll));
    writer.set_compression_level(cfg.flate_level);
    writer.set_content_normalization(cfg.normalize_content_streams);
    writer.set_object_stream_mode(if cfg.generate_object_streams {
        ObjectStreamMode::Generate
    } else {
        ObjectStreamMode::Preserve
    });
    writer.set_suppress_original_object_ids(true);
    writer.write()?;
    let output = writer.get_buffer()?;

    let mut notes = Vec::new();
    if cfg.preservation != crate::PreservationConfig::functional() {
        notes.push(format!(
            "Applied semantic-preservation policy: {} page(s); flattened {} of {} annotation entries into page content, retained {} inert Link visual shell(s), and dropped {} unflattened annotation entries. Annotation subtypes seen: {:?}; unflattened subtypes before visual-shell pruning: {:?}. Dropped leaf-page dictionary keys: {:?}. Dropped intermediate page-tree keys: {:?}. Dropped source Catalog keys: {:?}.",
            preservation.pages,
            preservation.annotation_entries_flattened,
            preservation.annotation_entries_seen,
            preservation.link_visual_shells_retained,
            preservation.annotation_entries_dropped_unflattened,
            preservation.annotation_subtypes_seen,
            preservation.unflattened_annotation_subtypes,
            preservation.dropped_page_keys,
            preservation.dropped_page_tree_keys,
            preservation.dropped_catalog_keys
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
    if flate.streams_selected > 0 {
        notes.push(format!(
            "Selected {} lone-Flate stream(s) for recompression after measuring about {} bytes of encoded savings.",
            flate.streams_selected, flate.estimated_savings_bytes
        ));
    }

    let saved_bytes = input.len() as isize - output.len() as isize;
    let saved_percent = if input.is_empty() {
        0.0
    } else {
        saved_bytes as f64 * 100.0 / input.len() as f64
    };
    let report = OptimizationReport {
        before,
        after_bytes: output.len(),
        saved_bytes,
        saved_percent,
        privacy_items_removed: scrub.removed,
        jpeg_metadata_bytes_removed: scrub.jpeg_metadata_bytes_removed,
        hidden_text_items_removed: hidden_text.removed,
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
        let (output, report) = optimize_pdf(&input, &Config::print())?;

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

        let (output, report) = optimize_pdf(&jpeg_input, &Config::print())?;
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
