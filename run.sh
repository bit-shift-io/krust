#!/usr/bin/env bash
set -e
cd "$(dirname "$0")/client-wasm"
wasm-pack build --target web
cd ..
cargo run "$@"