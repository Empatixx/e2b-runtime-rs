#!/usr/bin/env bash
set -euo pipefail

docker run -d --name s3 -p 9000:9000 \
  -e RUSTFS_ACCESS_KEY=minio -e RUSTFS_SECRET_KEY=minio123 \
  rustfs/rustfs:latest /data
until curl -sf -o /dev/null http://127.0.0.1:9000/health; do sleep 0.5; done
curl -sf --aws-sigv4 "aws:amz:us-east-1:s3" --user minio:minio123 -X PUT http://127.0.0.1:9000/e2b-test
