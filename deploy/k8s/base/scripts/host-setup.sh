#!/usr/bin/env bash
set -euo pipefail

NBDS_MAX="${NBDS_MAX:-256}"
HUGEPAGES="${HUGEPAGES:-4096}"
PACKAGES=(rsync e2fsprogs iproute2 iptables nftables)

log() { echo "host-setup: $*"; }
die() { echo "host-setup: $*" >&2; exit 1; }

install_packages() {
  local missing=()
  for package in "${PACKAGES[@]}"; do
    dpkg -s "$package" >/dev/null 2>&1 || missing+=("$package")
  done
  [ "${#missing[@]}" -eq 0 ] && return 0
  log "installing ${missing[*]}"
  DEBIAN_FRONTEND=noninteractive apt-get update -qq
  DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${missing[@]}"
}

load_modules() {
  printf 'nbd\ntun\nkvm\n' > /etc/modules-load.d/e2b.conf
  echo "options nbd nbds_max=${NBDS_MAX} max_part=16" > /etc/modprobe.d/e2b-nbd.conf
  cat > /etc/udev/rules.d/97-nbd-device.rules <<'EOF'
ACTION=="add|change", KERNEL=="nbd*", OPTIONS:="nowatch"
KERNEL=="nbd*", GROUP="disk", MODE="0660"
EOF
  udevadm control --reload-rules
  udevadm trigger
  [ -e /dev/nbd0 ] || modprobe nbd nbds_max="$NBDS_MAX" max_part=16
  modprobe tun
  [ -e /dev/kvm ] || die "/dev/kvm is missing: the node size has no nested virtualization"
  local have
  have="$(cat /sys/module/nbd/parameters/nbds_max)"
  [ "$have" -ge "$NBDS_MAX" ] || die "nbd is loaded with nbds_max=$have, need $NBDS_MAX; reboot the node"
}

reserve_hugepages() {
  cat > /etc/sysctl.d/90-e2b.conf <<EOF
vm.nr_hugepages=${HUGEPAGES}
vm.max_map_count=1048576
net.ipv4.ip_forward=1
net.ipv4.tcp_max_syn_backlog=65535
EOF
  sync
  echo 3 > /proc/sys/vm/drop_caches
  echo 1 > /proc/sys/vm/compact_memory
  sysctl -q -p /etc/sysctl.d/90-e2b.conf
  local have
  have="$(awk '/^HugePages_Total/ {print $2}' /proc/meminfo)"
  [ "$have" -ge "$HUGEPAGES" ] || die "reserved $have of $HUGEPAGES huge pages; use a node size with more memory or lower HUGEPAGES"
}

advertise_hugepages() {
  local marker=/var/lib/e2b/hugepages
  [ "$(cat "$marker" 2>/dev/null)" = "$HUGEPAGES" ] && return 0
  echo "$HUGEPAGES" > "$marker"
  log "restarting the kubelet so it advertises $HUGEPAGES huge pages"
  systemctl restart kubelet
}

install -d -m 0755 /var/lib/e2b/bin /fc-versions /fc-kernels /fc-busybox /fc-envd /fc-vm /orchestrator /var/run/netns
install_packages
load_modules
reserve_hugepages
advertise_hugepages
log "ok"
