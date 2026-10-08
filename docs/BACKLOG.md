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
| CI wait loops in `scripts/ci/kvm-spike.sh` have no timeout | any |
| The S3 test name and credentials still say "minio" | any |

## Orchestrator: block layer (M2)

| Item | Target |
|---|---|
| `export_diff` writes at the stream position, with 2 syscalls per block and no `copy_file_range` coalescing | M7 |
| Exporting a partial last block would hit EOF (no overlay does this today) | M7 |
| `Overlay` assumes a `PAGE_SIZE` block size; a 2 MiB-page memfile never goes through an overlay today, but rootfs with other block sizes would | any |
| Writes past `size` extend the cache file; the block index wraps above 16 TiB | M8 |
| Missing test: unaligned read crossing into a nil mapping | M6 |

## Orchestrator: NBD (M3a)

| Item | Target |
|---|---|
| `ioctl` arguments go through `as i32`, which truncates above 8 TiB | M8 |
| The server has no request length cap or range check (upstream caps writes at 32 MiB) | M8 |
| A truncated request header is treated as a clean EOF | any |
| Blocking file I/O in server writes (`write_at` / `write_zeroes_at`) | M6 |
| `nbd_contract` leaves mounts and devices behind when it panics | any |
| ublk exposure (M3b): needs a bare-metal node with kernel 6.12+ | M3b |

## Orchestrator: host plumbing (M4)

| Item | Target |
|---|---|
| Host `ip_forward` sysctl is set on every slot create; make it a startup check | M8 |
| Firecracker joins the cgroup after spawn instead of `CLONE_INTO_CGROUP` | M8 |
| The Firecracker console file grows without bound; send it to a logger | M8 |
| `vm_contract` has no cleanup guard when it panics | any |
| `MAX_SLOT` is 16383 vs upstream 32766 (deliberate; revisit for large nodes) | M8 |

## Orchestrator: cold boot (M5)

| Item | Target |
|---|---|
| `put_file` does not `abort()` a failed multipart upload (orphaned S3 parts); its test stays under `BufWriter`'s 10 MiB so multipart is untested | M7 |
| Teardown leaves `firecracker.sock` and `console.log` in the work dir | M8 |
| `examples/cold_boot` installs its signal handler only after boot; SIGTERM during boot tears nothing down | any |
| `/init` omits `hyperloopIP`, `lifecycleID`, `volumeMounts`, `caBundle`; MMDS `address` is `""` (upstream `http://<ip>/logs`) | M8 |
| A cold boot cancelled while `expose` is mid-attach leaks that NBD device (no `Drop` for `NbdDevice`, see M3a) | M8 |
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size; upstream uses netlink connect | M8 |
| If killing Firecracker fails, it still holds the nbd fd while the disk closes; log it | M8 |
| Egress check cannot probe 169.254.169.254 (MMDS answers it); Azure WireServer 168.63.129.16 is not blocked, check upstream | M8 |

## Orchestrator: resume (M6)

| Item | Target |
|---|---|
| Huge-page (2 MiB) templates: the server handles the region page size, but CI reserves no hugetlb pages and nothing tests it | M8 |
| Prefetch from a build-time page trace (needs template build) | template build |
| Firecracker `use_memfd` (memfd copy-out instead of `process_vm_readv`) and `use_sync_wp` (synchronous WP events) are not used; dirty tracking uses WP_ASYNC + `GET /memory/dirty` | M8 |
| Rate limiter reset `PATCH`es after snapshot load (upstream sends empty limiters) | M8 |
| Faults are served one page at a time; no read-ahead of neighbouring pages into the guest | M8 |
| `resume` builds the memfile `BuildFile` in the caller; a template cache should own it | M8 |
| Huge pages: `MAX_RUN_PAGES` counts pages, so a 2 MiB-page run can exceed `process_vm_readv`'s MAX_RW_COUNT; cap runs by bytes | M8 |
| A failed `PUT /snapshot/load` still waits up to 10 s for the uffd handshake; cancel it when the load fails | M8 |
| A memory server that dies during resume shows up as an envd timeout; race init against the server task too | M8 |
| A shared chunk fetch whose waiters are all cancelled stays in `in_flight` unpolled; spawn the fetch and share its handle | M8 |
| The received uffd fd is assumed `O_NONBLOCK`; set it with `fcntl` on receipt | any |
| The uffd handshake socket is `0777` (upstream parity); `0600` would do, since Firecracker runs as root | M8 |
| `Regions::addresses_of` / `total_size` are only used by tests; `testing::Pages::read_byte` is a safe fn dereferencing any address | any |
| `Chunker::fetch_attempts` / `open_with_timeout` are public only for tests | any |

## Orchestrator: pause (M7)

| Item | Target |
|---|---|
| In-place pause (snapshot and keep running, `ResumeInPlace`) | M8 |
| Deferred/background rootfs export (reflink, `copy_file_range`) and async memfile header | M8 |
| Dedup of the memory diff against the parent memfile | later |
| `metadata.json` is not written with a build | M8 |
| Pause has no envd freeze / fsfreeze before snapshot; the test runs `sync` in the guest itself | M8 |
| If write-protect is unavailable the resumed pause falls back to resident−empty (correct but large); surface it as a metric | M8 |
| The write-protect flag starts trusted before any WP copy succeeded; use Unknown/On/Off | M8 |
| Pending REMOVE events and in-flight faults are not settled before the removed set is read at pause | M8 |
| `memfile_pages` silently drops dirty bits beyond the region page list; error instead | any |
| `Sandbox::snapshot` is public but leaves the VM paused; make it crate-private or resume after | M8 |
| The server's 1 ms retry sleep is rebuilt on every completion; keep one timer across iterations | M8 |
| No KVM test with multiple memory regions (guests above the MMIO gap) | M8 |

## Orchestrator: gRPC (M8)

| Item | Target |
|---|---|
| Network slots are created per sandbox; upstream pre-creates 32 and recycles returned ones after 3 s | next |
| `InfoService`, `TemplateService`, `VolumeService`, `ChunkService`, gRPC health and the cmux HTTP side are not served | next |
| `SchedulingMetadata` and `resolved_firecracker_version` are left empty in responses | next |
| `RunningSandbox.config` (deprecated upstream) is not filled | API |
| Pause uploads synchronously inside the RPC; upstream returns after the snapshot and uploads in the background | next |
| No max-running / max-starting admission limits (`ResourceExhausted` texts upstream) | next |
| Local expiry is opt-in (`E2B_LOCAL_EXPIRY=1`); default leaves expiry and auto-pause to the API evictor like upstream | API |
| The slot index returns to the pool even when `Slot::destroy` failed; keep it out or log it | next |
| A missing or invalid `end_time` becomes "now" (matters only with local expiry) | next |
| Status texts: "no free network slot" vs upstream "max number of running sandboxes on node reached (%d), please retry" | next |
| Update racing expiry can return OK for a sandbox already stopping | next |
| Startup sweep removes every `ns-<N>` netns and all of `/sys/fs/cgroup/e2b/*` and `E2B_WORK_DIR`; assumes a dedicated node | next |
| No test of a cancelled Create / Pause RPC (fixed by spawning, not pinned by a test) | next |
| Many backlog lines still say "M8"; re-target them now that M8 shipped | next |

