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

## Status

Early but functional. The core currently provides:

- whole-document fresh rewrites, garbage collection, object-stream generation, and content normalization;
- high-level structural/risk profiling and pathological-Flate recompression detection;
- metadata, JPEG metadata, active-content, attachment, form/signature, and incremental-history privacy cleanup;
- conservative invisible-text analysis that distinguishes OCR overlays, accessibility text, hidden layers, outside-page text, likely fake-redaction leaks, and uncertain invisible content;
- policy-driven deletion of approved hidden-text operators from decoded page content streams, with category defaults and per-finding overrides;
- a client-side WASM review UI that analyzes first and asks the user what invisible text to remove before rewriting.

Native formatting, strict Clippy, workspace tests, and the `wasm32-unknown-unknown` build are CI gates. The WASM crate enables `getrandom`'s browser JS backend because `flpdf` uses randomness for PDF encryption IV generation.

Image transcoding, semantic stream/font/resource deduplication, inline-image externalization, resource pruning, and corpus-driven structural normalization remain active optimization work.
