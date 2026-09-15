# pdf-redox thread handoff — 2026-09-15

## Canonical locations

- Server working tree: `/srv/scratch/pdf-desht`
- GitHub: `https://github.com/codeandsolder/pdf-redox`
- Branch: `main`
- Canonical Notion project: `https://app.notion.com/p/Datasheet-PDF-normalization-compression-research-3d4f74be9020816db31be12bf88871f6`
- Architecture page: `https://app.notion.com/p/3dbf74be90208135ade6e9d916d40ffa`
- Validation detail page: `https://app.notion.com/p/3dbf74be90208145b1e8fb3ad5d56670`
- Representative production corpus: `/srv/scratch/pdf-redox-writer-corpus`
- Fully validated parser-free baseline: `/srv/scratch/pdf-desht-build/hayro-parser-free-final-20260915/output`
- Post-quality-pass equivalence run: `/srv/scratch/pdf-desht-build/hayro-quality-final-20260915/output`

## Current production architecture

The Hayro/COW migration is complete. Production `analyze_pdf()` and `optimize_pdf()` use:

`immutable/lazy Hayro source -> sparse copy-on-write overlay -> optimization passes -> compact fresh writer`

`flpdf` is no longer used as a document parser or mutable production document graph. It remains vendored for detached, document-agnostic utilities whose implementations are already mature: content-token parsing, stream filter codecs, image transforms, and compatibility/reference tests.

A minimal patched `hayro-syntax` 0.7.2 is vendored under `third_party/hayro-syntax`. The local change exposes final trailer state and xref object IDs needed by the COW layer, eliminating the former temporary flpdf trailer parse.

## Final commits

- `a5eb1f2` — `Complete Hayro COW production migration`
- `b81c7e9` — `Refactor COW traversal and tighten Rust quality checks`

The quality pass centralizes read-only output-graph traversal, removes duplicated Hayro holder scans and specialized stream-dedup implementations, replaces the form dependency tuple with named state, aligns `sha2` to 0.11 so `cargo tree -d` is empty, pins the development toolchain to Rust 1.98.1, and adds a real Rust 1.92 MSRV CI job.

## Validation state

The completed parser-free migration was compared against the previously pixel/text-validated Hayro production baseline on 405 PDFs / 17,612 pages:

- 405/405 optimized outputs byte-for-byte identical;
- aggregate output exactly `627,863,773` bytes on both sides;
- baseline page counts and `pdftotext` output exact on 405/405 files;
- baseline render comparison exact on 17,612/17,612 pages at 12 DPI;
- 75 preserved pre-migration analysis reports exact on every optimization-relevant field, including hidden-text findings and warnings.

After integrating the rust-skills2 quality refactor, the complete 405-file corpus was optimized again and remains 405/405 byte-for-byte identical to the parser-free baseline.

Final code gates after integration:

- `cargo +1.98.1 fmt --all -- --check` — pass;
- `cargo +1.98.1 clippy --locked --workspace --all-targets --all-features -- -D warnings` — pass;
- `cargo +1.98.1 test --locked --workspace --all-features` — 87/87 unit tests plus doctests pass;
- `cargo +1.92.0 check --locked --workspace --all-targets --all-features` — declared MSRV pass;
- `cargo +1.98.1 check --locked -p pdf-redox-wasm --target wasm32-unknown-unknown` — pass;
- `cargo +1.98.1 tree -d` — no duplicate dependency versions.

Use `TMPDIR=/srv/scratch/pdf-desht/.tmp` for large Rust builds/tests because the host `/tmp` tmpfs can fill during rustdoc/MSRV compilation.

## Migration bugs found by corpus validation

Two issues were caught before committing the final architecture:

1. A lazy detached direct stream in `C6488617.pdf` exposed an assumption that flpdf helper results were already concrete. Direct values are now materialized before type inspection.
2. `esp32-p4_technical_reference_manual_en.pdf` exposed a deep/cyclic resource graph accidentally copied into the detached Flate encoder bridge. Recompression now detaches only `/Filter` and `/DecodeParms`, the only dictionary keys the encoder consumes.

Both files now produce byte-identical output to the validated baseline.

## Remaining technical debt / next work

The migration itself is no longer the active project. Reasonable follow-ups are independent product/optimizer work:

- upstream or otherwise maintain the tiny Hayro trailer/xref accessor patch;
- broaden public API documentation (`missing_docs` is intentionally not enabled globally yet);
- add crates.io metadata (`readme`, `keywords`, `categories`) if publishing the crates becomes a goal;
- continue optimization research (font normalization/subsetting, semantic resource reuse, repeated page furniture, raster policy) as separate features with the same corpus gates;
- keep flpdf detached helpers only where they provide tested algorithms; new document-level behavior should stay on Hayro/COW.

## Git discipline

`main` tracks `origin/main`. The migration and quality work are committed locally. Before starting new feature work, verify whether the current commits have been pushed and whether CI is green remotely.
