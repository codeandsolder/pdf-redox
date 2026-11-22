# fax 0.3.0 packed-line encoder patch

The adjacent fax directory is a reproducible source snapshot of crates.io fax
0.3.0 (MIT), not a separate project or fork. UPSTREAM_SHA256 is the checksum
from the pre-patch Cargo.lock, verified against the published .crate archive.
Only source, manifest, license, readme, examples and tests are retained.

The single patch projects MSB-first packed bilevel rows into color transition
positions in 64-bit chunks, sharing the original Group 4 encoder state machine.
Unused tail bits do not participate. The per-pixel entry point is unchanged,
and the pdf-redox tests compare exact encoded bytes against it.

Refresh or verify using the cached archive or an independently downloaded
fax-0.3.0.crate:

```sh
third_party/fax-patches/refresh.sh /path/to/fax-0.3.0.crate
third_party/fax-patches/refresh.sh /path/to/fax-0.3.0.crate --check
```

Both the main and fuzz workspaces use the same local Cargo patch. Remove it
when an upstream release provides an equivalent packed-line API.
