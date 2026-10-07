#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

DIR=${1:-/opt/e2b}
SIZE=2G
WORK=$(mktemp -d)
CONTAINER=e2b-guest-export
trap 'sudo rm -rf "$WORK"; docker rm -f "$CONTAINER" >/dev/null 2>&1 || true' EXIT

cargo build -q -p envd
cp target/debug/envd docker/envd.service "$WORK/"
docker build -q -f docker/guest.Dockerfile -t e2b-guest:dev "$WORK" >/dev/null
docker create --name "$CONTAINER" e2b-guest:dev >/dev/null
mkdir "$WORK/root"
docker export "$CONTAINER" | sudo tar -x -C "$WORK/root"
echo "nameserver 8.8.8.8" | sudo tee "$WORK/root/etc/resolv.conf" >/dev/null
echo "e2b" | sudo tee "$WORK/root/etc/hostname" >/dev/null
sudo mkdir -p "$DIR"
sudo rm -f "$DIR/rootfs.ext4"
sudo mkfs.ext4 -q -F -d "$WORK/root" "$DIR/rootfs.ext4" "$SIZE"
echo "E2B_ROOTFS=$DIR/rootfs.ext4" >> "${GITHUB_ENV:-/dev/null}"
