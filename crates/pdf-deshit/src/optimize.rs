use crate::{
    Config, FlatePolicy, ImagePolicy, OptimizationReport, Result, analyze_pdf,
    dedup::{
        canonicalize_font_program_streams, canonicalize_image_xobjects,
        canonicalize_metadata_streams,
    },
    flate::apply_flate_policy,
    hidden_text::apply_hidden_text_policy,
    scrub::scrub_pdf,
};
use flpdf::{
    ImageOptimizationOptions, ImageOptimizationStats, ObjectStreamMode, PageDocumentHelper, Pdf,
    PdfWriter, QPDFLogger, StreamDataMode, optimize_images_with_stats,
};
use std::io::Cursor;

pub fn optimize_pdf(input: &[u8], cfg: &Config) -> Result<(Vec<u8>, OptimizationReport)> {
    let before = analyze_pdf(input)?;
    let mut pdf = Pdf::open(Cursor::new(input.to_vec()))?;
    let hidden_text = apply_hidden_text_policy(&mut pdf, &cfg.hidden_text)?;
    let scrub = scrub_pdf(&mut pdf, &cfg.privacy)?;
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
    let raster_transform = match &cfg.image_policy {
        ImagePolicy::Preserve | ImagePolicy::Print { .. } => ImageOptimizationStats::default(),
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
    let image_dedup = if cfg.deduplicate_image_xobjects {
        canonicalize_image_xobjects(&mut pdf)?
    } else {
        Default::default()
    };
    if cfg.prune_resources {
        PageDocumentHelper::new(&mut pdf).remove_unreferenced_resources()?;
    }
    let flate = apply_flate_policy(&mut pdf, cfg.flate_policy, cfg.flate_level)?;

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
    if let ImagePolicy::Print { .. } = &cfg.image_policy {
        notes.push(
            "Print raster policy is not implemented yet; no resolution-aware downsampling was performed."
                .to_owned(),
        );
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
    if image_dedup.references_canonicalized > 0 {
        notes.push(format!(
            "Canonicalized {} duplicate Image XObject reference(s) across {} duplicate image stream object(s).",
            image_dedup.references_canonicalized, image_dedup.duplicate_streams_detected
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
        image_duplicate_streams_detected: image_dedup.duplicate_streams_detected,
        image_duplicate_raw_bytes: image_dedup.duplicate_raw_bytes,
        image_references_canonicalized: image_dedup.references_canonicalized,
        raster_images_transcoded: raster_transform.images_optimized,
        raster_references_reused: raster_transform.references_reused,
        raster_original_encoded_bytes: raster_transform.original_encoded_bytes,
        raster_optimized_encoded_bytes: raster_transform.optimized_encoded_bytes,
        flate_streams_selected_for_recompression: flate.streams_selected,
        flate_estimated_savings_bytes: flate.estimated_savings_bytes,
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
                pdf.new_stream_with_data(Rc::new(b"q 200 0 0 200 0 0 cm /Im0 Do Q\n".to_vec()))?;
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
}
