#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

LOG=${RUNNER_TEMP:-/tmp}/sandbox.log
BOOT=

start_sandbox() {
  sudo -E env "E2B_RESUME=$1" target/debug/examples/sandbox >> "$LOG" 2>&1 &
  BOOT=$!
  for _ in $(seq 1 180); do curl -sf http://127.0.0.1:49983/health && return 0; sleep 1; done
  curl -sf http://127.0.0.1:49983/health
}

stop_sandbox() {
  [ -n "$BOOT" ] || return 0
  sudo kill -TERM "$BOOT" 2>/dev/null || true
  wait "$BOOT" || true
  BOOT=
}

trap 'stop_sandbox; cat "$LOG"' EXIT

check_egress() {
  local host_ip server
  host_ip=$(ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')
  python3 -m http.server 8099 --bind "$host_ip" >/dev/null 2>&1 &
  server=$!
  for _ in $(seq 1 20); do curl -sf -o /dev/null "http://$host_ip:8099" && break; sleep 0.5; done
  (cd examples/sdk-playground && npm install --silent && BLOCKED_URL="$host_ip:8099" E2B_DEBUG=true node egress.mjs)
  kill "$server"
  sudo ip netns exec ns-4020 nft list chain inet slot-firewall PREROUTE_FILTER | grep -q 'ip daddr @filtered_always_denylist drop' \
    || { echo "guest firewall has no private-range drop rule"; exit 1; }
  sudo ip netns exec ns-4020 nft list set inet slot-firewall filtered_always_denylist | grep -q '10.0.0.0/8' \
    || { echo "guest firewall does not deny the private ranges"; exit 1; }
}

cargo build -q -p orchestrator --example sandbox

start_sandbox 0
check_egress
scripts/sdk-run.sh
stop_sandbox

start_sandbox 1
grep "resumed in" "$LOG"
scripts/sdk-run.sh
