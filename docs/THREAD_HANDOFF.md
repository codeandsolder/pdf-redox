# pdf-redox thread handoff — 2026-09-15

## Canonical locations

- Server working tree: `/srv/scratch/pdf-desht`
- GitHub: `https://github.com/codeandsolder/pdf-redox`
- Branch: `main`
- Canonical Notion project: `https://app.notion.com/p/Datasheet-PDF-normalization-compression-research-3d4f74be9020816db31be12bf88871f6`
- Architecture page: `https://app.notion.com/p/3dbf74be90208135ade6e9d916d40ffa`
- Validation detail page: `https://app.notion.com/p/3dbf74be90208145b1e8fb3ad5d56670`
- Representative writer corpus: `/srv/scratch/pdf-redox-writer-corpus`
- Writer/production validation root: `/srv/scratch/pdf-desht-build/hayro-writer-validation-20260914`
- First-pass validation root: `/srv/scratch/pdf-desht-build/hayro-metadata-privacy-20260915-v2`

## Current validated state

Latest validated and pushed code checkpoint: **`d1f98ce` — `Migrate metadata scrub to Hayro COW`**.

Target architecture remains:

`immutable/lazy Hayro source -> sparse copy-on-write overlay -> reachability/optimization passes -> compact fresh writer`

Production `optimize_pdf()` still uses flpdf for the mature optimizer pipeline. The Hayro writer and migrated metadata pass remain migration-only behind hidden `--hayro-rewrite-experimental`; do not describe the production optimizer as Hayro-backed yet.

Current final gates at `d1f98ce` are green: formatter, strict workspace/all-target/all-feature Clippy, **81/81 workspace tests**, wasm32 check, and `git diff --check`.

## First migrated optimizer pass: metadata privacy

`d1f98ce` moves semantic trailer ownership into `EditDocument` and ports the COS-level metadata scrub onto the sparse Hayro/COW graph.

The migrated pass removes trailer `/Info` and `/ID` plus reachable `/Metadata`, `/PieceInfo`, and `/LastModified`. JPEG marker scrubbing, attachments, signatures, and other best-effort privacy behavior remain on flpdf. Unsupported Hayro experimental privacy switches fail explicitly.

Traversal is single-pass for untouched source objects: key inspection and outgoing-reference collection use the same Hayro parse. Only dictionaries that actually need mutation are materialized into the overlay. A focused regression matches the existing flpdf removal accounting and verifies sparse materialization.

405-file validation:

- rewrite: 405/405;
- reparse/page count: 405/405;
- extracted text: 405/405 exact;
- render: **17,612/17,612 pages pixel-identical at 12 DPI**;
- output: **781,744,244 bytes** vs 819,424,570 bytes for the plain validated Hayro rewrite;
- metadata/history reachability reduction: **37,680,326 bytes**.

Removed across the corpus: 371 document IDs, 399 Info dictionaries, 5,387 XMP references, 711 PieceInfo references, and 864 LastModified entries.

`ALLK.pdf` release sanity check on the current build: plain rewrite ~0.29–0.31 s; metadata rewrite ~0.42–0.44 s. Do not resurrect the earlier debug-vs-release comparison as a performance result.

## Hayro writer baseline

Checkpoint `9a7e5b0` is the validated trailer-preserving fresh writer baseline. On the same 405-file / 777,561,367-byte / 17,612-page corpus:

- rewrite/reparse/page count: 405/405;
- text extraction: 405/405 exact;
- render: 17,612/17,612 pixel-identical pages;
- 12 encrypted inputs rewrite correctly without stale `/Encrypt` state;
- output: 819,424,570 bytes (1.053839098x aggregate; median file ratio 1.001604x).

Object-stream expansion/classic xref remains the main Hayro writer compactness deficit, but it is not a correctness blocker.

Semantic trailer state is now owned by `EditDocument`; the temporary flpdf trailer bridge is used only during document construction. A minimal Hayro accessor is still committed locally at `/srv/scratch/hayro-trailer-upstream`, branch `pdf-redox-expose-trailer`, commit `d950536` (`Expose final trailer dictionary from XRef`). It has not been pushed upstream and no PR has been opened.

## Production flpdf compatibility checkpoint

`321d9f7` fixed empty-stream compression compatibility. flpdf had emitted zero-byte streams carrying `/Filter /FlateDecode`, causing Poppler to stop a page `/Contents` array before later visible content. The fix follows qpdf's compatibility behavior: remove source filter state but do not label an empty encoded payload as Flate.

Corrected production corpus remains fully green: 405/405 structural, 405/405 exact text, 17,612/17,612 exact render, zero malformed `/Length 0` + `/FlateDecode` outputs, and 643,059,556 output bytes.

## Next ordered work

1. Migrate a second small dictionary-oriented optimizer slice before introducing any generic COW visitor abstraction; let two concrete passes determine the reusable API.
2. Strong candidates: non-JPEG/non-attachment best-effort privacy dictionary surgery (`/Thumb`, `/AA`, dangerous action references, form values, catalog JavaScript/name roots) or another preservation key-pruning slice.
3. Keep attachments/signatures/JPEG transforms separate because they currently depend on specialized flpdf helpers or byte transforms.
4. Keep production `optimize_pdf()` on flpdf until a coherent pass group has Hayro equivalence.
5. Preserve the 405-file structural/text and 17,612-page render gates after each migration group.
6. Decide separately whether to upstream Hayro trailer accessor `d950536`; do not mix unrelated lint cleanup into that contribution.
7. Defer image transforms, graph-wide dedup, and object-stream/xref-stream output work until the sparse mutation architecture has another pass or two of real use.

## Git/auth discipline

`origin` is `https://github.com/codeandsolder/pdf-redox.git`, `main` tracks `origin/main`, and a repo-local credential helper is configured. The GitHub token remains outside the repo; never print or embed it in a remote URL.
