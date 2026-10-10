#!/usr/bin/env bash
# Runs a command in the Linux test image: privileged for netns, nbd and uffd; /tmp on tmpfs so mknod works.
set -euo pipefail
cd "$(dirname "$0")/.."
docker build -q -t envd-rs-test -f docker/test.Dockerfile docker >/dev/null
docker run --rm --privileged --tmpfs /tmp:exec \
  -v "$PWD":/src -v envd-rs-target:/src/target -v envd-rs-cargo:/usr/local/cargo/registry \
  -w /src envd-rs-test "$@"
