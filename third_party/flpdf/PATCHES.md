# Local flpdf fork

This directory contains a minimal vendored copy of `flpdf` 0.5.1 from
`https://github.com/fulgur-rs/flpdf`, based on upstream commit
`264fe48261387a06d293771ca02932e769abbc42` (2026-09-08).

Only the library crate, its optional `flpdf-libjpeg-compat` sibling, workspace
metadata, README, and licenses are vendored. Upstream CLI, CI, fuzzing, docs,
and large integration-test fixtures are intentionally omitted.

## Local image-optimization extensions

The local fork keeps qpdf-compatible behavior as the default while exposing a
few opt-in controls needed by pdf-deshit:

- `ImageOptimizationOptions::jpeg_quality` controls DCT/JPEG quality. The
  default remains qpdf-compatible quality 75.
- `min_savings_bytes` and `min_savings_percent` require an encoded-size win
  before replacing a lossless image. Defaults remain equivalent to upstream:
  any positive saving is accepted.
- `optimize_images_with_stats()` preserves the existing `optimize_images()`
  API and additionally reports unique images transformed, source/optimized
  encoded bytes, and shared source references reused.
- shared indirect source Image XObjects are transcoded once and the resulting
  Image XObject is reused across resource dictionaries. Distinct source
  objects are never merged merely because their bytes match; pdf-deshit's
  separate exact-image canonicalizer handles that case.
- `optimize_images_with_resize_targets()` accepts explicit `(page, source
  image) -> pixel target` entries and uses `fast_image_resize` with Lanczos3
  before JPEG encoding. This path is intentionally separate from
  qpdf-compatible image optimization. It mutates only direct page Image
  XObject bindings selected by the caller, copy-on-write isolates the page's
  effective `/Resources` and `/XObject` dictionaries only after a resize has
  cleared the final encoded-size gate, and reuses one resized object for
  matching shared-source/target pairs. It currently accepts only conservative
  8-bit DeviceGray/DeviceRGB DCT sources with a single DCT filter and no
  `/Mask`, `/SMask`, `/Decode`, or `/DecodeParms`.
- `Pdf::num_warnings()` is public in the local fork so a placement-aware caller
  can snapshot qpdf-style repair diagnostics around content parsing and veto
  dimension-changing transforms when parsing required recovery.
- `ImageOptimizationStats` additionally reports resize count and source/output
  pixel totals for callers using the targeted resize path.

The original `PlDct::new_compressor()` still takes the quality-75 path.
Custom-quality callers use `new_compressor_with_quality()`.

## Validation

The fork retains upstream unit tests embedded in `src/`. Local regression
coverage additionally verifies that configured JPEG quality changes output and
that the savings gate rejects an otherwise-smaller conversion. pdf-deshit's
own tests construct a two-page PDF sharing one lossless image and verify one
transcode plus one cached-reference reuse through a complete write/reopen
round trip.

When updating upstream, rebase these small extensions first rather than
copying newer source over this directory blindly.
