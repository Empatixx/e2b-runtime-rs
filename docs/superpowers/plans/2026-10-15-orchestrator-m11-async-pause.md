# Orchestrator M11: async pause

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pause returns once the snapshot is in the node's template cache, as upstream `snapshotAndCacheSandbox` + `AddSnapshot` do. The upload to storage runs in the background with upstream's retry policy. A resume of the paused build on the same node reads only local files.

**Architecture:**
- **`sandbox/snapshot.rs`:**
  - `Sandbox::snapshot_into(dir, build_id) -> LocalSnapshot` writes the snapfile, memfile diff and rootfs diff into `dir` and returns the `Build` headers and the stats. It no longer uploads.
  - `LocalSnapshot::upload(storage)` uploads in the V3 order: the three files and both headers in parallel.
  - `Sandbox::pause(storage, build_id)` stays as the library one-shot (snapshot into the work dir, upload, remove, stop) for tests and the example.
- **`block/chunker.rs`.** `Chunker::local(path, object)` adopts a complete local file. Every block is present, so no fetch ever happens.
- **`block/build_file.rs`:**
  - `DiffStore::add_local(build_id, artifact, path)` registers a local diff, pinned until `unpin(build_id)`.
  - Entries get a 25 h sliding TTL and disk-pressure eviction: every second, while cache-disk usage is above `BUILD_CACHE_MAX_USAGE_PERCENTAGE` (default 90), the unpinned entry with the soonest expiry is doomed and deleted 60 s later, unless it is used in the meantime.
- **`template/cache.rs`.** `TemplateCache::add_snapshot(build, snapfile, dir) -> Arc<Template>` inserts a template whose cells are already set.
- **`server/uploads.rs`:**
  - `Uploads`: `build_id → watch` of the outcome (`Pending`, `Landed`, `Failed`), plus a `TaskTracker`;
  - `start(snapshot, storage)` spawns the retried upload;
  - before uploading the headers it waits for every parent build referenced by the headers' mappings that is still pending on this node (upstream `Uploads.wait`);
  - `wait_all()` is called at shutdown.
- **Retry policy (`server/retry.rs`):**
  - 20 min per attempt;
  - 2 h in total;
  - backoff 5 s ×2, capped at 2 min;
  - a missing object stops the retries.
- **Lifecycle Pause:**
  1. `snapshot_into(template dir)`;
  2. `add_local` for both diffs;
  3. `add_snapshot`;
  4. stop the VM (fast now: the slot goes back to the pool);
  5. reply;
  6. `uploads.start(...)`.
  - On upload success the local diffs are unpinned. They stay cached and are evicted by TTL or disk pressure.
  - On failure they stay pinned and the template stays cached, marked failed. That is logged.

**Spec:** `docs/superpowers/specs/2026-10-13-orchestrator-fast-resume-design.md`, section M11.

## Global Constraints

- **Pause errors:** keep `Internal "error snapshotting sandbox '<id>': <e>"` for snapshot failures. Upload failures never reach the Pause caller.
- **Reply after teardown:** the Pause reply is still sent after the VM teardown. This is the M8 ruling: the id is free for a same-id Create when the reply arrives. The upload is what becomes asynchronous.
- **Shutdown** waits for uploads after draining (upstream `Server.Close`).
- **`metadata.json` waits for M12,** where the prefetch mapping needs it.

## Review Focus

1. **A resume of the paused build during the upload** reads local diffs only. The storage objects may still be missing.
2. **A child pause during the parent's upload** uploads its headers only after the parent landed.
3. **Eviction never deletes a pinned (unuploaded) local diff,** and never deletes a file while a reader holds its chunker.
4. **Shutdown with an upload in flight** waits for it.
5. **A failed upload** leaves the build resumable on this node until its TTL ends.

---

### Task 1: snapshot into a directory, local chunkers

- [ ] **Unit tests:**
  - `chunker.rs`: `Chunker::local` serves reads with zero fetches, even when storage is empty;
  - `build_file.rs`: a `BuildFile` whose header maps to a local diff build reads the local bytes.
- [ ] **Implement:**
  - `LocalSnapshot`, `snapshot_into` and `LocalSnapshot::upload`;
  - `pause` built from them;
  - `Chunker::local`, `DiffStore::add_local`, `unpin`.
- [ ] **Linux:** `resume_contract` keeps passing. It uses `pause`, so the path is unchanged.
- [ ] **Commit.**

### Task 2: retry policy and uploads

- [ ] **Unit tests**, with paused time:
  - `retry.rs`: delays are 5, 10, 20, 40, 80, 120, 120 s; the total budget is 2 h; a missing object stops the retries;
  - `uploads.rs`:
    - a child waits for a pending parent;
    - a failed parent fails the child with `parent upload <id> failed`;
    - `wait_all` returns after every upload ends.
- [ ] **Implement:** `retry.rs`, `uploads.rs`.
- [ ] **Commit.**

### Task 3: async Pause in the node

- [ ] **Implement:**
  - `add_snapshot`;
  - lifecycle Pause as above;
  - `Node` owns `Uploads`;
  - shutdown calls `uploads.wait_all()`.
- [ ] **Contract `grpc_contract`:**
  - Pause, then immediately Create of the paused build: envd answers and the note is there;
  - `uploads` reports the build pending or landed;
  - after `wait_all` the storage has the five objects.
  - Measure Pause time and the same-node resume time, and print both.
- [ ] **Commit.**

### Task 4: DiffStore TTL and disk pressure

- [ ] **Unit tests**, with paused time and a fake usage function:
  - TTL eviction removes the cache file;
  - a pinned local diff survives;
  - disk pressure dooms the soonest-expiring entry and deletes it 60 s later;
  - a lookup within 60 s rescues it.
- [ ] **Implement:** this task, plus the eviction tick started by `Node::start_background`.
- [ ] **Commit.**

### Task 5: docs, review, CI

- [ ] README, skill, backlog, the night-report numbers.
- [ ] Opus review of the M11 range, then fix the findings.
- [ ] CI green.
