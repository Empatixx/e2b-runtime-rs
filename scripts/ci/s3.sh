#!/usr/bin/env bash
set -euo pipefail

docker run -d --name s3 -p 9000:9000 \
  -e RUSTFS_ACCESS_KEY=test-access-key -e RUSTFS_SECRET_KEY=test-secret-key \
  rustfs/rustfs:1.0.1 /data

for _ in $(seq 1 120); do
  status=$(curl -s -o /dev/null -w '%{http_code}' --aws-sigv4 "aws:amz:us-east-1:s3" --user test-access-key:test-secret-key \
    -X PUT http://127.0.0.1:9000/e2b-test || true)
  case "$status" in
    200|409) exit 0 ;;
  esac
  sleep 0.5
done
echo "RustFS did not accept the test bucket within 60s" >&2
docker logs s3 >&2
exit 1
