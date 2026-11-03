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

/// A bounded, deterministic priority sample from a single input PDF.
///
/// The smallest decoded SHA-256 digests win, subject to per-shape, per-file,
/// and remaining global byte limits. Unlike one bucket per possible shape,
/// retained decoded memory is bounded independently of the number of shapes.
#[derive(Default)]
struct PerFileCandidates {
    items: Vec<(ShapeKey, CompressionSampleCandidate)>,
    retained_bytes: usize,
}

impl PerFileCandidates {
    fn remove_worst(&mut self, shape: Option<&ShapeKey>) -> bool {
        let index = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, (key, _))| shape.is_none_or(|required| key == required))
            .max_by_key(|(_, (_, candidate))| candidate.decoded_sha256)
            .map(|(index, _)| index);
        let Some(index) = index else {
            return false;
        };
        let (_, removed) = self.items.swap_remove(index);
        self.retained_bytes -= removed.decoded.len();
        true
    }

    fn consider(
        &mut self,
        candidate: CompressionSampleCandidate,
        args: &Args,
        global_shapes: &BTreeMap<ShapeKey, usize>,
        selected_hashes: &BTreeSet<[u8; 32]>,
        summary: &mut Summary,
    ) {
        let size = candidate.decoded.len();
        if size > args.max_sample_bytes {
            summary.oversize_pruned += 1;
            return;
        }
        if summary.selected_samples >= args.max_samples {
            summary.budget_pruned += 1;
            return;
        }
        let remaining_bytes = args
            .max_decoded_bytes
            .saturating_sub(summary.selected_decoded_bytes);
        if size > remaining_bytes {
            summary.budget_pruned += 1;
            return;
        }
        if selected_hashes.contains(&candidate.decoded_sha256)
            || self
                .items
                .iter()
                .any(|(_, retained)| retained.decoded_sha256 == candidate.decoded_sha256)
        {
            summary.exact_duplicates_pruned += 1;
            return;
        }
        let shape = ShapeKey::from_candidate(&candidate);
        if global_shapes.get(&shape).copied().unwrap_or_default() >= args.global_shape {
            summary.global_shape_pruned += 1;
            return;
        }

        self.retained_bytes += size;
        self.items.push((shape.clone(), candidate));
        if self.items.iter().filter(|(key, _)| *key == shape).count() > args.per_file_shape
            && self.remove_worst(Some(&shape))
        {
            summary.per_file_shape_pruned += 1;
        }
        if self.items.len() > args.per_file_samples && self.remove_worst(None) {
            summary.per_file_budget_pruned += 1;
        }
        while self.retained_bytes > remaining_bytes {
            if !self.remove_worst(None) {
                break;
            }
            summary.budget_pruned += 1;
        }
    }

    fn into_sorted(mut self) -> Vec<(ShapeKey, CompressionSampleCandidate)> {
        self.items
            .sort_by_key(|(_, candidate)| candidate.decoded_sha256);
        self.items
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
        let file_type = entry.file_type()?;
        // Do not follow symlinked directories: loops and alias duplicates are common.
        if file_type.is_dir() {
            collect_pdfs(&path, output)?;
        } else if file_type.is_file()
            && path
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

type ToolResult<T> = Result<T, Box<dyn std::error::Error>>;

struct CorpusRun {
    args: Args,
    manifest: BufWriter<File>,
    summary: Summary,
    selected_hashes: BTreeSet<[u8; 32]>,
    global_shapes: BTreeMap<ShapeKey, usize>,
    selected_sources: BTreeSet<String>,
    source_names: BTreeMap<String, String>,
}

impl CorpusRun {
    fn new(args: Args) -> ToolResult<Self> {
        if args.per_file_shape == 0
            || args.per_file_samples == 0
            || args.global_shape == 0
            || args.max_samples == 0
        {
            return Err("selection limits must be non-zero".into());
        }
        prepare_output(&args.output)?;
        let manifest = BufWriter::new(File::create(args.output.join("manifest.jsonl"))?);
        let source_names = if args.input.is_dir() {
            load_source_names(&args.input)
        } else {
            BTreeMap::new()
        };
        let summary = Summary {
            input_root: args.input.to_string_lossy().into_owned(),
            ..Summary::default()
        };
        Ok(Self {
            args,
            manifest,
            summary,
            selected_hashes: BTreeSet::new(),
            global_shapes: BTreeMap::new(),
            selected_sources: BTreeSet::new(),
            source_names,
        })
    }

    fn scan(&mut self) -> ToolResult<()> {
        let mut pdfs = Vec::new();
        collect_pdfs(&self.args.input, &mut pdfs)?;
        pdfs.sort();
        self.summary.pdf_candidates = pdfs.len();
        for path in &pdfs {
            self.scan_pdf(path)?;
        }
        Ok(())
    }

    fn record_failure(&mut self, source: &str, error: impl std::fmt::Display) {
        self.summary.pdf_failures += 1;
        if self.summary.failure_examples.len() < 20 {
            self.summary
                .failure_examples
                .push(format!("{source}: {error}"));
        }
    }

    fn scan_pdf(&mut self, path: &Path) -> ToolResult<()> {
        let processed_pdf = display_source(&self.args.input, path);
        let source = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| self.source_names.get(name))
            .cloned()
            .unwrap_or_else(|| processed_pdf.clone());
        let input = match fs::read(path) {
            Ok(input) => input,
            Err(error) => {
                self.record_failure(&source, error);
                return Ok(());
            }
        };
        let mut per_file = PerFileCandidates::default();
        let scan = visit_compression_sample_candidates(&input, |candidate| {
            per_file.consider(
                candidate,
                &self.args,
                &self.global_shapes,
                &self.selected_hashes,
                &mut self.summary,
            );
        });
        let scan = match scan {
            Ok(scan) => scan,
            Err(error) => {
                self.record_failure(&source, error);
                return Ok(());
            }
        };
        self.summary.pdfs_scanned += 1;
        add_scan_stats(&mut self.summary.scan, scan);
        for (shape, candidate) in per_file.into_sorted() {
            self.write_candidate(&source, &processed_pdf, shape, candidate)?;
        }
        Ok(())
    }

    fn write_candidate(
        &mut self,
        source: &str,
        processed_pdf: &str,
        shape: ShapeKey,
        candidate: CompressionSampleCandidate,
    ) -> ToolResult<()> {
        if self.selected_hashes.contains(&candidate.decoded_sha256) {
            self.summary.exact_duplicates_pruned += 1;
            return Ok(());
        }
        if self.global_shapes.get(&shape).copied().unwrap_or_default() >= self.args.global_shape {
            self.summary.global_shape_pruned += 1;
            return Ok(());
        }
        if self.summary.selected_samples >= self.args.max_samples
            || self
                .summary
                .selected_decoded_bytes
                .saturating_add(candidate.decoded.len())
                > self.args.max_decoded_bytes
        {
            self.summary.budget_pruned += 1;
            return Ok(());
        }

        let (processing_flate_bytes, size_flate_bytes) =
            compression_baseline_sizes(&candidate.decoded)?;
        let digest = hex_digest(&candidate.decoded_sha256);
        let sample_rel = format!("samples/{digest}.bin");
        fs::write(self.args.output.join(&sample_rel), &candidate.decoded)?;
        let entry = ManifestEntry {
            sample: sample_rel,
            decoded_sha256: digest,
            source_pdf: source.to_owned(),
            processed_pdf: processed_pdf.to_owned(),
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
        serde_json::to_writer(&mut self.manifest, &entry)?;
        self.manifest.write_all(b"\n")?;

        self.selected_hashes.insert(candidate.decoded_sha256);
        *self.global_shapes.entry(shape).or_default() += 1;
        self.summary.selected_samples += 1;
        self.summary.selected_decoded_bytes += candidate.decoded.len();
        self.summary.selected_processing_flate_bytes += processing_flate_bytes;
        self.summary.selected_size_flate_bytes += size_flate_bytes;
        *self
            .summary
            .selected_kind_counts
            .entry(kind_name(candidate.kind).to_owned())
            .or_default() += 1;
        self.selected_sources.insert(source.to_owned());
        Ok(())
    }

    fn finish(mut self) -> ToolResult<()> {
        self.manifest.flush()?;
        self.summary.selected_source_pdfs = self.selected_sources.len();

        fs::write(
            self.args.output.join("summary.json"),
            format!("{}\n", serde_json::to_string_pretty(&self.summary)?),
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
            self.args.input.display(),
            self.args.per_file_shape,
            self.args.per_file_samples,
            self.args.global_shape,
            self.summary.selected_samples,
            self.summary.selected_decoded_bytes,
            self.summary.selected_source_pdfs,
        );
        fs::write(self.args.output.join("README.md"), readme)?;
        println!("{}", serde_json::to_string_pretty(&self.summary)?);
        Ok(())
    }
}

fn main() -> ToolResult<()> {
    let mut run = CorpusRun::new(Args::parse())?;
    run.scan()?;
    run.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            input: PathBuf::new(),
            output: PathBuf::new(),
            max_samples: 10,
            max_decoded_bytes: 100,
            max_sample_bytes: 32,
            per_file_shape: 2,
            per_file_samples: 10,
            global_shape: 10,
        }
    }

    fn sample(id: u8, kind: CompressionSampleKind, len: usize) -> CompressionSampleCandidate {
        CompressionSampleCandidate {
            object: ObjectHandle::Existing(pdf_redox::ObjectId::new(1, 0)),
            kind,
            source_was_flate: false,
            encoded_bytes: len,
            decoded: vec![id; len],
            decoded_sha256: [id; 32],
            entropy_bits_per_byte: 0.0,
            unique_byte_values: 1,
            zero_bytes: 0,
            ascii_like_bytes: 0,
        }
    }

    #[test]
    fn per_file_selection_preserves_smallest_hashes_per_shape() {
        let args = args();
        let mut selection = PerFileCandidates::default();
        let mut summary = Summary::default();
        for id in [8, 4, 6, 2, 4] {
            selection.consider(
                sample(id, CompressionSampleKind::Other, 4),
                &args,
                &BTreeMap::new(),
                &BTreeSet::new(),
                &mut summary,
            );
        }
        let retained = selection
            .into_sorted()
            .into_iter()
            .map(|(_, candidate)| candidate.decoded_sha256[0])
            .collect::<Vec<_>>();
        assert_eq!(retained, [2, 4]);
        assert_eq!(summary.per_file_shape_pruned, 2);
        assert_eq!(summary.exact_duplicates_pruned, 1);
    }

    #[test]
    fn per_file_selection_bounded_by_count_and_bytes() {
        let mut args = args();
        args.max_decoded_bytes = 8;
        args.per_file_shape = 10;
        args.per_file_samples = 3;
        let mut selection = PerFileCandidates::default();
        let mut summary = Summary::default();
        for id in [8, 6, 4, 2, 1] {
            selection.consider(
                sample(id, CompressionSampleKind::Other, 4),
                &args,
                &BTreeMap::new(),
                &BTreeSet::new(),
                &mut summary,
            );
            assert!(selection.items.len() <= 3);
            assert!(selection.retained_bytes <= 8);
        }
        let retained = selection
            .into_sorted()
            .into_iter()
            .map(|(_, candidate)| candidate.decoded_sha256[0])
            .collect::<Vec<_>>();
        assert_eq!(retained, [1, 2]);
    }

    #[test]
    fn per_file_selection_obeys_existing_global_limits() {
        let mut args = args();
        args.max_decoded_bytes = 9;
        let mut selection = PerFileCandidates::default();
        let mut summary = Summary {
            selected_decoded_bytes: 6,
            ..Summary::default()
        };
        selection.consider(
            sample(1, CompressionSampleKind::Other, 4),
            &args,
            &BTreeMap::new(),
            &BTreeSet::new(),
            &mut summary,
        );
        assert!(selection.items.is_empty());
        assert_eq!(summary.budget_pruned, 1);
    }
}
