#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

CONTAINER=envd-rs-sdk

docker build -q -f docker/Dockerfile -t envd-rs:dev . >/dev/null
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker run -d --name "$CONTAINER" -p 49983:49983 envd-rs:dev >/dev/null
trap 'docker rm -f "$CONTAINER" >/dev/null' EXIT

until curl -sf http://localhost:49983/health; do sleep 0.2; done
curl -sf -X POST http://localhost:49983/init \
  -H 'content-type: application/json' \
  -d '{"defaultUser":"user","defaultWorkdir":"/home/user"}'

scripts/sdk-run.sh "$@"
