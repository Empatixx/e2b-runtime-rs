# Orchestrator M6: Snapshot and Resume over userfaultfd

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Take a full snapshot of a cold-booted sandbox and store it as a build: snapfile, memfile and rootfs diff, each with a V3 header. Then resume a new sandbox from that build. Guest memory is served lazily over userfaultfd from 4 MiB chunks, and the rootfs is an NBD overlay over the diff. envd must answer, keep what the guest wrote before the snapshot, and pass the official SDK tests.

**Architecture:**
- **Stable paths inside Firecracker.** As upstream does, Firecracker runs under `unshare -m` with a private tmpfs at `/fc-vm`, holding:
  - `/fc-vm/rootfs.ext4`, a symlink to this sandbox's `/dev/nbdN`;
  - `/fc-vm/kernel/vmlinux`.

  The snapfile records only these fixed paths, so a resume on any nbd device works. The script ends in `exec`, which keeps the pid equal to the Firecracker pid for the cgroup and kill.
- **`firecracker/client.rs`** gains:
  - `pause`, `resume`, `create_snapshot` and `load_snapshot`;
  - `memory_mappings` and `memory_info` (the E2B fork's `GET /memory/mappings` and `GET /memory`).
- **`memory.rs` + `memory/`** hold the userfaultfd side. `unsafe` and ioctls stay hidden inside small types:
  - `region.rs`: the handshake regions (`base_host_virt_addr`, `size`, `offset`, `page_size_kib`, which is in bytes) and address ↔ memfile offset.
  - `uffd.rs`: an owned uffd fd with `read_events`, `copy`, `zero` and `wake`.
  - `handshake.rs`: a unix listener. It accepts Firecracker's connection within 10 s and receives the JSON regions plus the uffd through `SCM_RIGHTS`.
  - `server.rs`: the page-fault loop over tokio `AsyncFd`:
    - a fault reads one page from the memfile `BuildFile` and installs it with `UFFDIO_COPY`;
    - `REMOVE` marks pages, which later fault as zero;
    - `EEXIST` → wake; `EAGAIN` → retry; `ESRCH` → ignore;
    - any other error stops the loop with an error;
    - a stop signal ends the loop.
  - `snapshot.rs`: memfile export. Dirty = resident − empty from `GET /memory`. Those pages are copied out of the Firecracker process with `process_vm_readv` into a compact diff file. The header comes from `DiffMetadata::to_diff_header` over an all-nil parent.
- **`sandbox.rs`:**
  - `Sandbox::snapshot(&self, storage, build_id) -> Result<Build, String>`: pause, `PUT /snapshot/create`, memfile export, rootfs `export_diff`, then upload all artifacts and headers.
  - `SandboxBuilder::resume(build)`: in parallel, the uffd listener, rootfs overlay exposure and slot. Then cgroup, Firecracker, `PUT /snapshot/load`, the handshake, the page-fault server, `PATCH /vm Resumed`, `PUT /mmds` and envd `/init`.
  - `Resources` gains the memory server, stopped after Firecracker is killed.
- **Backlog items that start to matter now** are folded in:
  - the tap MAC;
  - bounded envd attempts and a race against Firecracker exiting;
  - MMDS before `InstanceStart` on cold boot;
  - shared chunk fetch results;
  - block I/O off the tokio workers;
  - range presence instead of O(blocks).

**Tech Stack:** Rust 1.98, tokio (`AsyncFd`, net), nix (`ioctl`, `socket` recvmsg/SCM_RIGHTS, `uio` process_vm_readv, `sys::mman` in tests), libc `uffd_msg` / `uffdio_*` structs, roaring, the E2B Firecracker fork v1.14 (`/memory`, `/memory/mappings`), GitHub Actions ubuntu-24.04 with `/dev/kvm` as root.

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`:
- milestone 6 ("Resume: uffd handshake and serve loop, snapshot load, prefetch" — proof: "sandbox from snapshot, start latency measured");
- "How a sandbox runs" → Create / Resume.

## Global Constraints

- Code style follows `CLAUDE.md`:
  - readability first, small well-named methods, method chains, builders;
  - named fields, no `.0`, no comments, no `#[allow]`;
  - `unsafe` and raw syscalls live only inside the small types in `memory/uffd.rs`, `memory/handshake.rs` and `memory/snapshot.rs`.
- Upstream parity where Firecracker or envd sees it:
  - `PUT /snapshot/create` `{"snapshot_type":"Full","snapshot_path":…}` with no `mem_file_path`.
  - `PUT /snapshot/load`:

    ```json
    {"snapshot_path":…,"enable_diff_snapshots":false,"resume_vm":false,
     "mem_backend":{"backend_type":"Uffd","backend_path":…}}
    ```

  - `PATCH /vm` `{"state":"Paused"}` / `{"state":"Resumed"}`.
  - Handshake: regions JSON ≤ 1024 bytes, one cmsg, `fds[0]` = uffd, `fds[1]` (memfd) ignored. Every region has the same page size, either 4096 or 2097152.
  - Artifact names are `{build}/memfile`, `{build}/memfile.header`, `{build}/snapfile`, `{build}/rootfs.ext4` and `{build}/rootfs.ext4.header`.
  - Tap MAC `02:FC:00:00:00:06`; guest MAC stays `02:FC:00:00:00:05`.
- Page size is 4 KiB in this milestone. The server is written for the region page size, so 2 MiB is supported. Templates with huge pages are a backlog item, because CI does not reserve hugetlb pages.
- Prefetch needs the build-time page trace from the template build (later spec). M6 has none: no prefetch beyond the chunker's 4 MiB read-ahead. Recorded in the backlog.
- Never `cargo clean`, never touch `target/` by hand, never apply anything to a kube context. Linux-only tests run in CI; push to `main` and watch the run.
- Before each commit run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`.

## Review Focus

1. **A resumed guest must never see a wrong page.**
   - The offset is `addr - region.base + region.offset`, with `addr` rounded down to the page.
   - A page outside every region stops the server with an error; it is never served with zeros.
   - Pinned by `region::tests` and by `server` tests that check the bytes in the faulting region.
2. **Removed pages come back as zeros, not stale memfile data.** After a balloon or `madvise(DONTNEED)` remove, a refault gets a zero page. Pinned by `server::tests::removed_pages_fault_back_as_zero`.
3. **Concurrent faults on the same chunk fetch it once and all succeed.** If the fetch fails, every waiter gets the error at once; nobody retries serially. Pinned by `chunker::tests::waiters_share_one_failed_fetch`.
4. **Firecracker or the server dying does not hang the caller.**
   - Resume races envd init against Firecracker exiting.
   - A server error is reported by `stop()`.
   - Teardown kills Firecracker before stopping the server, so no fault is left blocked forever.
5. **The snapshot is consistent and complete.**
   - The memfile header maps exactly the resident non-empty pages to the build, and everything else to nil.
   - The rootfs diff header is merged over the template header.
   - A file written by the guest before the snapshot reads back after resume. Pinned by `resume_contract::resumed_sandbox_keeps_files_and_answers_quickly`.

---

### Task 1: Firecracker in a private mount namespace, stable rootfs path, tap MAC

**Files:**
- Create: `crates/orchestrator/src/firecracker/launch.rs`
- Modify: `crates/orchestrator/src/firecracker.rs`, `crates/orchestrator/src/firecracker/process.rs`, `crates/orchestrator/src/sandbox.rs`, `crates/orchestrator/src/network/slot.rs`, `crates/orchestrator/tests/vm_contract.rs`

**Interfaces:**
- Produces:
  - `firecracker::launch::Launch { binary: PathBuf, socket: PathBuf, namespace: String, console: PathBuf, rootfs: PathBuf, kernel: PathBuf }` with `Launch::builder()`.
  - `Launch::script(&self) -> String`.
  - `launch::GUEST_ROOTFS: &str = "/fc-vm/rootfs.ext4"` and `launch::GUEST_KERNEL: &str = "/fc-vm/kernel/vmlinux"`.
  - `FirecrackerProcess::spawn(&Launch) -> Result<Self, String>`.
- `VmConfig.rootfs` and `VmConfig.kernel` passed by `Sandbox` become `GUEST_ROOTFS` and `GUEST_KERNEL`.

- [ ] **Step 1: Failing unit test** `launch::tests::script_mounts_a_private_tmpfs_and_execs_firecracker_in_the_netns`. For a fixed `Launch`, `script()` must equal:

  ```
  mount --make-rprivate / && mount -t tmpfs tmpfs /fc-vm -o X-mount.mkdir && ln -s '/dev/nbd3' '/fc-vm/rootfs.ext4' && mkdir -p '/fc-vm/kernel' && ln -s '/opt/e2b/vmlinux' '/fc-vm/kernel/vmlinux' && exec ip netns exec 'ns-7' '/opt/e2b/firecracker' --api-sock '/run/sbx/fc.sock'
  ```

  Each path goes through a `shell_quote` helper that wraps in single quotes and escapes `'` as `'\''`. Another test, `shell_quote_escapes_single_quotes`, pins that.
- [ ] **Step 2: Run** `cargo test -p orchestrator launch`.
  - Expected: fails, because the module is missing.
- [ ] **Step 3: Implement.**
  - `process.rs` spawns `unshare -m -- sh -c <script>`.
  - The tap MAC in `slot.rs` is set by an `ip link set tap0 address 02:FC:00:00:00:06` step inside `create_tap`, with `TAP_MAC` a pub const.
  - `Sandbox::spawn_firecracker` builds the `Launch` from the disk path and `config.kernel`. `boot` passes `GUEST_ROOTFS` / `GUEST_KERNEL` to `VmConfig`.
  - `vm_contract` switches to `Launch`.
  - The `network_contract` slot test asserts the tap MAC: the output of `ip netns exec ns-4001 cat /sys/class/net/tap0/address` equals `02:fc:00:00:00:06`.
- [ ] **Step 4:** Run local tests (green). Push and expect CI green, with every VM test still booting.
- [ ] **Step 5: Commit** `feat(orchestrator): Firecracker in a private mount namespace with stable guest paths, tap MAC`.

### Task 2: Firecracker client — pause, resume, snapshots, memory info

**Files:**
- Modify: `crates/orchestrator/src/firecracker/client.rs`
- Create: `crates/orchestrator/src/firecracker/memory_info.rs`

**Interfaces:**
- Produces:
  - `pause()`, `resume()`, `create_snapshot(&Path)` and `load_snapshot(&Path, &Path /*uffd socket*/)`.
  - `memory_mappings() -> Result<Vec<GuestRegion>, String>`.
  - `memory_info() -> Result<MemoryInfo, String>`.
  - `GuestRegion { base_host_virt_addr: u64, size: u64, offset: u64, page_size: u64 }` and `MemoryInfo { resident: RoaringBitmap, empty: RoaringBitmap }` (from the u64 word arrays, bit `i` of word `w` is page `w*64+i`). Both are serde-deserialised. `serde` with `derive` becomes a dependency.
  - Internal: a `request(method, path, Option<Value>) -> Result<Bytes, String>` shared by `put`, `patch` and `get`, with the same timeout and error texts. The texts read `PATCH /vm: 400 …`, `GET /memory: …`.

- [ ] **Step 1: Failing tests** with the fake Firecracker. It now records methods and answers `GET` with a canned JSON body per path.
  - `pause_and_resume_patch_the_vm_state`.
  - `create_snapshot_sends_a_full_snapshot_without_a_memory_file`.
  - `load_snapshot_uses_the_uffd_backend`, with the body exactly as in Global Constraints.
  - `memory_mappings_are_parsed`.
  - `memory_info_turns_word_bitmaps_into_page_sets`: `{"resident":[5,1],"empty":[4]}` gives resident pages {0,2,64} and empty pages {2}.
- [ ] **Step 2: Run** `cargo test -p orchestrator firecracker`.
  - Expected: compile errors.
- [ ] **Step 3: Implement.** `memory_info.rs` holds the two structs and a `bitmap_from_words(&[u64]) -> RoaringBitmap` written as a chain.
- [ ] **Step 4: Run.**
  - Expected: all firecracker tests pass.
- [ ] **Step 5: Commit** `feat(orchestrator): Firecracker pause, resume, snapshot and memory API`.

### Task 3: Chunker under page-fault load

**Files:**
- Modify: `crates/orchestrator/src/block/chunker.rs`, `crates/orchestrator/src/block/cache.rs`, `crates/orchestrator/src/block/tracker.rs`
- Modify: `docs/BACKLOG.md` (delete the three M2 lines this fixes)

**Interfaces:**
- Unchanged public API. Internals change as follows:
  - each chunk has a `tokio::sync::OnceCell`-like shared fetch slot: `Mutex<HashMap<u64, Shared<Result<(), String>>>>`. Use `futures::future::Shared` over a boxed fetch future, removed from the map when it completes;
  - `BlockCache::fill` and `read_at` run inside `tokio::task::spawn_blocking` in the chunker. `BlockCache` becomes `Arc<BlockCache>` in `Chunker`;
  - `BlockTracker::is_present(range)` uses `RoaringBitmap::range_cardinality`.

- [ ] **Step 1: Failing tests.**
  - `waiters_share_one_failed_fetch`: storage is a fake whose `get_range` fails after 200 ms. 8 concurrent `read_at` calls on the same chunk all fail, `fetch_count` (attempts) is 1, and total time is < 1 s.
  - `a_fetch_timeout_is_reported`: a test-only `FETCH_TIMEOUT` override through `Chunker::open_with_timeout`, 100 ms, against a never-answering fake. The error contains `timed out after 100ms`.
  - `is_present_counts_a_range` in `tracker.rs`.

  The fake storage is a local `Storage` wrapped by a test-only object_store impl, `SlowStore`, inside `chunker.rs` tests, implementing `get_opts` with a delay or a failure.
- [ ] **Step 2: Run** `cargo test -p orchestrator chunker tracker`.
  - Expected: `waiters_share_one_failed_fetch` fails with 8 attempts; the others fail to compile.
- [ ] **Step 3: Implement.** Keep `ensure_chunk` → `fetch` readable:
  - `ensure_chunk` returns early when cached, otherwise awaits `self.shared_fetch(chunk)`.
  - `shared_fetch` gets or inserts a `Shared` future in the map. The future fetches and fills (fill under `spawn_blocking`) and removes itself from the map at the end.
- [ ] **Step 4: Run.**
  - Expected: green, including the existing chunker tests.
- [ ] **Step 5: Commit** `fix(orchestrator): chunk fetches are shared, cache I/O runs off the async workers`.

### Task 4: envd init — bounded attempts, race against Firecracker exit, MMDS before start

**Files:**
- Modify: `crates/orchestrator/src/envd.rs`, `crates/orchestrator/src/sandbox.rs`, `crates/orchestrator/src/firecracker/client.rs`, `crates/orchestrator/src/firecracker/process.rs`
- Modify: `docs/BACKLOG.md` (delete the three M5 lines this fixes)

**Interfaces:**
- `envd::ATTEMPT_TIMEOUT` becomes 100 ms, and each attempt is bounded by `min(ATTEMPT_TIMEOUT, deadline - now)`.
- `FirecrackerProcess::exited(&mut self) -> impl Future<Output = String>` resolves with `firecracker exited: <status>: <console tail>`.
- `Sandbox::init_envd` = `tokio::select!` of `envd::init` and `vm.exited()`.
- `FirecrackerClient::boot(&VmConfig, &MmdsMetadata)` sends `PUT /mmds` after `/mmds/config` and before `/actions`. `Sandbox::boot` drops its separate `set_mmds` call. Resume still calls `set_mmds` after `PATCH /vm Resumed`, as upstream does.

- [ ] **Step 1: Failing tests.**
  - `envd::tests::init_never_overshoots_the_deadline`: a server that accepts and never answers, with a 250 ms deadline. It returns in < 400 ms.
  - Client test `boot_sends_the_api_calls_in_order`: the path list now has `/mmds` between `/mmds/config` and `/actions`.
  - Contract test `sandbox_contract::a_guest_that_dies_is_reported_quickly`: zero rootfs, so the kernel panics and Firecracker exits after `panic=1`. Cold boot with `envd_timeout(120 s)` returns `Err` containing `firecracker exited` in < 20 s.
- [ ] **Step 2: Run** the unit tests locally (fail). Push and see the contract test fail in CI.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** locally and in CI.
  - Expected: green.
- [ ] **Step 5: Commit** `fix(orchestrator): bounded envd attempts, fail fast when the guest dies, MMDS before start`.

### Task 5: userfaultfd primitives — regions, uffd fd, handshake

**Files:**
- Create: `crates/orchestrator/src/memory.rs` (`pub mod region; #[cfg(linux)] pub mod uffd; #[cfg(linux)] pub mod handshake;`)
- Create: `crates/orchestrator/src/memory/region.rs`, `crates/orchestrator/src/memory/uffd.rs`, `crates/orchestrator/src/memory/handshake.rs`
- Modify: `crates/orchestrator/src/lib.rs` and `Cargo.toml` (nix features `socket`, `uio`, `mman`, `poll`; add `libc = "0.2"`)

**Interfaces:**
- **`region.rs`:**
  - `Region { base_host_virt_addr: u64, size: u64, offset: u64, page_size: u64 }`, deserialised from `page_size_kib`, falling back to `page_size`.
  - `Regions::parse(&[u8]) -> Result<Regions, String>`. It validates: non-empty, one shared page size, and that page size is 4096 or 2097152. The error texts are `uffd regions use different page sizes` and `unsupported uffd page size N`.
  - `Regions::page_size()`.
  - `Regions::offset_of(addr) -> Result<u64, String>`: the page-aligned memfile offset. For an address outside every region the error is `address 0x… is outside guest memory`.
  - `Regions::addresses_of(offset_range) -> Vec<Range<u64>>`: the reverse direction, used by tests and REMOVE.
- **`uffd.rs`:**
  - `Uffd { fd: OwnedFd }`, `Uffd::from_fd(OwnedFd)`.
  - `read_events(&self) -> io::Result<Vec<Event>>`, which reads until `EAGAIN`.
  - `Event::PageFault { address: u64, write: bool }` and `Event::Remove { start: u64, end: u64 }`. Any other event is an `Err` with `unexpected uffd event N`.
  - `copy(&self, dst: u64, src: &[u8]) -> io::Result<CopyOutcome>`, where `CopyOutcome::{Copied, AlreadyPresent, Retry, Gone}` maps `EEXIST`, `EAGAIN` or a short copy, and `ESRCH`.
  - `wake(&self, dst: u64, len: u64) -> io::Result<()>`.
  - Test-only: `Uffd::create_and_register(region: &mut [u8]) -> io::Result<Uffd>` (`userfaultfd(O_NONBLOCK|O_CLOEXEC)`, `UFFDIO_API`, `UFFDIO_REGISTER` missing mode). This lets Linux tests fault on their own memory without Firecracker.
- **`handshake.rs`:**
  - `UffdListener::bind(path) -> io::Result<UffdListener>` (`chmod 0o777`, like upstream).
  - `accept(self, timeout: Duration) -> Result<(Uffd, Regions), String>`: one `recvmsg` with a 1024-byte payload and a cmsg space for 2 fds. The errors are `uffd handshake: no connection within 10s`, `uffd handshake: no file descriptor` and `uffd handshake: <json error>`.

- [ ] **Step 1: Failing tests.**
  - `region::tests` (run everywhere):
    - parsing the `page_size_kib` form and the `page_size` form;
    - mixed page sizes rejected;
    - 8192 rejected;
    - `offset_of` maps the first page, a mid-page address (rounded down) and the second region;
    - an outside address is an error;
    - `addresses_of` round-trips.
  - `uffd::tests` (Linux, root or `vm.unprivileged_userfaultfd`; skip otherwise):
    - `copy_resolves_a_fault_from_another_thread`: mmap 4 pages, register, read page 2 in a thread, `read_events` gives `PageFault` at page 2, `copy` fills `0xAB`, and the thread sees `0xAB`.
    - `copying_a_present_page_reports_already_present`.
  - `handshake::tests` (Linux):
    - `accepts_regions_and_the_uffd`: a client thread connects, sends JSON plus an `eventfd` as a stand-in fd with `sendmsg` and `SCM_RIGHTS`, and `accept` returns the parsed regions.
    - `times_out_without_a_connection`: 100 ms.
- [ ] **Step 2: Run** `cargo test -p orchestrator region` locally (fail), then push for the Linux ones (fail).
- [ ] **Step 3: Implement.**
  - The `uffd_msg` layout and `uffdio_copy` / `uffdio_range` / `uffdio_api` / `uffdio_register` come from `libc` where it has them. Otherwise use `#[repr(C)]` structs in `uffd.rs`, with the ioctl numbers from `nix::request_code_readwrite!(0xAA, nr, size)`.
  - All `unsafe` stays in private fns of `uffd.rs` and `handshake.rs`.
- [ ] **Step 4: Run** locally and in CI.
  - Expected: green.
- [ ] **Step 5: Commit** `feat(orchestrator): userfaultfd regions, uffd fd and Firecracker handshake`.

### Task 6: Page-fault server

**Files:**
- Create: `crates/orchestrator/src/memory/server.rs`

**Interfaces:**
- Consumes `Uffd`, `Regions` and `BuildFile` (the memfile).
- Produces:
  - `MemoryServer::start(uffd: Uffd, regions: Regions, memfile: Arc<BuildFile>) -> MemoryServer`. It spawns the loop on tokio and owns a `watch`/`oneshot` stop sender plus a `JoinHandle<Result<(), String>>`.
  - `MemoryServer::stop(self) -> Result<(), String>` (async).
  - `MemoryServer::faults_served(&self) -> u64`.
- The loop:
  1. `AsyncFd::readable()`.
  2. `read_events`.
  3. Handle `Remove` first by adding pages to a `removed` RoaringBitmap.
  4. Serve each `PageFault` concurrently, at most 4096 in flight, through `futures::stream::iter(..).for_each_concurrent`. Removed pages get zeros; every other page is read with `memfile.read_at(offset, page)`.
  5. Apply `copy`: `Retry` re-queues the fault for the next round, `AlreadyPresent` wakes, `Gone` is ignored, and any `Err` ends the loop with `serve page at 0x…: <e>`.
  6. Stop ends the loop with `Ok(())`.

- [ ] **Step 1: Failing Linux tests.** They use `Uffd::create_and_register` over an mmap'd region, a `Regions` built for it, and a `BuildFile` over a local memfile with known bytes per page.
  - `serves_faults_with_memfile_bytes`: 3 threads read 3 pages and see the memfile bytes. `faults_served` is 3.
  - `removed_pages_fault_back_as_zero`: serve page 1, `madvise(MADV_DONTNEED)` it so the kernel sends `REMOVE`, read it again, and it is zero.
  - `an_unmapped_source_stops_the_server_with_an_error`: the memfile is shorter than the region. The faulting read stays blocked, so the test only reads from a thread it detaches and checks `stop()`. `stop()` returns `Err` containing `serve page at 0x`.
  - `stop_ends_an_idle_server`.
- [ ] **Step 2: Push. Expected:** fail (module missing).
- [ ] **Step 3: Implement** in small methods: `run`, `next_events`, `serve_fault`, `page_bytes`, `install`.
- [ ] **Step 4: Push. Expected:** green.
- [ ] **Step 5: Commit** `feat(orchestrator): userfaultfd page-fault server backed by the memfile`.

### Task 7: Snapshot a sandbox into a build

**Files:**
- Create: `crates/orchestrator/src/memory/snapshot.rs`
- Create: `crates/orchestrator/src/sandbox/build.rs`
- Modify: `crates/orchestrator/src/sandbox.rs`

**Interfaces:**
- **`memory::snapshot::export_memfile(pid: u32, regions: &[GuestRegion], info: &MemoryInfo, out: &mut File) -> io::Result<DiffMetadata>`:**
  - pages are numbered across regions in `/memory/mappings` order;
  - dirty = resident − empty;
  - each dirty page is read with `process_vm_readv` (coalesced runs) and written contiguously;
  - `DiffMetadata { dirty, empty, block_size: page_size }`.
- **`sandbox::build::Build { build_id: Uuid, template_id: Uuid, memfile: Header, rootfs: Header }`:**
  - `Build::load(storage, build_id, template_id)` reads both headers;
  - `Build::store(&self, storage)` writes both headers.
- **`Sandbox::snapshot(&self, storage: &Storage, build_id: Uuid) -> Result<Build, String>`:**
  1. `pause`.
  2. `create_snapshot(work_dir/snapfile)`.
  3. `memory_mappings` + `memory_info` → `export_memfile` into `work_dir/memfile`. The header is `to_diff_header` over `Header::new(Metadata::template(Uuid::nil(), page_size, total_size), vec![])`, giving `memfile` with build_id.
  4. The rootfs overlay `export_diff` into `work_dir/rootfs.diff`, with the header from `to_diff_header` over the template rootfs header.
  5. `put_file` the snapfile, memfile and rootfs diff, then store the headers.
  6. Return a `Build`.

  The sandbox stays paused; the caller `stop()`s it. `Sandbox` keeps its rootfs `Arc<Overlay>` and template header to do this; `SandboxConfig` already has `rootfs`.

- [ ] **Step 1: Failing tests.**
  - Unit test `snapshot::tests::exports_resident_non_empty_pages_compactly`. This runs on Linux and uses its own pid: allocate 4 pages, fill pages 0 and 2, and leave page 1 zero but resident. Make a `GuestRegion` for it and `MemoryInfo { resident: {0,1,2}, empty: {1} }`. The out file must be page0 ‖ page2 and the metadata dirty {0,2}.
  - The contract test comes with Task 8.
- [ ] **Step 2: Push. Expected:** fail.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Push. Expected:** green.
- [ ] **Step 5: Commit** `feat(orchestrator): snapshot a sandbox into a build (snapfile, memfile, rootfs diff)`.

### Task 8: Resume a sandbox from a build

**Files:**
- Modify: `crates/orchestrator/src/sandbox.rs`, `crates/orchestrator/src/sandbox/config.rs`, `crates/orchestrator/src/sandbox/resources.rs`
- Test: `crates/orchestrator/tests/resume_contract.rs`

**Interfaces:**
- **`SandboxBuilder::resume(self, build: &Build) -> Result<Sandbox, String>`:**
  - The builder no longer requires `kernel` for resume.
  - `rootfs` is the overlay over `BuildFile(build.rootfs, Rootfs)`; the caller builds it, as for cold boot.
  - `memfile: Arc<BuildFile>` is a new builder field, required for resume.
  - `Sandbox::snapfile` comes from storage `build.snapfile()` and is downloaded to `work_dir/snapfile` with `get` + write. The snapfile is small, ~tens of KiB.
- **Steps:**
  1. In parallel: `UffdListener::bind(work_dir/uffd.sock)`, expose plus slot (existing), and the snapfile download.
  2. Cgroup and spawn Firecracker (`Launch`).
  3. `load_snapshot(snapfile, uffd.sock)` concurrently with `listener.accept(10 s)`.
  4. `MemoryServer::start` → `resources.memory`.
  5. `resume()`, then `set_mmds`.
  6. envd init, raced against Firecracker exiting (Task 4).
- **`Resources::release`** order: kill Firecracker → stop memory server → close disk → cgroup → slot.

- [ ] **Step 1: Failing contract test** `resume_contract::resumed_sandbox_keeps_files_and_answers_quickly`, which requires `E2B_ROOTFS`:
  1. Cold boot slot 4030 with the token.
  2. Upload `hello from before the snapshot` to `/home/user/note.txt` with envd `POST /files?path=/home/user/note.txt&username=user`, multipart field `file`.
  3. `snapshot(storage, build_id)`, then `stop()`.
  4. Resume in slot 4031 with the new overlay and memfile built from `Build::load`, timing it.
  5. `GET /files?path=/home/user/note.txt&username=user` with the token returns the text, and `GET /health` returns 204.
  6. `faults_served > 0`.
  7. Print `resume took {ms} ms`, and assert < 10 s, which is generous for CI. The number is reported in the final message.
  8. `stop()`, then the slot and cgroup are gone.

  Second test: `resume_fails_cleanly_without_a_snapfile` (a build id with headers but no snapfile → `Err`, nothing left behind).
- [ ] **Step 2: Push. Expected:** fail (no `resume`).
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Push. Expected:** green. If the guest hangs after resume, look at the console tail and `faults_served`, and use superpowers:systematic-debugging.
- [ ] **Step 5: Commit** `feat(orchestrator): resume a sandbox from a snapshot over userfaultfd`.

### Task 9: SDK against a resumed sandbox, docs, backlog

**Files:**
- Modify: `crates/orchestrator/examples/cold_boot.rs`. Rename it to `sandbox.rs`. With `E2B_RESUME=1` it cold-boots, snapshots, stops and resumes, then forwards to the resumed sandbox and prints `resumed in N ms`.
- Modify: `scripts/ci/vm-sdk-tests.sh`. It runs `egress.mjs` and the SDK suite against the cold-booted sandbox, then again against a resumed one.
- Modify: `README.md`, `.claude/skills/project-structure/SKILL.md` (memory/, sandbox/build.rs, firecracker/launch.rs, memory_info.rs).
- Modify: `docs/BACKLOG.md`. Delete the fixed items (tap MAC, Overlay PAGE_SIZE note → reworded for the 2 MiB memfile, the chunker items, the M5 envd items). Add:
  - huge-page templates;
  - prefetch from a build trace;
  - memfd / `use_sync_wp` support;
  - server death detection during a running sandbox (watch the `JoinHandle`, M8);
  - the rate-limiter reset `PATCH`es after load.

- [ ] **Step 1:** Make the script and example changes.
- [ ] **Step 2: Push. Expected:** CI green, and the SDK suite passes twice (cold and resumed).
- [ ] **Step 3: Commit** `test(ci): official SDK tests against a resumed sandbox; docs and backlog for M6`.
