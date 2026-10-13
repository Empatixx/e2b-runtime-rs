#!/usr/bin/env bash
# Runs the Go envd and this envd side by side in the Linux test container and diffs their HTTP and Connect answers.
# Usage: scripts/parity/envd-diff.sh [path to the e2b-runtime checkout]
set -euo pipefail
cd "$(dirname "$0")/../.."
UPSTREAM=${1:-$HOME/IdeaProjects/e2b-runtime}
ARCH=$(docker info --format '{{.Architecture}}' | sed 's/aarch64/arm64/;s/x86_64/amd64/')
mkdir -p .envd-diff
(cd "$UPSTREAM/packages/envd" && GOOS=linux GOARCH=$ARCH CGO_ENABLED=0 go build -o "$OLDPWD/.envd-diff/envd-go" .)
exec scripts/linux-container.sh bash -c '
  set -euo pipefail
  cargo build -q -p envd --bin envd
  id user >/dev/null 2>&1 || useradd -m user
  .envd-diff/envd-go -isnotfc -no-cgroups -port 49983 >/tmp/go.log 2>&1 &
  target/debug/envd -isnotfc -no-cgroups -port 49984 >/tmp/rust.log 2>&1 &
  sleep 2
  python3 scripts/parity/envd-diff.py 49983 49984
'
