#!/usr/bin/env bash
# Builds the web version of the app into crates/gui/web/dist, where the native build picks it
# up for server mode (see crates/gui/build.rs): run this first, then build the app as usual.
# Adds the wasm32 target and installs the wasm-bindgen CLI matching Cargo.lock when missing.
#
# Usage: scripts/build-web.sh
set -euo pipefail

cd "$(dirname "$0")/.."
out=crates/gui/web/dist
version=$(sed -n '/^name = "wasm-bindgen"$/{n;s/^version = "\(.*\)"$/\1/p;}' Cargo.lock)
target_dir=$(cargo metadata --no-deps --format-version 1 | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')

if command -v rustup >/dev/null; then
  rustup target add wasm32-unknown-unknown
fi
if [[ "$(wasm-bindgen --version 2>/dev/null)" != "wasm-bindgen $version" ]]; then
  cargo install wasm-bindgen-cli --version "$version" --locked
fi

cargo build --release --locked --target wasm32-unknown-unknown -p itmo-labels
rm -rf "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out" "$target_dir/wasm32-unknown-unknown/release/itmo-labels.wasm"
ls -l "$out"
