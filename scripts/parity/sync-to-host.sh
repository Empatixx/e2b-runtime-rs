#!/usr/bin/env bash
set -euo pipefail

HOST="${1:?usage: sync-to-host.sh <ssh host> [path ...]}"
shift
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
REMOTE="${REMOTE:-/mnt/work/envd-rs}"

cd "$REPO"
git archive HEAD | ssh "$HOST" "mkdir -p $REMOTE && tar -x -C $REMOTE"
[ "$#" -eq 0 ] || tar -c "$@" | ssh "$HOST" "tar -x -C $REMOTE"
