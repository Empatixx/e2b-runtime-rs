#!/bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
export HOME=/root
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /dev/pts /dev/shm
mount -t devpts devpts /dev/pts
mount -t tmpfs tmpfs /dev/shm
mount -t tmpfs tmpfs /run
mount -t tmpfs tmpfs /tmp
mount -t cgroup2 cgroup2 /sys/fs/cgroup
hostname e2b
ip link set lo up
ip addr add 169.254.0.21/30 dev eth0
ip link set eth0 up
ip route add default via 169.254.0.22
echo "nameserver 8.8.8.8" > /etc/resolv.conf
/usr/bin/envd &
while true; do wait; sleep 1; done
