# Backlog: deferred review findings

Every final review ends with minors and deferred rulings. They are collected here so they don't get lost.

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `template build` with the template builder, `any` whenever convenient, `later` after that.

## envd

| Item | Target |
|---|---|
| hyper rejects a request with more than 100 headers with 431; Go has no count limit, only the 1 MiB size limit. A higher hyper limit allocates that many slots on every request | later |
| A 431 from hyper has an empty body and no `Content-Type`; Go writes `431 Request Header Fields Too Large` as a `text/plain; charset=utf-8` body | later |
| `GET /files` copies through user space; Go serves a file with one `sendfile`, about 12 % faster on 2 CPUs | perf |
| 50 to 100 concurrent 1 MiB uploads peak at higher RSS than Go (the hyper read buffer grows up to 1 MiB per connection, the Go header limit, plus chunks in flight) | perf |
| Not yet compared with Go: gopsutil formulas in `/metrics`, the `txeh` hosts output, `Content-Type: ""` on 204, the `invalid signal` text, an invalid `Connect-Timeout-Ms`, filesystem RPC internals beyond spot checks | API |

## Orchestrator: storage

| Item | Target |
|---|---|
| A rate-limited GCS write is recognised by object_store's `Server returned non-2xx status code: 429` text, because its error types are private | later |

## Orchestrator: volumes over NFS

| Item | Target |
|---|---|
| The NFS proxy runs on nfsserve (vendored with a small patch), not go-nfs; the `NFS_PROXY_*` logging, tracing, metrics, handle cache limit and directory verifier settings are not read, and the handle table has no size limit | API |
| The portmapper answers 2049 for every program; Go answers 2049 only for NFS and mountd over TCP and 0 otherwise | later |

## Orchestrator: shared chunk cache

| Item | Target |
|---|---|
| `SHARED_CHUNK_CACHE_PATH` and the `use-nfs-for-snapshots`, `use-nfs-for-templates` and `use-nfs-for-building-templates` flags (a chunk cache on a shared filestore) are not ported | perf |

## Orchestrator: cold boot (M5)

| Item | Target |
|---|---|
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size. The `nowatch` udev rule from `scripts/ci/nbd-host.sh` stops the inotify probes; the kernel's own change events are still probed | perf |

## Orchestrator: resume (M6)

| Item | Target |
|---|---|
| The uffd handshake socket is `0777` (upstream parity); `0600` would do, since Firecracker runs as root | API |

## Orchestrator: pause (M7)

| Item | Target |
|---|---|
| If write-protect is unavailable the resumed pause falls back to resident−empty (correct but large); surface it as a metric | API |
| No KVM test with multiple memory regions (guests above the MMIO gap) | API |

## Orchestrator: gRPC (M8)

| Item | Target |
|---|---|
| Local expiry is opt-in (`E2B_LOCAL_EXPIRY=1`); default leaves expiry and auto-pause to the API evictor like upstream | API |
| Startup sweep removes every `ns-<N>` netns and all of `/sys/fs/cgroup/e2b/*` and `SANDBOX_CACHE_DIR`; assumes a dedicated node | API |

## Orchestrator: fast resume (M9)

| Item | Matters from |
|---|---|
| Slot links, addresses and routes use `ip` execs; upstream uses netlink in-process (only matters for pool refill speed) | perf |
| NBD connects with ioctls and one socket; upstream uses generic netlink and can do multi-conn. A netlink port was measured and reverted (a connect right after a device is released waits 100 to 250 ms in `blk_mq_freeze_queue_wait`, the ioctl connect takes about 17 ms); the fixed port is kept outside the repository on the maintainer's machine | perf |

## Orchestrator: async pause (M11)

| Item | Matters from |
|---|---|
| A process that exits during forced shutdown before starting sandboxes finish leaves their slots and cgroups to the next startup sweep | API |

## Orchestrator: template builder (M13 to M16)

| Item | Matters from |
|---|---|

## Orchestrator: upstream sync (bfd917908 to 2a78eba4b)

| Item | Target |
|---|---|
| Delta counters and histograms keep up to 20 000 series per export interval; upstream lifts the cap entirely, but the Rust SDK preallocates its maps by the limit and rejects an unbounded one | later |
| The template cache has no residency gauges (`orchestrator.templates.cache.entries`, pinned refs, oldest pin age, mapping bytes, layer entries by kind and generation band, unlanded layers) and no `evictions` counter by reason | later |
| The gRPC server pings every 15 s like upstream, but hyper has no keepalive enforcement policy, so a client that pings more often than every 5 s is not sent `too_many_pings` | later |

## Orchestrator: peer chunk sharing

| Item | Target |
|---|---|
| A peer transition re-reads the diff from storage with the header it already holds; the chunker reads only uncompressed objects, so a build a Go peer uploads compressed is unreadable after the transition (upstream reloads the header and its frame table) | later |
| `snapshot-cache-drop-provisional-header` is not read and the `orchestrator.sandbox.snapshot.dead_structure_outcome` and `dead_structure_bytes` counters are not recorded; the provisional header stays in the template until the entry goes | later |

## Orchestrator: in-place checkpoint

| Item | Target |
|---|---|
| The memory in-place checkpoint is tested only with the synchronous copy. The deferred export needs a template built with `free-page-reporting` on, and no test builds one yet | perf |
| No end-to-end test of the filesystem-only in-place checkpoint over gRPC (needs KVM and a cluster with `filesystem-only-checkpoint` on) | API |


## Orchestrator: ublk rootfs exposure

| Item | Target |
|---|---|
| Not measured inside a Firecracker guest yet (the Colima VM has no KVM); run `fio` at queue depth 1 in a guest on CI or AKS for both transports | perf |
| Each request hops from the queue thread to tokio and copies its data once more; a 4k read at queue depth 1 ties with NBD at p50 (60 us against 57 us) | perf |
| Zero copy (`UBLK_F_AUTO_BUF_REG`) needs kernel 6.15; AKS runs 6.8 | later |
| Queue depth 128 with a 512 KiB buffer per slot can touch up to 64 MiB per device under heavy writeback | perf |
| A process that serves a device and also has it open hangs on exit; the orchestrator opens the device only for the flush before a pause | later |
| No recovery of devices after an orchestrator restart (`UBLK_F_USER_RECOVERY`); dead devices are removed at start instead | later |
