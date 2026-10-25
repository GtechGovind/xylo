#!/usr/bin/env bash
# Compile a real Rust guest, package it, then exercise actual installed authority.
set -euo pipefail

workbench_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$workbench_root"
export CARGO_INCREMENTAL=0
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$workbench_root/target}"
sdk_manifest="$workbench_root/sdk/plugins/Cargo.toml"

rustup target add --toolchain 1.98.0 wasm32-unknown-unknown
cargo test --locked --manifest-path "$sdk_manifest" --workspace
cargo clippy --locked --manifest-path "$sdk_manifest" --workspace --all-targets -- -D warnings
cargo build --locked --release --manifest-path "$sdk_manifest" \
  -p workbench-text-report --target wasm32-unknown-unknown
mkdir -p "$CARGO_TARGET_DIR"
sdk_run="$(mktemp -d "$CARGO_TARGET_DIR/plugin-sdk-validation.XXXXXX")"
cargo run --locked --manifest-path "$sdk_manifest" -p workbench-plugin-pack -- \
  "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/workbench_text_report.wasm" \
  "$workbench_root/sdk/plugins/examples/text-report/package.json" "$sdk_run/text-report"
cargo run --locked -p workbench-capabilities --example validate_plugin_sdk -- \
  "$sdk_run/text-report/plugin.json" | tee "$sdk_run/validation.json"
printf 'SDK package and validation evidence: %s\n' "$sdk_run"
