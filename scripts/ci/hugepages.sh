#!/usr/bin/env bash
set -euo pipefail

HUGEPAGES=${HUGEPAGES:-1024}
ATTEMPTS=${HUGEPAGES_ATTEMPTS:-5}

reserved() {
  awk '/^HugePages_Total/ {print $2}' /proc/meminfo
}

for attempt in $(seq 1 "$ATTEMPTS"); do
  sync
  echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null
  echo 1 | sudo tee /proc/sys/vm/compact_memory >/dev/null
  echo "$HUGEPAGES" | sudo tee /proc/sys/vm/nr_hugepages >/dev/null
  [ "$(reserved)" -ge "$HUGEPAGES" ] && break
  echo "attempt $attempt: reserved $(reserved) of $HUGEPAGES huge pages"
  sleep 2
done
grep -E '^HugePages_(Total|Free)' /proc/meminfo
[ "$(reserved)" -ge "$HUGEPAGES" ] || { echo "reserved $(reserved) of $HUGEPAGES huge pages"; exit 1; }
