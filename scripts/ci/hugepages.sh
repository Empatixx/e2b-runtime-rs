#!/usr/bin/env bash
set -euo pipefail

HUGEPAGES=${HUGEPAGES:-1024}

echo "$HUGEPAGES" | sudo tee /proc/sys/vm/nr_hugepages >/dev/null
grep -E '^HugePages_(Total|Free)' /proc/meminfo
reserved=$(awk '/^HugePages_Total/ {print $2}' /proc/meminfo)
[ "$reserved" -ge "$HUGEPAGES" ] || { echo "reserved $reserved of $HUGEPAGES huge pages"; exit 1; }
