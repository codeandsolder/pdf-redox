use crate::{
    Config, ImagePolicy, OptimizationReport, Result, analyze_pdf,
    hidden_text::apply_hidden_text_policy, scrub::scrub_pdf,
};
use flpdf::{ObjectStreamMode, Pdf, PdfWriter, StreamDataMode};
use std::io::Cursor;

pub fn optimize_pdf(input: &[u8], cfg: &Config) -> Result<(Vec<u8>, OptimizationReport)> {
    let before = analyze_pdf(input)?;
    let mut pdf = Pdf::open(Cursor::new(input.to_vec()))?;
    let hidden_text = apply_hidden_text_policy(&mut pdf, &cfg.hidden_text)?;
    let scrub = scrub_pdf(&mut pdf, &cfg.privacy)?;

    let mut writer = PdfWriter::new(&mut pdf);
    writer.set_output_memory()?;
    writer.set_preserve_unreferenced_objects(false);
    writer.set_stream_data_mode(StreamDataMode::Compress);
    writer.set_recompress_flate(cfg.recompress_flate);
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
        notes,
    };
    Ok((output, report))
}
