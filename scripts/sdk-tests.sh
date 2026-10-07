#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

SDK_COMMIT=3221e8ec21f093613d104d92c6149cbbcc03d755
SDK_DIR=target/e2b-sdk
CONTAINER=envd-rs-sdk

docker build -q -f docker/Dockerfile -t envd-rs:dev . >/dev/null
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" -p 49983:49983 envd-rs:dev >/dev/null
trap 'docker rm -f "$CONTAINER" >/dev/null' EXIT

until curl -sf http://localhost:49983/health; do sleep 0.2; done
curl -sf -X POST http://localhost:49983/init \
  -H 'content-type: application/json' \
  -d '{"defaultUser":"user","defaultWorkdir":"/home/user"}'

[ -d "$SDK_DIR" ] || git clone -q https://github.com/e2b-dev/E2B.git "$SDK_DIR"
git -C "$SDK_DIR" fetch -q --depth 1 origin "$SDK_COMMIT"
git -C "$SDK_DIR" checkout -q "$SDK_COMMIT"

cd "$SDK_DIR"
pnpm install --frozen-lockfile --silent
pnpm --filter @e2b/dockerfile-utils run build >/dev/null
cd packages/js-sdk
E2B_DEBUG=true pnpm exec vitest run --project unit tests/sandbox/commands tests/sandbox/files tests/sandbox/pty "$@"
