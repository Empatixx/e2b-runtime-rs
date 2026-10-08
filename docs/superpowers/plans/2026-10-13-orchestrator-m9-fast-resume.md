# Orchestrator M9: fast resume

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Port the upstream parts that make resume fast:
- the template cache;
- the network slot pool;
- the NBD device pool and concurrent dispatch;
- the concurrent resume pipeline;
- a phase timer, so every change is measured.

**Architecture:**
- **`sandbox/phases.rs`.** `PhaseTimer` records named durations and renders `template=12ms slot=0ms …`.
- **`template.rs` + `template/`:**
  - `cache.rs` is `TemplateCache`: `build_id → Arc<Template>`, with a sliding TTL and an `Arc` pin;
  - `template.rs` is `Template`, holding `OnceCell` futures for the snapfile path, metadata, memfile header and rootfs header;
  - `Node` asks the cache instead of calling `Build::load` and `download_snapfile`.
- **`network/pool.rs`** is `NetworkPool`:
  - takes a `SlotFactory` trait with `create(index)` and `destroy(slot)`; production uses `Slot`, tests use a fake;
  - `network/storage.rs` is `SlotStorage`, the index allocator with the leaked-namespace fallback;
  - `server/slots.rs` is deleted.
- **`exposure/nbd/pool.rs`** is `DevicePool`: a bitset, a ready channel of 64, and a populate task. `NbdDevice::attach` takes its index from the pool. The readiness poll runs every 100 µs.
- **`exposure/nbd/server.rs`.** Concurrent dispatch over a `BlockDevice` trait, implemented by `Overlay`.
- **`sandbox/resume.rs`.** It runs upstream's promise graph:
  1. `try_join!(slot, memory listener, rootfs overlay)`;
  2. then `try_join!(spawn firecracker, expose NBD then relink rootfs)`;
  3. then `join!(load_snapshot, accept handshake)`;
  4. then rate limiters, resume, mmds and envd.
  - Firecracker's rootfs is `<work_dir>/rootfs.link`. It points to `/dev/null` first and to `/dev/nbdN` later.
  - The cgroup is joined in `pre_exec`.
  - There is no `PUT /logger`.

**Tech Stack:** tokio (mpsc, Notify, time pause in tests), the existing crates, and `fixedbitset` for the NBD bitset (or a `Vec<bool>`; pick the simpler one, no new dependency if a `BTreeSet` reads as well).

**Spec:** `docs/superpowers/specs/2026-10-13-orchestrator-fast-resume-design.md`, section M9.

## Global Constraints

- **Network pool:**
  - new-slot channel capacity 31 (`NewSlotsPoolSize - 1`);
  - reused queue of 100;
  - `RETURN_DELAY` of 3 s;
  - populate sleeps 50 ms after an error (ruling: upstream has no backoff).
- **Template cache:**
  - TTL of 25 h, sliding on lookup;
  - 1 min grace after the last pin is dropped;
  - snapfile at `{template_cache_dir}/{build}/cache/{uuid}/snapfile`.
- **NBD:**
  - `NBD_POOL_SIZE` defaults to 64;
  - populate sleeps 50 ms after an error;
  - release retries every 500 ms;
  - the connect poll runs every 100 µs.
- **NBD dispatch:**
  - backend error replies with error 1 (EPERM);
  - a write over 32 MB is fatal: the server ends with `nbd write of <n> bytes exceeds 33554432`.
- **envd init:** 50 ms per attempt, 5 ms delay; `Node` passes `ENVD_TIMEOUT` (default 10 s). The library default stays 60 s, because cold boot of a systemd guest needs it in CI. This is a ruling.
- **Firecracker:**
  - `PATCH /network-interfaces/<iface>` with `{"iface_id": <iface>, "tx_rate_limiter": {}}`;
  - `PATCH /drives/rootfs` with `{"drive_id": "rootfs", "rate_limiter": {}}`;
  - MMDS `address` = `http://<host_ip>/logs`.

## Review Focus

1. **Every resource is released on a failure in any branch of the concurrent resume.** That covers the slot (returned to the pool), the NBD device (released to the pool), the uffd socket, the cgroup and Firecracker.
2. **A slot recycled into the reused queue is clean.** Nothing from the old sandbox survives in conntrack or ARP. Upstream does not flush either, so this matches upstream.
3. **Pool shutdown destroys every slot and every pending return.** No `ns-*` is left after `close`.
4. **A template whose fetch failed is not cached forever.** It is cached until TTL eviction, as upstream, and a later Create gets the same error text.
5. **Concurrent NBD dispatch never interleaves reply bytes.**

---

### Task 1: Phase timer and baseline

- [ ] **Unit test** `phases::tests::renders_named_durations_in_order`:
  - `PhaseTimer::start()`;
  - `.mark("slot")` and `.mark("rootfs")`;
  - `render()` matches `^slot=\d+ms rootfs=\d+ms total=\d+ms$`.
- [ ] **Implement** `PhaseTimer`.
  - `Sandbox` keeps the timer's report: `Sandbox::resume_phases() -> &str`.
  - `start_resumed` marks `template`, `slot+rootfs`, `firecracker`, `load_snapshot`, `resume_vm`, `mmds` and `envd_init`.
  - `resume_contract` and `grpc_contract` print `resume phases: …`.
- [ ] **Commit**, push and record the baseline breakdown in the ledger.

### Task 2: Template cache

- [ ] **Unit tests** in `template/cache.rs`, on local storage with paused tokio time:
  - **one fetch:** two lookups of one build share one `Arc<Template>`, and the snapfile is downloaded once (count GETs through a storage directory listing or a counter in a test backend);
  - **parallel fetch:** headers, snapfile and metadata are fetched in parallel; metadata is optional, and a missing `metadata.json` yields the default;
  - **TTL:** after 25 h without a lookup the entry is evicted and its directory removed;
  - **sliding TTL:** a lookup at 24 h extends it;
  - **pin:** an entry pinned by an outstanding `Arc` survives eviction, and after the pin is dropped it is gone 1 min later;
  - **missing object:** a missing snapfile gives `FileNotFound` (an enum) that `Node` maps to `FailedPrecondition "sandbox files for '<id>' not found"`.
- [ ] **Implement:**
  - `TemplateCache::new(storage, dir)`;
  - `get(build_id) -> Arc<Template>`, which never fails; it starts the fetch in the background, as upstream `go tmpl.Fetch`;
  - `Template::snapfile().await -> Result<PathBuf, TemplateError>`;
  - `Template::build().await -> Result<Build, TemplateError>`;
  - `Template::metadata().await`;
  - an eviction task that ticks every second.
- [ ] **Wire it in:** `Node` uses the cache, `SandboxBuilder::resume` takes `&Template`, and `download_snapfile` and the three `exists` calls go away.
- [ ] **Config:** `E2B_TEMPLATE_CACHE_DIR`, defaulting to `{cache_dir}/template`.
- [ ] **Commit.**

### Task 3: Network slot pool

- [ ] **Unit tests** in `network/pool.rs`, with a fake factory and paused time:
  - `get` prefers a reused slot over a new one;
  - `populate` keeps 31 new slots ready;
  - `return_async` recycles a slot after 3 s, and not before;
  - with a full reused queue a returned slot is destroyed and its index freed;
  - a factory error does not stop populate;
  - `close` waits for pending returns and destroys everything.
- [ ] **Unit tests** in `network/storage.rs`:
  - indices are unique;
  - an index whose namespace still exists is skipped;
  - a leaked index is reused only when nothing else is free.
  - The namespace check takes a closure, so the tests do not depend on the host.
- [ ] **Implement:**
  - `Slot::destroy` keeps the netns when an earlier step failed;
  - `NetworkPool::get().await -> Result<Slot, String>`;
  - `return_slot(slot)`, which is async and tracked;
  - `close().await`.
- [ ] **Wire it in:**
  - `Node` owns `Arc<NetworkPool>` and starts populate in `Node::new` on Linux;
  - `lifecycle` returns the slot;
  - `Sandbox` resources no longer destroy the slot, because the pool owns it;
  - `main` calls `pool.close()` at shutdown;
  - `grpc_contract`'s leak check now runs after `node.close()`.
- [ ] **Linux contract** `network_contract::pool_recycles_a_returned_slot`: get, then return, then after 3 s `get` returns the same index, and envd networking still works through it (ping the host IP from the netns).
- [ ] **Commit.**

### Task 4: NBD device pool and concurrent dispatch

- [ ] **Unit tests** in `server.rs`, with a fake `BlockDevice` whose first read waits on a `Notify`:
  - a second read is replied to before the first;
  - a backend error replies 1 and serving continues;
  - a 33 MB write ends the server with the exact error text.
- [ ] **Unit test** `pool.rs`: `next_free` skips a device that is in use (fake `is_free` closure) and never hands one index out twice.
- [ ] **Implement:**
  - the `BlockDevice` trait with `read_at`, `write_at` and `write_zeroes_at`;
  - dispatch: one reader loop, each READ or WRITE in a `JoinSet` task, replies through `Arc<Mutex<OwnedWriteHalf>>`;
  - `DevicePool` with `get().await` and `release(index)`, retrying every 500 ms;
  - a 100 µs connect poll.
- [ ] **Linux:** `nbd_contract` keeps passing, plus a test that writes and reads 64 MiB in parallel through `/dev/nbdN` and checks the data.
- [ ] **Commit.**

### Task 5: Concurrent resume

- [ ] **Unit tests:**
  - `launch.rs`: the script links `/fc-vm/rootfs.ext4` to `<work_dir>/rootfs.link`, not to the device;
  - `client.rs`: `set_rate_limits` sends the two PATCHes with the exact bodies; `load_snapshot` is not preceded by `PUT /logger`;
  - `mmds.rs`: the address is serialised;
  - `envd.rs`: the attempt timeout is 50 ms.
- [ ] **Implement** `start_resumed` as the promise graph in Architecture:
  - each resource goes into `Resources` the moment it exists;
  - the cgroup is joined in `pre_exec` through a small safe wrapper `Cgroup::join_on_exec(&mut Command)`;
  - a failed cgroup create logs a warning and continues.
- [ ] **Linux:** the whole suite and the SDK tests pass. `resume_contract` prints the phase breakdown, which goes into the ledger.
- [ ] **Commit.**

### Task 6: Docs, backlog, measurement, review

- [ ] **README:** document the new env vars, `E2B_TEMPLATE_CACHE_DIR` and `NBD_POOL_SIZE`.
- [ ] **Skill:** add the layout of `template/`, `network/pool`, `nbd/pool` and `sandbox/phases` to the project-structure skill.
- [ ] **Backlog:** add netlink network setup, the nftables firewall, generic-netlink NBD and multi-conn.
- [ ] **Measure:** compare resume before and after, and keep the numbers for the night report.
- [ ] **Review:** opus review of the M9 range; fix the findings.
- [ ] **Commit and push**, with CI green.
