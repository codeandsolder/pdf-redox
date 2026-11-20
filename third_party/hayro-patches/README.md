# Hayro syntax patches

The working source in `../hayro-syntax` is a generated vendor snapshot of the pinned
[upstream Hayro](https://github.com/LaurenzV/hayro) revision in `UPSTREAM_REV`.
Only this crate is vendored. The sibling image codecs are obtained from the
same exact revision of the upstream repository, not from our fork.

The patch series retains source spans, trailer/object ID enumeration, standalone
decoding, local xref recovery and decode resource limits. Upstream now has its
own object-stream member-index cache and JBIG2 arithmetic exhaustion checks;
those fork-specific implementations are deliberately not included.

To reproduce the tracked tree, obtain the pinned revision in a Hayro checkout and run:

```sh
third_party/hayro-patches/refresh.sh /path/to/hayro
third_party/hayro-patches/refresh.sh /path/to/hayro --check
```

Update the pin, rebase the numbered patches, and update the sibling codec revisions
in the standalone manifest patch together. Do not hand-edit generated files without
updating the corresponding patch. Run the Hayro syntax regressions in the rebased
upstream checkout and pdf-redox's native and WASM checks before committing an update.
The Cargo build needs no patch application step.

The reusable API series is published at
https://github.com/codeandsolder/hayro/tree/upstream/pdf-redox-apis-20261120
for submission upstream. Remove patches once equivalent functionality is upstream.
