#!/usr/bin/env bash
set -euo pipefail

patches="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
vendor="$(dirname -- "$patches")/fax"
archive="${1:?usage: refresh.sh /path/to/fax-0.3.0.crate [--check]}"
mode="${2:-}"
if [[ -n "$mode" && "$mode" != --check ]]; then
    echo "Unknown argument: $mode" >&2
    exit 2
fi
printf '%s  %s\n' "$(cat "$patches/UPSTREAM_SHA256")" "$archive" | sha256sum -c
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
tar -xzf "$archive" --strip-components=1 -C "$scratch" \
    fax-0.3.0/Cargo.toml fax-0.3.0/Cargo.toml.orig fax-0.3.0/LICENSE \
    fax-0.3.0/README.md fax-0.3.0/src fax-0.3.0/examples fax-0.3.0/tests
for patch in "$patches"/*.patch; do
    git -C "$scratch" apply --no-index "$patch"
done
if [[ "$mode" == --check ]]; then
    diff -ru "$scratch" "$vendor"
else
    rsync -a --delete -- "$scratch/" "$vendor/"
fi
