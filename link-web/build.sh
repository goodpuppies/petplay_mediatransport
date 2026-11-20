#!/bin/sh
# Build the browser link module: wasm, then bindings for browsers (pkg-web) and Deno (pkg-deno,
# for tests). Needs `rustup target add wasm32-unknown-unknown` and wasm-bindgen-cli =0.2.130
# (the crate's pinned version): `cargo install wasm-bindgen-cli --version =0.2.130 --locked
# --root ../target/tools`.
set -e
cd "$(dirname "$0")"
cargo build --release --target wasm32-unknown-unknown --target-dir ../target/link-web
WASM=../target/link-web/wasm32-unknown-unknown/release/petplay_link_web.wasm
BINDGEN=${WASM_BINDGEN:-../target/tools/bin/wasm-bindgen}
"$BINDGEN" --target web --out-dir pkg-web "$WASM"
"$BINDGEN" --target deno --out-dir pkg-deno "$WASM"
