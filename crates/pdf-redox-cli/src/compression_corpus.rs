use clap::Parser;
use pdf_redox::{
    CompressionSampleCandidate, CompressionSampleKind, CompressionSampleScanStats, ObjectHandle,
    compression_baseline_sizes, visit_compression_sample_candidates,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{BufWriter, Write as _},
    path::{Path, PathBuf},
};

const DEFAULT_MAX_SAMPLES: usize = 2_048;
const DEFAULT_MAX_DECODED_BYTES: usize = 128 * 1024 * 1024;
const DEFAULT_MAX_SAMPLE_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_PER_FILE_SHAPE: usize = 1;
const DEFAULT_PER_FILE_SAMPLES: usize = 16;
const DEFAULT_GLOBAL_SHAPE: usize = 8;

#[derive(Debug, Parser)]
#[command(
    name = "pdf-redox-compression-corpus",
    about = "Extract a bounded, shape-diverse Deflate corpus from processed PDFs"
)]
struct Args {
    /// PDF file or directory tree to scan.
    input: PathBuf,
    /// Destination directory. It must not already contain corpus files.
    output: PathBuf,
    /// Maximum number of retained payloads.
    #[arg(long, default_value_t = DEFAULT_MAX_SAMPLES)]
    max_samples: usize,
    /// Maximum total decoded bytes retained across all payloads.
    #[arg(long, default_value_t = DEFAULT_MAX_DECODED_BYTES)]
    max_decoded_bytes: usize,
    /// Maximum decoded size of one retained payload.
    #[arg(long, default_value_t = DEFAULT_MAX_SAMPLE_BYTES)]
    max_sample_bytes: usize,
    /// Representatives retained for one coarse shape from a single PDF.
    #[arg(long, default_value_t = DEFAULT_PER_FILE_SHAPE)]
    per_file_shape: usize,
    /// Maximum retained payloads contributed by one PDF.
    #[arg(long, default_value_t = DEFAULT_PER_FILE_SAMPLES)]
    per_file_samples: usize,
    /// Representatives retained for one coarse shape across the full corpus.
    #[arg(long, default_value_t = DEFAULT_GLOBAL_SHAPE)]
    global_shape: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
struct ShapeKey {
    kind: CompressionSampleKind,
    size_log2: u8,
    source_ratio_decile: u8,
    entropy_half_bits: u8,
    zero_decile: u8,
    ascii_decile: u8,
}

impl ShapeKey {
    fn from_candidate(candidate: &CompressionSampleCandidate) -> Self {
        let len = candidate.decoded.len();
        Self {
            kind: candidate.kind,
            size_log2: usize::BITS
                .saturating_sub(len.leading_zeros())
                .saturating_sub(1) as u8,
            source_ratio_decile: ratio_decile(
                if candidate.source_was_flate {
                    candidate.encoded_bytes
                } else {
                    len
                },
                len,
            ),
            entropy_half_bits: (candidate.entropy_bits_per_byte * 2.0)
                .round()
                .clamp(0.0, 16.0) as u8,
            zero_decile: ratio_decile(candidate.zero_bytes, len),
            ascii_decile: ratio_decile(candidate.ascii_like_bytes, len),
        }
    }
}

fn ratio_decile(numerator: usize, denominator: usize) -> u8 {
    if denominator == 0 {
        return 0;
    }
    let value = numerator.saturating_mul(10) / denominator;
    value.min(10) as u8
}

#[derive(Debug, Serialize)]
struct ManifestEntry {
    sample: String,
    decoded_sha256: String,
    source_pdf: String,
    processed_pdf: String,
    object: String,
    kind: CompressionSampleKind,
    source_was_flate: bool,
    encoded_bytes: usize,
    decoded_bytes: usize,
    processing_flate_bytes: usize,
    size_flate_bytes: usize,
    entropy_bits_per_byte: f64,
    unique_byte_values: u16,
    zero_bytes: usize,
    ascii_like_bytes: usize,
    shape: ShapeKey,
}

#[derive(Debug, Default, Serialize)]
struct Summary {
    input_root: String,
    pdf_candidates: usize,
    pdfs_scanned: usize,
    pdf_failures: usize,
    scan: CompressionSampleScanStats,
    per_file_shape_pruned: usize,
    per_file_budget_pruned: usize,
    exact_duplicates_pruned: usize,
    global_shape_pruned: usize,
    oversize_pruned: usize,
    budget_pruned: usize,
    selected_samples: usize,
    selected_decoded_bytes: usize,
    selected_processing_flate_bytes: usize,
    selected_size_flate_bytes: usize,
    selected_kind_counts: BTreeMap<String, usize>,
    selected_source_pdfs: usize,
    failure_examples: Vec<String>,
}

fn kind_name(kind: CompressionSampleKind) -> &'static str {
    match kind {
        CompressionSampleKind::Image => "image",
        CompressionSampleKind::Form => "form",
        CompressionSampleKind::Pattern => "pattern",
        CompressionSampleKind::Font => "font",
        CompressionSampleKind::Icc => "icc",
        CompressionSampleKind::Metadata => "metadata",
        CompressionSampleKind::EmbeddedFile => "embedded-file",
        CompressionSampleKind::ObjectStream => "object-stream",
        CompressionSampleKind::Xref => "xref",
        CompressionSampleKind::Other => "other",
    }
}

fn hex_digest(digest: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(64);
    for &byte in digest {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

fn object_label(handle: ObjectHandle) -> String {
    match handle {
        ObjectHandle::Existing(id) => format!("{} {} R", id.number(), id.generation()),
        ObjectHandle::New(id) => format!("new:{}", id.index()),
    }
}

fn collect_pdfs(path: &Path, output: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if path.is_file() {
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            output.push(path.to_path_buf());
        }
        return Ok(());
    }
    let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::path);
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_pdfs(&path, output)?;
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("pdf"))
        {
            output.push(path);
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct SourceRunEntry {
    sha256: String,
    source_name: String,
}

fn load_source_names(root: &Path) -> BTreeMap<String, String> {
    let mut names = BTreeMap::new();
    let path = root.join("results.jsonl");
    let Ok(contents) = fs::read_to_string(path) else {
        return names;
    };
    for line in contents.lines() {
        let Ok(entry) = serde_json::from_str::<SourceRunEntry>(line) else {
            continue;
        };
        names.insert(format!("{}.pdf", entry.sha256), entry.source_name);
    }
    names
}

fn display_source(root: &Path, path: &Path) -> String {
    if root.is_dir() {
        path.strip_prefix(root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    } else {
        path.file_name()
            .unwrap_or(path.as_os_str())
            .to_string_lossy()
            .into_owned()
    }
}

fn add_scan_stats(total: &mut CompressionSampleScanStats, current: CompressionSampleScanStats) {
    total.reachable_streams += current.reachable_streams;
    total.unsupported_filter_streams += current.unsupported_filter_streams;
    total.empty_streams += current.empty_streams;
    total.decode_failures += current.decode_failures;
    total.candidates += current.candidates;
}

fn prepare_output(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() {
        let mut entries = fs::read_dir(path)?;
        if entries.next().transpose()?.is_some() {
            return Err(format!("output directory is not empty: {}", path.display()).into());
        }
    } else {
        fs::create_dir_all(path)?;
    }
    fs::create_dir_all(path.join("samples"))?;
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    if args.per_file_shape == 0
        || args.per_file_samples == 0
        || args.global_shape == 0
        || args.max_samples == 0
    {
        return Err("selection limits must be non-zero".into());
    }
    prepare_output(&args.output)?;

    let mut pdfs = Vec::new();
    collect_pdfs(&args.input, &mut pdfs)?;
    pdfs.sort();

    let manifest_file = File::create(args.output.join("manifest.jsonl"))?;
    let mut manifest = BufWriter::new(manifest_file);
    let mut summary = Summary {
        input_root: args.input.to_string_lossy().into_owned(),
        pdf_candidates: pdfs.len(),
        ..Summary::default()
    };
    let mut selected_hashes = BTreeSet::new();
    let mut global_shapes = BTreeMap::<ShapeKey, usize>::new();
    let mut selected_sources = BTreeSet::new();
    let source_names = if args.input.is_dir() {
        load_source_names(&args.input)
    } else {
        BTreeMap::new()
    };

    for path in &pdfs {
        let processed_pdf = display_source(&args.input, path);
        let source = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| source_names.get(name))
            .cloned()
            .unwrap_or_else(|| processed_pdf.clone());
        let input = match fs::read(path) {
            Ok(input) => input,
            Err(error) => {
                summary.pdf_failures += 1;
                if summary.failure_examples.len() < 20 {
                    summary.failure_examples.push(format!("{source}: {error}"));
                }
                continue;
            }
        };

        let mut per_file = BTreeMap::<ShapeKey, Vec<CompressionSampleCandidate>>::new();
        let scan = visit_compression_sample_candidates(&input, |candidate| {
            let shape = ShapeKey::from_candidate(&candidate);
            let bucket = per_file.entry(shape).or_default();
            if bucket.len() < args.per_file_shape {
                bucket.push(candidate);
            } else {
                summary.per_file_shape_pruned += 1;
            }
        });
        let scan = match scan {
            Ok(scan) => scan,
            Err(error) => {
                summary.pdf_failures += 1;
                if summary.failure_examples.len() < 20 {
                    summary.failure_examples.push(format!("{source}: {error}"));
                }
                continue;
            }
        };
        summary.pdfs_scanned += 1;
        add_scan_stats(&mut summary.scan, scan);

        let mut file_candidates = per_file
            .into_iter()
            .flat_map(|(shape, candidates)| {
                candidates
                    .into_iter()
                    .map(move |candidate| (shape.clone(), candidate))
            })
            .collect::<Vec<_>>();
        // Digest order is deterministic but pseudo-random with respect to PDF object order and
        // stream kind, so the per-file cap cannot systematically prefer one semantic family.
        file_candidates.sort_by_key(|(_, candidate)| candidate.decoded_sha256);
        let mut selected_from_file = 0_usize;
        for (shape, candidate) in file_candidates {
            if selected_from_file >= args.per_file_samples {
                summary.per_file_budget_pruned += 1;
                continue;
            }
            if candidate.decoded.len() > args.max_sample_bytes {
                summary.oversize_pruned += 1;
                continue;
            }
            if selected_hashes.contains(&candidate.decoded_sha256) {
                summary.exact_duplicates_pruned += 1;
                continue;
            }
            if global_shapes.get(&shape).copied().unwrap_or(0) >= args.global_shape {
                summary.global_shape_pruned += 1;
                continue;
            }
            if summary.selected_samples >= args.max_samples
                || summary
                    .selected_decoded_bytes
                    .saturating_add(candidate.decoded.len())
                    > args.max_decoded_bytes
            {
                summary.budget_pruned += 1;
                continue;
            }

            let (processing_flate_bytes, size_flate_bytes) =
                compression_baseline_sizes(&candidate.decoded)?;
            let digest = hex_digest(&candidate.decoded_sha256);
            let sample_rel = format!("samples/{digest}.bin");
            fs::write(args.output.join(&sample_rel), &candidate.decoded)?;
            let entry = ManifestEntry {
                sample: sample_rel,
                decoded_sha256: digest,
                source_pdf: source.clone(),
                processed_pdf: processed_pdf.clone(),
                object: object_label(candidate.object),
                kind: candidate.kind,
                source_was_flate: candidate.source_was_flate,
                encoded_bytes: candidate.encoded_bytes,
                decoded_bytes: candidate.decoded.len(),
                processing_flate_bytes,
                size_flate_bytes,
                entropy_bits_per_byte: candidate.entropy_bits_per_byte,
                unique_byte_values: candidate.unique_byte_values,
                zero_bytes: candidate.zero_bytes,
                ascii_like_bytes: candidate.ascii_like_bytes,
                shape: shape.clone(),
            };
            serde_json::to_writer(&mut manifest, &entry)?;
            manifest.write_all(b"\n")?;

            selected_hashes.insert(candidate.decoded_sha256);
            *global_shapes.entry(shape).or_default() += 1;
            selected_from_file += 1;
            summary.selected_samples += 1;
            summary.selected_decoded_bytes += candidate.decoded.len();
            summary.selected_processing_flate_bytes += processing_flate_bytes;
            summary.selected_size_flate_bytes += size_flate_bytes;
            *summary
                .selected_kind_counts
                .entry(kind_name(candidate.kind).to_owned())
                .or_default() += 1;
            selected_sources.insert(source.clone());
        }
    }
    manifest.flush()?;
    summary.selected_source_pdfs = selected_sources.len();

    fs::write(
        args.output.join("summary.json"),
        format!("{}\n", serde_json::to_string_pretty(&summary)?),
    )?;
    let readme = format!(
        "# pdf-redox Deflate compression corpus\n\n\
Derived from processed PDFs under `{}`. Payloads are decoded bytes from reachable streams that are\n\
unfiltered or use exactly one Flate filter. JPEG/JPX/CCITT and other unrelated codec payloads are\n\
excluded. Exact decoded duplicates are removed globally. Similar samples are bounded by a coarse\n\
shape key (semantic kind, log2 size, source compression-ratio decile, entropy half-bit bucket,\n\
zero-byte decile, ASCII-like decile), with at most {} representative(s) per shape in one source PDF,\n\
{} total sample(s) from one source PDF, and {} representative(s) globally per shape.\n\n\
Selected: {} samples, {} decoded bytes from {} source PDFs.\n\n\
`manifest.jsonl` records provenance and baseline pdf-redox level-7/level-9 compressed sizes.\n",
        args.input.display(),
        args.per_file_shape,
        args.per_file_samples,
        args.global_shape,
        summary.selected_samples,
        summary.selected_decoded_bytes,
        summary.selected_source_pdfs,
    );
    fs::write(args.output.join("README.md"), readme)?;

    println!("{}", serde_json::to_string_pretty(&summary)?);
    Ok(())
}
