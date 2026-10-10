#!/usr/bin/env bash
set -euo pipefail
exec "$(dirname "$0")/linux-container.sh" cargo clippy --workspace --all-targets -- -D warnings
