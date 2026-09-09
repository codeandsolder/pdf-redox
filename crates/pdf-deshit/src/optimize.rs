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
use flpdf::{ObjectStreamMode, PageDocumentHelper, Pdf, PdfWriter, StreamDataMode};
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
    match cfg.image_policy {
        ImagePolicy::Preserve => {}
        _ => notes.push("Image policy is configured, but the first implementation currently performs structural/lossless PDF rewrites only; raster transcode is the next pass.".to_owned()),
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
        flate_streams_selected_for_recompression: flate.streams_selected,
        flate_estimated_savings_bytes: flate.estimated_savings_bytes,
        notes,
    };
    Ok((output, report))
}
