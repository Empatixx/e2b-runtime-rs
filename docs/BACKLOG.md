# Backlog: deferred review findings

Every final review ends with minors and deferred rulings. They are collected here so they don't get lost.

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `template build` with the template builder, `ublk` with ublk exposure, `any` whenever convenient, `later` after that.

## Orchestrator: NBD (M3a)

| Item | Target |
|---|---|
| ublk exposure (M3b): needs a bare-metal node with kernel 6.12+ | M3b |

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
| `RunningSandbox.config` (deprecated upstream) is not filled | API |
| Local expiry is opt-in (`E2B_LOCAL_EXPIRY=1`); default leaves expiry and auto-pause to the API evictor like upstream | API |
| Startup sweep removes every `ns-<N>` netns and all of `/sys/fs/cgroup/e2b/*` and `E2B_WORK_DIR`; assumes a dedicated node | API |

## Orchestrator: fast resume (M9)

| Item | Matters from |
|---|---|
| Slot links, addresses and routes use `ip` execs; upstream uses netlink in-process (only matters for pool refill speed) | perf |
| NBD connects with ioctls and one socket; upstream uses generic netlink and can do multi-conn | perf |

## Orchestrator: node services (M10)

| Item | Matters from |
|---|---|
| Deprecated `ServiceInfoResponse` fields 101–103 stay 0 (`#[allow(deprecated)]` is not allowed) | API |
| `max_sandboxes` and the starting limit are read once from env; upstream refreshes them from feature flags every 30 s | API |

## Orchestrator: async pause (M11)

| Item | Matters from |
|---|---|
| No peer-to-peer chunk routing: another node resuming a build whose upload is still running gets `FailedPrecondition` | API |
| A failed upload is only logged; upstream also counts it in a metric and keeps the template marked for the superseded-release logic | API |
| A process that exits during forced shutdown before starting sandboxes finish leaves their slots and cgroups to the next startup sweep | API |
| `Flush` is answered at once while earlier writes may still be in flight (overlay is never fsynced anyway) | ublk |

## Orchestrator: prefetch (M12)

| Item | Matters from |
|---|---|
| `RESUME_PREFETCH_SOURCE=last-cycle` and `both` (fetch-only warming of the newest layer) are not implemented; they act as `off` and `init` | API |
| Pause harvest (record a trace with a throwaway resume after each pause) is not implemented; upstream has it off by default | API |
| Huge pages (2 MiB) and `use_sync_wp` are not used | template build |

## Orchestrator: backlog sweep (2026-10-08)

| Item | Matters from |
|---|---|
| Pause bounds the memory server settle with 60 s ("timed out after 60s waiting for the memory server to settle"); Go waits on its lock bounded only by the request context | API |
| Every slot sets `net.ipv4.ip_forward=1` inside its netns (`slot.rs`); Go does not, and it is unclear what enables it there | API |

## Orchestrator: template builder (M13–M16)

| Item | Matters from |
|---|---|
| GCS takes credentials from object_store's env (`GOOGLE_SERVICE_ACCOUNT*`, `GOOGLE_APPLICATION_CREDENTIALS`); `GOOGLE_SERVICE_ACCOUNT_BASE64` signs uploads and also authenticates the client, upstream uses ADC for the client | template build |
| Azure connection strings are split into `AccountName`/`AccountKey`/`BlobEndpoint`; SAS signing is object_store's, not upstream's user-delegation fallback with its error texts | template build |
| The Docker Hub `AZURE_ACR` remote repository (ACR token exchange with DefaultAzureCredential) is not ported; startup fails with a "not ported" error | template build |
| ECR tokens use static env credentials (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`, `AWS_REGION`); upstream's default AWS config also reads profiles and instance roles | template build |
| Our envd is a glibc binary, so it only runs in images with a new enough glibc (CI builds from `debian:trixie-slim`); upstream's envd is static. Build envd for `x86_64-unknown-linux-musl` for template builds | template build |
| Feature flags read through LaunchDarkly upstream are fixed at their defaults: `build-provision-version` (0), `build-envd-memory-protection` (false), `build-reserved-disk-space-mb` (256), `build-base-rootfs-size-limit-mb` (25000), `build-kernel-version`/`build-firecracker-version` (env defaults) | template build |
| Finalize and optimize flags are fixed at their defaults: `build-io-engine` (Sync), `free-page-reporting` and `free-page-hinting-config` (off, so `balloon` is `{"reporting":false,"hinting":false}`), `build-kernel-cmdline-args` (none), `build-cpu-template` (none); `build-ensure-free-disk-space` (false) is read from env `BUILD_ENSURE_FREE_DISK_SPACE` instead | template build |
| The injected files and symlink layers are gzipped with flate2, so their logged `sha256:` digests differ from Go's compress/gzip output for the same tar | template build |
