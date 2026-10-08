# Orchestrator M10: node services

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve the parts of upstream's node API that the E2B API uses to find, schedule and drain a node:
- `InfoService`;
- `GET /health` and gRPC health on the gRPC port;
- admission limits on Create;
- the graceful shutdown order.

**Architecture:**
- **`proto/orchestrator/info.proto`** is copied verbatim and compiled in the same `compile_protos` call. It has no package, so it lands in the `_` module next to `SandboxService`.
- **`server/status.rs`.** `NodeStatus` holds the status, `changed_at` and `outstanding_work`, behind a `parking_lot::RwLock`. It has `override_to(new) -> Result<(), Status>`, which follows upstream's rules, `shutting_down()` and `health()`.
- **`server/metrics.rs`.**
  - `HostMetrics` is a poller sampling every 10 s, using `sysinfo` for CPU percent, CPU count, memory and disks.
  - Hugepages come from parsing `/proc/meminfo`.
  - `MachineInfo` comes from parsing `/proc/cpuinfo` (`cpu family`, `model`, `model name`, `flags`) plus `std::env::consts::ARCH`.
  - The parsers are pure functions with unit tests on fixture text.
- **`server/info.rs`.** `InfoServer` implements `InfoService` from `Node`, `NodeStatus` and `HostMetrics`.
- **`server/health.rs`.** The axum handler for `/health`.
- **`server/admission.rs`.** `Admission` holds `max_sandboxes` and the starting semaphore (`tokio::sync::Semaphore`). Its `admit(node) -> Result<Permit, Status>` returns upstream's error texts.
- **`main.rs`:**
  - one `TcpListener`;
  - an axum `Router` = tonic `Routes` (SandboxService, InfoService, tonic-health) plus `/health`;
  - `axum::serve`, which speaks HTTP/1 and h2c on the same port, as upstream cmux does.
- **Shutdown:**
  1. ShuttingDown;
  2. sleep `SHUTDOWN_DRAIN_SLEEP_MS` (default 15000);
  3. drain: wait until the registry is empty, polling every 5 s, unless a second signal arrives, which deletes every sandbox;
  4. stop serving;
  5. close the pools.

**Tech Stack:** axum 0.8 (already in the workspace), tonic 0.14 `Routes::into_axum_router`, tonic-health 0.14, sysinfo.

**Spec:** `docs/superpowers/specs/2026-10-13-orchestrator-fast-resume-design.md`, section M10.

## Global Constraints

- **Status override errors:** `FailedPrecondition "cannot override node status to <Name>"`, where `<Name>` is the proto enum name (`ShuttingDown`, `Standby`). Setting the same status again is OK and keeps `changed_at`.
- **`/health`:**
  - `Content-Type: application/json`;
  - body `{"status":"healthy","version":"<commit>"}` followed by `\n`;
  - `version` = `E2B_COMMIT` at build time, default `unknown`;
  - status 503 only for Unhealthy;
  - Draining, Standby and ShuttingDown read `draining` with status 200.
- **Admission, in this order:**
  1. reserve the id (AlreadyExists, as today);
  2. if count ≥ max: `ResourceExhausted "max number of running sandboxes on node reached (<n>), please retry"`;
  3. wait up to 15 s for a permit, else `ResourceExhausted "too many sandboxes resuming on this node, please retry"`.
- **Admission defaults:**
  - `MAX_SANDBOXES_PER_NODE` = 200;
  - `MAX_STARTING_INSTANCES_PER_NODE` = 3;
  - a request timeout of 60 s around the whole Create, after which the reply is `DeadlineExceeded "request timed out"`.
- **ServiceInfo:**
  - `service_roles = [Orchestrator]`;
  - `service_version` = crate version and `service_commit` = commit;
  - `service_id` = a uuid made once per process;
  - `labels` from `NODE_LABELS` (comma-separated);
  - `max_sandboxes` = the configured limit;
  - the deprecated fields are filled as upstream: 101 = vcpu sum, 102 and 103 = memory and disk sums in MiB.
- **Disk sum:** `metric_disk_allocated_bytes` is the sum of `total_disk_size_mb` × 1 MiB over running sandbox configs.

## Review Focus

1. **The shared port.** A plain gRPC client, grpcurl-style, and an HTTP/1 `curl /health` both work on the same port.
2. **Permits.** A Create that fails or is cancelled while it holds a permit releases it, because the `OwnedSemaphorePermit` drops.
3. **`outstanding_work`** returns to 0 after Create finishes, whether it succeeds or fails, and after a lifecycle ends.
4. **A second signal during drain** stops every sandbox. Nothing hangs.
5. **Override to ShuttingDown is rejected**, and so is Draining → Standby.

---

### Task 1: info.proto, NodeStatus, InfoService

- [ ] Copy `info.proto` verbatim and add it to `build.rs`.
- [ ] **Unit tests:**
  - `status.rs`:
    - the transition table;
    - the error text;
    - the same status keeps `changed_at`;
    - the health mapping;
  - `metrics.rs`: the `/proc/meminfo` and `/proc/cpuinfo` parsers on fixture text, including an empty arm64 family that falls back to `arm64` and `0`;
  - `info.rs`: sums over two fake running sandboxes.
- [ ] **Implement:** `NodeStatus`, `HostMetrics` (poller), `InfoServer`.
- [ ] **Commit.**

### Task 2: one port for gRPC and `/health`

- [ ] **Unit test:** the `health.rs` handler, through `tower::ServiceExt::oneshot`, returns the exact body and status for each state.
- [ ] **Implement:** the axum router in `main.rs`, built by `server::router(node, status, metrics) -> axum::Router` so tests can use it.
- [ ] **Contract** `grpc_contract`:
  - `ServiceInfo` over gRPC shows 1 running sandbox and its vcpu and memory;
  - `GET /health` on the same port returns 200 `healthy`;
  - `ServiceStatusOverride(Draining)` makes `/health` say `draining`.
- [ ] **Commit.**

### Task 3: admission

- [ ] **Unit tests** in `admission.rs`, with paused time:
  - the max count error;
  - the semaphore wait times out at 15 s with the exact text;
  - a permit is released on drop.
- [ ] **Implement:** wire into `Node::create`, including `outstanding_work` tracking and the 60 s request timeout.
- [ ] **Contract:** with `MAX_SANDBOXES_PER_NODE=1`, a second Create gets the ResourceExhausted text.
- [ ] **Commit.**

### Task 4: graceful shutdown

- [ ] **Unit test:** `drain` with paused time returns once the registry empties. A second signal future stops the sandboxes; the fake registry records the Delete commands.
- [ ] **Implement:** the shutdown order in `main.rs`, plus `SHUTDOWN_DRAIN_SLEEP_MS`.
- [ ] **Commit.**

### Task 5: docs, review

- [ ] README, skill, backlog.
- [ ] Opus review of the M10 range, then fix the findings.
- [ ] CI green.
