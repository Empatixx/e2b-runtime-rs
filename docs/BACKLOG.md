# Backlog: deferred review findings

Every final review ends with minors and deferred rulings. They are collected here so they don't get lost.

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `template build` with the template builder, `any` whenever convenient, `later` after that.

## Orchestrator: cold boot (M5)

| Item | Target |
|---|---|
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size; upstream uses netlink connect | perf |

## Orchestrator: resume (M6)

| Item | Target |
|---|---|
| Huge-page (2 MiB) templates: the server handles the region page size, but CI reserves no hugetlb pages and nothing tests it | perf |
| Firecracker `use_sync_wp` (synchronous WP events) is not used; dirty tracking uses WP_ASYNC + `GET /memory/dirty`, so the in-place checkpoint stays off | perf |
| Faults are served one page at a time; no read-ahead of neighbouring pages into the guest | perf |
| The uffd handshake socket is `0777` (upstream parity); `0600` would do, since Firecracker runs as root | API |

## Orchestrator: pause (M7)

| Item | Target |
|---|---|
| In-place pause (snapshot and keep running, `ResumeInPlace`) | perf |
| Deferred/background rootfs export (reflink, `copy_file_range`) and async memfile header | perf |
| Dedup of the memory diff against the parent memfile | later |
| If write-protect is unavailable the resumed pause falls back to resident−empty (correct but large); surface it as a metric | API |
| No KVM test with multiple memory regions (guests above the MMIO gap) | API |

## Orchestrator: gRPC (M8)

| Item | Target |
|---|---|
| `VolumeService`, `ChunkService` and the cmux HTTP side are not served | API |
| Local expiry is opt-in (`E2B_LOCAL_EXPIRY=1`); default leaves expiry and auto-pause to the API evictor like upstream | API |
| Startup sweep removes every `ns-<N>` netns and all of `/sys/fs/cgroup/e2b/*` and `E2B_WORK_DIR`; assumes a dedicated node | API |

## Orchestrator: fast resume (M9)

| Item | Matters from |
|---|---|
| Slot links, addresses and routes use `ip` execs; upstream uses netlink in-process (only matters for pool refill speed) | perf |
| NBD connects with ioctls and one socket; upstream uses generic netlink and can do multi-conn | perf |

## Orchestrator: async pause (M11)

| Item | Matters from |
|---|---|
| No peer-to-peer chunk routing (`ChunkService`): another node resuming a build whose upload is still running gets `FailedPrecondition` | API |
| A failed upload is only logged; upstream also counts it in a metric and keeps the template marked for the superseded-release logic | API |
| A process that exits during forced shutdown before starting sandboxes finish leaves their slots and cgroups to the next startup sweep | API |

## Orchestrator: prefetch (M12)

| Item | Matters from |
|---|---|
| Huge pages (2 MiB) and `use_sync_wp` are not used | template build |

## Orchestrator: template builder (M13 to M16)

| Item | Matters from |
|---|---|
| `build-kernel-cmdline-args`, `build-cpu-template`, `free-page-reporting` and `free-page-hinting-config` are read from LaunchDarkly but not applied to the VM (`balloon` stays `{"reporting":false,"hinting":false}`) | template build |

## Orchestrator: upstream sync (bfd917908 to 2a78eba4b)

| Item | Target |
|---|---|
| The reboot CPU template override (`reboot-cpu-template-override`, `{"template":...}` wrapper) and the CPU template apply on a filesystem boot have no Rust counterpart | API |
| Metrics keep the SDK default cardinality limit of 2000 series per interval: the Rust SDK preallocates by the limit, so it is not lifted for delta counters and histograms as upstream does | perf |
| The disk estimate of a pause has no memfile dedup copy because the Rust node does not dedup memory diffs | later |
| Team ID labels are missing on the metrics Rust does not record yet (envd init duration, rootfs and memory seal durations, snapshot diff sizes, build durations) | later |
| No per-sandbox LaunchDarkly log routing (`logs-write-config`), mutual TLS, gRPC server credentials or connection age | later |

## Orchestrator: vCPU hotplug and filesystem boot

| Item | Target |
|---|---|
| The sandbox metrics observer polls only sandboxes in the node registry; upstream also observes template build sandboxes, and logs the envd OOM kills from the same poll (here `oom_monitor.rs` does that per sandbox) | later |
| The reboot path has no pre-boot filesystem recovery (`fsRecoverPreBoot`, e2fsck journal replay and the `fs_recovery` create label) | API |
| The reboot path has no offline envd upgrade before the cold boot (`envdOfflineUpgradePreBoot`) | API |
| The `resume-build` dev tool (`defaultRunVcpu`) has no Rust counterpart | later |

## Orchestrator: peer chunk sharing

| Item | Target |
|---|---|
| Peer reads record no `orchestrator.read.*` storage metrics yet, and peer channels carry no otelgrpc client stats | perf |
| Headers go on the wire as V3 bytes; upstream sends V5 with `IncompletePendingUpload`, so a Go orchestrator cannot read a Rust peer header | later |
| A peer transition re-reads from storage without reloading the header, because Rust headers have no V4 compression | later |
| The wait for a parent that a peer still uploads polls storage on a timer; upstream also subscribes to a Redis upload-done channel | perf |

## Orchestrator: volumes

| Item | Target |
|---|---|
| `Chrooted` has only what the volume service uses; upstream also has rename, remove, symlink, readlink, chtimes, temp files and the trailing-slash rules for them, which only the NFS proxy needs | NFS proxy |
| `chmod` goes through `/proc/self/fd` (upstream tries `fchmodat2` first); a host without procfs cannot chmod | later |
| `CreateVolume` reports the full volume path in a mkdir failure; Go reports the first path component that failed | later |

## Orchestrator: sandbox proxy

| Item | Target |
|---|---|
| A connection stuck in the h2c preface is closed by a graceful shutdown; Go keeps the hijacked connection until the peer closes or the proxy is closed | later |
| The idle timeout starts after the first finished request on both HTTP/1 and h2c; Go also times a fresh h2c connection | later |
| Malformed requests get hyper's 400 and 431 replies; Go answers with its own text bodies | later |
| The upstream dialer ignores `HTTP_PROXY` and `NO_PROXY`; Go's transport reads them with `ProxyFromEnvironment` | later |
| A chunked response without a content type is never sniffed; Go sniffs it only when the body write wins a race against the header flush | later |
| Upstream TLS without skip-verify trusts the bundled web roots; Go trusts the system roots (the sandbox proxy only dials TLS with skip-verify) | later |

## Orchestrator: in-place checkpoint

| Item | Target |
|---|---|
| The memory in-place checkpoint is not ported: it needs `use_sync_wp` in the uffd serve loop, the CoW window and deferred memory export, the balloon device read and the deferred rootfs seal; every memory checkpoint reports route `sync_wp_off` and resumes fresh | perf |
| The filesystem-only checkpoint exports the rootfs synchronously from the live overlay; upstream can swap in a fresh COW cache and seal in the background (`defer-rootfs-export`) | perf |
| Pause and checkpoint skip the cgroup freeze and unfreeze, the pre-pause reclaim, the hinting drain and the snapshot admission grace (`pause-admission-grace-milliseconds`) | perf |
| The filesystem-only `Pause` RPC does not quiesce the guest yet; `GuestQuiesce` is ready for it | API |
| A snapshot has no lineage (`AbandonsPredecessor`) in the template cache | later |
| No end-to-end test of the filesystem-only in-place checkpoint over gRPC (needs KVM and a cluster with `filesystem-only-checkpoint` on) | API |

