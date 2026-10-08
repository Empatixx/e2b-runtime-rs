# Orchestrator M8: gRPC SandboxService

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship an `orchestrator` binary that serves upstream's `SandboxService` over gRPC (tonic), on port `GRPC_PORT` (default 5008). The RPCs are:
- **Create:** resume a sandbox from a build in storage.
- **Update:** change `end_time`.
- **List.**
- **Delete.**
- **Pause:** snapshot into a new build, then stop.

Each sandbox has its own task. The task tears the sandbox down when one of these happens:
- it receives a command;
- it reaches its `end_time`;
- Firecracker, the NBD server or the memory server dies.

Slot indices come from a pool.

**Architecture:**
- **Proto.** `proto/orchestrator/orchestrator.proto` is copied verbatim from upstream. It has no `package`, so the wire paths are `/SandboxService/<Rpc>`. A `build.rs` runs `tonic-prost-build` over it.
- **`server.rs` + `server/`:**
  - `service.rs` implements `SandboxService` for `SandboxServer`. Status codes and texts come from upstream `sandboxes.go`:
    - Create, already running: `AlreadyExists "sandbox '<id>' is already running on this node: …"`;
    - Create, build files missing: `FailedPrecondition "sandbox files for '<id>' not found"`;
    - Create, other errors: `Internal "failed to create sandbox: <e>"`;
    - Update and Pause, not found: `NotFound "sandbox not found"`;
    - Delete, not found: `NotFound "sandbox '<id>' not found"`;
    - Pause, snapshot error: `Internal "error snapshotting sandbox '<id>': <e>"`.
  - `registry.rs` holds the sandbox map (`id → SandboxHandle`), with reservation for AlreadyExists.
  - `lifecycle.rs` is the per-sandbox task. It owns the `Sandbox` and receives `Command::{Delete, Pause{reply}, Update{end_time}}` over an mpsc channel. It `select!`s over commands, the end-time sleep, and `sandbox.died()`. On exit it stops the sandbox, removes the registry entry and returns the slot index.
  - `slots.rs` is `SlotPool`: free indices in `1..=MAX_SLOT`, `acquire()` / `release(index)`.
  - `config.rs` holds `NodeConfig` from env:
    - `GRPC_PORT`;
    - `E2B_FIRECRACKER`, `E2B_KERNEL`;
    - `E2B_STORAGE_DIR` (local) or `E2B_S3_BUCKET` / `E2B_S3_ENDPOINT` / `E2B_S3_KEY` / `E2B_S3_SECRET`;
    - `E2B_WORK_DIR`, `E2B_CACHE_DIR`.
- **`Sandbox::died(&mut self) -> String`** resolves when Firecracker exits, the memory server ends, or the NBD server ends. `MemoryServer::ended()` and `Exposure::ended()` are futures driven by `watch` channels that close when their task ends.
- **Backlog items folded in:**
  - **`NbdDevice` `Drop`.** Dropping the device disconnects it. A detached thread then joins the worker and releases the claim last.
  - **`setsid`.** Firecracker is started under `setsid`.
  - **Cgroup controllers.** `+cpu +memory` are enabled in `/sys/fs/cgroup/e2b/cgroup.subtree_control`.
  - **Server-death watch.** Covered by `died()` above.
- **Expiry deviates from upstream** (ruling): upstream leaves expiry to the API evictor. Until we have an API, the lifecycle task kills a sandbox at `end_time`.

**Tech Stack:** tonic 0.14 + tonic-prost + tonic-prost-build, prost-types (Timestamp), tokio (mpsc, oneshot, watch).

**Spec:** milestone 8 ("gRPC server: SandboxService Create/Delete/Pause/List/Update, sandbox map, timeouts" — proof: "gRPC lifecycle end to end").

## Global Constraints

- Style as in `CLAUDE.md`.
- The proto file is never edited.
- Create always resumes from `build_id` (upstream has no separate template path).
- Create returns `client_id` = the node id from `NODE_ID` (default `"local"`).
- List returns:
  - `config` echoed;
  - `client_id`, `start_time`, `end_time`;
  - `sandbox_id`, `team_id`, `execution_id`;
  - `vcpu`, `ram_mb`.
- Checkpoint returns `Unimplemented "checkpoint is not supported"`.

## Review Focus

1. **Nothing leaks after Delete, Pause, expiry, or a guest that dies.** No slot, no cgroup, no NBD connection, no registry entry.
2. **Concurrent Create of the same id** returns `AlreadyExists` for the second caller, and the first create is unaffected.
3. **Pause of a sandbox that is being deleted** returns `NotFound`; nothing panics.
4. **The slot index goes back to the pool** only after the slot is destroyed.

---

### Task 1: Watchable deaths, `NbdDevice` drop, `setsid`, cgroup controllers

- [ ] **Failing tests (Linux):**
  - `nbd_contract::dropping_an_exposure_disconnects_the_device`.
  - `server::tests::ended_resolves_when_the_server_stops`.
  - `network_contract`: Firecracker runs in its own session (`/proc/<pid>/stat` field 6 == pid).
  - `network_contract`: the sandbox cgroup has `memory` and `cpu` in `cgroup.controllers`.
- [ ] **Implement:**
  - `Exposure::ended`, `MemoryServer::ended`, `Sandbox::died`;
  - `NbdDevice` `Drop`;
  - `setsid` prefix in `FirecrackerProcess::spawn`;
  - `Cgroup::create` enables controllers on the parent.
- [ ] **Commit.**

### Task 2: Proto, codegen, `SlotPool`, registry

- [ ] Copy the proto, add `build.rs` and the deps.
- [ ] **Unit tests:**
  - `slots::tests`: acquire is unique, release reuses, and exhaustion errors with `no free network slot`;
  - `registry::tests`: reserving an id twice fails.
- [ ] **Commit.**

### Task 3: Lifecycle and service

- [ ] **Contract test** `grpc_contract::sandbox_lifecycle_over_grpc`:
  1. Build a template build with the library (cold boot → pause).
  2. Start the real tonic server on `127.0.0.1:0`.
  3. Run, in order:
     - Create(build) → `client_id`;
     - List → 1 sandbox;
     - Create the same id → `AlreadyExists`;
     - Update `end_time` → List shows it;
     - Pause → new build;
     - Create(paused build) → envd answers and the file from before is there;
     - Delete → `NotFound` on a second Delete.
  4. Afterwards nothing is left: no namespaces `ns-*` from the pool range, and the cgroups are gone.
- [ ] **Contract test** `grpc_contract::expired_sandbox_is_removed`: Create with `end_time` = now + 3 s, and List is empty within 15 s.
- [ ] **Implement** the lifecycle, service and `main.rs`.
- [ ] **Commit.**

### Task 4: Binary, docs, backlog

- [ ] `main.rs` serves on `GRPC_PORT`, with health via `tonic-health`, if it is cheap.
- [ ] README: how to run.
- [ ] Skill: layout update.
- [ ] Backlog:
  - pre-created network slots (upstream pre-creates 32);
  - InfoService;
  - TemplateService;
  - cmux HTTP;
  - scheduling metadata.
- [ ] **Commit.**
