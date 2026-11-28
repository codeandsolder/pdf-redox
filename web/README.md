# Web build

```sh
cargo install wasm-pack
wasm-pack build crates/pdf-redox-wasm --target web --release --out-dir ../../web/pkg
python3 -m http.server -d web 8080
```

The site has no backend. The PDF bytes stay in the browser process.

The WASM API returns ordinary JSON-compatible report objects and a `Uint8Array` for the optimized PDF. Hidden-text policies are JSON-compatible objects. No dynamic JavaScript evaluation is required.

Runtime regression test (from the repository root, using the matching wasm-bindgen CLI):

```sh
cargo build --locked -p pdf-redox-wasm --target wasm32-unknown-unknown
wasm-bindgen --target nodejs --out-dir target/wasm-node target/wasm32-unknown-unknown/debug/pdf_redox_wasm.wasm
node web/test-wasm.cjs target/wasm-node
```
