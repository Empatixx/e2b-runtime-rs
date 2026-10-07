# Orchestrator M7: Pause with Memory and Rootfs Diffs

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Pause a running sandbox (cold-booted or resumed) into a new build. The build stores only what changed:
- **Memory:** the pages the guest wrote since the last resume.
- **Rootfs:** the blocks written through the NBD overlay.

Both headers are merged over the parent build's headers. A sandbox can then go through pause → resume → pause → resume and keep its files, its tmpfs and its running processes.

**Architecture:**
- **Dirty memory, upstream WP_ASYNC style.** The memory server installs read faults with `UFFDIO_COPY_MODE_WP`. The guest's first write to such a page clears the write-protect bit in the page table, and no event is sent. At pause, the E2B fork's `GET /memory/dirty` reads those bits from the pagemap:
  - pages installed by a write fault are reported as written;
  - pages faulted in but never written stay out of the diff and inherit the parent mapping.
- **Fallback.** If Firecracker did not register the memory for write-protect (`UFFDIO_COPY` with WP returns `EINVAL`), the server falls back to plain copies and marks itself "not write-protecting". Pause then uses `GET /memory` resident − empty. That is bigger but still correct; a pagemap read would miss writes there.
- **Empty pages.** Pages the guest gave back (`REMOVE`) and never wrote again become nil mappings: empty = removed − dirty.
- **Snapshot.** `Sandbox::snapshot` picks the memory source:
  - a resumed sandbox uses the dirty bitmap, with its memfile header as the parent;
  - a cold-booted sandbox uses resident − empty over a nil parent (M6).

  Rootfs stays `Overlay::export_diff` over the base header.
- **Pause.** `Sandbox::pause(self, storage, build_id)` is snapshot + stop, like upstream's destroy path.

**Tech Stack:** as M6. Adds `GET /memory/dirty` from the E2B fork and `/proc/self/pagemap` in a Linux unit test.

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`, milestone 7 ("Pause: dirty bitmap, memory and rootfs export, headers, upload" — proof: "pause → resume keeps files and processes").

## Global Constraints

- Same code style and rules as M6 (`CLAUDE.md`, `docs/GOAL.md`).
- `GET /memory/dirty` → `{"bitmap":[u64…]}`. Bit *i* of word *w* is page `w*64+i`, and pages are in `/memory/mappings` order.
- Header merge as upstream:
  - dirty → new build;
  - empty → nil, and empty wins over dirty in the diff;
  - then the diff wins over the parent;
  - metadata is `parent.next_generation(build_id)`.

## Review Focus

1. **Data loss through a missed write.** If write-protect was not in force, the dirty bitmap must never be trusted. The fallback path must be taken. Pinned by `server::tests::falls_back_to_plain_copies_without_write_protect` and by the pause contract.
2. **Wrong mapping after two generations.** After pause → resume → pause, the second memfile header must resolve each page to the right build. Unchanged pages come from the first build; pages written after the first resume come from the second build. Pinned by the contract (a file in tmpfs written in each generation).
3. **Removed-then-written pages** are dirty, not empty: dirty wins over removed.
4. **Pause size.** The second memfile diff must be well below total memory, otherwise the dirty tracking is not working. Pinned by an assertion of < 50 % of memory.

---

### Task 1: `GET /memory/dirty` and write-protected read faults

**Files:** `firecracker/memory_info.rs`, `firecracker/client.rs`, `memory/uffd.rs`, `memory/server.rs`

- [ ] **Failing tests:**
  - client: `memory_dirty_is_parsed` (`{"bitmap":[3]}` → {0,1}).
  - Linux `uffd::tests::copy_with_write_protect_arms_the_pagemap_bit`:
    1. register MISSING|WP with `UFFD_FEATURE_WP_ASYNC | UFFD_FEATURE_EVENT_REMOVE` (test helper `register_write_protected`);
    2. `copy(dst, src, true)`;
    3. `/proc/self/pagemap` bit 57 (`PM_UFFD_WP`) is set;
    4. after a write to the page it is clear.
  - Linux `server::tests::falls_back_to_plain_copies_without_write_protect`: the region is registered MISSING only, a read fault is served, and `server.write_protecting()` is false.
  - Linux `server::tests::read_faults_are_write_protected`: a WP-registered region gives `write_protecting()` true.
- [ ] **Implement:**
  - `Uffd::copy(dst, src, write_protect: bool)`. Mode `UFFDIO_COPY_MODE_WP = 1 << 1` when true. `EINVAL` with WP becomes `CopyOutcome::NoWriteProtect`.
  - The server keeps `AtomicBool write_protect`, starting true. On `NoWriteProtect` it sets it false and retries plain.
  - Write faults are always copied plain.
  - `MemoryServer::{write_protecting(), removed_pages() -> RoaringBitmap}`. Removed pages are memfile page indices.
- [ ] **Commit:** `feat(orchestrator): write-protected read faults and the dirty-page API`.

### Task 2: Memory diff of a resumed sandbox

**Files:** `memory/snapshot.rs`, `sandbox/snapshot.rs`

- [ ] **Failing test:** `snapshot::tests::exports_the_given_dirty_pages_and_records_empty_ones`. The generalised `export_pages(pid, regions, &dirty, &empty, out)` copies only `dirty` and records `empty − dirty`.
- [ ] **Implement:**
  - `export_memfile(pid, regions, info)` becomes `export_pages(pid, regions, &(resident − empty), &empty)`.
  - `Sandbox::export_memory` chooses the source:
    - memory server present and `write_protecting()` → `memory_dirty()`, with empty = `removed_pages` (memfile indices mapped back to bitmap indices) − dirty;
    - otherwise → `memory_info()`.
  - The parent is `config.memfile` header when resuming. Return the diff size in bytes as `Build`-adjacent info: `Snapshot { build: Build, memfile_bytes: u64, rootfs_bytes: u64 }`. `snapshot()` returns `Snapshot`; existing callers use `.build`.
- [ ] **Commit:** `feat(orchestrator): memory diff of a resumed sandbox from the dirty bitmap`.

### Task 3: `pause` and the two-generation contract

**Files:** `sandbox.rs` (`pub async fn pause(self, storage, build_id) -> Result<Snapshot, String>`), `tests/resume_contract.rs`

- [ ] **Failing contract test** `pause_and_resume_twice_keeps_files_memory_and_processes`:
  1. Cold boot in slot 4033.
  2. Write disk file A and `/dev/shm/a` (tmpfs, memory only).
  3. Start `/bin/sleep 600` through envd `process.Process/Start` (Connect, JSON envelope, read the first message, then drop the stream).
  4. Pause → build 1. Resume in slot 4034.
  5. Write B and `/dev/shm/b`.
  6. Pause → build 2. Print `pause 2: memfile N bytes of M, rootfs K bytes`.
  7. Resume in slot 4035.
  8. Assert:
     - A, B, `/dev/shm/a` and `/dev/shm/b` all read back;
     - `process.Process/List` (unary JSON) contains a `sleep` process;
     - memfile N < M / 2.
  9. Stop.
- [ ] **Implement** `pause`, plus the Connect helpers in the test file.
- [ ] **Push.** Expected: green. Report the numbers in the ledger.
- [ ] **Commit:** `feat(orchestrator): pause a sandbox into a diff build`.

### Task 4: Docs, backlog

- [ ] Update the skill (`Snapshot`, `pause`) and the README status line.
- [ ] Backlog:
  - in-place pause (keep the sandbox running);
  - memfd copy-out;
  - deferred/background rootfs export;
  - dedup against the parent;
  - `metadata.json`.
- [ ] **Commit:** `docs: M7 layout and backlog`.
