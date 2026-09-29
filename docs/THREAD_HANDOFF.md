# pdf-redox thread handoff — 2026-09-15

## Canonical locations

- Server working tree: `/srv/scratch/pdf-desht`
- GitHub: `https://github.com/codeandsolder/pdf-redox`
- Branch: `raster-layout-normalization` (experimental optimizer work; base HEAD `241f6b9`)
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

## Raster/vector normalization validation — 2026-09-16

The active experimental optimizer tree is now unified between cold storage (`/srv/scratch/pdf-desht`, branch `raster-layout-normalization`) and the Vast test checkout (`/workspace/pdf-redox-current2`, branch `parser-unify-20260916`). The two trees were hash-compared across every modified/untracked implementation file and synchronized only where they differed. The abandoned `oxiarc-tiff` experiment was not retained; there are no remaining `oxiarc-tiff` Cargo references. `/.tmp/` is now ignored because the cold host intentionally uses a repo-local TMPDIR for large Rust builds.

The print profile remains at the intentional 600 PPI product default. Two older tests that specifically exercise 450 PPI downsampling now set `max_image_ppi = Some(450)` explicitly instead of accidentally depending on the historical default.

Latest implementation gates on the unified tree are green:
- Rust 1.98.1 formatting and strict workspace Clippy (`-D warnings`): pass;
- workspace tests: **106/106 pass**, plus doctests;
- `git diff --check`: pass;
- Rust 1.92 MSRV workspace check on the Vast test tree: pass;
- wasm32 `pdf-redox-wasm` check: pass;
- `cargo tree -d`: no duplicate dependency versions.

### Exact-render acceptance sample

The final combined experimental command was:
`--profile optimize --normalize-raster-layout --compact-vector-paths --prune-resources`

Nine deliberately awkward PDFs were compared source-vs-output with Poppler `pdftoppm -r 12` using raw PPM byte equality. Final result: **9/9 PDFs, 4,349/4,349 pages pixel-identical**.

The sample includes `C2275.pdf`, the historical malformed-empty-Flate cases (`C157624.pdf` and the 427-page electrophoresis book), the lazy-stream regression (`C6488617.pdf`), `ALLK.pdf`, `C5754309.pdf`, `OP07CDR.pdf`, ESP32-C6, and the 3,078-page ESP32-P4 manual that previously caught the deep/cyclic resource-detachment bug. ESP32-P4 itself finished **3,078/3,078 exact**, outputting 20,628,610 bytes from a 21,110,639-byte source.

The two documents involved in the final hidden-text regression (`C6488617.pdf` and the electrophoresis book) also retain byte-identical `pdftotext -enc UTF-8` output after the fix.

### Render regression 1: vector fill compaction

`ALLK.pdf` page 200 exposed a real low-resolution rasterization difference. Vector compaction merged separately painted adjacent rectangles into the mathematically identical union, but one member was only 4.68 pt wide. At 12 DPI Poppler device-snapped the independent thin paint to a visible pixel column; after merging, three yellow pixels disappeared. The source and compacted page were already identical at 24, 48, 72, 96, 150, and 300 DPI.

Fix: vector compaction now refuses an adjacent-rectangle merge when either member is under **6 pt in page space**, i.e. under one pixel at the 12 DPI corpus-validation floor. Regression coverage is `does_not_merge_page_space_thin_member`.

General lesson: identical continuous geometry does not imply identical device rasterization when independently painted subpixel geometry is coalesced.

### Render regression 2: hidden-text physical pruning

After the vector fix, `C6488617.pdf` page 61 and the electrophoresis book page 148 still differed. Pass bisection showed raster-layout normalization alone was responsible. In both cases the hidden-text analyzer itself classified the affected strings as `covered-by-opaque-fill`, category `other-invisible`, `suggested_action = keep`, but the physical-prune helper ignored that policy and deleted every occlusion finding based on approximate text bounds.

Fix: zero-opacity text remains eligible for physical removal when it is not a semantic OCR/accessibility layer, but fill/image occlusion is pruned only when the analyzer itself returns `suggested_action = remove`. The regression test `physical_prune_respects_ambiguous_occlusion_policy` covers ambiguous occlusion, likely-redaction removal, and zero-opacity behavior. The CLI/config documentation was also corrected to stop calling approximate occlusion “provably invisible.”

General lesson: approximate glyph bounds are evidence, not proof of invisibility; destructive normalization must preserve uncertain findings.

## Working-tree discipline after the 2026-09-16 validation

The raster/vector feature work was already an uncommitted experimental tree before this validation pass. It remains intentionally uncommitted rather than retroactively bundling all pre-existing experimental changes into a surprise commit. Before committing, review the feature diff as one coherent change set and decide whether to split raster layout, vector compaction, shared hidden-text scanning, reporting/CLI wiring, and flpdf helper changes into separate commits.

## Processing optimizer checkpoint — 2026-09-18

Current accepted frozen release:

- binary: `/srv/scratch/pdf-desht-build/corpus-opportunities-candidate11-20260918/pdf-redox`
- binary SHA-256: `dd0ad5faa7ae1c28f6e58137f9298e5a06f8bba1ce647a9f4b4e0562452636ff`
- clean target-pruned source SHA-256: `3ce246094793f1670b0c137ec587c8f19312d9526445e66372fd9a902d2b7d34`
- source snapshot: `/srv/scratch/pdf-desht-build/corpus-opportunities-candidate11-20260918/source-snapshot.tar.zst`
- working tree remains the broad uncommitted `raster-layout-normalization` experiment; do not surprise-commit/reset it.

Full source-hash-locked acceptance gate is green: workspace tests + doctests, strict workspace/all-target/all-feature Clippy, rustdoc `-D warnings`, Rust 1.92 MSRV, wasm32, no duplicate dependency versions, and fuzz manifest. Source hash matched before/after.

Candidate 10 -> candidate 11 identity sweep: 50/50 SHA-identical, covering 25 historical vector hits and 25 no-ops, zero failures/mismatches.

### Exact Form-XObject impossibility preflight

Inherited from candidate 10. Reachable `/Subtype /Form` streams are collected once and their raw encoded payload digests checked before recursive resource/font/image normalization. If every raw payload is unique, exact Form dedup is impossible because the full fingerprint contains those raw bytes verbatim, so the expensive resource-graph/fixed-point/holder-rewrite work is skipped.

Focused candidate-9 -> candidate-10 stress set: aggregate Form-dedup 19.367 s -> 4.668 s (~76% reduction), wall 120.951 s -> 109.183 s, 9/9 outputs SHA-identical. The real GigaDevice positive remains exactly 1 duplicate Form / 1 rewritten reference / 5,035 raw bytes.

### Exact Image-XObject impossibility preflight

Candidate 11 generalizes the same proof to exact Image XObjects. Reachable `/Subtype /Image` streams are collected once; if every raw image payload digest is unique, neither exact mask canonicalization nor parent-image exact dedup can find a duplicate, so both fingerprint passes plus XObject-holder rewrite scanning are skipped.

Any repeated payload digest falls through to the previous exact two-pass image implementation unchanged. A digest collision can only cause a conservative fallback, never a false skip.

Mixed candidate-10 -> candidate-11 stress A/B (seven expensive no-hits + four real positives):
- aggregate image-dedup 7.877 s -> 5.869 s (~25.5% reduction);
- aggregate wall 118.462 s -> 105.975 s;
- 11/11 outputs SHA-identical;
- Fortior MCU no-hit 1.809 s -> 0.659 s;
- Bosch no-hit 0.778 s -> 0.328 s;
- GigaDevice DDR no-hit 0.493 s -> 0.243 s;
- Allwinner no-hit 0.332 s -> 0.191 s.

Positive image-dedup results remain exact:
- Fortior BLDC: 129 duplicate images / 129 rewrites / 16,431 raw bytes;
- Anlogic: 240 / 240 / 132,000;
- Geehy MCU: 22 / 22 / 3,916;
- Awinic: 156 / 156 / 957,454.

### Rejected follow-ups

Candidate 12 added raw-payload impossibility preflights to page-content and annotation-appearance dedup. It remained byte-exact but was a net loss: page-content aggregate 3.374 s -> 3.774 s and wall 108.699 s -> 109.942 s. The page-content portion was reverted.

Candidate 13 isolated appearance-only. It also remained byte-exact but regressed overall: appearance aggregate 4.782 s -> 5.426 s and wall 125.842 s -> 136.853 s. Some documents improved sharply (e.g. GigaDevice MCU and C&K), but Fortior MCU/Bosch paid more than those gains. Candidate 13 was rejected.

The live source was restored bit-for-bit to accepted candidate 11 and verified against source hash `3ce246094793f1670b0c137ec587c8f19312d9526445e66372fd9a902d2b7d34`.

### Next optimization targets

Remeasure on candidate 11 before modifying source:
1. raster-layout normalization;
2. repeated-page-object removal;
3. resource pruning.

Historical no-op time before the candidate-9/10/11 dedup improvements was roughly 443 s, 153 s, and 109 s respectively, but those totals are stale enough that current candidate-11 measurements should determine priority.

Detailed worklog: `https://app.notion.com/p/3dff74be902081aabfc9d8bc4f388dc5`


## Raster-layout performance checkpoint — 2026-09-19

### Accepted candidate 14 — shared decoded raster storage

Candidate 14 supersedes candidate 11 as the accepted optimizer checkpoint.

- Frozen release: `/srv/scratch/pdf-desht-build/corpus-opportunities-candidate14-20260919/pdf-redox`
- Binary SHA-256: `eaf496c642cb1a848c39134a443b8988bf4936490762c2ca6b1bc936e89ba1aa`
- Change: `SampleImage.data` and alpha raster buffers use shared `Arc<[u8]>` storage, so the hidden-visibility cache can be reused by merge planning without deep-copying decoded multi-megabyte image buffers. Rewrites convert to owned `Vec` only when modified raster bytes actually need to be emitted.
- Correctness surface: no raster-selection, merge eligibility, PDF syntax, or rewrite semantics changed.

Prepared candidate-11 -> candidate-14 A/B:
- 6/6 whole outputs SHA-identical.
- Both real raster-positive controls retained identical raster statistics and whole-file bytes.
- Aggregate `raster/image-materialize`: **8301.319 ms -> 15.857 ms**.
- CKS positive control: **3547.946 ms -> 1.652 ms** image-materialize, raster-layout **17.284 s -> 13.572 s**, whole output SHA-identical.
- Ebyte no-hit: **4396.992 ms -> 1.323 ms** image-materialize in the full run. A separate uncontended run showed the expected multi-second wall/raster win as well.
- Whole-run wall time was effectively flat (126.416 s -> 126.456 s) because large run-to-run variance in unrelated target scanning on C&K/Fortior masked the eliminated copy cost. Treat the stage-local copy elimination plus exact positive controls as the acceptance evidence, not that noisy aggregate wall number.

Artifacts:
- `/srv/scratch/pdf-desht-build/bench_candidate14_raster_arc_20260919.py`
- `/srv/scratch/pdf-desht-build/bench_candidate14_raster_arc_20260919.log`
- `/srv/scratch/pdf-desht-build/candidate14-raster-arc-ab-20260919/summary.json`

### Rejected candidate 15 — hidden-text deep-scan gate (historical)

Candidate 15 was the first hidden-text pre-gate experiment. It is rejected; the section below preserves the original design rationale for archaeology.

Observation: raster target scanning currently invokes the full physical-hidden-text scanner on every page whenever hidden-paint pruning is enabled. That scanner builds font metrics, ToUnicode maps, ExtGState/image/property metadata, and performs full hidden-text geometry tracking even on pages that cannot contain a physically-prunable mechanism.

The raster scanner already has a conservative necessary-condition proof for the only physical hidden-text mechanisms this optimizer actually removes:
- zero-opacity text requires text painted under an ExtGState;
- later opaque-fill/image occlusion requires a covering paint after text.

Candidate 15 therefore performs the cheap raster/vector parse first. Pages with neither signal record an exact empty shared hidden-text result without constructing the expensive hidden-text resource model. Candidate pages run the full hidden-text scan as a second pass. Pages later rewritten by raster reconstruction still fall back to the existing post-rewrite hidden-text path.

The candidate-14 source files touched by this experiment were snapshotted first under:
`/srv/scratch/pdf-desht-build/candidate14-source-20260919/`

Historical validation plan:
- gate: library tests, strict workspace Clippy, release build;
- A/B: candidate 14 vs 15 on the six raster timing controls plus `C6488617.pdf` and `thermo-electrophoresis-handbook.pdf` as hidden-text-sensitive regression controls;
- require whole-output SHA equality and identical raster/hidden-text result counters.


### Candidates 15–17 outcome

The hidden-text pre-gate line is rejected. Candidates 15–17 all preserved output exactly, including explicit synthetic zero-opacity and dark-cover destructive positives, but the gating work caused target-scan regressions. Candidate 17 was the narrowest version (dark single-`re` fill tracking, direct fill-color state, `q/Q` restoration, malformed-color handling); it passed 170/170 tests plus strict Clippy but still moved C&K target scan about 2.80 s -> 3.77 s and Ebyte about 266 ms -> 344 ms. Do not revive this approach without an architectural change. Candidate-17 source evidence is preserved at `/srv/scratch/pdf-desht-build/candidate17-source-final-20260919/`; frozen binary SHA-256 `ea3eb2ef9f71e04be44ef03bc98e7a13961c6bf1c305ed5d563021db291c5b20`.

### Accepted candidate 18 — bounded cross-target decoded-image cache

Candidate 18 supersedes candidate 14 as the accepted optimizer checkpoint.

- Frozen release: `/srv/scratch/pdf-desht-build/corpus-opportunities-candidate18-20260919/pdf-redox`
- Binary SHA-256: `c6d57c361b723ebae5f3193a3597ca56affd63c44e3337c3495787766eb16e3d`
- Source snapshot: `/srv/scratch/pdf-desht-build/candidate18-source-20260919/`
- Snapshot `raster_layout.rs` SHA-256: `45b357ce4a951f3b7f3a236dd849824a0547a1167ce93919a351f6d70c7f2653`

Candidate 18 caches the exact existing `image_info(document, handle, true)` result across page/Form targets for one raster-layout normalization pass. Candidate 14 already made decoded color/alpha buffers `Arc<[u8]>`, so cache hits share immutable raster bytes. No selection, visibility, rewrite, or PDF-syntax semantics changed. The cache is bounded to 128 MiB decoded raster payload / 1,024 entries and clears itself when the budget is reached.

Acceptance evidence:
- full project gate green: fmt, `git diff --check`, strict workspace/all-target/all-feature Clippy, 164/164 workspace tests plus doctests, rustdoc `-D warnings`, Rust 1.92 MSRV, wasm32, empty `cargo tree -d`, fuzz manifest;
- seven-document candidate-14 -> candidate-18 stress A/B: 7/7 whole outputs SHA-identical, with exact raster-result counters on both positive controls;
- aggregate hidden-visibility 25.011 s -> 3.479 s (~86% reduction); aggregate raster-layout 86.985 s -> 61.336 s despite severe unrelated shared-host timing noise;
- Ebyte LoRa loaded A/B hidden-visibility 8.296 s -> 0.297 s; a separate instrumented run recorded 56 cache hits / 18 misses, 65,852,476 retained decoded bytes and zero clears;
- Ebyte RF hidden-visibility 2.132 s -> 0.282 s;
- CKS positive retained exactly 931 mask bakes, 24,990,447 cropped pixels and 157 native-fragment paints while hidden-visibility fell 11.159 s -> 0.666 s;
- Fortior BLDC positive retained exactly 413 mask bakes / 70,529 cropped pixels while hidden-visibility fell 0.959 s -> 0.215 s;
- Ebyte peak RSS 96,596 KiB -> 113,140 KiB (+16,544 KiB, ~17%) while the same sampled run's wall time fell 14.04 s -> 3.50 s.

Artifacts: `/srv/scratch/pdf-desht-build/candidate18-shared-image-cache-ab-20260919/summary.json`, `/srv/scratch/pdf-desht-build/candidate18-memory-20260919/summary.json`, `/srv/scratch/pdf-desht-build/candidate18_full_gate_20260919.log`. Supplemental broad validation completed **50/50 SHA-identical** on the standard stratified set (25 historical vector hits + 25 no-ops), with zero failures/mismatches. Summary: `/srv/scratch/pdf-desht-build/candidate18-identity50-20260919/summary.json`.

Next raster work should target allocation/work inside the existing one-pass shared raster/vector/hidden callback stack. Do not add another preliminary hidden-text scan: candidates 15–17 already demonstrated that this loses on real documents.

### Candidate 19 — rejected shared `Rc<Vec<u8>>` scalar payloads

Candidate 19 changed lightweight `ContentScalar::{Name,String,Operator}` payloads from `Vec<u8>` to `Rc<Vec<u8>>` to eliminate deep clones across the shared hidden/raster/vector callback fanout. Profiling justified the experiment: C&K carries 15,648,280 scalars through the shared scanner, including 5,687,246 byte-bearing scalars and 5,509,303 operators.

The representation also reduced `size_of::<ContentScalar>()` from 32 B to 16 B, but CPU-accounted C&K A/B rejected it. Across three alternating pairs, candidate 18 used 38.503757 CPU-s total vs candidate 19 at 38.783477 CPU-s: **candidate 19 +0.73% CPU**. RSS was essentially identical and all outputs were SHA-identical. The control-block allocation/refcount work did not repay the avoided `Vec` clones.

Artifacts:
- `/srv/scratch/pdf-desht-build/candidate19-fast-ck-cpu-ab-20260919/summary.json`
- rejected source snapshot: `/srv/scratch/pdf-desht-build/candidate19-source-20260919/`

### Candidate 20 — borrowed operators, acceptance validation in progress

Candidate 20 restores candidate-18 scalar ownership and instead adds a compatibility-preserving borrowed operator callback to flpdf: `handle_operator(&[u8], offset, length)`. The default callback reconstructs the old owned `ContentScalar::Operator`, while the hot raster/vector/hidden callback chain overrides it and fans the same borrowed operator token through all four scanner layers. Numeric/name/string operands, malformed-token fallback and inline-image framing remain unchanged.

Quiet benchmark environment: `pc-sentinel-runner`, 12 CPUs / 31 GiB RAM. Binaries and source PDFs are staged under `/workspace/pdf-redox-c20-bench`; measured C&K runs are pinned to one CPU and show ~99.9% CPU share.

Five alternating C&K pairs:
- candidate 18: 100.980155 CPU-s total, 20.196031 s mean;
- candidate 20: 98.862459 CPU-s total, 19.772492 s mean;
- whole-process CPU: **-2.10%**;
- target scan mean: **5714.045 ms -> 5141.833 ms (-10.01%)**;
- peak RSS: effectively unchanged, 394,576 -> 394,836 KiB;
- all five outputs/counters exact.

Seven additional semantic/control pairs on the PC runner passed **7/7 SHA-identical and 7/7 exact counters**:
- Ebyte: CPU -3.78%, target scan 542.905 -> 514.042 ms;
- Fortior MCU: CPU -3.86%, target scan 3032.492 -> 2808.294 ms;
- CKS positive: exact 931 masks / 24,990,447 cropped pixels / 157 native-fragment paints;
- Fortior BLDC positive: exact 413 masks / 70,529 cropped pixels;
- historical hidden-text ambiguity output exact;
- synthetic zero-opacity and dark-cover positives each still prune exactly one hidden-text item.

Control-set aggregate: CPU 108.328492 -> 107.368256 s (-0.89%); target scan 11,796.245 -> 11,139.895 ms (-5.56%). Reconstruction-heavy positives are overall neutral while scan-heavy documents improve.

Durable artifacts:
- `/srv/scratch/pdf-desht-build/candidate20-pc-runner-20260919/pc-ck-cpu-summary.json`
- `/srv/scratch/pdf-desht-build/candidate20-pc-runner-20260919/pc-controls-summary.json`
- source snapshot: `/srv/scratch/pdf-desht-build/candidate20-source-20260919/`

Candidate 20 is not promoted until production-LTO binary, full root + vendored-flpdf quality gate, and broad candidate-18 -> candidate-20 identity sweep complete.

### Candidate 20 — accepted processing baseline

Candidate 20 is accepted and supersedes candidate 18.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate20-20260919/pdf-redox`

SHA-256:
`e682307dca7ea8f5798be0c95ff81f59836e525f8fbe803638f9094441788b9d`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate20-source-20260919/`

Acceptance summary:
- 5 alternating quiet C&K pairs on `pc-sentinel-runner`, pinned to one CPU: whole-process CPU **-2.10%**, target-scan **-10.01%**, RSS unchanged at ~395 MiB, 5/5 exact outputs/counters.
- 7 semantic/control pairs: 7/7 SHA-identical + exact counters. Scan-heavy Ebyte/Fortior MCU improved ~3.8% whole CPU; reconstruction-heavy CKS/BLDC stayed essentially neutral overall while preserving exact raster rewrites.
- Production candidate-18 → candidate-20 stratified identity sweep: **50/50 SHA-identical**, 25 historical hits + 25 no-ops, zero failures/mismatches.
- Immutable-source final gate passed with candidate-20 source hash assertions before and after:
  - root workspace: fmt, diff, strict Clippy, 164 tests + doctests, rustdoc, Rust 1.92 MSRV, wasm32, duplicate-dependency check, fuzz check;
  - vendored flpdf default-feature strict Clippy with only four independently known baseline lint classes allowed;
  - flpdf tests: **2708 passed**, 13 ignored, exactly two independently reproduced candidate-18 baseline failures skipped;
  - flpdf doctests: **52 passed**, 1 ignored;
  - explicit borrowed-operator and inline-image-resynchronization tests passed.
- The two skipped flpdf tests were separately rerun against candidate 18 and failed identically, so they are baseline defects, not candidate-20 regressions.

Durable acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate20-acceptance-20260919.json`

Next optimization direction (candidate 21): keep the parser boundary unchanged. C&K still generates ~9.96M numeric scalar events; within vector analysis the same operand is stored independently by FillScanner, PathBlockScanner and TransformedBlockScanner. First experiment should centralize the vector-side operand buffer and pass borrowed operand slices to the three vector state machines, reducing duplicated operand writes without changing hidden/raster semantics. Benchmark on the quiet PC runner before considering a broader parser-level batching API.

### Candidate 21 — accepted processing baseline

Candidate 21 is accepted and supersedes candidate 20.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate21-20260919/pdf-redox`

SHA-256:
`fe53fe050c146e529cedb857fe6ce4dce4e259c0e49854c5221bd19df15cb08a`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate21-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate21-acceptance-20260919.json`

Change: parser/flpdf, hidden-text, raster and FillScanner remain unchanged. `ProcessingFactorScanner` now owns one factor-side operand buffer; each scalar/object operand is stored once and passed as a borrowed slice to both `PathBlockScanner` and `TransformedBlockScanner`. Their standalone callback paths keep their private buffers for compatibility. A dedicated test proves the shared factor scanner produces field-for-field identical path/transformed block results to the standalone scanners.

Quiet C&K CPU-pinned A/B, 5 alternating pairs:
- candidate 20 CPU: 104.765176 s total / 20.953035 s mean;
- candidate 21 CPU: 92.441160 s total / 18.488232 s mean;
- whole-process CPU: **-11.76%**;
- median CPU: **-13.45%**;
- target scan: **5483.782 ms -> 4112.433 ms (-25.01%)**;
- peak RSS slightly lower: 394,460 -> 394,140 KiB;
- 5/5 outputs and raster counters exact.

Positive/control A/B:
- Fortior MCU: CPU **33.0067 -> 29.4538 s (-10.76%)**, target scan -22.47%, exact output;
- CKS reconstruction-positive: CPU **18.0982 -> 17.8934 s (-1.13%)**, target scan -18.04%, exact **931 masks / 24,990,447 cropped pixels / 157 native-fragment paints**;
- Fortior BLDC: CPU **60.3913 -> 53.3165 s (-11.71%)**, target scan -12.98%, exact **413 masks / 70,529 cropped pixels**.

Broad production validation: **50/50 SHA-identical**, standard 25 historical hits + 25 no-ops, zero failures/mismatches.

Full immutable-source root gate passed: fmt/diff/strict Clippy, **165 tests**, doctests/rustdoc, Rust 1.92 MSRV, wasm32, duplicate-dependency check, fuzz check. `flpdf/content_stream.rs` is byte-identical to accepted candidate 20, so the standalone flpdf gate from candidate 20 carries forward unchanged.

Artifacts:
- `/srv/scratch/pdf-desht-build/candidate21-pc-runner-20260919/`
- `/srv/scratch/pdf-desht-build/candidate21-identity50-pc-20260919/`
- `/srv/scratch/pdf-desht-build/candidate21_full_gate_20260919.txt`

Next candidate 22: remove the remaining outer vector operand duplication between `FillScanner` and the now-shared `ProcessingFactorScanner`. Keep parser/hidden/raster unchanged; make `ProcessingPageScanner` own the single vector operand buffer and pass borrowed slices into fill and factor state machines. Measure separately on the quiet PC runner.


### Candidates 22–24 — raster fidelity split; candidate 24 accepted

Candidate 22 tested a relaxed stencil transform for Microsoft Print To PDF-style binary-alpha sprites: constant visible DeviceGray/RGB/CMYK color may be converted from color+SMask to a 1-bit stencil even when fully transparent source samples contain a different color. On CKS it cut output 3,525,661 -> 3,148,040 B and CPU ~7.9%, but it was not render-exact because viewers interpolate RGB and alpha separately. It was initially rejected for exact mode.

Candidate 23 preserved separate DeviceRGB + SMask interpolation while packing binary RGB component samples to 1 bpc. CKS output 3,512,296 B (-0.38% vs candidate 21); 64/64 pages rendered byte-identically at 96 DPI. This established the exact path but the size payoff was modest.

Candidate 24 makes the distinction explicit and is now the accepted baseline:
- frozen binary: `/srv/scratch/pdf-desht-build/corpus-opportunities-candidate24-20260919/pdf-redox`
- SHA-256: `35e6373d1254fde49fcfcec3ca11813f426b8385cd6c28a97f18e88664744b4b`
- source snapshot: `/srv/scratch/pdf-desht-build/candidate24-source-20260919/`
- acceptance manifest: `/srv/scratch/pdf-desht-build/candidate24-acceptance-20260919.json`

Policy:
- normal mode defaults to the relaxed stencil transform; on the CKS visual comparison this removes the producer's fuzzy white-RGB/alpha interpolation halo and was judged visually cleaner;
- `--exact-raster-rendering` preserves separate color/alpha interpolation semantics;
- exact binary component packing remains enabled in exact mode.

Validation:
- full immutable-source workspace gate green: fmt/diff/strict Clippy, 167 tests, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency and fuzz checks;
- standard production sweep 50/50 byte-identical to candidate 21;
- CKS normal: 3,148,035 B, 847 stencils / 846 relaxed; 64/64 pages render byte-identically at 96 DPI to aggressive candidate 22;
- CKS exact: 3,512,296 B and byte-for-byte candidate 23; candidate 23 renders 64/64 pages identically to candidate 21.

Next broad raster target: cross-page AlphaCrop result reuse. CKS paints the same huge CEIC header/logo XObject on all 64 pages. Raster-layout independently crops/re-encodes it per target, then the later repeated-page-object stage collapses those rewritten copies back to one. A stage-wide cache keyed by immutable source XObject plus deterministic crop result should remove most of that wasted `raster/apply-plans` CPU while preserving output.


### Candidate 25 — accepted processing baseline

Candidate 25 supersedes candidate 24.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate25-20260919/pdf-redox`

SHA-256:
`b300a60559ba6db5f0ea3bd2207b9ca86ee079e54d1b880dcd7a3bba77640c1c`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate25-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate25-acceptance-20260919.json`

Change: AlphaCrop rewritten XObjects are cached stage-wide by immutable source image handle. Each content target still computes local placement/resource naming, but later uses of the same source image reuse the first cropped/re-encoded image object instead of recompressing it.

CKS quiet PC A/B, 3 alternating pairs:
- median whole-process CPU **15.493 -> 5.853 s (-62.2%)**;
- raster-layout mean **12,499 -> 2,729 ms (-78.2%)**;
- apply-plans **11,198 -> 1,413 ms (-87.4%)**;
- peak RSS mean **123,079 -> 113,311 KiB**;
- output **3,148,035 -> 3,151,344 B (+3,309 B / +0.105%)**.

Rendering:
- normal mode candidate 24 vs 25: **64/64 pages byte-identical at 96 DPI**;
- pages 1, 14, 16, 25, 62 exact at 600 DPI;
- exact-raster mode: 64/64 render-identical; raster ~2.00 s, apply ~1.14 s on the cold-storage validation run.

Broad validation:
- standard production sweep **50/50 byte-identical** to candidate 24, 25 hits + 25 no-ops;
- full immutable-source workspace gate green: strict Clippy, **167 tests**, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency/fuzz checks.

Next likely raster target: cross-target AlphaCrop cache hits still run `crop_merge_plan`, including large pixel-buffer slices/copies, before discovering that the rewritten XObject already exists. Cache deterministic crop geometry so repeat hits can update placement/counters without copying source pixel buffers.


### Candidate 26 — accepted processing baseline

Candidate 26 supersedes candidate 25.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate26-20260919/pdf-redox`

SHA-256:
`ef61a91fd6633f56b6973c15a1a6efdfc25249f66fb67dee5547b04dedcb1ff6`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate26-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate26-acceptance-20260919.json`

Change: AlphaCrop plans are lightweight. They retain the precomputed crop rectangle but do not eagerly clone RGB/alpha planes. On a cross-target AlphaCrop cache hit, apply-plans updates only crop geometry and reuses the shared encoded XObject; no source-plane clone, crop-buffer allocation, or recompression occurs.

Quiet CKS candidate-25 -> candidate-26 A/B, 3 alternating pairs:
- median whole-process CPU **6.089 -> 5.505 s (-8.1%)**;
- raster-layout **2873 -> 2297 ms (-20.1%)**;
- apply-plans **1491 -> 1047 ms (-29.7%)**;
- peak RSS **111,915 -> 91,657 KiB**;
- output **3,151,344 B -> 3,151,344 B**, byte-for-byte identical.
Exact-raster CKS is also byte-for-byte identical at 3,515,658 B.

Broad validation: standard 50-doc sweep **50/50 byte-identical**, 25 hits + 25 no-ops, zero failures. Full immutable-source workspace gate green: fmt/diff/strict Clippy, 167 tests, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency and fuzz checks.

Next candidate 27: memoize `alpha_crop()` bounds by immutable source XObject so repeated 600-DPI masks are scanned once rather than once per page/draw. Expected payoff is modest; benchmark before promotion.


### Candidate 27 — accepted processing baseline

Candidate 27 supersedes candidate 26.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate27-20260919/pdf-redox`

SHA-256:
`0a7348d1dc0b6e7343376f1c1f549573d19e7e61ed826f54b0ac8ca44b99975b`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate27-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate27-acceptance-20260919.json`

Change: stage-wide memoization of alpha visible bounds by immutable source XObject. Repeated `alpha_crop()` calls reuse successful, empty and no-op results instead of rescanning every alpha pixel.

Quiet CKS candidate-26 -> candidate-27 A/B, 3 alternating pairs:
- median whole-process CPU **5.832 -> 5.518 s**;
- aggregate CPU **17.761 -> 17.112 s (-3.7%)**;
- raster-layout **2432 -> 2155 ms (-11.4%)**;
- apply-plans flat/noisy **1092 -> 1122 ms**;
- peak RSS ~91.7 MiB unchanged;
- output **3,151,344 B**, byte-for-byte identical.

Broad validation: standard 50-doc sweep **50/50 byte-identical**, 25 hits + 25 no-ops, zero failures. Full immutable-source workspace gate green: fmt/diff/strict Clippy, 167 tests, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency and fuzz checks.

Next candidate 28: make stencil encoding lazy. `make_image()` currently compresses the full 8-bit color plane before evaluating a 1-bit stencil. If `stencil_cost < compressed_alpha_cost`, stencil already beats any possible `compressed_color + compressed_alpha`; return it without color compression. CKS emits 715 stencils / 729 rebuilt binary images, so this should target the remaining apply-plans cost directly.


### Candidate 28 — accepted processing baseline

Candidate 28 supersedes candidate 27.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate28-20260919/pdf-redox`

SHA-256:
`c864e15f2a3c7f5316db8c37e6571278e283ecba9511cea339ff977be294121f`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate28-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate28-acceptance-20260919.json`

Change: lazy RGB Flate for stencil candidates. After compressing the 1-bit stencil and 8-bit alpha, if stencil cost is already below compressed-alpha cost alone, adding any encoded color payload cannot change the winner, so make_image returns the stencil without compressing the full 8-bit color plane. Borderline cases keep the previous exact comparison.

Quiet CKS candidate-27 -> candidate-28 A/B, 3 alternating pairs:
- median whole CPU **5.473 -> 5.218 s**;
- aggregate CPU **16.680 -> 15.633 s (-6.3%)**;
- raster-layout **2051 -> 1639 ms (-20.1%)**;
- apply-plans **1078 -> 650 ms (-39.8%)**;
- peak RSS ~91.7 MiB unchanged;
- output **3,151,344 B**, byte-for-byte identical.
Exact-raster output is also byte-for-byte identical at 3,515,658 B.

Broad validation: standard 50-doc sweep **50/50 byte-identical**, 25 hits + 25 no-ops, zero failures. Full immutable-source workspace gate green: fmt/diff/strict Clippy, 167 tests, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency and fuzz checks.

Reporting note: `raster_binary_image_encoded_bytes_saved` is a conservative lower bound on shortcut stencil wins because the discarded RGB compressed length is intentionally not computed. CKS reports 80,251 vs old 283,166 with identical PDF bytes.

Next experiment: processing mode still uses zlib level 9 globally. Benchmark 9/6/3 before changing policy.


### Candidate 29 — accepted processing baseline

Candidate 29 supersedes candidate 28.

Frozen production binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate29-20260919/pdf-redox`

SHA-256:
`0b3cb2121db1ff322d79a8a6acdc03883b5f5f82be903b530ba432c8e699f463`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate29-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate29-acceptance-20260919.json`

Change: CLI processing mode now defaults Flate rewrites to zlib level 5 instead of 9. Size mode remains level 9. `--flate-level 0..9` is available as an explicit validated override.

12-document mixed level study:
- level 9: 22.781 s CPU, 15,006,090 B;
- level 6: 16.771 s (-26.4%), 15,155,846 B (+1.00%);
- level 5: 15.735 s (-30.9%), 15,198,120 B (+1.28%);
- level 3: 15.562 s (-31.7%), 15,338,888 B (+2.22%).
Level 5 is the clear knee.

Quiet CKS candidate-28 -> candidate-29 A/B, 3 alternating pairs:
- aggregate CPU **14.462 -> 11.597 s (-19.8%)**;
- median CPU **4.800 -> 4.041 s**;
- raster-layout **1557 -> 1145 ms (-26.4%)**;
- apply-plans **623 -> 175 ms (-71.9%)**;
- peak RSS ~91.7 MiB unchanged;
- output **3,151,344 -> 3,219,099 B (+2.15%)**;
- stencils **715 -> 717**.

Renderer validation at 72 dpi, all 64 CKS pages:
- exact-raster: **0 / 32,117,248 pixels differ**;
- normal: **36 / 32,117,248 pixels differ**, pages 14 and 55 only, max channel delta 63.
Size mode is 12/12 byte-identical to candidate 28.

Full immutable-source workspace gate green: strict Clippy, 167 library tests + 4 CLI tests, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency and fuzz checks.

Next candidate 30: hidden-text empty-result proof. CKS raster shared scan reports no prunable hidden ranges, but pages with raster plans are still placed in the standalone physical-hidden-text fallback and reparsed. If shared hidden scan completed with an empty range list, mark the page shared-complete even when raster plans exist. This requires no range remapping and should preserve output exactly while saving ~0.24-0.44 s on CKS.


### Candidate 30 — accepted processing baseline

Candidate 30 supersedes candidate 29.

Frozen binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate30-20260919/pdf-redox`

SHA-256:
`24e492263b449202cdb2b7584ec9c158a9a908014c475b1970cc6af4e31361d6`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate30-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate30-acceptance-20260919.json`

Change: pages with raster plans can reuse a completed empty shared hidden-text result when no pending raster-prune rewrite changed the content. This removes the standalone physical-hidden-text reparse without any offset remapping. Mixed prune+reconstruction pages retain the old fallback.

Quiet CKS candidate-29 -> candidate-30 A/B:
- aggregate CPU **10.214 -> 8.565 s (-16.1%)**;
- median CPU **3.422 -> 2.837 s**;
- raster-layout **1023 -> 982 ms**;
- physical-hidden-text **471.5 -> 0.006 ms**;
- output **3,219,099 B**, byte-for-byte identical;
- hidden removals 0; stencils 717.

Broad validation: candidate-29 -> candidate-30 standard sweep **50/50 byte-identical**, 25 hits + 25 no-ops. Full immutable-source workspace gate green: strict Clippy, 167 library tests + 4 CLI tests, doctests/rustdoc, Rust 1.92 MSRV, wasm32, dependency and fuzz checks.

Next candidate 31 investigation: profile `font-table-strip`. `font_glyph_usage()` reparses page/Form content independently with `FontUsageScanner`; measure glyph-usage scan vs graph walk vs actual SFNT decode/subset/strip/re-encode before changing architecture.


### Candidate 31 — accepted processing baseline

Candidate 31 supersedes candidate 30.

Frozen binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate31-20260919/pdf-redox`

SHA-256:
`657ea72b14641b8fec582557f7d5d2af6b0bcea33b90d0d155d0380917cc62a7`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate31-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate31-acceptance-20260919.json`

Change: font-table optimization now discovers TrueType/CIDFontType2 programs whose `/FontDescriptor` is embedded in nested direct dictionaries. The traversal follows direct dictionary/array structure only and never chases indirect references; referenced objects are already visited independently by the output-object walk. Programs known through the pre-existing indirect-descriptor graph keep the previously validated retain-GID outline-subsetting path. Newly discovered direct-only programs receive rendering-table stripping but **not** outline subsetting. If no legacy subset-capable program exists, the independent `font_glyph_usage()` content scan is skipped entirely.

Why the conservative split matters: the first candidate applied retain-GID subsetting to the newly recovered direct CIDFontType2 programs. It saved more bytes, but changed Poppler rendering of the Chinese word `谢谢` on page 1 of `01383_EEC_Common_Mode_Filters_C42394176.pdf` while text extraction remained identical. Disabling direct-only outline subsetting restored byte-identical renders at 24/72/144/300 DPI. The accepted candidate therefore keeps the large table-strip win without extending glyph-subsetting semantics beyond the already validated topology.

CKS pathology fixture:
- candidate 30: **3,219,099 B**, SHA `2fa316d3fd9d9d1f4d6e5bb0813be2eb56bd54b49bcb3a38a2a8132a94da284d`;
- candidate 31: **2,384,414 B**, SHA `a4d32175b4b37180630d41470e2b5aaef7569854eb75c3a86a7ba9881a400e7a`;
- delta: **-834,685 B (-25.9%)**;
- 13 direct-only embedded TrueType programs are optimized, with no direct-only outline subsetting;
- 64/64 pages render byte-identically at 24 and 96 DPI in the focused validation, and extracted text is byte-identical in normal and `-layout` modes.

Quiet CKS candidate-30 -> candidate-31 A/B, five alternating pairs:
- aggregate CPU **8.367 -> 7.314 s (-12.6%)**;
- median CPU **1.660 -> 1.450 s (-12.6%)**;
- aggregate wall **10.214 -> 8.742 s (-14.4%)**;
- median wall **1.804 -> 1.550 s (-14.1%)**;
- median `font-table-strip` **260.9 -> 41.4 ms (-84.1%)**;
- median raster-layout is effectively flat (**615.8 -> 614.3 ms**).

Targeted font-miss validation:
- among the 40 largest historical font-program misses, candidate 31 changes 8 files;
- accepted conservative policy saves **3,494,178 B aggregate** versus candidate 30 across those 8;
- all 8/8 have identical default and layout text extraction;
- all pages are byte-identical at 24 DPI;
- first/middle/last pages are byte-identical at 144 DPI.

Final regression union: **86/86 expected hashes matched**, zero failures or mismatches. This includes the standard 50-document control set, the 40-document font-miss sensitivity set, and all 8 intended changed outputs.

Full immutable-source gate is green: root fmt/diff, strict workspace/all-target/all-feature Clippy, **168 library tests + 4 CLI tests**, doctests/rustdoc with warnings denied, Rust 1.92 MSRV, wasm32, duplicate-dependency check, fuzz manifest, plus vendored `flpdf` validation (**2708 tests + 52 doctests**) and the focused borrowed-operator / inline-image-resynchronization tests.

Next candidate 32: classify the remaining large embedded-font misses. Candidate 31 proves that font topology, not only SFNT transformation cost, is still leaving substantial payload untouched. Separate unsupported font-program types/containers/filters from additional reachable TrueType graph shapes before changing optimization policy.


### Candidate 32 — accepted processing baseline

Candidate 32 supersedes candidate 31.

Frozen binary:
`/srv/scratch/pdf-desht-build/corpus-opportunities-candidate32-20260919/pdf-redox`

SHA-256:
`58758470d93a2c4b6b1368885b353776175284940f00cfda98a35e4fa832cc69`

Source snapshot:
`/srv/scratch/pdf-desht-build/candidate32-source-20260919/`

Acceptance manifest:
`/srv/scratch/pdf-desht-build/candidate32-acceptance-20260919.json`

Change: embedded-font optimization now treats PDF's one-element filter-array spelling `/Filter [ /FlateDecode ]` as equivalent to scalar `/Filter /FlateDecode`. The original resolved filter and `/DecodeParms` object shape is passed unchanged into the existing flpdf codec, which already implements array-filter semantics. Multi-filter arrays remain ineligible. Programs newly unlocked only by this spelling are conservatively limited to rendering-table stripping; candidate 32 does **not** broaden outline/glyph subsetting to them.

This closes a syntactic eligibility hole affecting WPS, Office and other generated PDFs. The standard candidate-31 -> candidate-32 50-document control sweep is **50/50 byte-identical**, zero failures.

Exhaustive targeted corpus sweep:
- selected all 98 documents with embedded font programs and an exact `[ /FlateDecode ]` filter entry;
- **98/98** processed successfully;
- **20** changed and **78** remained byte-identical;
- **163 additional font programs** optimized;
- **0 additional glyph subsets**;
- aggregate output delta **-2,753,058 B** versus candidate 31.

All 20 changed outputs pass semantic/render validation:
- default and `pdftotext -layout` extraction byte-identical;
- **119/119 pages** byte-identical at 24 DPI;
- first/middle/last pages byte-identical at 144 DPI for every document;
- no failures.

Idempotence: all **20/20** changed candidate-32 outputs are exact SHA-identical after a second optimizer pass, with zero font optimizations remaining.

Representative quiet A/B, five alternating pairs:
- 3PEAK current-sense: **1,053,728 -> 663,806 B (-37.0%)**; aggregate CPU **1.346 -> 1.445 s (+7.4%)**, about +20 ms CPU per run; median wall **270.6 -> 304.1 ms**.
- amsem ESD: **872,115 -> 506,419 B (-41.9%)**; aggregate CPU **1.496 -> 1.474 s (-1.5%)**; median wall **301.4 -> 309.7 ms**.
The small runtime cost on positive files is accepted in processing mode because the rewrite removes large authoring/layout font tables while unchanged files retain candidate-31 output exactly.

Full immutable-source gate is green: root fmt/diff, strict workspace/all-target/all-feature Clippy, **170 library tests + 4 CLI tests**, doctests/rustdoc with warnings denied, Rust 1.92 MSRV, wasm32, dependency/fuzz checks, plus vendored `flpdf` **2708 tests + 52 doctests** and the focused borrowed-operator/inline-image-resynchronization tests.

Next candidate 33 investigation: scalar-Flate documents still contain large embedded CID TrueType payloads that produce zero font optimizations. Start with `00929_ChipLink_Tech_DC-DC_Converters_C2995665.pdf` (8 embedded CID TrueType Identity-H fonts, ~1.1 MB font-program payload, scalar Flate, zero candidate-32 font optimizations). Instrument per-program graph inclusion/filter/SFNT/table decisions to distinguish another reachability/descriptor shape from genuinely unprofitable or unsupported SFNT structures.

### Candidate 34 — accepted adaptive codec for reconstructed compact-color stripes

Candidate 34 supersedes candidate 33 as the accepted processing baseline for raster stripe reconstruction.

Artifacts:
- frozen binary: `/srv/scratch/pdf-desht-build/candidate34-adaptive-stripe-jpeg-20260919/pdf-redox`
- binary SHA-256: `9365fdb51fc032f2508b8baa72440f7eeb5b2d7d2081ecf774b64723cd0e6b08`
- gate source snapshot: `/srv/scratch/pdf-desht-build/candidate34-gate-source-20260919/`
- acceptance manifest: `/srv/scratch/pdf-desht-build/candidate34-acceptance-20260919.json`
- full gate log: `/srv/scratch/pdf-desht-build/candidate34_full_gate_20260919.log`, marker `CANDIDATE34_FULL_GATE_OK`

Change: reconstructed images whose source used compact DCT/JPX color coding no longer have to become huge lossless Flate RGB streams. The merged raster is still produced structurally; candidate 34 compares a libjpeg-turbo JPEG quality-85 color stream against the existing lossless candidate and keeps JPEG only when smaller. Binary/stencil paths remain preferred where they win.

Normal-document corpus (`/srv/scratch/upload/pdf_corpora/pdf_corpora.zip`): 196/196 processed, zero failures. Exactly three PDFs changed versus candidate 33 and no unrelated output changed. Aggregate delta versus candidate 33: **-5,960,149 B**.
- STM32G4 DMA/DMAMUX: 5,240,984 -> 1,708,280 B; source 1,753,488 B; 61 groups / 427 paints still merged.
- STM32G4 Analog Comparators: 3,150,053 -> 1,091,117 B; source 1,141,483 B; 55 groups / 278 paints still merged.
- CTI OCXO: 594,577 -> 226,068 B; source 222,738 B; 1 group / 23 paints still merged.
Known lossless-stripe controls Alcor Micro and AP Memory are byte-for-byte identical to candidate 33.

Quality/idempotence: ST all-page 96-DPI incremental minimum PSNR versus candidate 33 is 44.08/46.12 dB; sampled 144-DPI PSNR is 46.7-47.7 dB. All three positives are exact SHA fixed points on second pass with zero further stripe work. OCXO is a visual outlier, but most of its source-vs-output difference already comes from the mandatory stripe reconstruction; JPEG q85 is not the dominant change.

### Candidate 35 — accepted crop economics + source-budgeted reconstructed JPEG

Candidate 35 supersedes candidate 34.

Artifacts:
- frozen binary: `/srv/scratch/pdf-desht-build/candidate35-crop-economics-20260919/pdf-redox`
- binary SHA-256: `e01464ff4a305f91a5aadd1eed995b17ca40d16546e46ced0e1d3e6214056145`
- gate source snapshot: `/srv/scratch/pdf-desht-build/candidate35-gate-source-20260919/`
- `raster_layout.rs` SHA-256: `8f2a32808a0d1edcf7947794be944a6e9af8c4408eb74040f02cca747e6b36aa`
- acceptance manifest: `/srv/scratch/pdf-desht-build/candidate35-acceptance-20260919.json`
- full gate log: `/srv/scratch/pdf-desht-build/candidate35_full_gate_20260919.log`, marker `CANDIDATE35_FULL_GATE_OK`

Changes:
1. Transparent/background cropping now requires at least **20 px total removed margin**, defined as `(old_width-new_width)+(old_height-new_height)`.
2. Singleton alpha-crop rewrites are prepared without graph mutation and applied only when compressed candidate color+alpha(+background) payload is strictly smaller than the current encoded color+attached-mask payload. Rejected crops allocate no PDF objects and do not increment crop statistics.
3. Reconstructed compact-color stripes carry the aggregate encoded source-color budget, deduplicated by source image object. JPEG still starts at q85; q82/80/78/75 are tried only when q85 already beats the lossless candidate but exceeds that source budget.

Normal-document corpus replay:
- **196/196** successful, zero failures.
- **34** outputs changed versus candidate 34; **162** byte-identical.
- net delta versus candidate 34: **-552,505 B**.
- aggregate delta versus source inputs: **-97,348,923 B**.
- **0/196 outputs larger than source**.

Known crop-growth regressions are closed:
- `2605.05242v1.pdf`: candidate 34 was 84,435 B larger than source; candidate 35 is **181,582 B smaller** and becomes 51/51-page source-exact at 24 DPI instead of differing on five pages.
- `x-nucleo-drp1m1.pdf`: candidate 35 is **13,554 B smaller than source** and restores 6/6 source-exact pages at 24 DPI.
- `2604.07012v1.pdf`: candidate 35 is **68,571 B smaller than source** and restores 16/16 source-exact pages at 24 DPI.
- ESP32-P4 hardware guide: candidate 35 is **5,250 B smaller than source** and removes one of candidate 34's two source-render differences while retaining only profitable crops.

Stripe/JPEG positives improve further without dropping any stripe reconstruction:
- STM32G4 DMA/DMAMUX: **1,708,280 -> 1,653,435 B** versus candidate 34; 61 groups / 427 paints remain merged; source delta -100,053 B.
- STM32G4 Analog Comparators: **1,091,117 -> 1,061,311 B**; 55 groups / 278 paints remain merged; source delta -80,172 B.
- CTI OCXO: **226,068 -> 212,610 B**, now **10,128 B smaller than source**; 1 group / 23 paints remain merged.
Candidate-34→35 sampled incremental PSNR is 47.33 dB minimum on DMA and 48.15 dB on Comparators. Source-relative ST quality is essentially unchanged; OCXO source-relative sampled quality improves slightly.

All-changed validation:
- **34/34** candidate-34→35 outputs have byte-identical default and `pdftotext -layout` extraction.
- **31/31** crop/economics changed files have a source-render mismatch count no worse than candidate 34; many rejected crops restore exact source rendering.
- **33/34** are exact SHA fixed points on pass two.
- the sole exception, `x-nucleo-s2868a2.pdf`, has a pre-existing deferred repeated-page-object cleanup: candidate 34 itself changes by -941 B on its second pass; candidate 35 changes by -932 B. Candidate 35 does no second-pass raster work there.

Full immutable-source gate passed: strict workspace/all-target/all-feature Clippy, root fmt/diff, workspace tests/docs, Rust 1.92 MSRV, wasm32, duplicate-dependency and fuzz checks, plus vendored flpdf **2709 tests** and **52 passing doctests / 1 ignored**, and focused borrowed-operator/inline-image-resynchronization tests.

### Candidate 36 target — bounded pure-translation stroke-run batching

DEFOND Rocker Switches remains a suspiciously large vector case. A naive rewrite that bakes each translation into absolute line coordinates removes **6.8 MB raw** but makes Flate output **1.11 MB larger**, because it destroys repeated local-coordinate structure.

A better exact-structure transform batches consecutive blocks of the form:
`q 1 0 0 1 tx ty cm x0 y0 m x1 y1 l S Q`
into one saved graphics-state scope, retaining local path coordinates and expressing later placements as relative pure translations. Simulation on the current DEFOND processing output finds **606,459 strokes** in **293 runs** across 58 streams.

Unbounded batching estimates **1,195,354 B** Flate-level-5 saving. To bound renderer floating-point CTM accumulation, restart from an absolute translation periodically:
- 16-stroke reset: 831,701 B saved
- 32: 987,964 B
- **64: 1,080,779 B**
- 128: 1,134,796 B
- 256: 1,164,058 B
- unbounded: 1,195,354 B

Start candidate 36 conservatively at a **64-stroke reset interval**. Before production implementation, build a QDF/fix-qdf prototype and verify multi-DPI render equivalence; then implement structurally through the content parser rather than regex.
