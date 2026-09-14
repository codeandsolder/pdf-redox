# pdf-redox thread handoff — 2026-09-14

This file is the operational handoff for continuing the current Hayro migration in a fresh ChatGPT thread.

## Canonical locations

- Server working tree: `/srv/scratch/pdf-desht`
- GitHub: `https://github.com/codeandsolder/pdf-redox`
- Branch: `main`
- Canonical Notion research page: `https://app.notion.com/p/Datasheet-PDF-normalization-compression-research-3d4f74be9020816db31be12bf88871f6`
- Architecture child: `https://app.notion.com/p/3dbf74be90208135ade6e9d916d40ffa`
- Hayro evaluation clone: `/srv/scratch/pdf-fork-eval/hayro`
- Repo-specific GitHub token is stored outside the repo at `/srv/scratch/upload/github_token.txt`. A repo-local credential helper is already configured and `git push --dry-run -u origin main` succeeded. Never print or embed the token in a remote URL.

## Stable pushed state

Latest fully validated/pushed code checkpoint before the current dirty trailer experiment:

- `9c7f86c` — `Add compact Hayro COW writer`
- `f341c57` — `Add lazy overlay-aware reachability`
- `cfb733d` — `Rename to pdf-redox and add Hayro COW source layer`
- `311a951` — `Add preservation policies and rendering-only font pruning`

At `9c7f86c`, native strict Clippy, workspace tests, wasm32 build, and `git diff --check` were green. The core suite was 77/77 after the writer tests were correctly forced to rebuild.

Do not assume the current working tree is validated. See the dirty-tree section below.

## Architecture decision

Hayro is the selected long-term parser/source layer. Do **not** fork it pre-emptively.

Target architecture:

`immutable/lazy Hayro source -> sparse copy-on-write overlay -> reachability/optimization passes -> compact fresh writer`

The existing `flpdf` backend remains temporarily for mature optimization/mutation functionality while passes are migrated incrementally. The goal is not to preserve an eager full-document object graph long term.

Hayro is pinned at the benchmarked revision `a728f7cb6826c1e167973b8fd71867ebb4cf39af` rather than tracking a moving branch.

`codeandsolder/rust-skills2` was used as a cleanup/style reference during the migration. In particular: borrow/zero-copy first, typed errors, narrow public APIs, sparse/COW ownership, measure memory, avoid unnecessary `unsafe`, and keep backend-specific types out of the public API.

## Why Hayro was selected

Dedicated AWS benchmark on a clean `t4g.small` (2 vCPU / 2 GiB ARM64), using 1,000 SafeDocs PDFs totaling 2,203,991,895 bytes. Source file reads were outside parser timing. Two rounds were run in opposite orders and were extremely stable.

| Mode | Success | Mean CPU/PDF | Peak RSS |
| --- | ---: | ---: | ---: |
| Hayro open/page tree | 997/1000 | 0.334 ms | 262 MiB |
| Hayro full walk | 997/1000 | 6.540 ms | 262 MiB |
| lopdf load | 996/1000 | 4.749 ms | 524 MiB |
| lopdf rewrite | 996/1000 | 9.800 ms | 800 MiB |
| PDFOxide open | 986/1000 | 1.220 ms | 509 MiB |
| PDFOxide full walk | 955/1000 | 21.516 ms | 855 MiB |
| PDFOxide rewrite | 975/1000 | 17.421 ms | 881 MiB |

Interpretation: Hayro's borrowed/lazy model gives a large real CPU and memory advantage. lopdf remains a good "ship sooner" alternative, but its eager model is measurable baggage. PDFOxide is no longer a serious fork-base candidate.

AWS benchmark infrastructure was fully cleaned up after the run. The only intentionally retained AWS object is the reusable no-ingress security group `pdf-benchmark-general-sg` (`sg-0d8e36ec8d1c71ce0`).

## Implemented Hayro/COW foundation

Pushed code already includes:

- `SourcePdf` backed by Hayro and immutable source bytes.
- Stable typed existing/new object handles.
- Sparse `ObjectOverlay` with replace/delete/add semantics.
- On-demand source object materialization into owned COS values.
- Source-backed stream payloads so editing a stream dictionary does not copy the encoded payload.
- `/Length` treated as derived writer state rather than mutable source state.
- Overlay-aware reachability that scans untouched Hayro COS without materializing the graph.
- Undefined source references are treated with PDF null semantics in the writer, while dangling overlay references/deletions remain hard errors.
- Public load errors are wrapped in `SourceLoadError`; Hayro error types are not part of the public API.

The compact writer at `9c7f86c` provides:

- dense output object renumbering;
- reachability-based garbage collection;
- direct serialization from borrowed Hayro objects for untouched source objects;
- serialization of owned COW replacements/new objects;
- source-backed stream emission and regenerated `/Length`;
- generic COS names/strings/arrays/dictionaries;
- exact large-integer preservation (no i64 -> f64 round-trip);
- decimal expansion for real values that Rust would otherwise print in exponent form;
- fresh classic xref/trailer generation;
- wasm32-safe classic-xref offset handling.

The writer is intentionally exposed only as `EditDocument::write_compact_experimental()` and is not yet the production `optimize_pdf()` path.

## Current dirty tree — UNVALIDATED LOCAL WORK

As of the handoff, `git status --short` in `/srv/scratch/pdf-desht` shows exactly:

```text
 M crates/pdf-redox-cli/src/main.rs
 M crates/pdf-redox/src/source.rs
 M crates/pdf-redox/src/writer.rs
```

These changes are **not committed** and must be revalidated before checkpointing.

Current prototype content:

1. `pdf-redox-cli/src/main.rs`
   - Adds a hidden `--hayro-rewrite-experimental` mode.
   - Runs `EditDocument::write_compact_experimental()` directly.
   - Uses `.redox.pdf` as default output suffix.

2. `pdf-redox/src/source.rs`
   - Keeps the same source `Arc<Vec<u8>>` alongside Hayro.
   - Adds a temporary `flpdf` trailer bridge using a `Cursor` over the shared source buffer, avoiding a second full-document source copy.
   - Converts only trailer-shell values into pdf-redox owned COS, preserving indirect references without resolving their subgraphs.
   - Filters writer-owned trailer/xref/encryption keys.
   - Has a 256-level direct trailer nesting guard.

3. `pdf-redox/src/writer.rs`
   - Includes preserved trailer values as extra reachability roots.
   - Emits semantic trailer state such as `/Info`, `/ID`, and custom extension entries.
   - Drops input xref-stream mechanics from fresh classic-xref output.
   - Adds classic-trailer and xref-stream trailer preservation tests/fixtures.

This dirty work appeared farther along than the conversational progress messages implied. Inspect the actual diff first; do not reconstruct it from memory.

## Hayro upstream gap and local proof patch

Hayro already parses the final trailer dictionary for both classic xref tables and xref streams inside `XRef::new`, uses it for encryption, `/Root`, `/Info` metadata, etc., then discards the raw dictionary bytes.

No existing Hayro GitHub issue was found for exposing the final trailer dictionary.

A local upstream-style proof patch currently exists in:

`/srv/scratch/pdf-fork-eval/hayro/hayro-syntax/src/xref.rs`

A snapshot of only that clean upstream-targeted diff is also preserved in this repo at `docs/hayro-trailer-access.patch` so the work does not depend on the evaluation clone surviving.

It:

- retains a small owned copy of the already-parsed final trailer dictionary bytes in `XRef`;
- exposes `XRef::trailer() -> Option<Dict<'_>>`;
- works for both classic trailers and xref-stream dictionaries;
- has two direct regression tests covering `/Info`, `/ID`, custom trailer keys, and xref-stream `/Type /XRef` visibility.

The two new trailer tests pass.

Do **not** infer that the whole Hayro tree is strict-Clippy clean under Rust 1.98. New Rust 1.98 Clippy lints flag many pre-existing `chunks_exact*` and `sort_by` sites in Hayro/Hayro JPEG2000. Also two unrelated existing Hayro tests depend on fixture paths not available from the evaluation clone invocation. These are unrelated to the trailer patch.

The local Hayro clone also contains old untracked `hayro-syntax/examples/corpus_bench.rs` benchmark artifacts from parser evaluation. Do not mix them into an upstream patch.

The preferred long-term solution is to upstream this small trailer accessor rather than maintain a Hayro fork. Until that lands, the dirty pdf-redox tree uses flpdf only as a temporary trailer-shell parser over the same shared source bytes.

## Immediate next steps for the new thread

1. Revalidate live server/repo state before changing anything.
2. Inspect `git diff` for the three dirty pdf-redox files; preserve the current prototype unless a concrete bug is found.
3. Run formatter, strict Clippy, workspace tests, and wasm32 build on the dirty trailer-preservation implementation.
4. Fix any compile/test issues. Pay particular attention to:
   - trailer direct values and indirect references;
   - xref-stream input cleanup (`/Type`, `/W`, `/Index`, `/Length`, filters, `/Prev`, `/XRefStm`, `/Encrypt`, `/Size`, `/Root` ownership);
   - `/Info` and arbitrary custom trailer references being included in reachability;
   - `/ID` byte-for-byte preservation policy;
   - encrypted-input behavior: fresh output should not carry stale `/Encrypt` state unless explicit re-encryption is implemented.
5. If clean, commit/push the trailer-preserving experimental writer separately.
6. Save the Hayro trailer accessor as a clean patch/branch and consider opening an upstream issue/PR; do not add unrelated Rust 1.98 Clippy cleanup to it.
7. Run the experimental Hayro rewrite across the existing representative PDF corpora. Compare:
   - parse/rewrite success count;
   - reparse success of output;
   - page count;
   - rendered output versus current flpdf rewrite (existing project render-validation workflow);
   - text extraction/preservation where relevant;
   - output size and wall/CPU/RSS.
8. Only after corpus-level preservation validation should optimizer passes begin migrating from flpdf onto the Hayro/COW overlay.

## Git/auth state

`origin` is `https://github.com/codeandsolder/pdf-redox.git` and `main` tracks `origin/main`.

The server has a repo-local credential helper that reads `/srv/scratch/upload/github_token.txt` at authentication time. A dry-run push proved write authentication. Use ordinary server-side `git push`; do not expose the token in logs, chat, process arguments, or remote URLs.

## Important discipline

- Pushed commit `9c7f86c` is the last known fully validated code checkpoint.
- Current trailer bridge/writer/CLI modifications are local and unvalidated until the next thread reruns the gates.
- Do not replace `optimize_pdf()` with the Hayro writer just because unit tests pass; corpus/render preservation comes first.
- Do not fork Hayro unless an actual upstream-blocking parser/API need survives an upstream contribution attempt.
