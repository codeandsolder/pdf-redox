//! Extraction of real PDF stream payloads for Deflate benchmark corpora.

use crate::{EditDocument, FlateLevel, ObjectHandle, OwnedDictionary, OwnedObject, Result};
use serde::Serialize;
use sha2::{Digest as _, Sha256};

/// Coarse semantic class of a PDF stream used to keep benchmark corpora diverse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompressionSampleKind {
    /// Image XObject payload.
    Image,
    /// Form XObject payload.
    Form,
    /// Tiling/shading pattern payload.
    Pattern,
    /// Embedded font-program payload.
    Font,
    /// ICC profile payload.
    Icc,
    /// Metadata/XML payload.
    Metadata,
    /// Embedded-file payload.
    EmbeddedFile,
    /// PDF object-stream payload.
    ObjectStream,
    /// PDF cross-reference stream payload.
    Xref,
    /// Stream without a stronger self-describing classification, commonly page content or CMaps.
    Other,
}

/// One decoded stream that is directly suitable for plain Deflate compression.
#[derive(Debug, Clone)]
pub struct CompressionSampleCandidate {
    /// Source PDF object handle.
    pub object: ObjectHandle,
    /// Coarse semantic stream class.
    pub kind: CompressionSampleKind,
    /// Whether the source stream was already Flate-compressed.
    pub source_was_flate: bool,
    /// Encoded payload length in the source PDF.
    pub encoded_bytes: usize,
    /// Decoded bytes passed to a Deflate encoder.
    pub decoded: Vec<u8>,
    /// SHA-256 of `decoded`.
    pub decoded_sha256: [u8; 32],
    /// Shannon entropy of the decoded byte histogram, in bits per byte.
    pub entropy_bits_per_byte: f64,
    /// Number of distinct byte values present in the decoded payload.
    pub unique_byte_values: u16,
    /// Count of zero bytes in the decoded payload.
    pub zero_bytes: usize,
    /// Count of printable ASCII plus common ASCII whitespace bytes.
    pub ascii_like_bytes: usize,
}

/// Counters from scanning one PDF for compression-corpus candidates.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CompressionSampleScanStats {
    /// Reachable streams inspected.
    pub reachable_streams: usize,
    /// Streams rejected because their filter chain is not unfiltered or lone-Flate.
    pub unsupported_filter_streams: usize,
    /// Empty decoded streams skipped.
    pub empty_streams: usize,
    /// Eligible streams whose filter chain failed to decode.
    pub decode_failures: usize,
    /// Non-empty candidates delivered to the visitor.
    pub candidates: usize,
}

fn resolved_name(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    key: &[u8],
) -> Option<Vec<u8>> {
    let value = dictionary.get(key)?;
    match document.resolve_owned_value(value).ok().flatten()? {
        OwnedObject::Name(name) => Some(name),
        _ => None,
    }
}

fn sample_kind(document: &EditDocument, dictionary: &OwnedDictionary) -> CompressionSampleKind {
    let type_name = resolved_name(document, dictionary, b"Type");
    let subtype = resolved_name(document, dictionary, b"Subtype");

    if subtype.as_deref() == Some(b"Image") {
        return CompressionSampleKind::Image;
    }
    if subtype.as_deref() == Some(b"Form") {
        return CompressionSampleKind::Form;
    }
    if dictionary.contains_key(b"PatternType".as_slice()) {
        return CompressionSampleKind::Pattern;
    }
    if matches!(
        subtype.as_deref(),
        Some(b"Type1C" | b"CIDFontType0C" | b"OpenType")
    ) || dictionary.contains_key(b"Length1".as_slice())
        || dictionary.contains_key(b"Length2".as_slice())
        || dictionary.contains_key(b"Length3".as_slice())
    {
        return CompressionSampleKind::Font;
    }
    if dictionary.contains_key(b"N".as_slice())
        && (dictionary.contains_key(b"Alternate".as_slice())
            || dictionary.contains_key(b"Range".as_slice()))
    {
        return CompressionSampleKind::Icc;
    }
    if type_name.as_deref() == Some(b"Metadata") || subtype.as_deref() == Some(b"XML") {
        return CompressionSampleKind::Metadata;
    }
    if type_name.as_deref() == Some(b"EmbeddedFile") {
        return CompressionSampleKind::EmbeddedFile;
    }
    if type_name.as_deref() == Some(b"ObjStm") {
        return CompressionSampleKind::ObjectStream;
    }
    if type_name.as_deref() == Some(b"XRef") {
        return CompressionSampleKind::Xref;
    }
    CompressionSampleKind::Other
}

fn source_was_flate(document: &EditDocument, dictionary: &OwnedDictionary) -> bool {
    let Some(filter) = dictionary.get(b"Filter".as_slice()) else {
        return false;
    };
    let Ok(Some(filter)) = document.resolve_owned_value(filter) else {
        return false;
    };
    match filter {
        OwnedObject::Name(name) => matches!(name.as_slice(), b"FlateDecode" | b"Fl"),
        OwnedObject::Array(values) if values.len() == 1 => values.first().is_some_and(|value| {
            matches!(
                document.resolve_owned_value(value),
                Ok(Some(OwnedObject::Name(name))) if matches!(name.as_slice(), b"FlateDecode" | b"Fl")
            )
        }),
        _ => false,
    }
}

fn byte_statistics(bytes: &[u8]) -> (f64, u16, usize, usize) {
    let mut counts = [0_usize; 256];
    let mut zero_bytes = 0_usize;
    let mut ascii_like_bytes = 0_usize;
    for &byte in bytes {
        counts[usize::from(byte)] += 1;
        zero_bytes += usize::from(byte == 0);
        ascii_like_bytes += usize::from(matches!(byte, b'\t' | b'\n' | b'\r' | 0x20..=0x7e));
    }

    let len = bytes.len() as f64;
    let mut entropy = 0.0;
    let mut unique = 0_u16;
    for count in counts.into_iter().filter(|count| *count != 0) {
        unique += 1;
        let probability = count as f64 / len;
        entropy -= probability * probability.log2();
    }
    (entropy, unique, zero_bytes, ascii_like_bytes)
}

/// Measure the current pdf-redox Processing and Size Flate encoders on one decoded payload.
///
/// # Errors
///
/// Returns an error if either zlib stream cannot be encoded.
pub fn compression_baseline_sizes(decoded: &[u8]) -> Result<(usize, usize)> {
    let processing = crate::stream_codec::encode_flate(decoded, FlateLevel::PROCESSING)?.len();
    let size = crate::stream_codec::encode_flate(decoded, FlateLevel::SIZE)?.len();
    Ok((processing, size))
}

/// Visit every reachable non-empty stream whose bytes can be represented as plain Deflate input.
///
/// Unfiltered streams and streams with exactly one Flate filter are decoded to their logical byte
/// payload. Other filter chains are deliberately skipped: this corpus is intended to benchmark the
/// bytes pdf-redox can directly hand to its ordinary Deflate encoder, rather than JPEG/CCITT/JPX
/// codecs. The visitor is called one stream at a time so callers can perform bounded selection
/// without retaining an entire pathological PDF's decoded streams in memory.
///
/// # Errors
///
/// Returns an error when the PDF cannot be parsed or reachability traversal fails. Individual
/// stream decode failures are counted and skipped because malformed filter payloads should not
/// prevent extracting unrelated streams.
pub fn visit_compression_sample_candidates(
    input: &[u8],
    mut visitor: impl FnMut(CompressionSampleCandidate),
) -> Result<CompressionSampleScanStats> {
    let document = EditDocument::from_bytes(input.to_vec())?;
    let handles = document.reachable_streams()?;
    let mut stats = CompressionSampleScanStats {
        reachable_streams: handles.len(),
        ..CompressionSampleScanStats::default()
    };

    for handle in handles {
        let Some(OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        if !crate::stream_codec::is_unfiltered_or_lone_flate(&document, &dictionary)? {
            stats.unsupported_filter_streams += 1;
            continue;
        }
        let encoded_bytes = data.bytes(document.source())?.len();
        let Ok(decoded) = document.decoded_stream_data(handle) else {
            stats.decode_failures += 1;
            continue;
        };
        if decoded.is_empty() {
            stats.empty_streams += 1;
            continue;
        }

        let decoded_sha256 = Sha256::digest(&decoded).into();
        let (entropy_bits_per_byte, unique_byte_values, zero_bytes, ascii_like_bytes) =
            byte_statistics(&decoded);

        visitor(CompressionSampleCandidate {
            object: handle,
            kind: sample_kind(&document, &dictionary),
            source_was_flate: source_was_flate(&document, &dictionary),
            encoded_bytes,
            decoded,
            decoded_sha256,
            entropy_bits_per_byte,
            unique_byte_values,
            zero_bytes,
            ascii_like_bytes,
        });
        stats.candidates += 1;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ClassicPdfBuilder;

    fn fixture() -> Result<Vec<u8>> {
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>")?;
        let decoded = b"BT /F1 12 Tf (compression corpus) Tj ET\n".repeat(32);
        let encoded = crate::stream_codec::encode_flate(&decoded, FlateLevel::PROCESSING)?;
        pdf.stream(4, b"/Filter /FlateDecode", &encoded)?;
        pdf.finish(1)
    }

    #[test]
    fn extracts_decoded_lone_flate_stream() -> Result<()> {
        let input = fixture()?;
        let mut samples = Vec::new();
        let stats = visit_compression_sample_candidates(&input, |sample| samples.push(sample))?;
        assert_eq!(stats.candidates, 1);
        assert_eq!(samples.len(), 1);
        let sample = &samples[0];
        assert!(sample.source_was_flate);
        assert_eq!(sample.kind, CompressionSampleKind::Other);
        assert!(sample.decoded.starts_with(b"BT /F1"));
        let (processing, size) = compression_baseline_sizes(&sample.decoded)?;
        assert!(processing <= sample.decoded.len());
        assert!(size <= processing);
        Ok(())
    }
}
