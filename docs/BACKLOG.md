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
