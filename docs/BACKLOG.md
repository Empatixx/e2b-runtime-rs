# Backlog: deferred review findings

Every final review ends with minors and deferred rulings. They are collected here so they don't get lost.

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `template build` with the template builder, `ublk` with ublk exposure, `any` whenever convenient, `later` after that.

## envd

| Item | Target |
|---|---|
| `/init` and `/files/compose` bodies are capped at axum's 2 MB default (Go has no cap) | API |

## Orchestrator: storage and header (M1)

| Item | Target |
|---|---|
| The `locate` out-of-bounds text lacks Go's colon (`size: N`) | any |
| `Header::encode` writes any version (Go refuses outside 1..=3) | API |
| `delete_prefix` deletes one object at a time (`delete_stream` would batch) | API |
| CI wait loops in `scripts/ci/kvm-spike.sh` have no timeout | any |
| The S3 test name and credentials still say "minio" | any |

## Orchestrator: block layer (M2)

| Item | Target |
|---|---|
| `export_diff` writes at the stream position, with 2 syscalls per block and no `copy_file_range` coalescing | perf |
| Exporting a partial last block would hit EOF (no overlay does this today) | API |
| `Overlay` assumes a `PAGE_SIZE` block size; a 2 MiB-page memfile never goes through an overlay today, but rootfs with other block sizes would | any |
| Writes past `size` extend the cache file; the block index wraps above 16 TiB | API |
| Missing test: unaligned read crossing into a nil mapping | API |

## Orchestrator: NBD (M3a)

| Item | Target |
|---|---|
| `ioctl` arguments go through `as i32`, which truncates above 8 TiB | API |
| A truncated request header is treated as a clean EOF | any |
| Blocking file I/O in server writes (`write_at` / `write_zeroes_at`) | perf |
| `nbd_contract` leaves mounts and devices behind when it panics | any |
| ublk exposure (M3b): needs a bare-metal node with kernel 6.12+ | M3b |

## Orchestrator: host plumbing (M4)

| Item | Target |
|---|---|
| Host `ip_forward` sysctl is set on every slot create; make it a startup check | API |
| The Firecracker console file grows without bound; send it to a logger | API |
| `vm_contract` has no cleanup guard when it panics | any |
| `MAX_SLOT` is 16383 vs upstream 32766 (deliberate; revisit for large nodes) | API |

## Orchestrator: cold boot (M5)

| Item | Target |
|---|---|
| `put_file` does not `abort()` a failed multipart upload (orphaned S3 parts); its test stays under `BufWriter`'s 10 MiB so multipart is untested | API |
| Teardown leaves `firecracker.sock` and `console.log` in the work dir | API |
| `examples/cold_boot` installs its signal handler only after boot; SIGTERM during boot tears nothing down | any |
| `/init` never sends `caBundle` (upstream sends the node CA bundle when one is configured) | API |
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size; upstream uses netlink connect | perf |
| If killing Firecracker fails, it still holds the nbd fd while the disk closes; log it | API |
| Egress check cannot probe 169.254.169.254 (MMDS answers it); Azure WireServer 168.63.129.16 is not blocked, check upstream | API |

## Orchestrator: resume (M6)

| Item | Target |
|---|---|
| Huge-page (2 MiB) templates: the server handles the region page size, but CI reserves no hugetlb pages and nothing tests it | perf |
| Prefetch from a build-time page trace (needs template build) | template build |
| Firecracker `use_sync_wp` (synchronous WP events) is not used; dirty tracking uses WP_ASYNC + `GET /memory/dirty`, so the in-place checkpoint stays off | perf |
| Faults are served one page at a time; no read-ahead of neighbouring pages into the guest | perf |
| Huge pages: `MAX_RUN_PAGES` counts pages, so a 2 MiB-page run can exceed `process_vm_readv`'s MAX_RW_COUNT; cap runs by bytes | perf |
| A failed `PUT /snapshot/load` still waits up to 10 s for the uffd handshake; cancel it when the load fails | API |
| A memory server that dies during resume shows up as an envd timeout; race init against the server task too | API |
| A shared chunk fetch whose waiters are all cancelled stays in `in_flight` unpolled; spawn the fetch and share its handle | API |
| The received uffd fd is assumed `O_NONBLOCK`; set it with `fcntl` on receipt | any |
| The uffd handshake socket is `0777` (upstream parity); `0600` would do, since Firecracker runs as root | API |
| `Regions::addresses_of` / `total_size` are only used by tests; `testing::Pages::read_byte` is a safe fn dereferencing any address | any |
| `Chunker::fetch_attempts` / `open_with_timeout` are public only for tests | any |

## Orchestrator: pause (M7)

| Item | Target |
|---|---|
| In-place pause (snapshot and keep running, `ResumeInPlace`) | perf |
| Deferred/background rootfs export (reflink, `copy_file_range`) and async memfile header | perf |
| Dedup of the memory diff against the parent memfile | later |
| Pause has no envd freeze / fsfreeze before snapshot; the test runs `sync` in the guest itself | API |
| If write-protect is unavailable the resumed pause falls back to resident−empty (correct but large); surface it as a metric | API |
| The write-protect flag starts trusted before any WP copy succeeded; use Unknown/On/Off | API |
| Pending REMOVE events and in-flight faults are not settled before the removed set is read at pause | API |
| `memfile_pages` silently drops dirty bits beyond the region page list; error instead | any |
| `Sandbox::snapshot` is public but leaves the VM paused; make it crate-private or resume after | API |
| The server's 1 ms retry sleep is rebuilt on every completion; keep one timer across iterations | perf |
| No KVM test with multiple memory regions (guests above the MMIO gap) | API |

## Orchestrator: gRPC (M8)

| Item | Target |
|---|---|
| `VolumeService`, `ChunkService` and the cmux HTTP side are not served | API |
| `RunningSandbox.config` (deprecated upstream) is not filled | API |
| Local expiry is opt-in (`E2B_LOCAL_EXPIRY=1`); default leaves expiry and auto-pause to the API evictor like upstream | API |
| The slot index returns to the pool even when `Slot::destroy` failed; keep it out or log it | API |
| A missing or invalid `end_time` becomes "now" (matters only with local expiry) | API |
| Status texts: "no free network slot" vs upstream "max number of running sandboxes on node reached (%d), please retry" | API |
| Update racing expiry can return OK for a sandbox already stopping | API |
| Startup sweep removes every `ns-<N>` netns and all of `/sys/fs/cgroup/e2b/*` and `E2B_WORK_DIR`; assumes a dedicated node | API |
| No test of a cancelled Create / Pause RPC (fixed by spawning, not pinned by a test) | API |

## Orchestrator: fast resume (M9)

| Item | Matters from |
|---|---|
| Slot links, addresses and routes use `ip` execs; upstream uses netlink in-process (only matters for pool refill speed) | perf |
| No nftables `slot-firewall` with allow/deny sets and DSCP; private ranges are dropped with iptables FORWARD rules | API |
| NBD connects with ioctls and one socket; upstream uses generic netlink and can do multi-conn | perf |

## Orchestrator: node services (M10)

| Item | Matters from |
|---|---|
| Deprecated `ServiceInfoResponse` fields 101–103 stay 0 (`#[allow(deprecated)]` is not allowed) | API |
| `metric_memory_used_bytes` is total − available (sysinfo); gopsutil reports total − free − buffers − cached | API |
| `max_sandboxes` and the starting limit are read once from env; upstream refreshes them from feature flags every 30 s | API |
| Fresh (non-resume) creates use `TryAcquire` upstream; every Create here is a resume and waits up to 15 s | template build |

## Orchestrator: async pause (M11)

| Item | Matters from |
|---|---|
| No peer-to-peer chunk routing: another node resuming a build whose upload is still running gets `FailedPrecondition` | API |
| A failed upload is only logged; upstream also counts it in a metric and keeps the template marked for the superseded-release logic | API |
| Disk-pressure eviction dooms one entry per second; upstream re-ticks immediately after an eviction | API |
| A slot is recycled even when the sandbox teardown failed; destroy it instead (M9–M11 review) | API |
| The snapshot dir leaks when caching fails, a cached template is replaced while pinned, or `snapshot_into` fails (empty dirs) | API |
| `Uploads` keeps one outcome per pause forever; drop landed ones | API |
| Host metrics sampling (`statvfs` per mount, `/proc`) runs on a runtime worker; move it to `spawn_blocking` | API |
| `return_slot` after `NetworkPool::close`, and sandboxes still `Starting` at forced shutdown, are only cleaned by the next startup sweep | API |
| `Flush` is answered at once while earlier writes may still be in flight (overlay is never fsynced anyway) | ublk |

## Orchestrator: prefetch (M12)

| Item | Matters from |
|---|---|
| `RESUME_PREFETCH_SOURCE=last-cycle` and `both` (fetch-only warming of the newest layer) are not implemented; they act as `off` and `init` | API |
| Pause harvest (record a trace with a throwaway resume after each pause) is not implemented; upstream has it off by default | API |
| Huge pages (2 MiB) and `use_sync_wp` are not used | template build |
| Stream batches are 256 KiB; upstream reads `max(block size, MinChunkerReadSizeKB)` | API |
| Kernel and Firecracker versions in `metadata.json` are copied from the parent; nothing probes the binaries | template build |
| A panicking chunk fetch task leaves its session entry; later readers get "ended without the data" (M12 review) | API |
| Prefault copies run on tokio workers one after another; use blocking workers for real parallel `UFFDIO_COPY` | API |
| Prefetch keeps trying every page after the guest memory is gone; upstream cancels on `ErrClosed` | API |
| A prefault can pass the untouched check just before a REMOVE batch is recorded and bring back a freed page (the snapshot still exports it as empty) | API |
| `metadata.json` nested `Value` objects serialize with sorted keys; Go writes struct order; `{"version":1.5}` reads as V1 where Go errors | API |
| A failed `metadata.json` write also skips caching the diffs, and uploads then retry against a missing file | API |
| `collect_prefetch` returns the mapping; writing `metadata.json`, invalidating the template and log-only failure belong to the template build | template build |
| Prefault installs zero pages that upstream skips | API |

## Orchestrator: template builder (M13–M16)

| Item | Matters from |
|---|---|
| GCS takes credentials from object_store's env (`GOOGLE_SERVICE_ACCOUNT*`, `GOOGLE_APPLICATION_CREDENTIALS`); `GOOGLE_SERVICE_ACCOUNT_BASE64` signs uploads and also authenticates the client, upstream uses ADC for the client | template build |
| Azure connection strings are split into `AccountName`/`AccountKey`/`BlobEndpoint`; SAS signing is object_store's, not upstream's user-delegation fallback with its error texts | template build |
| `delete_prefix` with an empty prefix lists the whole bucket; upstream S3/Azure refuse with "refusing to delete objects with an empty prefix" | template build |
| Base hash `cmdline:` and `cpu-template:` keys are not added; upstream takes them from feature flags that default to empty | template build |
| The Docker Hub `AZURE_ACR` remote repository (ACR token exchange with DefaultAzureCredential) is not ported; startup fails with a "not ported" error | template build |
| ECR tokens use static env credentials (`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`/`AWS_SESSION_TOKEN`, `AWS_REGION`); upstream's default AWS config also reads profiles and instance roles | template build |
| Our envd is a glibc binary, so it only runs in images with a new enough glibc (CI builds from `debian:trixie-slim`); upstream's envd is static. Build envd for `x86_64-unknown-linux-musl` for template builds | template build |
| Kernel and Firecracker binaries come from `E2B_KERNEL`/`E2B_FIRECRACKER`, not from `HOST_KERNELS_DIR/<version>` and `FIRECRACKER_VERSIONS_DIR/<version>` as upstream; the recorded versions are only labels | template build |
| Feature flags read through LaunchDarkly upstream are fixed at their defaults: `build-provision-version` (0), `build-envd-memory-protection` (false), `build-reserved-disk-space-mb` (256), `build-base-rootfs-size-limit-mb` (25000), `build-kernel-version`/`build-firecracker-version` (env defaults) | template build |
| A rate-limited layer index write (GCS `ResourceExhausted`) fails the build with `error saving UUID to hash mapping`; upstream logs a warning, skips the index entry and still logs `Saved:`. object_store retries 429 itself and hides the status, so the storage layer must expose a rate-limited error first | template build |
| The `ensurefreedisk` phase is not ported; upstream runs it between the steps and finalize only with `build-ensure-free-disk-space` (default false): a VM-less host-side ext4 grow over NBD to `freeDiskSizeMB` (proto field 17), see section 1 of `docs/superpowers/research/2026-10-21-m16-upstream-digest.md` | template build |
| Build sandboxes have no lifetime; upstream kills a finalize sandbox after 20 min and an optimize resume after 5 min. Our commands carry their own timeouts (configure 5 min, ready 10 min), the start command and the prefetch resumes do not | template build |
| `/init` from build sandboxes always sends `"envVars": {}`; Go's `EnvVars` is `omitempty` and sends nothing for an empty map | template build |
| Finalize and optimize flags are fixed at their defaults: `build-io-engine` (Sync), `free-page-reporting` and `free-page-hinting-config` (off, so `balloon` is `{"reporting":false,"hinting":false}`), `build-kernel-cmdline-args` (none), `build-cpu-template` (none) | template build |
| The injected files and symlink layers are gzipped with flate2, so their logged `sha256:` digests differ from Go's compress/gzip output for the same tar | template build |
