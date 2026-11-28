#!/usr/bin/env bash
set -euo pipefail

# Build and execute the real bindings; cargo check alone cannot detect runtime
# traps such as unsupported std::time::Instant or incorrect JS value conversion.
root=$(git rev-parse --show-toplevel)
cd "$root"
metadata=$(cargo metadata --locked --format-version 1)
version=$(jq -er '[.packages[] | select(.name == "wasm-bindgen") | .version] | unique | if length == 1 then .[0] else error("expected one wasm-bindgen version") end' <<< "$metadata")
target_dir=$(jq -er '.target_directory' <<< "$metadata")
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT

# Use the release matching Cargo.lock, rather than a separately maintained
# version pin or an unbounded cargo install. The release includes its checksum.
case "$(uname -sm)" in
    'Linux x86_64') platform=x86_64-unknown-linux-musl ;;
    'Linux aarch64') platform=aarch64-unknown-linux-gnu ;;
    *) echo 'Unsupported wasm-bindgen CLI host' >&2; exit 1 ;;
esac
bundle="wasm-bindgen-$version-$platform"
archive="$bundle.tar.gz"
base="https://github.com/wasm-bindgen/wasm-bindgen/releases/download/$version"
curl --fail --silent --show-error --location "$base/$archive" -o "$work/$archive"
curl --fail --silent --show-error --location "$base/$archive.sha256sum" -o "$work/$archive.sha256sum"
read -r digest _ < "$work/$archive.sha256sum"
[[ "$digest" =~ ^[0-9a-f]{64}$ ]]
(cd "$work" && printf '%s  %s\n' "$digest" "$archive" | sha256sum -c -)
tar -xzf "$work/$archive" -C "$work" "$bundle/wasm-bindgen"
cli="$work/$bundle/wasm-bindgen"
test "$("$cli" --version)" = "wasm-bindgen $version"

cargo build --locked -p pdf-redox-wasm --target wasm32-unknown-unknown
"$cli" --target nodejs --out-dir "$work/pkg" "$target_dir/wasm32-unknown-unknown/debug/pdf_redox_wasm.wasm"
node web/test-wasm.cjs "$work/pkg"
