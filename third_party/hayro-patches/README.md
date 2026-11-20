# Hayro syntax patches

The working source in `../hayro-syntax` is a generated vendor snapshot of the pinned
[upstream Hayro](https://github.com/LaurenzV/hayro) revision in `UPSTREAM_REV`.
Only this crate is vendored. The sibling image codecs are obtained from the
same exact revision of the upstream repository, not from our fork.
The unused upstream Python operator generator is omitted; its generated Rust files are retained.

The patch series retains source spans, trailer/object ID enumeration, standalone
decoding, local xref recovery and decode resource limits. Upstream now has its
own object-stream member-index cache and JBIG2 arithmetic exhaustion checks;
those fork-specific implementations are deliberately not included.

To reproduce the tracked tree, obtain the pinned revision in a Hayro checkout and run:

```sh
third_party/hayro-patches/refresh.sh /path/to/hayro
third_party/hayro-patches/refresh.sh /path/to/hayro --check
```

Update the pin (including the workspace's Git dependency), rebase the numbered patches, and update the sibling codec revisions
in the standalone manifest patch together. Do not hand-edit generated files without
updating the corresponding patch. Run the Hayro syntax regressions in the rebased
upstream checkout and pdf-redox's native and WASM checks before committing an update.
The root manifest declares Hayro directly from upstream at the pinned revision.
Cargo's `[patch."https://github.com/LaurenzV/hayro"]` selects the local
syntax snapshot instead. The independent fuzz workspace declares the same
override, since Cargo does not propagate patches from dependencies. Any external
workspace consuming pdf-redox must likewise declare the override until upstream
provides the required APIs. The Cargo build needs no patch application step.

The reusable API series is published at
https://github.com/codeandsolder/hayro/tree/upstream/pdf-redox-apis-20261120
for submission upstream. Remove patches once equivalent functionality is upstream.
