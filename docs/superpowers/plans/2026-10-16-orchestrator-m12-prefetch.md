# Orchestrator M12: memory prefetch, metadata.json, streaming chunks, memfd

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** After a resume the guest finds its working set already in memory, as upstream's init-trace prefetch provides. The baseline shows that `envd_init` takes 1.4–1.9 s, and almost all of it is page faults.

**Upstream facts (from the code, keep them exact):**
- **`metadata.json`** (`template_metadata.go`), written with `json.Marshal`, keys in this order:
  - `version` (2);
  - `template{build_id, kernel_version, firecracker_version}`;
  - `context{user?, workdir?, env_vars?}`;
  - `start?`, `from_image?`, `from_template?`;
  - `prefetch?{memory: {indices[u64], access_types["r"|"w"|"p"], block_size i64} | null}`;
  - `cmdline_args?`, `balloon?`, `cpu_template?`, `build_cpu_template?`, `filesystem_only?`, `fs_quiesced?`.
  - Deserialize: a `version` that is not a number, or is ≤ 1, drops every other field and yields `{version: 1}`.
  - A missing `metadata.json` makes a local V1 file: `{"version":1,"template":{"build_id":"","kernel_version":"","firecracker_version":""},"context":{}}`.
- **Pause** writes `SameVersionTemplate(new build id, kernel, fc version)`. It keeps the context, start, from_*, cmdline, balloon and cpu templates, and drops `prefetch`.
- **Tracker** (`prefetch_tracker.go`):
  - block size = uffd page size;
  - records the first touch of each block as `{index, order (counter from 1), access r|w|p}`;
  - demand faults are recorded after they are installed or found already present;
  - prefaults are recorded as `p`;
  - collecting the trace stops tracking.
- **Optimize** (`optimize/builder.go`, `prefetch.go`):
  - 2 resumes, each with a 5 min timeout;
  - each run collects the trace after `/init`, then closes the sandbox;
  - intersection: keep the indices of run 0 that are present in every run; the order is the integer average; the access type is run 0's, or `r` if the runs differ;
  - sort by order; `block_size` = run 0's;
  - an empty intersection writes `"memory": null`;
  - any failure only logs, and the build keeps metadata without prefetch.
- **Prefetcher** (`uffd/prefetch/prefetcher.go`, `prefault.go`):
  - source `RESUME_PREFETCH_SOURCE` = `init` by default (also `off`, `last-cycle`, `both`);
  - indices in mapping order; memfile offset = `index * block_size`; fetch through the memfile `BuildFile`;
  - `UFFDIO_COPY` with `MODE_WP` at the guest address of the region that contains the offset;
  - data length must equal the page size;
  - skip pages that are already present, removed or zero; EEXIST counts as present; EAGAIN is not retried;
  - installed pages are tracked as `p`;
  - 16 fetch workers and 8 copy workers;
  - starts before the slot, rootfs and Firecracker; copies only after the uffd handshake;
  - a failure never fails the resume.
- **`last-cycle`:** the header mappings whose build_id is the header's own build, sorted by offset. Fetch only, into the chunk cache.

**Architecture:**
- **`template/metadata.rs`.** `TemplateMetadata` and `MemoryPrefetch`, serde structs with upstream keys and `skip_serializing_if`. `from_json` applies the V1 rule. `same_version(build_id)` and `with_prefetch(mapping)`.
- **`Template`** gets a `metadata()` cell: fetch `metadata.json`, or fall back to V1.
- **Pause** writes `metadata.json` from the parent template's metadata via `same_version`. It is uploaded with the snapshot and served from the cache.
- **`memory/tracker.rs`.** `PageStates` (present, removed or zero per page) and `PrefetchTrace` (first touch per block, order and access, `stop()`). `MemoryServer` owns both, records demand faults, exposes `trace()`, and has `prefault(offset, data) -> Prefaulted`.
- **`memory/prefetch.rs`:**
  - `Prefetcher::run(server: oneshot of MemoryServer handle, memfile: Arc<BuildFile>, mapping)`;
  - a `buffer_unordered(16)` fetch stream feeds 8 copy tasks;
  - started in `start_resumed` before the rootfs and Firecracker, and handed the server once the handshake is in.
- **`template/optimize.rs`.** `optimize(build, resume: impl Fn() -> Future<Output = Result<PrefetchTrace, String>>)` intersects two runs and writes `metadata.json` (library function; the template-build spec will call it).
- **`block/chunker.rs` streaming.** A fetch session streams `get` into the cache in page-size batches and advances `ready`. A reader waits on a `watch` until its own blocks are present.
- **memfd:**
  - the handshake accepts a second fd and maps it read-only;
  - `use_memfd: true` is sent on load when `USE_MEMFD` (default on) and the handshake actually got the fd;
  - pause copies dirty pages from the memfd mapping, falling back to `process_vm_readv` without it.
  - CI must show whether our Firecracker 1.14.1 build sends the memfd. If it does not, rule and keep the fallback.

**Spec:** `docs/superpowers/specs/2026-10-13-orchestrator-fast-resume-design.md`, section M12.

## Review Focus

1. **A prefault racing a demand fault** on the same page: no double install, no lost wakeup, nothing counted twice.
2. **Pages a prefault must never touch:** a page removed by the guest (balloon or `MADV_DONTNEED`) is not resurrected by a stale trace.
3. **A trace index outside every region** is skipped, not a panic.
4. **`metadata.json`** round-trips byte-compatibly with upstream's keys; unknown fields are tolerated.
5. **A streaming reader** whose session fails falls back to the error and never reads garbage.

---

### Task 1: metadata.json

- [ ] **Unit tests:**
  - serialize the examples above;
  - the V1 rule;
  - `same_version` drops `prefetch`;
  - `with_prefetch`.
- [ ] **Implement:** the `metadata()` cell; pause writes and uploads `metadata.json`; `add_snapshot` keeps it.
- [ ] **Commit.**

### Task 2: page states and the prefetch trace in the memory server

- [ ] **Unit tests:** `tracker.rs` (first touch only, order counter, stop) and the page-state transitions.
- [ ] **Implement:** recording in `server.rs`; `trace()`; `prefault`, which skips non-absent pages and treats EEXIST as present.
- [ ] **Commit.**

### Task 3: prefetcher in resume, optimize

- [ ] **Unit tests:**
  - the intersection rules;
  - the `last-cycle` mapping from a header;
  - the prefetcher with a fake installer: order kept, skipped pages counted, an index out of regions skipped.
- [ ] **Implement:** `prefetch.rs`; start it in `start_resumed`; record `prefetched=N` in the phases; `optimize` with the 2-run intersection writing `metadata.json`.
- [ ] **Linux contract `resume_contract`:** build a template, run optimize, then resume. The phases show `prefetched > 0` and fewer demand faults during `envd_init` than without prefetch. Print both resume times.
- [ ] **Commit.**

### Task 4: streaming chunker

- [ ] **Unit test:** a slow storage that yields the first 64 KiB, then waits. A read of page 0 returns before the rest of the chunk arrives.
- [ ] **Implement:** this needs `Storage::get_stream(path, range)` (object_store `get_opts` with a range, and a bytes stream).
- [ ] **Commit.**

### Task 5: memfd

- [ ] **Unit tests:** the handshake parses one or two fds; the pause export chooses the memfd source when it is present.
- [ ] **Implement and measure:** pause before and after in CI.
- [ ] **Commit.**

### Task 6: docs, review, night report

- [ ] README, skill, backlog.
- [ ] Opus review of the M12 range, then fix the findings.
- [ ] CI green.
- [ ] Rewrite `docs/NIGHT-REPORT.md` in Czech with every number from M9 to M12.
- [ ] Delete the finished ledgers.
