# Plan: remaining Go parity gaps after the 2026-10-10 upstream sync

Target: the Rust orchestrator (`crates/orchestrator`) behaves exactly like upstream Go at `~/IdeaProjects/e2b-runtime` (HEAD `2a78eba4b`). `main` is at `3e24704`, green on Linux (only the two uffd write-protect tests fail on Mac Docker, a kernel limit).

Work alone, one item at a time, in the order below. Commit and push each item to `main` when its checks are green.

## Rules for every item

1. Read `CLAUDE.md` and `.claude/skills/project-structure/SKILL.md` first and follow them: no comments in source, small named functions, method chains, `bon` builders, unsafe and syscalls behind small safe types, crates.io module layout, Linux only (no macOS stubs).
2. Go first: read the Go code named in the item and every `_test.go` next to it. Port the Go test cases as Rust tests first (same cases, same expected texts, codes, metric names and attributes), then write the Rust code until they pass.
3. Parity check before committing: write a short table in the commit message body with each Go function and its Rust counterpart, and each Go test and its Rust test. Anything Go has and Rust does not gets ported, not skipped. Skip only API, DB, edge, nfsproxy, CI or lint code that never runs in the orchestrator.
4. Tests: run only what the change touches, through the locked scripts:
   - `scripts/linux-container.sh cargo test -p orchestrator --lib <module>`
   - `scripts/linux-container.sh cargo test -p orchestrator --test <name>`
   - `scripts/clippy-linux.sh` once before each commit.
   No other `docker run`, no copies of the checkout. Before pushing an item, run `scripts/linux-container.sh cargo test -p orchestrator --lib` once.
5. Never `git stash`, `git checkout -- .`, `git reset --hard` or delete files you did not create. Never `cargo clean` the host `target/`. Commit with `git add <your files>`, push straight to `main`.
6. If the Docker disk fills (`No space left on device`), delete only artifacts older than an hour in the Docker volume: `docker run --rm -v envd-rs-target:/t alpine sh -c 'find /t/debug/deps /t/debug/build /t/debug/examples -mindepth 1 -maxdepth 1 -mmin +60 -exec rm -rf {} +'`.
7. Remove each fixed line from `docs/BACKLOG.md` in the same commit. Run `cargo fmt` before every commit. End commit messages with `Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>`.

## 1. Envd upgrade on resume, end to end

Rust has the pieces but nothing calls them: `src/envd/binary_cache/*`, `src/envd/upgrade.rs`, `src/envd/upgrade/target.rs` and `src/telemetry/server/envd_upgrade.rs` are not used outside `src/envd`.

Go to port:
- `pkg/server/sandboxes.go`: `maybeUpgradeEnvd` (line ~2353) and both callers (Create at ~457, the checkpoint resume at ~1771), including the fatal versus deferred error split.
- `pkg/sandbox/envdbin/cache.go` and `resolver.go`: how the cache is built at startup in `pkg/factories/run.go`, warmed, and asked on resume.
- The `/upgrade` call to envd with its handover body and the envd version checks; the envd side is already in `crates/envd/src/upgrade*`.
- Flags and metrics: the upgrade target flags (already in `featureflags`), the upgrade outcome metrics and span attributes.

Done when: a resume with an older envd in the snapshot and a newer cached binary upgrades envd like Go, every Go test of `envdbin` and of `maybeUpgradeEnvd` has a Rust test, and `grpc_contract` still passes.

## 2. In-place checkpoint, memory half

Rust today reports route `sync_wp_off` for every memory checkpoint and resumes fresh (`src/server/node/checkpoint.rs`, `checkpoint/route.rs`, `checkpoint/in_place.rs`, `src/sandbox/snapshot/in_place.rs`).

Go to port:
- `pkg/sandbox/uffd/userfaultfd/`: `userfaultfd.go` (the `useSyncWP` serve loop), `cow_window.go`, `cow_export.go`, `deferred.go`, `prefault.go`, `counter_reporter.go`, `metrics.go`. Rust counterparts live in `src/memory/server.rs`, `src/memory/server/*` and `src/memory/uffd.rs`.
- `pkg/sandbox/sandbox.go`: `Pause` with `maintainSandbox` (resume Firecracker in place, roll back on error), `memoryExportDeferred`, `waitMemorySealed`, the deferred rootfs seal.
- `pkg/sandbox/balloon_mode.go` and the balloon device read in `pkg/sandbox/fc/client.go`.
- `pkg/server/sandboxes.go` `Checkpoint` (line ~1299): the route gates now become real; `sync_wp_off` is driven by the flag like in Go.
- Also from the BACKLOG section "Orchestrator: in-place checkpoint": `defer-rootfs-export`, the cgroup freeze and unfreeze, pre-pause reclaim, hinting drain, `pause-admission-grace-milliseconds`, guest quiesce on the filesystem-only `Pause` RPC, snapshot lineage (`AbandonsPredecessor`).

Done when: a memory checkpoint with the flag on keeps the sandbox running and exports memory through the CoW window like Go, and every Go test in `uffd/userfaultfd` and the checkpoint tests in `pkg/server` has a Rust test. The two write-protect tests can only pass on a Linux kernel with uffd-wp for anonymous memory (x86 CI), so note that.

## 3. Sandbox proxy deviations

Rust: `src/proxy/*`. Go: `packages/shared/pkg/proxy/*` and `packages/orchestrator/pkg/proxy/*`.

Fix every line in the BACKLOG section "Orchestrator: sandbox proxy":
- A connection stuck in the h2c preface stays open on graceful shutdown until the peer closes or the proxy is closed, like Go.
- The idle timeout also times a fresh h2c connection, not only after the first finished request.
- Malformed requests get Go's own 400 and 431 text bodies, not hyper's.
- The upstream dialer honours `HTTP_PROXY` and `NO_PROXY` like Go's `ProxyFromEnvironment`.
- Content type sniffing of chunked responses matches Go.
- Upstream TLS without skip-verify trusts the system roots.

Done when: `proxy_contract` and `sandbox_proxy_contract` cover each case with Go's expected behaviour and pass.

## 4. Durable and provisional header

Rust has header V4 and V5 (`src/storage/header.rs`, `header/v4.rs`, `header/v5.rs`) but does not use Go's provisional header flow.

Go to port: `DurableHeader` and the provisional header in `pkg/sandbox/build/build.go`, `pkg/sandbox/build_upload.go`, `pkg/sandbox/sandbox.go`, `pkg/sandbox/template/cache.go`, `storage.go`, `storage_template.go` and `peerserver/header.go`. Serve V5 headers with `IncompletePendingUpload` to peers, reload the header on a peer transition, and subscribe to the Redis upload-done channel while waiting for a parent (BACKLOG section "Orchestrator: peer chunk sharing"). Also add the `orchestrator.read.*` metrics and otelgrpc client stats on peer channels.

Done when: `TestHeaderSource_Stream_ServesDurableHeader` and the nil-header test are ported and pass, and `chunks_contract` passes with V5 on the wire.

## 5. Remaining team.id metric labels

Go commit `2a78eba4b` labels metrics with `team.id`. Rust still lacks the metrics themselves for envd init duration, rootfs and memory seal durations, snapshot diff sizes and build durations. Add those metrics with Go's names, units, buckets and attributes (see `pkg/sandbox/snapshot_metrics.go`, `pkg/sandbox/sandbox.go`, `pkg/template/build/metrics/metrics.go`, `pkg/server/sandboxes.go`), with `team.id` on each. Also have the sandbox metrics observer cover build sandboxes like Go.

Done when: each Go metric test touched by `2a78eba4b` has a Rust test.

## Final check

After item 5, run once: `scripts/clippy-linux.sh` and `scripts/test-linux.sh --workspace --no-fail-fast`. Push to `main`. Report per item: Rust commits, Go tests ported, anything left in `docs/BACKLOG.md`.
