#!/usr/bin/env bash
# Runs `cargo test` inside a Linux container (PTY, inotify, setuid need Linux).
set -euo pipefail
cd "$(dirname "$0")/.."
docker run --rm \
  -v "$PWD":/src -v envd-rs-target:/src/target -v envd-rs-cargo:/usr/local/cargo/registry \
  -w /src rust:1.98 \
  bash -c 'apt-get update -qq && apt-get install -y -qq protobuf-compiler >/dev/null && cargo test "$@"' -- "$@"
