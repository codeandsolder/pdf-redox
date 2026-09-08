# Web build

```sh
cargo install wasm-pack
wasm-pack build crates/pdf-deshit-wasm --target web --release --out-dir ../../web/pkg
python3 -m http.server -d web 8080
```

The site has no backend. The PDF bytes stay in the browser process.
