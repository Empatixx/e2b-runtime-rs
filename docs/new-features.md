# New features and deliberate differences from Go

This runtime aims to behave like upstream Go. The items here are where it does something else on purpose, or adds something upstream does not have. Gaps that should still be closed are in [BACKLOG.md](BACKLOG.md).

## Rootfs exposure over ublk

`ROOTFS_EXPOSURE=ublk` attaches the sandbox rootfs through ublk instead of NBD. Upstream has only NBD. A ublk device attaches in 2.0 ms against 12.4 ms for NBD and needs no device pool. NBD stays the default.

Open items:

| Item | Target |
|---|---|
| Not measured inside a Firecracker guest yet (the Colima VM has no KVM); run `fio` at queue depth 1 in a guest on CI or AKS for both transports | perf |
| Each request hops from the queue thread to tokio and copies its data once more; a 4k read at queue depth 1 ties with NBD at p50 (60 us against 57 us) | perf |
| Zero copy (`UBLK_F_AUTO_BUF_REG`) needs kernel 6.15; AKS runs 6.8 | later |
| Queue depth 128 with a 512 KiB buffer per slot can touch up to 64 MiB per device under heavy writeback | perf |
| A process that serves a device and also has it open hangs on exit; the orchestrator opens the device only for the flush before a pause | later |
| No recovery of devices after an orchestrator restart (`UBLK_F_USER_RECOVERY`); dead devices are removed at start instead | later |

## NBD over ioctls

NBD devices connect with ioctls and one socket. Upstream uses generic netlink and can open several connections per device. A netlink port was measured and reverted: a connect right after a device is released waits 100 to 250 ms in `blk_mq_freeze_queue_wait`, while the ioctl connect takes about 17 ms. The netlink port is kept outside the repository on the maintainer's machine.

Open items:

| Item | Target |
|---|---|
| No multi-connection NBD | perf |
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size. The `nowatch` udev rule from `scripts/ci/nbd-host.sh` stops the inotify probes; the kernel's own change events are still probed | perf |

## Dirty pages without confirmed write-protect

The memory server finds out at run time whether the kernel supports write-protected copies. Until that is confirmed, or when it is not supported, a pause takes the dirty set as resident minus empty pages. The result is correct but larger. Upstream always reads the pagemap and needs write-protect to work. `orchestrator.sandbox.snapshot.memory.dirty_source` counts pauses by source (`pagemap`, `resident_unconfirmed`, `resident_unsupported`, `resident_no_uffd`). Upstream does not have this metric.

## NFS proxy on nfsserve

The volume proxy runs on the nfsserve crate, vendored with a small patch in `vendor/nfsserve`, instead of go-nfs. The mount check, the confinement, the handle limit and the settings follow upstream. The remaining differences are in the backlog.

## Local expiry

`E2B_LOCAL_EXPIRY=1` lets the node expire and auto-pause sandboxes itself. By default it is off, and expiry and auto-pause are left to the API evictor, like upstream.

## Parity tooling

- `scripts/parity/envd-diff.sh` runs the Go envd and this envd side by side in the Linux test container and diffs their answers.
- `scripts/parity/upstream-integration.sh` runs upstream's integration suite against either orchestrator.
