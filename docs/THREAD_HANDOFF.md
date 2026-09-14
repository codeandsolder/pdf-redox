# pdf-redox thread handoff — 2026-09-14

## Canonical locations

- Server working tree: `/srv/scratch/pdf-desht`
- GitHub: `https://github.com/codeandsolder/pdf-redox`
- Branch: `main`
- Canonical Notion project: `https://app.notion.com/p/Datasheet-PDF-normalization-compression-research-3d4f74be9020816db31be12bf88871f6`
- Architecture page: `https://app.notion.com/p/3dbf74be90208135ade6e9d916d40ffa`
- Validation detail page: `https://app.notion.com/p/3dbf74be90208145b1e8fb3ad5d56670`
- Representative writer corpus: `/srv/scratch/pdf-redox-writer-corpus`
- Validation artifacts: `/srv/scratch/pdf-desht-build/hayro-writer-validation-20260914`

## Current validated state

Latest validated and pushed production checkpoint: **`321d9f7` — `Fix compression of empty PDF streams`**. The detailed validation record is `docs/HAYRO_WRITER_VALIDATION_2026-09-14.md`; the repository should be rechecked for synchronization after any documentation-only checkpoint.

Target architecture remains:

`immutable/lazy Hayro source -> sparse copy-on-write overlay -> reachability/optimization passes -> compact fresh writer`

Hayro is the selected source/parser layer. Do not fork it pre-emptively. Production `optimize_pdf()` still uses flpdf while passes migrate incrementally; the Hayro writer remains hidden behind `--hayro-rewrite-experimental`.

## Hayro writer

Checkpoint `9a7e5b0` introduced the validated trailer-preserving Hayro/COW writer. On the 405-file / 777,561,367-byte / 17,612-page corpus:

- rewrite/reparse/page count: 405/405;
- text extraction: 405/405 exact;
- 12 DPI render: 17,612/17,612 pixel-identical pages;
- 12 encrypted inputs rewrite correctly without stale `/Encrypt` state;
- output: 819,424,570 bytes (1.053839098x aggregate; median file ratio 1.001604x).

Object-stream expansion/classic xref explains the Hayro size tail. It is a compactness follow-up, not a correctness blocker.

The temporary trailer-shell bridge still uses flpdf over the same shared source buffer. A minimal Hayro accessor is committed locally at `/srv/scratch/hayro-trailer-upstream`, branch `pdf-redox-expose-trailer`, commit `d950536` (`Expose final trailer dictionary from XRef`). The worktree is clean; nothing has been pushed upstream.

## Production flpdf writer compatibility fix

A corpus comparison exposed one real production regression in `C157624.pdf`: all 72 pages lost a later Form XObject invocation after an empty page-content stream.

Root cause: flpdf emitted a zero-byte stream with `/Filter /FlateDecode`. qpdf explicitly disables compression for empty streams because its Flate pipeline produces no zlib bytes until data is written. Poppler stopped processing the page `/Contents` array at the malformed stream.

The writer now keeps empty streams on the filtering path to remove source filter parameters but disables replacement Flate compression. Replacement `/FlateDecode` is derived from actual encode flags, not the global compression policy.

Regression coverage exists in both flpdf and the normal pdf-redox workspace suite. A scan of the old production corpus found exactly two malformed outputs: `C157624.pdf` and `Electrophoresis in Practice 4th ed.www.forumakademi.org.pdf`.

Corrected production corpus:

- rewrite/reparse/page count: 405/405;
- extracted text: 405/405 exact;
- output: 643,059,556 bytes = 0.8270209700x input;
- patched corpus contains zero `/Length 0` + `/FlateDecode` dictionaries;
- `C157624.pdf`: 72/72 pixel-identical pages, -32 bytes vs old output;
- electrophoresis book: 427/427 pixel-identical pages, -20 bytes vs old output.

Corrected all-case rendering is also fully green: **405/405 files, 17,612/17,612 pixel-identical pages at 12 DPI**, including uppercase `.PDF`; zero render failures, page-count mismatches, or differing pixels.

Final current-tree gates at `321d9f7` are green: `cargo-fmt --all --check`; `cargo clippy --workspace --all-targets --all-features -- -D warnings`; `cargo test --workspace --all-features` (**80/80 tests pass**); `cargo check -p pdf-redox-wasm --target wasm32-unknown-unknown`; and `git diff --check`.

## Validation artifacts

Important files under `/srv/scratch/pdf-desht-build/hayro-writer-validation-20260914`:

- `hayro-results.jsonl`, `hayro-summary.json`
- `text-compare.jsonl`, `text-compare-summary.json`
- `render12/`
- `flpdf-optimize-results.jsonl`, `flpdf-optimize-summary.json` — old production baseline
- `flpdf-optimize-fixed/` — corrected production outputs
- `flpdf-optimize-fixed-results.jsonl`
- `flpdf-optimize-fixed-text.jsonl`
- `flpdf-render12-fixed/` — corrected all-case render validation
- `bench_hayro_vs_flpdf.py` — representative resource benchmark harness

## Next ordered work

1. Treat the empty-stream writer fix as a closed compatibility issue once the final checkpoint above is clean/pushed.
2. Decide whether to open the minimal Hayro trailer accessor upstream; do not mix unrelated Rust 1.98 lint cleanup into it.
3. Begin migrating production optimizer passes onto the Hayro/COW overlay one small pass at a time, preserving structural/text/render corpus gates after each group.
4. Prefer a simple dictionary/object mutation pass first. Avoid starting with image transforms or graph-wide dedup because they would pull eager flpdf graph semantics back into the new architecture.
5. Object-stream generation/xref streams remain the main writer-size follow-up once pass migration is underway.

## Git/auth discipline

`origin` is `https://github.com/codeandsolder/pdf-redox.git`, `main` tracks `origin/main`, and a repo-local credential helper is configured. The GitHub token remains outside the repo; never print or embed it in a remote URL.
