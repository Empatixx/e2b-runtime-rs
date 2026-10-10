#!/usr/bin/env bash
# Runs `cargo test` inside a Linux container (PTY, inotify, setuid, netns need Linux).
set -euo pipefail
exec "$(dirname "$0")/linux-container.sh" cargo test "$@"
