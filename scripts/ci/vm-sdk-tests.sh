#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

LOG=${RUNNER_TEMP:-/tmp}/cold_boot.log
cargo build -q -p orchestrator --example cold_boot
sudo -E target/debug/examples/cold_boot > "$LOG" 2>&1 &
BOOT=$!
trap 'sudo kill -TERM $BOOT 2>/dev/null; wait $BOOT || true; cat "$LOG"' EXIT

for _ in $(seq 1 120); do curl -sf http://127.0.0.1:49983/health && break; sleep 1; done
curl -sf http://127.0.0.1:49983/health

HOST_IP=$(ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')
python3 -m http.server 8099 --bind "$HOST_IP" >/dev/null 2>&1 &
SERVER=$!
for _ in $(seq 1 20); do curl -sf -o /dev/null "http://$HOST_IP:8099" && break; sleep 0.5; done
(cd examples/sdk-playground && npm install --silent && BLOCKED_URL="$HOST_IP:8099" E2B_DEBUG=true node egress.mjs)
kill $SERVER
DROPPED=$(sudo ip netns exec ns-4020 iptables -w -L FORWARD -v -n -x | awk '$3 == "DROP" && $9 ~ /^(10\.0\.0\.0\/8|172\.16\.0\.0\/12|192\.168\.0\.0\/16)$/ { sum += $1 } END { print sum + 0 }')
[ "$DROPPED" -gt 0 ] || { echo "guest firewall did not drop the private-range probe"; exit 1; }

scripts/sdk-run.sh
