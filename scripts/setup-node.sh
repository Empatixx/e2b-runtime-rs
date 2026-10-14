#!/bin/sh
set -eu
ROOT=${E2B_SIMPLE_ROOT:-/mnt/simple}
AGENTENV_REV=f9a39636caa3d234bbb96d2406685d7a6d3dabc4
DEPS=https://github.com/kvcache-ai/firecracker/releases/download/aenv-deps

sudo apt-get install -y -q libclang-dev clang e2fsprogs iptables docker.io
sudo modprobe ublk_drv
sudo mkdir -p "$ROOT/deps" "$ROOT/data"
sudo chown "$(id -u)" "$ROOT" "$ROOT/deps"

cd "$ROOT/deps"
curl -fsSL "$DEPS/firecracker-1.15.1-patch-v1-x86_64.tgz" | tar xz
curl -fsSL -o vmlinux "$DEPS/vmlinux-6.1.175-x86_64"

cd "$ROOT"
[ -d AgentENV ] || git clone -q https://github.com/kvcache-ai/AgentENV.git
git -C AgentENV checkout -q "$AGENTENV_REV"
(cd AgentENV && cargo build --release -p uvm-ublk-daemon)
cp AgentENV/target/release/uvm-ublk-daemon deps/
