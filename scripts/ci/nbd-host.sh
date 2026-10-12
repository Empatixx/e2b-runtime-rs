#!/usr/bin/env bash
set -euo pipefail

NBDS_MAX=${NBDS_MAX:-128}

sudo tee /etc/udev/rules.d/97-nbd-device.rules >/dev/null <<'EOF'
# Disable inotify watching of change events for NBD devices
# https://lore.kernel.org/lkml/20220422054224.19527-1-matthew.ruffell@canonical.com/
ACTION=="add|change", KERNEL=="nbd*", OPTIONS:="nowatch"
KERNEL=="nbd*", GROUP="disk", MODE="0660"
EOF
sudo udevadm control --reload-rules
sudo udevadm trigger

if [ ! -e /dev/nbd0 ]; then
  sudo modprobe nbd nbds_max="$NBDS_MAX"
fi

if ! modinfo ublk_drv >/dev/null 2>&1; then
  sudo apt-get install -y -qq "linux-modules-extra-$(uname -r)"
fi
sudo modprobe ublk_drv
