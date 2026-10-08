---
name: project-structure
description: Use when adding or moving code in envd-rs — new endpoint, Connect RPC, middleware, helper, module or test. Says which file/folder it belongs in and the code style rules (crates.io-style layout, no mod.rs, no comments, KISS, Go-compatible API).
---

# envd-rs project structure

Layout follows rust-lang/crates.io: `foo.rs` declares submodules that live in `foo/`. Never create `mod.rs`.

```
Cargo.toml             workspace only: members = ["crates/*"], release profile
crates/orchestrator/   orchestrator crate (same layout rules as envd below)
  src/storage/         object_store backends (backend.rs: Storage::open per provider, signed UploadUrl;
                       backend/providers.rs: S3, GCS, Azure builders), spec.rs (StorageSpec, upstream storage URL
                       parsing), local_upload.rs (HMAC UploadSigner), build paths, header V3
  src/go_url.rs        Go net/url parse and String (go_url/escape.rs, go_url/query.rs url.Values)
  src/go_exec.rs       commands with Go exec error texts (exit status N, signal: killed, not found in $PATH);
                       go_humanize.rs (humanize.Bytes)
  src/builder/         template builder: config.rs (BuilderConfig env), template_files.rs (+ render.rs Go template
                       subset, layers.rs files/symlink tars, hash.rs filesHash/provision version; assets/template/),
                       ext4.rs (+ stats.rs debugfs parsing), oci/ (reference.rs go-containerregistry names, registry.rs
                       distribution client, auth.rs, ecr.rs SigV4, pull.rs GetPublicImage, untar.rs moby untar with
                       whiteouts, export.rs layers top-first, overlay_mount.rs new mount API, copy.rs rsync),
                       rootfs.rs (+ unpack.rs CreateExt4Filesystem), provision.rs (console marker scan),
                       phases.rs (BuildEnv) + phases/base.rs (+ build.rs), pipeline.rs (PhasePipeline)
  src/go_strconv.rs    strconv.ParseBool, strconv.Quote, caarlos0/env bool errors; go_os.rs (Go PathError text)
  src/block/           sparse cache + tracker, chunker (storage-backed or local), build file (DiffStore: TTL, pins,
                       disk-pressure eviction), overlay
  src/exposure/nbd/    NBD server (concurrent dispatch over the BlockDevice trait), ioctls, device.rs (attach),
                       pool.rs (DevicePool: process-wide ready devices on std threads, NBD_POOL_SIZE)
  src/network/         slot.rs (netns, veth, tap, host REDIRECTs per slot; HostSlots), pool.rs (NetworkPool: 32 new,
                       100 reused, 3s return delay, configure/reset egress), storage.rs (SlotStorage: index allocation,
                       leaked namespaces), firewall.rs (nftables slot-firewall: sets, DSCP, EgressRules),
                       settings.rs (SANDBOX_* network env)
  src/tcpfirewall/     egress TCP proxy on 5016/5017/5018: rules.rs (domain/CIDR allow-deny), sniff.rs (HTTP Host,
                       TLS SNI), proxy.rs (SO_ORIGINAL_DST, 30s dial)
  src/hyperloop/       hyperloop server on 5010 for the guest: server.rs (/me, /logs), logs.rs (SourceSandbox)
  src/routing/         publisher.rs (RoutingPublisher: sandbox:routing:{id} on insert/stopping, tombstones),
                       redis.rs (fred client: Redis::connect, RoutingCatalog, EventStream, RedisSettings)
  src/events.rs        SandboxEvent, EventsService fan-out to EventSinks (Redis stream gated by wh:{team})
  src/clickhouse/      batcher.rs (1000 rows / 1000 queue / 1s), endpoint.rs (DSN, native writer with TLS),
                       rows.rs (sandbox_events, sandbox_host_stats), sinks.rs
  src/hoststats.rs     per-sandbox host stats sampler (StatsIdentity, cgroup CPU/memory)
  src/oom.rs           OOM kill watermark from envd /metrics
  src/builder/         template-manager role: service.rs (TemplateService RPCs, TemplateBuilder trait), builds.rs
                       (BuildCache, 10 min TTL), logs.rs (BuildLog), phase.rs (Phase trait, run_phases, BuildError),
                       pipeline.rs (PhasePipeline), hash.rs (HashKeys), index.rs (LayerIndex)
  src/template.rs      Template: snapfile, headers and metadata fetched once per build (OnceCell); template/cache.rs
                       (TemplateCache: 25h sliding TTL, Arc pins, 1 min grace, add_snapshot), metadata.rs
                       (metadata.json with upstream keys, V1 fallback), optimize.rs (collect_prefetch: 2-run trace)
  src/phases.rs        PhaseTimer: resume phase durations and notes
  src/cgroup.rs        cgroup v2 per sandbox, named sbx-<id>-<random>; CgroupStats with memory.peak reset
  src/firecracker/     config (VmConfig builder), kernel_args.rs (Go buildKernelArgs), console.rs (line tap with the
                       FlushMetrics filter), client (API over unix socket), launch.rs (unshare -m script, /fc-vm paths),
                       memory_info.rs (GET /memory, /memory/mappings), mmds.rs, process
  src/memory/          userfaultfd: region.rs (handshake regions, address <-> memfile offset), uffd.rs (safe uffd fd),
                       handshake.rs (SCM_RIGHTS listener, uffd + optional memfd), server.rs (page-fault loop, installed
                       pages, Prefaulter), snapshot.rs (memfile export from the memfd or process_vm_readv),
                       trace.rs (PrefetchTracker, Trace intersection), prefetch.rs (init-trace prefetcher)
  src/envd.rs          envd POST /init client (upstream retry semantics); envd/version.rs (Go semver, snapshot >= 0.5.0);
                       envd/process.rs (Connect process.Start client, bash -l -c as a user)
  src/storage/build.rs Build: memfile + rootfs headers of a snapshot
  src/sandbox.rs       Sandbox lifecycle: cold_boot, stop; sandbox/config.rs builder, resources.rs (teardown, Drop),
                       snapshot.rs (Snapshot, LocalSnapshot, snapshot_into/pause_into/pause: dirty pages via /memory/dirty or resident-empty, rootfs diff, upload),
                       resume.rs (snapshot load over uffd)
  src/server/          gRPC node: config.rs (env; config/storage.rs: TEMPLATE_/BUILD_CACHE_ storage resolution),
                       upload.rs (local HMAC PUT /upload), node.rs (Node: storage, store, registry, network pool, template cache),
                       node/checkpoint.rs (Checkpoint resume-fresh), lifecycle.rs (task per sandbox: commands, end_time
                       expiry, death watch), service.rs (SandboxService), registry.rs (Go map semantics: reserve,
                       lifecycle-aware stop, stop_reserved), services.rs (ORCHESTRATOR_SERVICES roles),
                       scheduling.rs (SchedulingMetadata from headers), user_error.rs (UserError status details),
                       host_stats.rs and oom_monitor.rs (per-sandbox collectors), status.rs (NodeStatus, override rules, outstanding work), metrics.rs (host
                       metrics poller, /proc parsers), info.rs (InfoService), health.rs (/health on the tonic router),
                       admission.rs (max sandboxes, starting semaphore), shutdown.rs (drain), app.rs (App: one axum
                       router for SandboxService, InfoService, grpc health and /health), retry.rs (upload retry
                       policy), uploads.rs (Uploads: background snapshot uploads, parents land first)
  src/proto.rs         tonic::include_proto!("_") of proto/orchestrator/*.proto (no package upstream)
  src/main.rs          orchestrator binary: NodeConfig::from_env, App served by axum on GRPC_PORT (5008), upstream shutdown order
  examples/sandbox.rs  dev runner: cold or (E2B_RESUME=1) resumed sandbox, 127.0.0.1:49983 forwarded to envd
  tests/*_contract.rs  Linux-only (root, KVM, nbd) integration tests, run in CI
docker/guest.Dockerfile + envd.service  test microVM rootfs (Debian, envd under systemd)
scripts/ci/            CI helpers: vm-assets.sh, guest-rootfs.sh, vm-sdk-tests.sh, s3.sh, kvm-spike.sh
scripts/clippy-linux.sh clippy of the whole workspace in Docker (rust:1.98) to catch Linux-only compile errors on a Mac
proto/envd/            envd .proto copied from upstream E2B (never edit)
proto/orchestrator/    orchestrator, info, template-manager and volume .proto copied from upstream E2B (never edit), UPSTREAM_COMMIT
crates/envd/build.rs   codegen from ../../proto/envd
crates/envd/reference/ upstream envd OpenAPI + pinned commit
crates/envd/src/
  main.rs              parse config, init tracing, bind, serve. Nothing else.
  lib.rs               pub mod list only
  config.rs            CLI flags (Go `flag` compatible)
  app.rs               AppState (shared state, Arc), Defaults
  router.rs            build_router: every REST route + Connect services + layers, in one place
  auth.rs              who is calling: basic_username, request_user, URL signing
  middleware.rs        pub mod list
  middleware/          one file per tower middleware (access_token.rs, basic_auth.rs)
  controllers.rs       pub mod list
  controllers/         one file per area: health.rs, envs.rs, init.rs, filesystem.rs, ...
  controllers/<area>/  helpers of a big area (filesystem/entry.rs, ops.rs, watch.rs; files/download.rs, upload.rs, compose.rs, encoding.rs; process/spawn.rs, output.rs, events.rs, registry.rs)
  host.rs + host/      things done to the VM (clock, /etc/hosts, MMDS)
  user.rs              Linux users/groups
  envvars.rs           env vars for spawned processes
  util.rs + util/      small shared helpers (error_response.rs; fs.rs: OS error text, eval_symlinks; paths.rs; keepalive.rs; metadata.rs: user.e2b.* xattrs)
  proto.rs             include_generated!() only
crates/envd/tests/     HTTP contract tests against a real server (tests/common: spawn_server_with_state)
```

## Where does new code go

| Adding | Put it in |
|---|---|
| REST endpoint | handler fn in `controllers/<area>.rs`, route line in `router.rs` |
| Connect service | `controllers/<service>.rs` with `impl <Trait> for <Service>`, register in `router.rs` |
| Logic of a big controller | `controllers/<area>/<topic>.rs`, plain `pub fn` |
| Middleware for every request | `middleware/<name>.rs`, `.layer(...)` in `router.rs` |
| Shared state | field on `AppState` in `app.rs` |
| Helper used by 2+ areas | `util/<topic>.rs` |
| New top-level concern (processes, cgroups) | `<concern>.rs` + `<concern>/` at the crate `src/` root |

Handler names: verb + thing (`get_envs`, `post_init`). Connect methods keep the generated names.

## Code rules

- **No comments at all** — no `//`, `///`, `//!`. Names carry the meaning.
- **KISS**: no traits, newtypes, error enums or wrapper/delegating methods unless the compiler forces them. Free functions, `match`, `?`, iterator chains.
- **Chaining**: write code as method chains — `Option`/`Result`/iterator combinators (`map`, `and_then`, `filter`, `map_or`, `is_none_or`, `try_for_each`) rather than `if let`/`let else` with early returns, e.g. `headers.get(..).and_then(..).filter(..).map_or(DEFAULT, ..)`.
- **Builders**: build values with builder chains, e.g. `ErrorResponse::builder().status(StatusCode::NOT_FOUND).message(text).build()`. A struct created in several places gets its own `builder()`; use library builders (`OpenOptions`, `Response::builder`).
- **Named fields always**: no tuple structs (`struct Foo(Bar)`), no `.0`/`.1` field access.
- Errors are `String`, `io::Error`, `ConnectError` or `util::error_response::ErrorResponse` (REST JSON/plain error like Go `jsonError`) carrying the exact text the client sees.
- **API = upstream Go envd**: paths, status codes, JSON shapes, header names and error texts must match byte for byte. Check the Go source before writing; internals are plain Rust.
- Unit tests live in `#[cfg(test)] mod tests` at the bottom of the same file; HTTP behaviour goes in `crates/<crate>/tests/*_contract.rs`.
- `rustfmt.toml` sets `max_width = 120`; run `cargo fmt` and `cargo clippy --all-targets` before committing.

## Request flow

```
CORS -> require_token (middleware/access_token.rs)
     -> authenticate_user (middleware/basic_auth.rs, puts User into extensions)
     -> REST route from router.rs, or fallback to Connect router
     -> controller fn
```
