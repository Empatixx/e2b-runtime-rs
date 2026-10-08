#!/usr/bin/env bash
set -euo pipefail

FC_RELEASE=v1.14.1_621718e
KERNEL_RELEASE=2026.07.28
KERNEL_ASSET=vmlinux-6.1.158-amd64.bin
WORK=$(mktemp -d)
SOCKET="$WORK/fc.sock"

echo "host kernel: $(uname -r)"
curl -fsSL -o "$WORK/firecracker" "https://github.com/e2b-dev/fc-versions/releases/download/$FC_RELEASE/firecracker-amd64"
curl -fsSL -o "$WORK/vmlinux" "https://github.com/e2b-dev/fc-kernels/releases/download/$KERNEL_RELEASE/$KERNEL_ASSET"
chmod +x "$WORK/firecracker"
"$WORK/firecracker" --version

"$WORK/firecracker" --api-sock "$SOCKET" > "$WORK/console.log" 2>&1 &
FC_PID=$!
for _ in $(seq 1 200); do
  [ -S "$SOCKET" ] && break
  sleep 0.05
done
[ -S "$SOCKET" ] || { echo "firecracker API socket did not appear"; exit 1; }

api() {
  curl -fsS --unix-socket "$SOCKET" -X PUT "http://localhost/$1" -H 'content-type: application/json' -d "$2"
}
api machine-config '{"vcpu_count":1,"mem_size_mib":128}'
api boot-source "{\"kernel_image_path\":\"$WORK/vmlinux\",\"boot_args\":\"console=ttyS0 reboot=k panic=1 pci=off\"}"
api actions '{"action_type":"InstanceStart"}'

for _ in $(seq 1 100); do
  grep -q "Linux version" "$WORK/console.log" && break
  sleep 0.1
done
grep -m1 "Linux version" "$WORK/console.log" || { echo "guest kernel did not boot"; exit 1; }
kill "$FC_PID" 2>/dev/null || true

if sudo modprobe ublk_drv && [ -e /dev/ublk-control ]; then echo "ublk: available"; else echo "ublk: NOT available"; fi
if sudo modprobe nbd nbds_max=4 && [ -e /dev/nbd0 ]; then echo "nbd: available"; else echo "nbd: NOT available"; fi
