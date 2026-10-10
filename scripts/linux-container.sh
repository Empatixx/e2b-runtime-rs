#!/usr/bin/env bash
# Runs a command in the Linux test image: privileged for netns, nbd and uffd; /tmp on tmpfs so mknod works.
# One run at a time: parallel runs share the target volume, the netns slot indexes and the nbd devices.
set -euo pipefail
cd "$(dirname "$0")/.."
LOCK="${TMPDIR:-/tmp}/envd-rs-linux-container.lock"
until mkdir "$LOCK" 2>/dev/null; do
  holder=$(cat "$LOCK/pid" 2>/dev/null || true)
  if [ -n "$holder" ] && ! kill -0 "$holder" 2>/dev/null; then
    rm -rf "$LOCK"
    continue
  fi
  echo "waiting for another Linux test run (pid ${holder:-?})" >&2
  sleep 5
done
echo $$ > "$LOCK/pid"
trap 'rm -rf "$LOCK"' EXIT
docker build -q -t envd-rs-test -f docker/test.Dockerfile docker >/dev/null
docker run --rm --privileged --tmpfs /tmp:exec \
  -v "$PWD":/src -v envd-rs-target:/src/target -v envd-rs-cargo:/usr/local/cargo/registry \
  -w /src envd-rs-test "$@"
