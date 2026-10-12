# ublk rootfs exposure

Date: 2026-10-12. Status: draft for review.

## Goal

Expose the sandbox rootfs overlay through **ublk** as well as NBD. Both paths serve the same `Overlay`. A node setting picks the transport, and NBD stays the default.

This is a deliberate difference from upstream Go, which only knows NBD. Clients see no change: the API, the snapshot format and the bucket layout stay the same. Only the host side differs.

## Why

The spike on 2026-10-12 (Colima, aarch64, kernel 6.8, backend in RAM, our NBD server against libublk 0.4.8) measured:

| | NBD | ublk |
|---|---|---|
| attach a device, p50 | 11.0 ms | 1.2 ms |
| 4k random read, queue depth 1, p50 latency | 45 us | 30 us |
| 4k random write, queue depth 1, p50 latency | 87 us | 28 us |
| 128k sequential read, queue depth 1 | 1.4 GB/s | 2.2 GB/s |

Firecracker drives use `io_engine: Sync`, which sends one request at a time, so queue depth 1 is the case that matters. ublk also removes the NBD device pool, the 90 second NBD timeout and the slow netlink connect.

We have not measured inside a Firecracker guest yet: Colima has no KVM. That is the first check on CI.

## Out of scope

- Shared page cache for the memfile, OverlayBD and OCI images.
- Zero copy (needs kernel 6.15, AKS runs 6.8).
- ublk user recovery after an orchestrator restart.
- Removing NBD.

## Choosing the transport

A new node environment variable, read once at start:

```
ROOTFS_EXPOSURE=nbd    (default)
ROOTFS_EXPOSURE=ublk
```

Any other value stops the orchestrator at start with `invalid ROOTFS_EXPOSURE "<value>": want nbd or ublk`.

With `ublk`, the orchestrator checks at start that `/dev/ublk-control` exists. If it does not, it stops with `ublk is not available: /dev/ublk-control is missing, load the ublk_drv module`. There is no silent fallback to NBD, so a node never runs a transport its operator did not pick.

An environment variable fits better than a feature flag. The choice depends on the node's kernel modules, not on a team or a sandbox, and upstream has no flag for it.

## Layout

```
src/exposure.rs          pub mod nbd; pub mod ublk; Transport, Exposure, expose()
src/exposure/nbd/...     unchanged, except Exposure moves up
src/exposure/ublk.rs     pub mod device; pub mod queue;
src/exposure/ublk/device.rs   UblkDevice: one thread that adds, runs and deletes the device
src/exposure/ublk/queue.rs    the queue loop: ublk request to BlockDevice call and back
src/exposure/block_device.rs  sync_and_drop_cache(path), shared by both flush paths
```

`exposure.rs` becomes the one entry point. Callers in `sandbox.rs`, `sandbox/resume.rs`, `sandbox/resources.rs`, `builder/phases/ensure_free_disk/offline.rs` and the two contract tests import `crate::exposure::{Exposure, expose}` instead of `exposure::nbd`.

```rust
pub enum Transport { Nbd, Ublk }

pub enum Exposure {
    Nbd(nbd::Exposure),
    Ublk(ublk::Exposure),
}

pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure>
```

`Exposure` keeps the four methods callers use today: `path()`, `flush()`, `ended()` and `close()`. Each one is a `match` over the two variants. The transport comes from a `OnceLock<Transport>` set by `NodeConfig::from_env`. Tests set it with `exposure::use_transport(Transport::Ublk)` before the first `expose`.

## The ublk device

One `UblkDevice` per sandbox, with one OS thread that owns it. libublk keeps its control ring in a thread local, so the add, run and delete calls must all happen on that thread.

```
expose(overlay)
  spawn thread "ublk-<n>":
    UblkCtrlBuilder: 1 queue, depth 128, io_buf_bytes 512 KiB, UBLK_DEV_F_ADD_DEV
    run_target(
      params:  dev_size = overlay.size()
               logical and physical block 4096 (same as NBD set_block_size(PAGE_SIZE))
               no UBLK_ATTR_VOLATILE_CACHE, so the kernel sends no FLUSH (NBD sends none either)
               discard and write zeroes on, max 32 MiB each
      queue:   queue::serve(qid, dev, overlay, tokio handle)
      started: send dev_id over a oneshot
    )
    del_dev()
    drop the "serving" watch sender
  await dev_id, wait for /dev/ublkb<id>
  return Exposure { id, thread, serving }
```

One queue is enough. Firecracker sends one request at a time, and in the spike one queue had the best latency at queue depth 1. More queues mean more threads per sandbox for no gain.

### Queue loop and tokio

`Overlay::read_at` is async on tokio (it can fetch chunks from storage). libublk runs each queue on its own thread with a `smol` local executor. The queue task hands each request to tokio and awaits the result:

```
smol task per tag:
  loop:
    request = commit_and_fetch(previous result)
    result  = tokio_handle.spawn(perform(overlay, request)).await
```

`perform` matches on the op the same way `nbd::server::perform` matches on the command:

| ublk op | BlockDevice call | NBD command it mirrors |
|---|---|---|
| READ | `read_at` into an owned buffer, then copied into the tag's io buffer | Read |
| WRITE | `write_at` in `spawn_blocking` | Write |
| DISCARD, WRITE_ZEROES | `write_zeroes_at` in `spawn_blocking` | Trim, WriteZeroes |
| FLUSH | returns 0 (not sent, because the cache is not volatile) | Flush |
| anything else | `-EINVAL` | |

A `BlockDevice` error answers `-EIO`, and the queue keeps serving. That matches the NBD server, which replies with an error and goes on.

A tokio `JoinHandle` can be awaited from smol, because wakers work across runtimes. The extra copy on reads is the same copy the NBD server makes into its reply buffer.

### flush, ended, close

- `flush()`: `fsync` on `/dev/ublkb<id>`, then the `BLKFLSBUF` ioctl, run in `spawn_blocking`. This is the code `nbd::device::flush_device` has today, moved to `exposure/block_device.rs` so both transports share it. Error texts become `sync block device: ...` and `flush block device buffers: ...`. The caller still wraps them in `flushing COW device failed: ...`, the text clients see.
- `ended()`: resolves when the device thread drops its watch sender, the same way the NBD `Exposure` does.
- `close()`: in `spawn_blocking`, `UblkCtrl::new_simple(id)?.kill_dev()`. Then join the thread, which runs `del_dev`. Errors read `stop ublk device: ...` and `ublk device thread: ...`. Dropping an `Exposure` without `close` also kills the device. The spike showed that this works from a second thread.

## Host and build changes

| Where | Change |
|---|---|
| `deploy/k8s/base/scripts/host-setup.sh` | when `ROOTFS_EXPOSURE=ublk`: install `linux-modules-extra-$(uname -r)` if `ublk_drv` is missing, add `ublk_drv` to `/etc/modules-load.d/e2b.conf`, add `KERNEL=="ublkb*", OPTIONS:="nowatch"` to the udev rule (the same blkid probing problem as NBD) |
| `deploy/k8s/base/orchestrator.yaml` | mount host `/dev` as a hostPath. A privileged container only sees the devices that existed when it started, and `/dev/ublkcN` and `/dev/ublkbN` appear later |
| `scripts/ci/nbd-host.sh` | renamed to `block-host.sh`, also installs `linux-modules-extra` and loads `ublk_drv` |
| `docker/test.Dockerfile` | add `libclang-dev` (bindgen in `libublk-rs-sys`) |
| `scripts/linux-container.sh` | add `-v /dev:/dev` |
| `Cargo.toml` (orchestrator) | `libublk = "0.4"`, `smol = "2"` |

After an AKS node image upgrade the kernel changes, so `host-setup` installs the matching `linux-modules-extra` on every start, not once.

## Testing

- `tests/ublk_contract.rs`: the same cases as `nbd_contract.rs`, run against ublk. ext4 survives pause and resume, concurrent exposures get distinct devices, dropping an exposure removes the device, parallel writers and readers see their own data, and close works on a current thread runtime. Each case is skipped when `/dev/ublk-control` is missing, the same way the NBD cases check `ready()`.
- Unit tests in `exposure/ublk/queue.rs` for the op to `BlockDevice` mapping and the `-EIO` and `-EINVAL` answers, using a fake `BlockDevice`.
- Unit tests in `exposure.rs` for parsing `ROOTFS_EXPOSURE` and the two start errors.
- `vm_contract.rs` runs once per transport on CI, where KVM exists. CI also runs an in-guest `fio` at queue depth 1 against both, to confirm the spike numbers inside Firecracker.

## Docs

- `website/content/docs/orchestrator/block-devices.mdx` and `.cs.mdx`: a ublk section, the `ROOTFS_EXPOSURE` setting, host setup, and a clear note that this is not in upstream Go.
- `website/content/docs/design/performance.mdx` and `.cs.mdx`: the spike numbers and the attach time.
- `website/content/docs/design/principles.mdx` and `.cs.mdx`: `exposure` hides both transports behind `expose(overlay)`.
- `deploy/README.md`: the new variable and the module.
- `docs/BACKLOG.md`: zero copy on kernel 6.15+, user recovery, and multi-queue if Firecracker moves to the Async engine.

## Risks

- `ublk_drv` on AKS needs `linux-modules-extra`, which an image upgrade can remove. The start check turns this into a clear error, not stuck sandboxes.
- A bug in our queue code hangs guest IO, the same as a bug in the NBD server today.
- io_uring is used only by the orchestrator, which already runs as root. Guests cannot reach it.
