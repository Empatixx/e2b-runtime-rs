# Backlog: deferred review findings

Every final review ends with minors and deferred rulings. They are collected here so they don't get lost.

- When an item is fixed, delete its line in the same commit.
- "Target" is the milestone where the item starts to matter. Pick items up no later than that.

## envd

| Item | Target |
|---|---|
| PTY write and PTY resize share one mutex, so a blocked write delays `Update` (resize) | M5 |
| PTY start error text lacks the `fork/exec /bin/sh: ` prefix that Go has | M5 |
| `/init` and `/files/compose` bodies are capped at axum's 2 MB default (Go has no cap) | M5 |
| CORS sends `Any`/expose `*` instead of Go's explicit expose list and MaxAge | M8 |

## Orchestrator: storage and header (M1)

| Item | Target |
|---|---|
| The `locate` out-of-bounds text lacks Go's colon (`size: N`) | any |
| `Header::encode` writes any version (Go refuses outside 1..=3) | M7 |
| `delete_prefix` deletes one object at a time (`delete_stream` would batch) | M7 |
| CI wait loops in `scripts/ci/*.sh` have no timeout | any |
| `rustfs/rustfs:latest` is not pinned in CI | any |
| The S3 test name and credentials still say "minio" | any |

## Orchestrator: block layer (M2)

| Item | Target |
|---|---|
| Waiters queued behind a failed chunk fetch retry one after another (up to N×60 s in a storage outage). Share one fetch result per chunk. | M6 |
| `BlockTracker::is_present` is O(blocks); use range cardinality | M6 |
| Blocking file I/O inside async fns (`fill` does a 4 MiB pwrite on a tokio worker) | M6 |
| `export_diff` writes at the stream position, with 2 syscalls per block and no `copy_file_range` coalescing | M7 |
| Exporting a partial last block would hit EOF (no overlay does this today) | M7 |
| `Overlay` assumes a `PAGE_SIZE` block size (memfile overlays may differ) | M6 |
| Writes past `size` extend the cache file; the block index wraps above 16 TiB | M8 |
| Missing tests: chunk fetch timeout; unaligned read crossing into a nil mapping | M6 |

## Orchestrator: NBD (M3a)

| Item | Target |
|---|---|
| `ioctl` arguments go through `as i32`, which truncates above 8 TiB | M8 |
| The server has no request length cap or range check (upstream caps writes at 32 MiB) | M8 |
| A truncated request header is treated as a clean EOF | any |
| Blocking file I/O in server writes (`write_at` / `write_zeroes_at`) | M6 |
| No `Drop` cleanup for `Exposure` / `NbdDevice` | M8 |
| Death of the server task is not signalled until `close`; the lifecycle should watch the `JoinHandle` | M8 |
| `nbd_contract` leaves mounts and devices behind when it panics | any |
| ublk exposure (M3b): needs a bare-metal node with kernel 6.12+ | M3b |

## Orchestrator: host plumbing (M4)

| Item | Target |
|---|---|
| Host `ip_forward` sysctl is set on every slot create; make it a startup check | M8 |
| `tap0` MAC is not set (upstream `02:FC:00:00:00:06`), needed so a resumed guest's ARP cache stays valid | M6 |
| Cgroup silently reuses stale directories and enables no controllers (cpu/memory limits, `memory.peak`) | M8 |
| Firecracker joins the cgroup after spawn instead of `CLONE_INTO_CGROUP` | M8 |
| The Firecracker console file grows without bound; send it to a logger | M8 |
| Firecracker does not get its own session (`setsid`) | M8 |
| `vm_contract` has no cleanup guard when it panics | any |
| `MAX_SLOT` is 16383 vs upstream 32766 (deliberate; revisit for large nodes) | M8 |

## Orchestrator: cold boot (M5)

| Item | Target |
|---|---|
| envd `/init` attempt can overshoot the deadline by up to 1 s; per-attempt timeout 1 s vs upstream ~50 ms (bound each attempt by `min(attempt, deadline - now)`, use 100–200 ms) | M6 |
| A guest that dies during `/init` is waited for the full envd timeout; race init against the Firecracker process exiting (upstream `ErrFcProcessExited`) | M6 |
| MMDS is written after `InstanceStart`; upstream writes it before start and only with a token | M6 |
| `put_file` does not `abort()` a failed multipart upload (orphaned S3 parts); its test stays under `BufWriter`'s 10 MiB so multipart is untested | M7 |
| `sandbox_contract::any_nbd_connected` looks at every nbd device on the host; compare to a snapshot or check only our index | any |
| Teardown leaves `firecracker.sock` and `console.log` in the work dir | M8 |
| `examples/cold_boot` installs its signal handler only after boot; SIGTERM during boot tears nothing down | any |
| `/init` omits `hyperloopIP`, `lifecycleID`, `volumeMounts`, `caBundle`; MMDS `address` is `""` (upstream `http://<ip>/logs`) | M8 |
| A cold boot cancelled while `expose` is mid-attach leaks that NBD device (no `Drop` for `NbdDevice`, see M3a) | M8 |
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size; upstream uses netlink connect | M8 |
| If killing Firecracker fails, it still holds the nbd fd while the disk closes; log it | M8 |
| Egress check cannot probe 169.254.169.254 (MMDS answers it); Azure WireServer 168.63.129.16 is not blocked, check upstream | M8 |

