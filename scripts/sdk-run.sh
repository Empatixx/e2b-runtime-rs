#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

SDK_COMMIT=3221e8ec21f093613d104d92c6149cbbcc03d755
SDK_DIR=${E2B_SDK_DIR:-target/e2b-sdk}

[ -d "$SDK_DIR" ] || git clone -q https://github.com/e2b-dev/E2B.git "$SDK_DIR"
git -C "$SDK_DIR" fetch -q --depth 1 origin "$SDK_COMMIT"
git -C "$SDK_DIR" checkout -q "$SDK_COMMIT"

cd "$SDK_DIR"
pnpm install --frozen-lockfile --silent
pnpm --filter @e2b/dockerfile-utils run build >/dev/null
cd packages/js-sdk
E2B_DEBUG=true pnpm exec vitest run --project unit tests/sandbox/commands tests/sandbox/files tests/sandbox/pty "$@"
