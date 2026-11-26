#!/bin/sh
# The Quadra engine for the sandbox: compiler + runtime in WebAssembly
# (site/.vitepress/theme/sandbox/wasm), built from the crates of this repository.
set -e
core="$(cd "$(dirname "$0")/../.." && pwd)"
out="$core/site/.vitepress/theme/sandbox/wasm"
(cd "$core" && cargo build -p quadra-wasm --release --target wasm32-unknown-unknown)
# where cargo put it (target/, or a target folder shared with other checkouts)
target=$(cd "$core" && cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
wasm-bindgen --target web --no-typescript --out-dir "$out" "$target/wasm32-unknown-unknown/release/quadra_wasm.wasm"
if command -v wasm-opt >/dev/null 2>&1; then
  wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int -o "$out/quadra_wasm_bg.wasm" "$out/quadra_wasm_bg.wasm"
fi
ls -lh "$out"
