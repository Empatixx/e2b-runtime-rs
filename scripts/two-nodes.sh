#!/bin/sh
set -eu
ROOT=${E2B_SIMPLE_ROOT:-/mnt/simple}
BIN=$ROOT/repo/target/release/e2b-node
export E2B_API_KEY=${E2B_API_KEY:-e2b_simple_local}
export E2B_SIMPLE_S3_ACCESS_KEY=${E2B_SIMPLE_S3_ACCESS_KEY:-e2bsimple}
export E2B_SIMPLE_S3_SECRET_KEY=${E2B_SIMPLE_S3_SECRET_KEY:-e2bsimple-secret}

start_node() {
  name=$1 port=$2 slots=$3
  E2B_SIMPLE_DATA=$ROOT/data-$name E2B_SIMPLE_SLOT_BASE=$slots \
  E2B_SIMPLE_NODE_ID=node-$name E2B_SIMPLE_NODE_API_URL=http://127.0.0.1:$port \
  E2B_SIMPLE_API_ADDR=0.0.0.0:$port E2B_SIMPLE_PROXY_ADDR=0.0.0.0:$((port + 1)) E2B_DOMAIN=node-$name.localhost \
  RUST_LOG=${RUST_LOG:-info} nohup "$BIN" serve > "$ROOT/node-$name.log" 2>&1 &
}

pkill -x e2b-node || true
sleep 1
start_node a 3000 0
start_node b 3100 1000
