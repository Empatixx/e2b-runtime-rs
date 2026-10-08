# Orchestrator M9–M12: fast resume, node services, async pause, prefetch

## Goal

Make resume as fast as upstream E2B by porting the upstream Go business logic that makes it fast. Then add the node services that the API needs to find and schedule a node.

The port copies upstream's logic, defaults, limits, status codes and error texts. The code inside follows our conventions: small types, builders, method chains, tokio, and `unsafe` hidden in small types. A feature that upstream gates behind a flag uses upstream's default value; we add a flag only when the default is "on" and the feature needs an escape hatch.

**Baseline (M8, CI runner, 1 GiB guest):**
- resume takes 1.5–2.1 s;
- pause takes 556–705 ms;
- Pause returns only after the upload has finished.

**Upstream reference:** `~/IdeaProjects/e2b-runtime-origin-main-review/packages/orchestrator/pkg` (written `G/` below).

## How we measure

**Phase timing.** M9 starts by adding a phase timer to resume. Every phase logs its duration and `resume_contract` prints the breakdown. Phases: `template`, `slot`, `rootfs`, `memory_server`, `firecracker`, `load_snapshot`, `resume_vm`, `envd_init`.

**Reporting.** Each milestone reports resume time before and after in the night report. Timing is never a CI assertion, because runners are noisy.

**Target.** A resume on the node that paused the sandbox finishes in under 1 s on the CI runner by the end of M11. This is a goal for the report, not a test.

## M9: fast resume

### 1. Template cache (`G/sandbox/template/cache.go`, `storage_template.go`)

- **`TemplateCache`:**
  - keyed by build id;
  - TTL of 25 h that slides on each lookup;
  - an entry is `Arc<Template>`.
- **Pinning.** Upstream's pin refcount becomes the `Arc` itself: eviction skips an entry while `Arc::strong_count > 1`, and an entry that has just been unpinned gets a 1 min grace period (`unpinnedGraceTTL`).
- **`Template`:**
  - holds `snapfile`, `metadata`, memfile header and rootfs header, each a shared future (`tokio::sync::OnceCell`);
  - on a miss all four are fetched in parallel;
  - a fetch error is kept in the cell, so the failed template stays cached until it is evicted, as upstream.
- **Missing metadata.** `metadata.json` may be absent; then a V1 metadata value is used, as `G/.../storage_template.go`.
- **Missing objects.** A missing snapfile or header becomes `FailedPrecondition "sandbox files for '<id>' not found"` at Create, as today.
- **Disk layout.**
  - The snapfile is downloaded once per template instance to `{E2B_TEMPLATE_CACHE_DIR}/{build}/cache/{uuid}/snapfile`.
  - `E2B_TEMPLATE_CACHE_DIR` defaults to `{E2B_CACHE_DIR}/template`.
  - Eviction removes the instance directory.
- **Resume uses the template.** Resume no longer downloads the snapfile or checks object existence on every Create.

### 2. Network slot pool (`G/sandbox/network/pool.go`, `storage_local.go`)

- **`NetworkPool`.** A new-slot channel with capacity `NewSlotsPoolSize - 1` = 31, and a reused queue of 100.
- **`populate`:**
  - loops forever: acquire an index, create the slot, send it into the new-slot channel;
  - a creation error releases the index, logs, and continues without backoff (upstream behaviour). We add a 50 ms sleep so that a broken host does not spin a core; this deviation is recorded as a ruling.
- **`get`:**
  - first a non-blocking read of a reused slot, then a blocking read of a new slot;
  - per-request configuration is empty for us, because we have no DSCP and no custom firewall rules yet;
  - a failed configuration returns the slot with no delay.
- **`return_async`:**
  - after `ReturnDelay` = 3 s the slot is recycled into the reused queue;
  - when the queue is full, or recycling fails, the slot is destroyed and its index released;
  - return tasks are tracked so `close` can wait for them.
- **`close`:** waits for pending returns, then drains both queues and destroys every slot.
- **Index storage:**
  - a linear scan over `1..=MAX_SLOT`;
  - it skips taken indices and indices whose `/var/run/netns/ns-N` exists (leaked anchor);
  - on release, an index whose namespace still exists is remembered as leaked and handed out only when nothing else is free;
  - startup sweeps `ns-*` as today (upstream `ReclaimLeakedSlots`).
- **Teardown.** `Slot::destroy` keeps the namespace when an earlier step failed; the namespace stays as the reclaim anchor (upstream `RemoveNetwork`).
- **How slots are created.**
  - Slot creation keeps running `ip` and `iptables`, because upstream's `go-iptables` also execs `iptables`, and creation is now off the hot path.
  - Netlink for links and routes goes to the backlog.
  - Our extra firewall rules (FORWARD drops of private ranges, anti-spoof, `INPUT NEW DROP`) stay. They are the closest equivalent of upstream's nftables `slot-firewall` until that firewall is ported.
- **`SlotPool` goes away.** Its index allocator becomes the pool's index storage.

### 3. NBD device pool and dispatch (`G/sandbox/nbd/pool.go`, `dispatch.go`, `path_direct.go`)

- **`DevicePool`:**
  - a bitset sized from `/sys/module/nbd/parameters/nbds_max`;
  - holds `NBD_POOL_SIZE` = 64 ready devices;
  - its populate loop marks a device taken only if `/sys/block/nbdN/pid` is absent and `size` is 0, sleeping 50 ms on error.
- **`release`** re-checks that the device is free and retries every 500 ms while it is still in use.
- **Readiness.** The poll after connect drops from 10 ms to 100 µs, still waiting for pid and size (our M7 fix).
- **Dispatch:**
  - READ and WRITE run concurrently, each in its own task, and replies are serialised by a write lock;
  - backend errors reply with error 1 (EPERM) and the loop continues;
  - writes over 32 MB are fatal.
- **Kept on our side:**
  - We keep the ioctl connect path and `SEND_FLUSH`. Flush is how the rootfs is synced before export, while upstream drops FLUSH and fsyncs the device on close.
  - Generic-netlink connect and multi-conn go to the backlog. Upstream defaults to one connection.

### 4. Concurrent resume (`G/sandbox/sandbox.go` `ResumeSandbox`, `G/sandbox/fc/process.go` `Resume`)

**Rewrite of `start_resumed` as upstream's promise graph, with `tokio::try_join!`:**
- **memory:** template memfile, then bind the uffd socket and accept the handshake;
- **slot:** `NetworkPool::get`;
- **rootfs:** overlay, then NBD expose.

**Cleanup on failure.** Every resource is stored in `Resources` the moment it exists, so a failed branch never leaks a sibling's resource. Cleanup runs in reverse order, as upstream's cleanup lists.

**Firecracker launch:**
- The rootfs path inside the mount namespace is a symlink created by us in the work dir.
- It points to `/dev/null` until the NBD path is known, then it is re-pointed, as upstream `SymlinkForce`.
- Firecracker is spawned at the same time as the NBD connect and the uffd socket wait.
- **Cgroup join.** Firecracker joins its cgroup before exec: the child writes `0` to `cgroup.procs` in `pre_exec`. This is the Rust equivalent of `CLONE_INTO_CGROUP`. A cgroup that cannot be created is logged and skipped, as upstream.

**Firecracker API calls, in upstream's order:**
- no `PUT /logger`; Firecracker stdout and stderr go to the log file;
- `PUT /snapshot/load`;
- `PATCH /network-interfaces/eth0` and `PATCH /drives/rootfs` with empty rate limiters;
- `PATCH /vm Resumed`;
- `PUT /mmds` with `address: "http://<host_ip>/logs"`.

**envd `/init`:**
- each attempt times out after 50 ms;
- 5 ms between attempts;
- overall deadline 10 s (`ENVD_TIMEOUT`);
- a non-204 reply returns `unexpected status code: <n>`.

## M10: node services

### 1. Proto and gRPC

- **`info.proto`** is copied verbatim from upstream. It has no package, so its paths are `/InfoService/ServiceInfo` and `/InfoService/ServiceStatusOverride`.
- **`ServiceInfo`** fills the fields from upstream `service_info.go`:
  - `node_id` and `service_id`, where `service_id` is a uuid made once per process;
  - `service_version` (crate version) and `service_commit` (`E2B_COMMIT` at build time, default `unknown`);
  - status and `service_status_changed_at`;
  - `service_roles = [Orchestrator]`, `service_startup` and `labels` (`NODE_LABELS`, comma-separated);
  - `outstanding_work`, `max_sandboxes`;
  - sandbox sums: vcpu, memory and disk allocated, plus the three deprecated MiB fields;
  - host metrics from a poller that samples every 10 s: CPU percent and count, memory used and total, disks, hugepages from `/proc/meminfo`;
  - `machine_info` from `/proc/cpuinfo`.
- **`ServiceStatusOverride`** follows upstream's transition rules and returns `FailedPrecondition "cannot override node status to <Name>"`.
- **gRPC health** comes from `tonic-health` and reports SERVING.

### 2. `/health` on the same port

- One listener serves both: HTTP/1 `GET /health` goes to the health handler, and everything else is gRPC. This is upstream's cmux split, built in Rust by serving an axum router that holds the tonic routes and `/health` with HTTP/1 and HTTP/2 enabled.
- **Response:**
  - `Content-Type: application/json`;
  - body `{"status":"healthy|draining|unhealthy","version":"<commit>"}` followed by `\n`;
  - status 503 only for unhealthy;
  - Draining, Standby and ShuttingDown all read `draining` with status 200.

### 3. Admission (`G/server/sandboxes.go` Create)

- **Request timeout:** 60 s.
- **Max sandboxes:**
  - `MAX_SANDBOXES_PER_NODE`, default 200;
  - when the count is reached: `ResourceExhausted "max number of running sandboxes on node reached (<n>), please retry"`.
- **Starting semaphore:**
  - `MAX_STARTING_INSTANCES_PER_NODE`, default 3;
  - a resume waits up to 15 s, then gets `ResourceExhausted "too many sandboxes resuming on this node, please retry"`;
  - the permit is held until Create returns.
- **`outstanding_work`** counts in-flight Create calls and running lifecycles.

### 4. Graceful shutdown (`G/factories/run.go`)

1. The status becomes ShuttingDown.
2. The binary sleeps `SHUTDOWN_DRAIN_SLEEP`, default 15 s; tests set 0.
3. It drains: it waits until no sandbox is left, polling every 5 s, without killing.
4. It stops serving.
5. It waits for in-flight uploads (M11).
6. It closes the pools.

A second SIGTERM or SIGINT forces the stop: every sandbox is deleted, which is today's behaviour. This replaces the M8 ruling that SIGTERM always deletes.

## M11: async pause

**Pause steps** (`G/server/sandboxes.go` Pause, `snapshotAndCacheSandbox`; `G/sandbox/template/cache.go` `AddSnapshot`):
1. Snapshot into the template cache directory of the new build: snapfile, `metadata.json`, memfile diff and rootfs diff as local files.
2. **`AddSnapshot`.** The new build becomes a `Template` in the cache. Its snapfile and metadata point at the local files. Its diffs are registered in the `DiffStore` as local diffs, so a resume of this build on this node reads local files only.
3. Stop the VM and release the slot (asynchronously, as `stopSandboxAsync`).
4. Return the reply.
5. Upload in the background (`uploadSnapshotAsync`):
   - **Retries:** each attempt has a 20 min timeout, the whole upload has a 2 h budget, and backoff starts at 5 s, doubles each time and is capped at 2 min.
   - **Errors that stop the retries:** a missing object and a cancelled upload.
   - **Order:** V3 upload order, headers in parallel with data.
   - **Parent first:** a child build waits for its parent's upload future before its headers go up (`Uploads.wait`).
   - **Failure:** a failed upload leaves the template in the cache, marked failed, as the only copy until its TTL ends. It is logged and counted.
6. **Shutdown** waits for in-flight uploads.

**Our files today.** `metadata.json` is written and uploaded on every pause. We do not upload it today. It uses upstream's JSON shape (`template_metadata.go`).

**`DiffStore` gains** (`G/sandbox/build/cache.go`):
- a 25 h TTL;
- local diffs;
- disk-pressure eviction: each tick checks cache-disk usage against `BUILD_CACHE_MAX_USAGE_PERCENTAGE` (default 90); eviction runs from the soonest expiry, skips pinned entries, and deletes 60 s later unless the entry is touched again.

**Out of scope here:** peer-to-peer chunk transfer, Redis, provisional headers and dedup, V4 headers, superseded release.

## M12: memory prefetch, streaming chunks, memfd

### 1. Streaming chunker (`G/sandbox/block/streaming_chunk.go`)

- A fetch session streams the 4 MiB chunk into the mmap cache in batches of the block size.
- After each batch it advances `bytes_ready`.
- A reader waits only until its own block has arrived.
- The whole chunk is marked cached before the session ends.
- Each session has a 60 s timeout and is detached from the caller's cancellation.

### 2. Page and prefetch trackers (`G/sandbox/block/prefetch_tracker.go`, `uffd/userfaultfd`)

- The memory server records the first touch of each block:
  - index;
  - order, starting at 1;
  - access type: `r` for a read fault, `w` for a write fault, `p` for a prefetch.
- It records only after the outcome is `installed` or `already present`.
- Collecting the trace stops tracking.

### 3. Prefault and prefetcher (`G/sandbox/uffd/prefetch/prefetcher.go`, `prefault.go`)

- **Source.** The source is `RESUME_PREFETCH_SOURCE` with values `off`, `init`, `last-cycle` and `both`; the default is `init`.
- **`init`:**
  - the trace in `metadata.json` (`prefetch.memory.indices` and `access_types`, `block_size`) is fetched by 16 workers and prefaulted by 8 copy workers with `UFFDIO_COPY_MODE_WP`;
  - a page that is already present is skipped, and `EEXIST` counts as present;
  - prefetching starts before the slot, rootfs and Firecracker, and copying waits for the uffd handshake.
- **`last-cycle`** fetches the blocks of the build's own layer into the chunk cache without prefaulting.
- **Failures.** A prefetch failure never fails the resume.

### 4. Where the trace comes from

- **Optimize** (`G/template/build/phases/optimize`):
  - resumes a build twice, each run with a 5 min timeout;
  - collects the trace after envd `/init` and intersects the two runs;
  - writes `metadata.json`.
- Optimize is a library function today. The template-build spec will call it. CI runs it in `resume_contract` before the timed resume.
- **The pause harvest** (`G/server/prefetch_harvest.go`) is off by default upstream and goes to the backlog.

### 5. memfd (`G/sandbox/fc/memory.go`, `uffd.go` handshake)

- **Handshake.** The handshake accepts a second fd, the memfd, and maps it read-only.
- **Snapshot load** sends `use_memfd: true` when `USE_MEMFD` is on. The default is on, as upstream; it requires Firecracker ≥ 1.14, which we pin.
- **Pause** copies dirty pages from the memfd mapping instead of `process_vm_readv`. Without a memfd it falls back to today's path.
- **Pending check.** CI must show that our Firecracker binary actually sends the memfd. If it does not, M12 records a ruling and keeps the fallback.

**Out of scope here:** huge pages (no template build sets them yet), `use_sync_wp` (off upstream), in-place checkpoint, memfd dedup.

## Testing

**Unit tests** live next to the code. They cover:
- pool semantics with a fake slot factory: reused first, return delay, a full reused queue destroys, close drains;
- the template cache TTL and pin;
- the index storage leak fallback;
- NBD dispatch ordering;
- the status override rules;
- the `/health` mapping;
- admission errors;
- retry policy timing with paused tokio time;
- the trace intersection;
- the streaming waiter.

**Contract tests on CI:**
- **`resume_contract`:** prints the phase breakdown, checks the pool hands back a recycled slot, and checks prefetch installs pages before the first fault.
- **`grpc_contract`:**
  - `InfoService` and `/health` on the same port;
  - admission limits;
  - Pause returns before the upload has finished, and Create of the paused build on the same node works while the upload runs;
  - shutdown drains.
- **SDK tests** keep running against cold and resumed sandboxes.

## Order

M9 → M10 → M11 → M12. Each milestone gets its own plan in `docs/superpowers/plans/`, is executed task by task, is proven in CI, gets a fresh review, and gets a report line. When all four are done, `docs/NIGHT-REPORT.md` is rewritten with the numbers.

## Backlog after this spec

- netlink link and route setup;
- nftables slot firewall with allow and deny sets, and DSCP;
- generic-netlink NBD with multi-conn;
- pause harvest;
- huge pages;
- sync write-protect;
- peer-to-peer chunks;
- memfd dedup and V4 headers;
- `PUT /metrics` and host stats;
- egress proxy and hyperloop.
