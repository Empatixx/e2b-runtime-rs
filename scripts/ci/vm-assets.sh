#!/usr/bin/env bash
set -euo pipefail

FC_RELEASE=v1.14.1_621718e
KERNEL_RELEASE=2026.07.28
KERNEL_ASSET=vmlinux-6.1.158-amd64.bin
ALPINE=https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
DIR=${1:-/opt/e2b}

sudo mkdir -p "$DIR"
sudo curl -fsSL -o "$DIR/firecracker" "https://github.com/e2b-dev/fc-versions/releases/download/$FC_RELEASE/firecracker-amd64"
sudo curl -fsSL -o "$DIR/vmlinux" "https://github.com/e2b-dev/fc-kernels/releases/download/$KERNEL_RELEASE/$KERNEL_ASSET"
sudo curl -fsSL -o "$DIR/alpine.tar.gz" "$ALPINE"
sudo chmod +x "$DIR/firecracker"
sudo apt-get install -y -qq busybox-static >/dev/null
BUSYBOX_VERSION=$(busybox 2>&1 | head -1 | sed -n 's/^BusyBox v\([0-9.]*\).*/\1/p')
sudo mkdir -p "$DIR/busybox/$BUSYBOX_VERSION/amd64"
sudo cp "$(command -v busybox)" "$DIR/busybox/$BUSYBOX_VERSION/amd64/busybox"
{
  echo "E2B_FIRECRACKER=$DIR/firecracker"
  echo "E2B_KERNEL=$DIR/vmlinux"
  echo "E2B_ALPINE_TAR=$DIR/alpine.tar.gz"
  echo "HOST_BUSYBOX_DIR=$DIR/busybox"
  echo "BUSYBOX_VERSION=$BUSYBOX_VERSION"
} >> "${GITHUB_ENV:-/dev/null}"
