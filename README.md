# pdf-redox

A pure-Rust PDF normalization/optimization engine with a tiny client-side WebAssembly UI.

Design goals:

- preserve ordinary PDF document/rendering semantics by default while discarding derived or authoring-only state that is not needed to consume the document;
- allow a perceptual profile only when the user explicitly permits lossy transforms;
- provide a print profile with print-oriented image policy;
- keep semantic preservation independent from compression: links, forms, navigation, layers, tagged structure, output intents, viewer preferences, metadata, annotations, and unknown Catalog/page entries are explicit policy knobs;
- optionally perform a best-effort privacy scrub and report what was removed or merely detected;
- never require uploading the document to a server;
- preserve unsupported/opaque streams rather than damaging them.

Hayro is the source parser and codec layer. Source bytes stay immutable and lazily parsed, mutations live in a sparse copy-on-write object overlay, and reachable objects are serialized through the compact fresh writer. Fresh rewrites discard incremental-update history and unreachable objects instead of appending another revision. OpenType parsing, rebuilding, and glyph subsetting are delegated to Fontations (`skrifa`, `write-fonts`, and `skera`); the required Skera glyph-mapping API is available in upstream Skera 0.9, so Fontations is consumed directly from crates.io.

## Workspace

- `pdf-redox`: core library
- `pdf-redox-cli`: native CLI
- `pdf-redox-wasm`: WASM bindings
- `web/`: static no-framework site
- [pdf-redox-ocr](https://github.com/codeandsolder/pdf-redox-ocr): independently versioned Python/RapidOCR inference companion, using native Image `XObject` pixels, painted-page visibility proofs, and the Rust invisible-text importer provided by this repository.

## Status

Early but functional. The core currently provides:

- whole-document fresh rewrites, garbage collection, object-stream generation, and goal-derived page-content normalization (`processing` normalizes parseable streams while `size` preserves their lexical form);
- granular semantic-preservation policy applied directly to the normal parsed object graph, including appearance-only annotation flattening, leaf/intermediate page-tree pruning, Catalog pruning, and a rendering-preserving font policy that strips optional OpenType layout/device tables unused by positioned PDF rendering, subsets embedded outlines only when PDF-side glyph addressing remains provably valid, and densely remaps `CIDFontType2` programs only when every explicit `CIDToGIDMap` user can be rewritten atomically; future font editing/reflow support is treated as authoring data rather than ordinary document semantics (`--preserve-font-editing-support` opts back in); embedded page `/Thumb` preview caches and producer-private `/PieceInfo` authoring payloads are likewise discarded by default because they are derived/editor state rather than page semantics (`--preserve-authoring-data` opts back into the authoring bundle), while ordinary XMP metadata, optional-content state, and output intents remain preserved because they can carry practical document semantics or affect rendering;
- exact duplicate `/Metadata`, embedded font-program, `/ToUnicode` `CMap`, Type3 `/CharProcs`, page `/Contents`, annotation appearance Form streams, Image/Form `XObject`, and `ICCBased` profile canonicalization before garbage collection; metadata references are trusted even when a producer omits the stream's nominal `/Type /Metadata` entry;
- duplicate-aware inline-image externalization that fingerprints the expanded Image `XObject` dictionary plus exact encoded payload, rewrites only fingerprints repeated across multiple mutable page/Form content scopes whose duplicated payload clears a 1 KiB gate, keeps singleton/marginal or same-stream repeats inline, and shares one indirect Image `XObject` across scopes; the extra two-pass scan is skipped entirely when pre-analysis sees less than 1 KiB of duplicate inline payload;
- high-level structural/risk profiling plus size-gated selective Flate recompression;
- a real Perceptual raster path that converts eligible 8-bit DeviceGray/RGB/CMYK lossless Image `XObjects` to JPEG at the configured quality only when the encoded-size savings gate is met, while preserving pixel dimensions and reusing one transcoded object for shared source images;
- a resolution-aware Print raster path that tracks image placement through page transforms, nested Form `XObjects`, Form matrices, `/UserUnit`, and shared references, then downsamples conservative 8-bit DeviceGray/DeviceRGB sources to the configured PPI target (overridable with `max_image_ppi` / `--max-image-ppi`) using Lanczos3: existing JPEGs remain JPEG at the configured quality, while simple lone-Flate sources remain Flate-encoded with a fresh PNG predictor; both paths require the final encoded payload to clear the savings gate. Mutation is page-scoped to direct Image `XObject` bindings actually observed in that page's render graph, resources are copy-on-write isolated before replacement, and incomplete/recovered geometry disables all dimension-changing transforms for the document; masks, custom decode semantics, CMYK, palette/bilevel sources, and Form-local-only images are preserved unchanged;
- metadata, JPEG metadata, active-content, attachment, form/signature, and incremental-history privacy cleanup;
- conservative invisible-text analysis that distinguishes OCR overlays, accessibility text, hidden layers, outside-page text, likely fake-redaction leaks, and uncertain invisible content;
- policy-driven deletion of approved hidden-text operators from decoded page content streams, with category defaults and per-finding overrides;
- a client-side WASM review UI that analyzes first and asks the user what invisible text to remove before rewriting; the WASM layer keeps that exact-byte analysis in memory and reuses it for the subsequent rewrite after a SHA-256 identity check, avoiding a second full structural scan.

The workspace uses the pinned nightly in `rust-toolchain.toml` because the direct `zlib-rs-fast` compression engine uses portable SIMD. Stable Rust is not a supported build configuration for this repository.

Native formatting, strict Clippy, workspace tests, Rustdoc, and the `wasm32-unknown-unknown` build are CI gates.

Extending Print beyond the deliberately conservative simple JPEG/Flate Gray/RGB subset remains active optimization work. A corpus-wide scan found that almost all other oversampled non-JPEG images are bilevel, Indexed, masked, or otherwise semantically unsuitable for the current path, and found no safe Form-local-only Print candidates, so broad raster classification and Form-branch cloning are intentionally deferred. Resource pruning runs by invariant after namespace-qualified usage analysis; malformed or incompletely analyzed scopes are preserved conservatively rather than guessed at.

## Experimental GPU Flate

Build the native optimizer with Vulkan-assisted Deflate enabled:

```sh
cargo build --release -p pdf-redox-cli --bin pdf-redox --features gpu
pdf-redox input.pdf --output output.pdf --optimize-for size --flate-gpu-budget 512
```

On NVIDIA CUDA systems (including WSL2 with the NVIDIA driver and CUDA NVRTC runtime installed), build with the `cuda` feature instead and select the device explicitly:
```sh
cargo build --release -p pdf-redox-cli --bin pdf-redox --features cuda
pdf-redox input.pdf --output output.pdf --optimize-for size --flate-gpu-adapter cuda:0 --flate-gpu-budget 128 --flate-gpu-window-bytes 16777216
```

The CUDA backend performs the entire DEFLATE block pipeline (index, matches, token parsing, Huffman trees, bit packing, and checksums) on the device. It downloads only compressed output, checks its size cap after each bounded window, and reuses the GPU context and memory-bounded nonblocking CUDA streams. Independent windows can overlap on the existing worker pool while preserving output order. It can produce different valid encodings from the older match-table backend; select `cuda-match:0` only for reference measurements.

For eligible existing zlib-compressed PDF streams, CUDA also accepts the original compressed bytes. For compact members up to 8 MiB, CUDA discovers and validates entropy-segment boundaries, retains the compressed input and index, then decodes and recompresses directly on-device. Only a compact window schedule is returned to the host. Larger members or those exceeding bounded speculative-index resources use the exact streaming structural scanner, which overlaps subsequent windows with GPU compression without allocating the expanded input. Single-window streams retain decoded-input GPU compression, since the extra indexing and device-inflate work cannot be overlapped. Accepted streams are validated against the source checksum and the ordinary output cap. Streams with active predictors, external payloads, unsupported framing, or an applicable reuse-cache entry follow the existing decode path. Use `--flate-gpu-decoded-input` to benchmark the decoded-input CUDA path instead. The report distinguishes transcoded streams and decoded bytes processed, device-indexed streams, compact source upload bytes, and descriptor upload bytes.

GPU matching is explicitly opt-in. The final Flate recompression and unfiltered-stream compression passes share one lazily initialized GPU context. CPU-only and GPU-assisted batches overlap on the same worker pool, without increasing its configured CPU thread count. Library callers processing multiple PDFs can reuse an `OptimizationSession` with a fixed `Config` to retain both the worker pool and GPU context across documents; the one-shot CLI creates a single session. Streams smaller than 1 MiB remain on the CPU by default; adjust that threshold with `--flate-gpu-min-stream-bytes`, or choose an adapter with `--flate-gpu-adapter NVIDIA`. The search budget controls the maximum number of predecessor candidates visited per byte, not the compression level. To evaluate bounded, demand-driven GPU matching for capped recompression, use `--flate-gpu-window-bytes 4194304`; this preserves the whole-input GPU encoding while avoiding search of tails the encoder never reaches. To experiment with CPU routing for extremely tight compression caps, set `--flate-gpu-min-cap-per-mille 30` (3% of decoded input size). Use `--flate-gpu-retry-rejected` to retry only GPU output-cap failures with the CPU matcher, trading additional work for more compression opportunities. GPU and CPU encodings can differ while decoding to identical bytes, and the usual output-size acceptance limits remain in force. Size optimization (level 9) on large streams is the intended starting point; lower levels and small streams may be faster on the CPU. Other compression probes still use the CPU. Native builds without the feature and the WASM UI retain their existing CPU paths.

## Documentation scope

Repository documentation is limited to material that should version with the code: user/developer behavior, build and test instructions, APIs, and provenance for vendored modifications. Architecture research, validation chronicles, benchmark history, development handoffs, infrastructure paths, and transient project state are maintained in the project Notion workspace instead.
