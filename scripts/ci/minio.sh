#!/usr/bin/env bash
set -euo pipefail

docker run -d --name minio -p 9000:9000 \
  -e MINIO_ROOT_USER=minio -e MINIO_ROOT_PASSWORD=minio123 \
  minio/minio server /data
until curl -sf http://127.0.0.1:9000/minio/health/live; do sleep 0.5; done
docker run --rm --network host --entrypoint sh minio/mc -c \
  "mc alias set local http://127.0.0.1:9000 minio minio123 && mc mb --ignore-existing local/e2b-test"
