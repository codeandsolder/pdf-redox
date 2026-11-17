# pdf-redox independent OCR workflow

This companion is tracked in the main [pdf-redox repository](../README.md) under `ocr/`. On Acer, `/home/jan/tools/pdf-redox-ocr` is a convenience symlink to the canonical `/home/jan/projects/pdf-redox/ocr`; they contain the same files. The wrapper discovers its Rust build and Python adapter relative to its own directory.

PDF optimization is Rust; OCR inference uses RapidOCR in an isolated uv
environment; the Rust importer inserts invisible Unicode text.

**Current raster OCR (2026-11-17):** embedded PDF images are OCR'd at their
original source-pixel resolution, once per reusable Image XObject. When an
image-only page is assembled from many tiny bitmap fragments, the hybrid
groups their native pixels using actual PDF paint order/geometry, clips and
transforms, then OCRs the mosaic rather than each stripe or a downscaled
page. Repeated groups reuse recognition across pages, including translated
placements; quarter-turn text is rectified and returned to PDF coordinates.
Pages containing only native searchable text bypass OCR. On mixed pages, the
hybrid compares each embedded image's original pixel area against its projected
page-OCR pixels. Images with substantial lost resolution are OCR'd directly,
then their recognized lettering is removed *inside the original source image*
in a temporary render-only PDF. The remaining vector/outlined text is OCR'd
normally, and source-image labels are added only when a Poppler before/after
render comparison proves they were genuinely visible after later PDF painting.
Unsupported image encodings or uncertain visibility revert to normal page OCR.
Verification renders the image-text-masked page at OCR resolution, compares
it with exact-pixel cropped control regions around candidate text, and reuses
the full treatment raster for remaining page OCR. If proof fails, the
unaltered control page is rendered for fallback OCR. Each page is processed
and released before the next one, bounding raster memory.
The default command is still:

    ./run-auto.sh input.pdf searchable.pdf --raster-mode auto

Use `--raster-mode all` to collect additional embedded-image evidence on
mixed pages (may cost extra inference), `--raster-mode off` for normal
page-only comparison. Internal adapters are `raster_hybrid.py`,
`ocr_embedded_images.py`, `ocr_fragment_groups.py` and
`reconstruct_raster_mosaic.py`. The source-image memoization and document
raster reconstruction are independent of the Type3/vector glyph OCR path.
A detailed up-to-date experimental log is in Notion:
https://app.notion.com/p/41bf74be9020812e86a4f0bfe1d305ce
Mixed-page source masking and visibility proof:
https://app.notion.com/p/41bf74be902081a4beeee66b3cbddb6b
Source-isolated overlapping image proof:
https://app.notion.com/p/41bf74be902081bd8d0ceac4c3cb90b6

Before OCR, the adapter makes a temporary render-only PDF copy with searchable
native-font text made invisible (text rendering mode 3), preserving all Type3
glyphs (including outlined glyphs created by pdf-redox), existing path
geometry and photos. The original PDF is used for text-coordinate extraction,
exact hash binding, and final searchable output. Already-searchable native
words are **also** excluded after OCR to prevent mixed-line duplication.
Only selected pages with native searchable words are masked. Regular
non-Type3 font text is only suppressed when its font has a ToUnicode map
or recognizable standard encoding. Fonts with unknown/unrecoverable
Unicode and any Type3 fonts remain visible so they can still be OCRed.
Clipping text modes are preserved, avoiding changes to masked graphics. If selected
pages have no searchable text, masking is skipped. Use --keep-native-text
with batch_rapidocr.py or run-auto.sh to compare without preprocessing.

On 2026-11-16, Shenler selected pages 3,22,53 at 1800 px:
whole OCR-draft generation 13.75 -> 10.04 s (27% faster), inference only
11.85 -> 7.35 s (38% faster), native-overlap OCR rejects 97 -> 0.
Text detections 331 -> 219; accepted lines 197 -> 193; Unicode characters
689 -> 714. Type3 paint (2296 shows) survived the mask, while 213 native
text shows on these selected pages were made invisible. The final PDF never
contains the masked rendering. Accuracy still requires manual evaluation.
 All detected
text regions can be eligible without prior glyph deduplication. Native
searchable words are excluded spatially and model confidence determines whether
unreviewed predictions are used. OCR model output may contain mistakes.

## End-to-end automatic workflow

    /home/jan/tools/pdf-redox-ocr/run-auto.sh input.pdf output.pdf --min-confidence 0.97

Optional --pages 8,10, --max-side 1600, --ocr-threads 6. Uses scoped automatic temporary
directories under /home/jan/benchmarks and cleans up intermediate artifacts.

## Review-first workflow

    pdf-redox input.pdf --optimize-for processing -o optimized.pdf
    uv run --python 3.12 /home/jan/tools/pdf-redox-ocr/batch_rapidocr.py --pdf optimized.pdf --output draft.json
    # Review draft.json. Mark approved individual lines reviewed: true.
    pdf-redox-ocr-overlay optimized.pdf --ocr draft.json -o reviewed.pdf

OCR draft JSON schema 3 contains pdf_sha256, model provenance, pages with
one-based indices, OCR text/confidence/user-space quads and existing native
searchable-word quadrilaterals. The importer verifies the exact PDF hash and
rejects OCR lines overlapping existing native words, preventing duplicate
searchable text. Unreviewed lines require explicit --apply-unreviewed and
the specified confidence threshold; default is reviewed-only.

The vector Type3 catalog remains an independent compression/diagnostics
interface. To visualize glyph reuse, export a catalog separately with
--glyph-catalog and use render_coverage.py (research tool for glyph coverage).

OCR on rasterized images still has limitations: recognition mistakes,
complex page reading order, native-and-outlined mixed lines, and handwriting.
No OCR service is used; inference runs locally with ONNX Runtime.

## Performance settings and measurements (2026-11-16)

The adapter streams Poppler's uncompressed PPM raster bytes directly into
RapidOCR through an in-memory NumPy array; temporary PNG files and their
encoding costs are removed. The array contains the identical RGB pixels.

It batches pdftotext -bbox native-word extraction for contiguous page
ranges, limiting each process to 24 pages, and removes illegal C0 control
characters from Poppler's XHTML. Empty/undecodable text words do not exclude
OCR. This prevents malformed native PDF encodings from being mistaken for
already-searchable text and avoids redundant Poppler subprocess launches.

The default ONNX Runtime intra-op threads are limited to up to six, with
inter-op threads set to one. On Acer Ryzen 4700U, Walsin p8 OCR inference
was approximately 13-14 s warm with default ONNX threads versus ~7.0 s
with six threads, and Shenler p22 improved from 2.6 to ~1.7 s. The
recognized text was byte-identical. Alternative batches of 12, 24 or 32
gave little additional improvement; turning off the angle classifier
introduced errors on Shenler, so it remains enabled. Override with
--ocr-threads N.

Native masking and original-PDF text-preservation behavior is unchanged.
An OCR confidence score is not calibrated semantic correctness: this is
a throughput optimization, not an accuracy claim.


## Fast OCR-only mode

Use --ocr-only on run-auto.sh to add searchable Unicode directly to the
original PDF without running vector/stream optimization beforehand. This
is substantially faster when the PDF is already optimized or shrinking it
isn't needed; for large PDFs the optimizer may take longer than OCR on a
few selected pages. The final PDF preserves the original vector and font
objects, plus a small invisible OCR text layer. The original and the draft
are bound by exact SHA-256. By default, the wrapper still optimizes first
so pdf-redox compression benefits are preserved.

## Experimental structure-aware OCR diagnostics (2026-11-16)

The optional character-evidence path uses RapidOCR's per-character boxes and
pdf-redox's vector Type3 component catalog. It does not modify PDF output and
does not automatically label glyphs. Templates can be partial characters,
joined shapes, or diagram geometry. Repeated agreement is evidence, not truth.

Run this against a Processing-optimized document with its matching catalog:

    uv run --python 3.12 /home/jan/tools/pdf-redox-ocr/batch_rapidocr.py --pdf optimized.pdf --output ocr.json --char-evidence chars.json --pages 8,10
    uv run --offline --no-project --python 3.12 /home/jan/tools/pdf-redox-ocr/glyph_vote_evidence.py --glyph-catalog catalog.json --char-evidence chars.json --output votes.json

For whole-line structural similarity (a distinct diagnostic):

    uv run --offline --no-project --python 3.12 /home/jan/tools/pdf-redox-ocr/structural_evidence.py --glyph-catalog catalog.json --ocr ocr.json --output line-groups.json

Both diagnostics bind their evidence to the optimized PDF's exact SHA-256.
Character candidates require a single geometrically plausible vector
component, strong recognizer probability, multiple matched observations,
and unanimous labels; conflicts are reported explicitly. The normal OCR
workflow does not inject glyph-level hypotheses. Research-only annotation
can be opted into separately after additional independent corroboration.

Optional disjoint-page consistency gate (separate OCR source PDF SHA must match):

    uv run --offline --no-project --python 3.12 /home/jan/tools/pdf-redox-ocr/glyph_vote_evidence.py --glyph-catalog catalog.json --char-evidence chars-training.json --holdout-char-evidence chars-holdout.json --min-holdout-votes 3 --output votes-crossvalidated.json

The extra report classifies evidence candidates as consistent, conflicting or
insufficient holdout. It verifies the holdout pages are disjoint from training
pages, then requires repeated agreement; independent manual/secondary-model
validation is still needed before inserting Unicode globally.

## Contextual independent recognition and semantic A/B (research-only)

The primary independent check is Tesseract 5 LSTM `eng`, run against real
1600px PDF page images (`--psm 11 makebox`). Unlike isolated glyph OCR,
this preserves word context, so lowercase letters are more reliable. Source
raster pixel boxes are mapped through the same CropBox/UserUnit/Rotate-aware
mapping used by RapidOCR. PDF-SHA checks bind all three evidence records.

    uv run --python 3.12 tesseract_page_evidence.py --pdf optimized.pdf --pages 8,10,11,12,28 --output tess-pages.json
    uv run --python 3.12 compare_tesseract_page_boxes.py --pdf optimized.pdf --glyph-catalog catalog.json --votes votes-crossvalidated.json --tesseract-pages tess-pages.json --output tess-contextual.json
    uv run --offline --no-project --python 3.12 promote_glyph_labels.py --glyph-catalog catalog.json --votes votes-crossvalidated.json --contextual tess-contextual.json --labels candidate-labels.json --report candidate-proof.json
    pdf-redox-glyph-labels optimized.pdf --catalog catalog.json --labels candidate-labels.json -o candidate-searchable.pdf
    uv run --offline --no-project --python 3.12 verify_semantic_pdf.py --before optimized.pdf --after candidate-searchable.pdf --pages 8,10,11,28 --dpi 150,300 --output pixel-check.json

The candidate selector defaults to at least 10 character observations on at
least two pages, at least 98% *independent* Tesseract agreement, plus unanimous
RapidOCR training and disjoint holdout. The model has no CJK training here.
It exports a **research draft**, not manually validated text. On DEFOND, the
selected 19 templates cover 5,873 vector appearances on 60 of 74 pages.
The Rust GlyphLabels writer now Flate-encodes rewritten semantic streams,
reducing 3.33 MB of incidental uncompressed content to roughly 10 KB of
additional encoded PDF for the 5,873 annotations. Verified page paintings
remain pixel-identical at 150 and 300 DPI on selected pages. Searchable
characters are still fragmented into individual glyph spans; complete
text-run assembly and actual OCR skipping require further work.

## Structure-first word/run assembly and per-occurrence recognition (experimental)

The reusable Type3 shape catalog is also a PDF content-stream index. The
`structural_runs.py` adapter parses the original optimized PDF instructions in
**source show order**, verifies every (font resource, byte code, per-page glyph
ordinal) against its catalog, and groups only geometrically consecutive,
coplanar, positively advancing labeled outlines. It does not impose arbitrary
left-to-right order on overlapping, multicolumn or rotated content.
Unlabeled glyphs, unknown components, word-size gaps and native text-show
operations are explicit run barriers. Per-occurrence `GlyphLabels.overrides`
can add or suppress Unicode without globally conflating shared shapes.

Example, using an exact matching optimized PDF, catalog and a research label
set from the independent OCR adapter:

    uv run --python 3.12 structural_runs.py --pdf optimized.pdf --glyph-catalog catalog.json --labels labels.json --report runs.json --draft runs-draft.json

The generated schema-3 OCR draft sets **reviewed=false, confidence=0** on
every run: these are structural hypotheses, *not calibrated OCR probabilities*.
The existing Rust importer safely returns the original PDF bytes unmodified
by default, even with `--apply-unreviewed`. To experiment explicitly:

    pdf-redox-ocr-overlay optimized.pdf --ocr runs-draft.json --apply-unreviewed --min-confidence 0 -o hypothesis-searchable.pdf
    uv run --offline --no-project --python 3.12 verify_semantic_pdf.py --before optimized.pdf --after hypothesis-searchable.pdf --pages 8,10,11,12,28 --dpi 150,300 --output visual-identity.json

For OCR recognizer disagreements on the **same canonical vector shape**,
use per-occurrence votes rather than inventing a global l/I/i/0/O mapping:

    uv run --python 3.12 per_occurrence_votes.py --pdf optimized.pdf --glyph-catalog catalog.json --labels labels.json --rapid-chars chars-training.json chars-holdout.json --tesseract-pages tess-pages.json --output-labels appearance-labels.json --report appearance-evidence.json
    uv run --python 3.12 structural_runs.py --pdf optimized.pdf --glyph-catalog catalog.json --labels appearance-labels.json --report occurrence-runs.json --draft occurrence-runs-draft.json

The per-occurrence tool matches each RapidOCR character box and each
independent Tesseract `makebox` rectangle separately to the **same unique**
Type3 appearance, requiring RapidOCR char confidence >= 0.97 and exact
Unicode agreement. Any duplicate conflicting detections or model
disagreements are retained in a separate conflict report; source PDF SHA
must match exactly. Already mapped global shapes are not replaced
automatically (see explicit `--override-global` for a research comparison).
This is useful for ambiguous shapes, but only on pages actually covered by
**both** OCR models; Tesseract `eng` cannot independently verify Chinese text.

In the 74-page DEFOND sample, the strict original model agreed on 19
templates/5,873 placements and produced 1,247 runs/3,199 characters. The
larger evidence tier (`promote_glyph_labels.py --contextual
--min-contextual-observations 20 --min-contextual-agreement .95
--min-holdout-agreement .95 --min-holdout-votes 10`) proposed 23 templates
covering 10,100 full-document occurrences, producing 2,589 runs and 8,090
run characters. Five-page independent per-appearance evidence added **1,028**
source-specific Unicode overrides, including **24 l, 5 I and 2 i**
instances of the same reused template, and the final run assembler generated
**2,849 runs/9,062 characters in about 2.3 seconds** across all 74 pages.
The final experimental overlay costs ~54 kB and tested pages 8, 10, 11, 12,
28 remain pixel-identical at 150 and 300 DPI. None of these counts are
character-error-rate measurements or proof of skipped neural inference.

## Modern model comparison and VLM fallback — 2026-11-16

This adapter already used PP-OCRv6 **small** through RapidOCR 3.10; it is not an
obsolete PP-OCRv3/v4. The small detector + recognizer remains the default. A
full-page PP-OCRv6 **medium** recognizer can be selected for A/B experiments:

    uv run --python 3.12 batch_rapidocr.py --pdf optimized.pdf --pages 8,10,11,12,28 --max-side 1600 --ocr-threads 6 --rec-size medium --rec-model-path /home/jan/benchmarks/pdf-redox-smartocr-20261116/models/PP-OCRv6_rec_medium.onnx --output medium.json --char-evidence medium-chars.json

Alternatively, run medium recognition **only on uncertain lines**, reusing
small-model text boxes and native text masking:

    uv run --python 3.12 refine_ocr_lines.py --pdf optimized.pdf --primary-ocr small.json --secondary-model /home/jan/benchmarks/pdf-redox-smartocr-20261116/models/PP-OCRv6_rec_medium.onnx --max-primary-score 0.97 --report proposals.json --experimental-draft medium-candidates.json

This does not redetect pages. The adapter saves both proposed readings, keeps
the original if they differ only in NFKC width/whitespace, and always marks the
draft as unreviewed with confidence 0. The user must explicitly enable
experimental import; a larger model's probability is **not** evidence of
superior semantic correctness.

A stronger **PaddleOCR-VL-1.6** vision-language model is available as a
separate, structure-guided fallback. Its official GGUF model/projector are
stored under /home/jan/models/pdf-redox-vlm/paddleocr-vl16 on Asus. A prebuilt
llama.cpp b12444 Vulkan release is stored under
/home/jan/tools/pdf-redox-vlm/b12444-vulkan/llama-b12444. Start the model once,
with the NVIDIA RTX 2060 selected as Vulkan1, bound to 127.0.0.1:18754.

    LD_LIBRARY_PATH=/home/jan/tools/pdf-redox-vlm/b12444-vulkan/llama-b12444 /home/jan/tools/pdf-redox-vlm/b12444-vulkan/llama-b12444/llama-server -m /home/jan/models/pdf-redox-vlm/paddleocr-vl16/PaddleOCR-VL-1.6-GGUF.gguf -mm /home/jan/models/pdf-redox-vlm/paddleocr-vl16/PaddleOCR-VL-1.6-GGUF-mmproj.gguf --device Vulkan1 -ngl all -c 4096 --host 127.0.0.1 --port 18754 --parallel 1 --no-warmup

Acer first prepares line crops from the *original, native-text-masked PDF*
rather than passing a stale OCR output image. The selection stage uses the
original source PDF hash, current small OCR line confidence, unresolved Type3
component geometry, native-word overlap exclusion, and a fixed region budget.
On the benchmark, 12 of 442 detected OCR lines were selected:

    uv run --python 3.12 vlm_refine_pdf.py prepare --pdf optimized.pdf --ocr small.json --catalog catalog.json --labels appearance-labels.json --pages 8,10,11,12,28 --output-dir vlm-crops --max-regions 12 --max-primary-confidence 0.97 --min-unresolved-components 6 --min-chars 4 --max-side 2000 --upscale 2

Transfer the crop directory including manifest.json to Asus (use the scoped
benchmark folder), then run locally against the resident VLM server:

    uv run --python 3.12 /home/jan/tools/pdf-redox-vlm/vlm_refine_pdf.py infer --crop-dir vlm-crops --output vlm-results.json --max-tokens 280

Transfer the JSON result back to Acer and merge candidate alternatives into a
normal schema-3 OCR draft:

    uv run --python 3.12 vlm_refine_pdf.py merge --ocr small.json --manifest vlm-crops/manifest.json --vlm vlm-results.json --output vlm-candidates.json --audit vlm-audit.json

The VLM proposal is only applied to the original source line if the source
text and quad match the manifest, model name and source hash are correct, the
image crop SHA-256 is unchanged, the output is a single line and was not
truncated, and the proposed Unicode is meaningfully different after NFKC
width and whitespace equivalence. Every result is unreviewed with
confidence 0. The normal Rust importer returns the original PDF byte-for-byte;
for explicitly authorized research A/Bs:

    pdf-redox-ocr-overlay optimized.pdf --ocr vlm-candidates.json --apply-unreviewed --min-confidence 0 -o vlm-experimental.pdf

### Actual measured accuracy and performance

DEFOND 74-page PDF, same 5 pages 8/10/11/12/28, 1600px Poppler, 6 Acer ONNX
CPU threads, identical small detector. Independently manually transcribed
**86 selected lines / 2,174 Unicode characters**, containing technical
symbols, English, mixed Chinese+English, and technical numerical values.
These are a *targeted* sample, not a representative whole-corpus CER.

| Approach | Source predictions | Extra recognition | Character errors | Exact lines |
|---|---:|---:|---:|---:|
| PP-OCRv6 small | 442 lines in 12.5 s | none | 15 / 2174 (0.690%) | 78/86 |
| PP-OCRv6 medium on full pages | 443 lines in 35.1 s | 22.6 s over small | 14 (0.644%) | 75/86 |
| Small + medium recognition only on 54 low-confidence lines | same 442 detector lines | 6.06 s | 8 (0.368%) | 78/86 |
| Small + PaddleOCR-VL-1.6 on 12 structurally selected crops | same 442 detector lines | 13.9 s of resident VLM inference (12 crops) | **7 (0.322%)** | **79/86** |

Model download/initialization and GPU server startup are excluded from the
13.9-second resident VLM figure. The first cold VLM CLI sample, including
model loading, took 49.9s. The 12-crop resident batch includes short/long
Chinese, technical and diagram regions. Notably the VLM read the actual
diagram value **330~570 gf**, while small hallucinated **353 0S~0E** and
medium read **330~570 gt**; it also accurately reproduced several CJK mixed
paragraphs and the μ symbol. The new per-crop VLM adapter selected 12/442
lines and changed only 7 after NFKC/whitespace-equivalence filtering. Its
normal PDF overlay without explicit approval remains a byte-for-byte no-op;
experimental overlay applied 441 of 442 lines (one native-word overlap
excluded), adding 17,698 bytes over optimized DEFOND and preserving all
pixels on pages 8, 11, 12, 28 at 150 and 300 DPI (eight comparisons).

### Current architectural decision

Use the cheap RapidOCR PP-OCRv6 small detector/recognizer on unresolved page
regions and PDF-native shape recurrence plus occurrence labels for structure.
Keep medium as an optional recognizer-only escalation when CPU-only. Use the
more modern PaddleOCR-VL-1.6 on **a small number of difficult PDF-anchored
regions**, not on every page. It returns region/line text without character
boxes, so reuse original PDF coordinates/line quads for positioning. The
smart pipeline is not yet skipping first-pass neural detection, nor
independently proving CJK/text-run accuracy over a broad corpus.

Code has synthetic source-hash, rotation, stale manifest, NFKC spelling,
non-overlap, incomplete-output and explicit-approval tests. Benchmarks and
crop-source evidence remain in the scoped benchmark folders; no model weights
or Python environments are stored inside the Rust repository.

## PDF-native fast path and detector-free geometry feasibility (2026-11-16)

The default RapidOCR adapter now uses `native_only_pages.py` to check each
selected page before loading the neural model. It parses *all actual painting
instructions*, including recursively invoked Form XObjects. A page is
skipped only if all visible text shows use searchable native fonts and no
visible non-text paint, image, shaded object, unsafe font, visual annotation,
clipped text, or unknown content instruction is encountered. There is no
threshold based on the fraction of already recognized glyphs. It is a
structural searchability check, not a proof of semantic correctness for an
incorrectly authored ToUnicode CMap.

    uv run --python 3.12 batch_rapidocr.py --pdf optimized.pdf --output native-aware.json --pages 1,2,3 --max-side 1600

The normal output includes a page with zero new OCR lines and the original
native-word positions if the page was skipped. The optional character-box
sidecar includes the skipped page with no observations. If every page passes,
the ONNX recognizer is never initialized. To force an A/B of old behavior:

    uv run --python 3.12 batch_rapidocr.py --pdf optimized.pdf --output baseline.json --pages 1,2,3 --no-skip-native-only

On synthetic real PDF files with both ordinary native Helvetica and a nested
Form XObject using inherited resources, three safe pages took **0.06s** vs
**1.00s** with normal OCR, preserving all four native text-word quads.
On a two-page mixed file (safe native page + non-native painted page), the
new flow took 0.46s vs 0.77s and the unsafe page's OCR output was unchanged.
All 74 real DEFOND pages were correctly rejected by this extremely strict
whole-page fast path (non-native paint/clipping/annotations), so it cannot
replace OCR of outlined text. Pages 8/10 still return **identical original
OCR lines and per-character observations**.

For the more ambitious detector-free path, `vector_regions.py` generates
regions from the SHA-bound Type3 glyph appearance quads without consulting
any neural OCR or assigning missing glyph Unicode. `vector_recognize.py`
accepts those exact geometry regions, uses the original native-masked PDF
rendering and runs only PP-OCRv6 *recognition*, never its neural detector.
`evaluate_vector_regions.py` compares geometries to saved conventional OCR
boxes as an *offline diagnostic only*; it never supplies boxes at runtime.

    uv run --python 3.12 vector_regions.py --pdf optimized.pdf --catalog catalog.json --labels appearance-labels.json --pages 8,10,11,12,28 --output proposed.json --min-components 3 --gap-height 2.2 --cap-gap 16 --y-tolerance .38
    uv run --python 3.12 vector_recognize.py --pdf optimized.pdf --regions proposed.json --output tentative.json --report timings.json --max-side 1600
    uv run --offline --no-project --python 3.12 evaluate_vector_regions.py --proposals proposed.json --baseline-ocr previous-small.json --report offline-geometry.json

On DEFOND pages 8/10/11/12/28, height-first line clustering proposed 274
regions, covering >=80% of horizontal extent of 224/442 conventional
detected text lines. Detector-free recognition completed in **6.42s** vs
**12.5s** full-page model runtime. However only **38/86** hand-transcribed
lines were exactly reproduced (29% CER including missing lines) vs **78/86**
at 0.69% CER for standard small OCR. Many lines become fragmented or missing,
and raster/graphic text on page 28 has almost no Type3 geometry. This
alternative is **research only and must not replace normal full-page OCR**.

The key design constraint: recognized vector template counts alone cannot
prove complete words. On the same 74-page document, candidate Unicode
currently covers 11,128 of 27,434 Type3 appearances, with most directly
assembled text runs only 2–6 glyphs long. Research showed only eight
repeated detector-line signature matches across four groups out of 442
detections, so this particular sample does not justify a glyph-line cache.
New focused tests exercise rotated glyphs, outline grouping, SHA binding,
invalid page IDs, nested forms, bitmap XObjects, graphical path paint,
invisible text, unsafe fonts, native text preservation and accurate fallback.

## Current mixed-page image OCR and visibility-proof rendering (2026-11-17)

Image text is recognized from embedded raster source pixels, not from a
rescaled whole page. The automatic mixed-page path compares native image
sample count with the planned page-render sample count, then selectively
recognizes high-resolution images and removes their recognized text only in
a temporary copy used for remaining page OCR.

**Visibility evidence is page-specific.** A text label within a shared
Image XObject may be visible on one page and occluded on another. Per-page
acceptance uses a differential Poppler render of two temporary PDFs:
both have native searchable fonts hidden, but only the treatment PDF has
source-image text erased. Repeated image objects therefore are recognized
once yet admitted into the semantic PDF only where actually visible.

**One full treatment and cropped control proofs.** Poppler's `-x/-y/-W/-H`
crop output is pixel-identical to a corresponding full-page rectangle at the
same `-scale-to`. The verifier groups candidate label regions into at most
four crops (or uses a full control when grouping would cost too much). The
already-rendered full treatment page goes straight to PP-OCRv6, without a
separate OCR rendering. If the proof fails, the original full control page
is rendered for fallback recognition. Verification and OCR proceed page
by page with bounded raster memory and a shared PP-OCRv6 ONNX model.

**Overlapping image sources are verified separately.** If projected source
text polygons intersect, the verifier creates a temporary version with
**only one Image XObject's text erased**, leaving the other image layers
untouched. A cropped Poppler counterfactual now attributes actual visible
pixel changes to that exact source, not to the image hiding it. Verified source IDs, per-page **paint indices**, and original PDF text quads
are matched when merging OCR results, so identical text in an occluded
image cannot borrow a visible paint's proof. Only independently proven
image paints are suppressed in the temporary OCR-input raster.

**Repeated paints of the same Image XObject** now have independent proofs.
The temporary PDF replaces precisely one expanded `Do` paint instruction
with an alias of the text-masked image. When the instruction sits inside
a Form XObject, the implementation clones only that invocation's Form
path, including recursively nested Forms; shared Forms and resources used
elsewhere retain the original source pixels. The paint-order/identity
mapping comes from the PDF image placement interpreter. On two opaque
identical overlapping paints, only the upper paint contributes visible
text, so the pipeline masks and imports only its one OCR line.

The original source PDF and its visible page content are never modified.

The only output written to the user's semantic PDF is Rust invisible Unicode
text, with exact PDF-SHA matching, strict convex-polygon deduplication and
unchanged visible pixels. The temporary comparison PDFs are never exported.
If some image paints contain visible text while others are occluded, a
temporary **selective per-paint mask** removes only independently verified
appearances; page OCR retains the other images' visible content. When none are
verified, the native-font-hidden control page is used unchanged. The normal
page-only route remains for unsupported image encodings or geometry.

The Python regression tests check Poppler render invocation count, per-page
occlusion, source-image reuse, mixed native fonts, SHA provenance, exact
Unicode extraction through the rebuilt Rust CLI and pixel identity.
Use `--raster-mode all` to inspect every supported source image;
`--raster-mode auto` (the default) applies the sampling-based selector.

Detailed architecture, source snapshots and experiments:
https://app.notion.com/p/41bf74be902081a4beeee66b3cbddb6b
Source-isolated overlapping image proof:
https://app.notion.com/p/41bf74be902081bd8d0ceac4c3cb90b6
