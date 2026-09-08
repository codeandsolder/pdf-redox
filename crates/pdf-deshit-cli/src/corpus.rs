use pdf_deshit::{HiddenTextCategory, HiddenTextMechanism, PdfAnalysis, RiskKind, analyze_pdf};
use serde::Serialize;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

const FAILURE_EXAMPLE_LIMIT: usize = 100;

#[derive(Debug, Clone, Serialize)]
pub(crate) struct CorpusFileSummary {
    path: String,
    input_bytes: usize,
    pages: usize,
    objects: usize,
    streams: usize,
    image_bytes: usize,
    font_program_bytes: usize,
    metadata_stream_bytes: usize,
    duplicate_stream_wasted_bytes: usize,
    flate_recompress_potential_saving_bytes: usize,
    hidden_text_findings: usize,
    incremental_updates: usize,
}

impl CorpusFileSummary {
    fn from_analysis(root: &Path, path: &Path, analysis: &PdfAnalysis) -> Self {
        Self {
            path: display_path(root, path),
            input_bytes: analysis.input_bytes,
            pages: analysis.page_count,
            objects: analysis.object_count,
            streams: analysis.stream_count,
            image_bytes: analysis.image_raw_bytes,
            font_program_bytes: analysis.font_program_bytes,
            metadata_stream_bytes: analysis.metadata_stream_bytes,
            duplicate_stream_wasted_bytes: analysis.duplicate_stream_payload_wasted_bytes,
            flate_recompress_potential_saving_bytes: analysis
                .flate_recompress_potential_saving_bytes,
            hidden_text_findings: analysis.hidden_text.len(),
            incremental_updates: analysis.incremental_update_count,
        }
    }
}

#[derive(Debug, Serialize)]
struct CorpusFailure {
    path: String,
    error: String,
}

#[derive(Debug, Default, Serialize)]
struct CorpusTotals {
    input_bytes: u64,
    pages: u64,
    objects: u64,
    streams: u64,
    stream_raw_bytes: u64,
    images: u64,
    image_raw_bytes: u64,
    form_xobjects: u64,
    font_programs: u64,
    font_program_bytes: u64,
    metadata_streams: u64,
    metadata_stream_bytes: u64,
    duplicate_stream_payload_groups: u64,
    duplicate_stream_payload_wasted_bytes: u64,
    flate_streams: u64,
    flate_recompress_candidates: u64,
    flate_recompress_potential_saving_bytes: u64,
    incremental_updates: u64,
    hidden_text_findings: u64,
    warnings: u64,
}

#[derive(Debug, Serialize)]
pub(crate) struct CorpusReport {
    root: String,
    elapsed_ms: u128,
    pdf_candidates: usize,
    analyzed: usize,
    failed: usize,
    totals: CorpusTotals,
    filter_counts: BTreeMap<String, u64>,
    risk_counts: BTreeMap<RiskKind, u64>,
    documents_with_risk: BTreeMap<RiskKind, u64>,
    hidden_text_categories: BTreeMap<HiddenTextCategory, u64>,
    documents_with_hidden_text_category: BTreeMap<HiddenTextCategory, u64>,
    hidden_text_mechanisms: BTreeMap<HiddenTextMechanism, u64>,
    documents_with_hidden_text_mechanism: BTreeMap<HiddenTextMechanism, u64>,
    warning_counts: BTreeMap<String, u64>,
    largest_files: Vec<CorpusFileSummary>,
    top_flate_recompress_savings: Vec<CorpusFileSummary>,
    top_duplicate_stream_waste: Vec<CorpusFileSummary>,
    top_metadata_payload: Vec<CorpusFileSummary>,
    top_font_payload: Vec<CorpusFileSummary>,
    top_hidden_text: Vec<CorpusFileSummary>,
    failure_examples: Vec<CorpusFailure>,
}

pub(crate) fn analyze_corpus(
    root: &Path,
    top: usize,
) -> Result<CorpusReport, Box<dyn std::error::Error>> {
    if !root.is_dir() {
        return Err(format!("corpus input is not a directory: {}", root.display()).into());
    }

    let started = Instant::now();
    let mut files = Vec::new();
    collect_pdfs(root, &mut files)?;
    files.sort();

    let mut totals = CorpusTotals::default();
    let mut filter_counts = BTreeMap::new();
    let mut risk_counts = BTreeMap::new();
    let mut documents_with_risk = BTreeMap::new();
    let mut hidden_text_categories = BTreeMap::new();
    let mut documents_with_hidden_text_category = BTreeMap::new();
    let mut hidden_text_mechanisms = BTreeMap::new();
    let mut documents_with_hidden_text_mechanism = BTreeMap::new();
    let mut warning_counts = BTreeMap::new();
    let mut summaries = Vec::new();
    let mut failure_examples = Vec::new();
    let mut failed = 0_usize;

    for path in &files {
        let input = match std::fs::read(path) {
            Ok(input) => input,
            Err(error) => {
                failed += 1;
                push_failure(root, path, error.to_string(), &mut failure_examples);
                continue;
            }
        };
        let analysis = match analyze_pdf(&input) {
            Ok(analysis) => analysis,
            Err(error) => {
                failed += 1;
                push_failure(root, path, error.to_string(), &mut failure_examples);
                continue;
            }
        };

        accumulate_totals(&mut totals, &analysis);
        accumulate_counts(
            &analysis,
            &mut filter_counts,
            &mut risk_counts,
            &mut documents_with_risk,
            &mut hidden_text_categories,
            &mut documents_with_hidden_text_category,
            &mut hidden_text_mechanisms,
            &mut documents_with_hidden_text_mechanism,
            &mut warning_counts,
        );
        summaries.push(CorpusFileSummary::from_analysis(root, path, &analysis));
    }

    let analyzed = summaries.len();
    let largest_files = top_by(&summaries, top, |row| row.input_bytes);
    let top_flate_recompress_savings = top_by(&summaries, top, |row| {
        row.flate_recompress_potential_saving_bytes
    });
    let top_duplicate_stream_waste =
        top_by(&summaries, top, |row| row.duplicate_stream_wasted_bytes);
    let top_metadata_payload = top_by(&summaries, top, |row| row.metadata_stream_bytes);
    let top_font_payload = top_by(&summaries, top, |row| row.font_program_bytes);
    let top_hidden_text = top_by(&summaries, top, |row| row.hidden_text_findings);

    Ok(CorpusReport {
        root: root.display().to_string(),
        elapsed_ms: started.elapsed().as_millis(),
        pdf_candidates: files.len(),
        analyzed,
        failed,
        totals,
        filter_counts,
        risk_counts,
        documents_with_risk,
        hidden_text_categories,
        documents_with_hidden_text_category,
        hidden_text_mechanisms,
        documents_with_hidden_text_mechanism,
        warning_counts,
        largest_files,
        top_flate_recompress_savings,
        top_duplicate_stream_waste,
        top_metadata_payload,
        top_font_payload,
        top_hidden_text,
        failure_examples,
    })
}

fn collect_pdfs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let path = entry.path();
        if file_type.is_dir() {
            collect_pdfs(&path, out)?;
        } else if file_type.is_file() && is_pdf_path(&path) {
            out.push(path);
        }
    }
    Ok(())
}

fn is_pdf_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
}

fn display_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).map_or_else(
        |_| path.display().to_string(),
        |relative| relative.display().to_string(),
    )
}

fn push_failure(root: &Path, path: &Path, error: String, failures: &mut Vec<CorpusFailure>) {
    if failures.len() < FAILURE_EXAMPLE_LIMIT {
        failures.push(CorpusFailure {
            path: display_path(root, path),
            error,
        });
    }
}

fn accumulate_totals(totals: &mut CorpusTotals, analysis: &PdfAnalysis) {
    totals.input_bytes += analysis.input_bytes as u64;
    totals.pages += analysis.page_count as u64;
    totals.objects += analysis.object_count as u64;
    totals.streams += analysis.stream_count as u64;
    totals.stream_raw_bytes += analysis.stream_raw_bytes as u64;
    totals.images += analysis.image_count as u64;
    totals.image_raw_bytes += analysis.image_raw_bytes as u64;
    totals.form_xobjects += analysis.form_xobject_count as u64;
    totals.font_programs += analysis.font_program_count as u64;
    totals.font_program_bytes += analysis.font_program_bytes as u64;
    totals.metadata_streams += analysis.metadata_stream_count as u64;
    totals.metadata_stream_bytes += analysis.metadata_stream_bytes as u64;
    totals.duplicate_stream_payload_groups += analysis.duplicate_stream_payload_groups as u64;
    totals.duplicate_stream_payload_wasted_bytes +=
        analysis.duplicate_stream_payload_wasted_bytes as u64;
    totals.flate_streams += analysis.flate_stream_count as u64;
    totals.flate_recompress_candidates += analysis.flate_recompress_candidate_count as u64;
    totals.flate_recompress_potential_saving_bytes +=
        analysis.flate_recompress_potential_saving_bytes as u64;
    totals.incremental_updates += analysis.incremental_update_count as u64;
    totals.hidden_text_findings += analysis.hidden_text.len() as u64;
    totals.warnings += analysis.warnings.len() as u64;
}

#[allow(clippy::too_many_arguments)]
fn accumulate_counts(
    analysis: &PdfAnalysis,
    filter_counts: &mut BTreeMap<String, u64>,
    risk_counts: &mut BTreeMap<RiskKind, u64>,
    documents_with_risk: &mut BTreeMap<RiskKind, u64>,
    hidden_text_categories: &mut BTreeMap<HiddenTextCategory, u64>,
    documents_with_hidden_text_category: &mut BTreeMap<HiddenTextCategory, u64>,
    hidden_text_mechanisms: &mut BTreeMap<HiddenTextMechanism, u64>,
    documents_with_hidden_text_mechanism: &mut BTreeMap<HiddenTextMechanism, u64>,
    warning_counts: &mut BTreeMap<String, u64>,
) {
    for (filter, count) in &analysis.filter_counts {
        *filter_counts.entry(filter.clone()).or_default() += *count as u64;
    }

    let mut seen_risks = BTreeSet::new();
    for risk in &analysis.risks {
        *risk_counts.entry(risk.kind.clone()).or_default() += risk.count as u64;
        seen_risks.insert(risk.kind.clone());
    }
    for risk in seen_risks {
        *documents_with_risk.entry(risk).or_default() += 1;
    }

    let mut seen_categories = BTreeSet::new();
    let mut seen_mechanisms = BTreeSet::new();
    for finding in &analysis.hidden_text {
        *hidden_text_categories.entry(finding.category).or_default() += 1;
        *hidden_text_mechanisms.entry(finding.mechanism).or_default() += 1;
        seen_categories.insert(finding.category);
        seen_mechanisms.insert(finding.mechanism);
    }
    for category in seen_categories {
        *documents_with_hidden_text_category
            .entry(category)
            .or_default() += 1;
    }
    for mechanism in seen_mechanisms {
        *documents_with_hidden_text_mechanism
            .entry(mechanism)
            .or_default() += 1;
    }

    for warning in &analysis.warnings {
        *warning_counts.entry(warning.clone()).or_default() += 1;
    }
}

fn top_by(
    summaries: &[CorpusFileSummary],
    count: usize,
    key: impl Fn(&CorpusFileSummary) -> usize,
) -> Vec<CorpusFileSummary> {
    let mut rows = summaries.to_vec();
    rows.sort_unstable_by_key(|row| Reverse(key(row)));
    rows.truncate(count);
    rows
}
