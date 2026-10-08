#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
docker run --rm \
  -v "$PWD":/src -v envd-rs-target:/src/target -v envd-rs-cargo:/usr/local/cargo/registry \
  -w /src rust:1.98 \
  bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && rustup component add clippy >/dev/null 2>&1; cargo clippy --workspace --all-targets -- -D warnings'
