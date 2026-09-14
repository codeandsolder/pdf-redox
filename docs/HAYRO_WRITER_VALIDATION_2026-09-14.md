# Hayro writer validation — 2026-09-14

## Scope

Validation of the trailer-preserving Hayro/COW fresh writer and the current production flpdf optimizer before migrating optimization passes onto the Hayro overlay.

The experimental Hayro writer remains behind `EditDocument::write_compact_experimental()` / hidden `--hayro-rewrite-experimental`; production `optimize_pdf()` still uses flpdf for mutation and mature optimization passes.

## Hayro writer checkpoint

Checkpoint `9a7e5b0` (`Preserve trailer state in Hayro writer`) is fully validated and pushed.

Representative corpus: 405 PDFs, 777,561,367 input bytes, 17,612 pages, including 12 encrypted inputs.

- Hayro rewrite: 405/405.
- Poppler reparse: 405/405.
- Page count: 405/405.
- `pdftotext -enc UTF-8`: 405/405 exact byte match.
- 12 DPI Poppler rendering: 17,612/17,612 pixel-identical pages.
- Encrypted inputs rewrite to ordinary unencrypted fresh PDFs without stale `/Encrypt` state.
- Hayro writer output: 819,424,570 bytes, aggregate 1.053839098x input; median per-file ratio 1.001604x.

The size tail is caused by expansion of compressed object streams into ordinary indirect objects plus classic xref. This is a compactness follow-up, not duplicated stream payload data.

## Production flpdf comparison

Production `--profile optimize` succeeds on all 405 files. Before the writer compatibility fix it produced 643,059,608 bytes (0.8270210369x input), substantially smaller than the current Hayro writer because it performs the complete optimization pipeline and uses mature object-stream output.

The original production comparison found one semantic mismatch: `C157624.pdf` lost the visible/textual `Send Feedback` form invocation on all 72 pages. Extracted text was exact on 404/405 files and the old case-sensitive render harness showed 72 mismatching pages, all in that one file.

## Empty-stream compression bug

The failure reduced to the bare flpdf writer and specifically to compressed stream emission. `StreamDataMode::Preserve` and `StreamDataMode::Uncompress` preserved the file; default/compress mode did not.

The problematic source page had an empty content stream before later page content. flpdf rewrote the empty stream as a zero-byte stream with `/Filter /FlateDecode`. That is not a valid Flate stream. Poppler encountered the malformed stream in the page `/Contents` array and stopped processing subsequent streams, skipping the later `q /Iabc12014 Do Q` invocation.

Current qpdf has an explicit writer-level compatibility special case for empty streams: keep the stream on the filtered path so existing filters are removed, but disable compression rather than labelling zero output as Flate. flpdf had correctly modelled the low-level delayed zlib initialization but had missed this writer-level rule.

The fix therefore:

- recognizes materialized `/Length 0` in the canonical filter plan;
- returns a filtered plan with compression disabled;
- decides whether to add replacement `/FlateDecode` from actual `STREAM_ENCODE_COMPRESS` flags rather than the broader global compression policy;
- keeps source `/Filter`/`/DecodeParms` removal semantics intact.

Two regression layers cover it:

1. flpdf writer unit test: an empty stream remains zero bytes, source filter parameters are removed, and no replacement Flate filter is emitted.
2. pdf-redox production-level test: a page with an empty content stream followed by drawing content must retain the drawing after `optimize_pdf()` and the empty output stream must have no filter.

A scan of the old 405-output production baseline found exactly two PDFs with this malformed zero-byte-Flate pattern: `C157624.pdf` and `Electrophoresis in Practice 4th ed.www.forumakademi.org.pdf`.

After the fix:

- `C157624.pdf`: exact extracted text restored; 72/72 pages pixel-identical at 12 DPI; output shrinks by 32 bytes.
- `Electrophoresis in Practice 4th ed.www.forumakademi.org.pdf`: exact extracted text; 427/427 pages pixel-identical at 12 DPI; output shrinks by 20 bytes.
- patched 405-output scan: zero `/Length 0` + `/FlateDecode` dictionaries.

## Corrected production corpus

The corrected full production rerun completed:

- rewrite/reparse/page-count: 405/405;
- extracted text exact: 405/405;
- output total: 643,059,556 bytes = 0.8270209700x input (~17.30% smaller);
- only two files change output size relative to the old baseline: `C157624.pdf` (-32 bytes) and `Electrophoresis in Practice 4th ed.www.forumakademi.org.pdf` (-20 bytes).

The corrected all-case render harness includes both `.pdf` and `.PDF`: **405/405 files and 17,612/17,612 pages are pixel-identical at 12 DPI**, with zero render failures, missing outputs, page-count mismatches, differing channel bytes, or non-zero pixel deltas.

## Code gates

Final current-tree gates at `321d9f7` are green: `cargo-fmt --all --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features` (**80/80 tests pass**); `cargo check -p pdf-redox-wasm --target wasm32-unknown-unknown`; and `git diff --check`.

Standalone flpdf validation with the writer regression previously passed its full 2,718-test suite. The pdf-redox workspace-visible regression also passes independently.

## Upstream Hayro bridge

The temporary pdf-redox trailer bridge still uses flpdf only to parse the final trailer shell over the same shared source buffer. The preferred end state is the small Hayro accessor preserved in `docs/hayro-trailer-access.patch`.

A clean Hayro worktree at `/srv/scratch/hayro-trailer-upstream` now has that patch committed locally as `d950536` (`Expose final trailer dictionary from XRef`) on branch `pdf-redox-expose-trailer`. The worktree is clean. The commit has not been pushed upstream and no PR has been opened.

## Performance comparison

A representative benchmark harness is preserved as `bench_hayro_vs_flpdf.py`, but the final timing run was deliberately deferred on this server: it is restricted to one effective CPU while unrelated long-running jobs kept load around 7–12 with substantial storage wait. Running it concurrently would produce misleading wall-time numbers. Re-run the harness on an otherwise idle host (or a dedicated instance) before treating wall time as evidence; production-vs-Hayro is also intentionally end-to-end rather than writer-only.

Production-vs-Hayro timing is deliberately described as end-to-end rather than writer-only: production `--profile optimize` runs analysis, preservation/privacy/hidden-text policies, font work, all configured dedup passes, image transforms, Flate policy, content dedup, and then the flpdf writer. The hidden Hayro mode is essentially a fresh graph rewrite.

## Next migration slice

With writer preservation and the production writer regression checkpointed, migrate production passes incrementally onto the sparse Hayro/COW overlay. Prefer a structurally simple pass first; avoid starting with image transforms or graph-wide dedup because they would prematurely recreate flpdf's eager graph semantics. Every migrated pass should retain corpus structural/text/render gates before moving to the next group.
