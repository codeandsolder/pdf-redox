# pdf-deshit

A pure-Rust PDF normalization/optimization engine with a tiny client-side WebAssembly UI.

Design goals:

- do lossless structural cleanup by default;
- allow a perceptual profile only when the user explicitly permits lossy transforms;
- provide a print profile with print-oriented image policy;
- optionally perform a best-effort privacy scrub and report what was removed or merely detected;
- never require uploading the document to a server;
- preserve unsupported/opaque streams rather than damaging them.

The implementation uses `flpdf` for qpdf-style parsing and fresh rewrites. A fresh rewrite is important: it discards incremental-update history and unreachable objects instead of appending yet another revision.

## Workspace

- `pdf-deshit`: core library
- `pdf-deshit-cli`: native CLI
- `pdf-deshit-wasm`: WASM bindings
- `web/`: static no-framework site
- `third_party/flpdf/`: minimal vendored `flpdf` 0.5.1 fork used by the core; provenance and local API extensions are documented in `third_party/flpdf/PATCHES.md`

## Status

Early but functional. The core currently provides:

- whole-document fresh rewrites, garbage collection, object-stream generation, and optional page-content normalization;
- exact duplicate `/Metadata`, embedded font-program, Image XObject, and ICCBased profile canonicalization before garbage collection; metadata references are trusted even when a producer omits the stream's nominal `/Type /Metadata` entry;
- duplicate-aware inline-image externalization that fingerprints the expanded Image XObject dictionary plus exact encoded payload, rewrites only fingerprints repeated across multiple mutable page/Form content scopes whose duplicated payload clears a 1 KiB gate, keeps singleton/marginal or same-stream repeats inline, and shares one indirect Image XObject across scopes; the extra two-pass scan is skipped entirely when pre-analysis sees less than 1 KiB of duplicate inline payload;
- high-level structural/risk profiling plus size-gated selective Flate recompression;
- a real Perceptual raster path that converts eligible 8-bit DeviceGray/RGB/CMYK lossless Image XObjects to JPEG at the configured quality only when the encoded-size savings gate is met, while preserving pixel dimensions and reusing one transcoded object for shared source images;
- a resolution-aware Print raster path that tracks image placement through page transforms, nested Form XObjects, Form matrices, `/UserUnit`, and shared references, then downsamples conservative 8-bit DeviceGray/DeviceRGB JPEG sources to the configured PPI target using Lanczos3 only when the final JPEG also clears the encoded-size savings gate; mutation is page-scoped to direct Image XObject bindings actually observed in that page's render graph, resources are copy-on-write isolated before replacement, and incomplete/recovered geometry disables all dimension-changing transforms for the document; masks, custom decode semantics, CMYK, Form-local-only images, and non-JPEG sources are preserved unchanged;
- metadata, JPEG metadata, active-content, attachment, form/signature, and incremental-history privacy cleanup;
- conservative invisible-text analysis that distinguishes OCR overlays, accessibility text, hidden layers, outside-page text, likely fake-redaction leaks, and uncertain invisible content;
- policy-driven deletion of approved hidden-text operators from decoded page content streams, with category defaults and per-finding overrides;
- a client-side WASM review UI that analyzes first and asks the user what invisible text to remove before rewriting.

Native formatting, strict Clippy, workspace tests, and the `wasm32-unknown-unknown` build are CI gates. The WASM crate enables `getrandom`'s browser JS backend because `flpdf` uses randomness for PDF encryption IV generation.

Extending Print downsampling beyond the deliberately conservative existing-JPEG subset and broader photographic/lossless-image classification remain active optimization work. A corpus-wide scan found no safe Form-local-only Print candidates, so Form-branch cloning is intentionally deferred. The opt-in resource-pruning pass has been corpus-tested and remains default-off because it produced negligible size wins.
