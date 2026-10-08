---
name: project-structure
description: Use when adding or moving code in envd-rs — new endpoint, Connect RPC, middleware, helper, module or test. Says which file/folder it belongs in and the code style rules (crates.io-style layout, no mod.rs, no comments, KISS, Go-compatible API).
---

# envd-rs project structure

Layout follows rust-lang/crates.io: `foo.rs` declares submodules that live in `foo/`. Never create `mod.rs`.

```
Cargo.toml             workspace only: members = ["crates/*"], release profile
crates/orchestrator/   orchestrator crate (same layout rules as envd below)
  src/storage/         object_store backends, build paths, header V3
  src/block/           sparse cache + tracker, chunker, build file, overlay
  src/exposure/nbd/    NBD server (concurrent dispatch over the BlockDevice trait), ioctls, device.rs (attach),
                       pool.rs (DevicePool: process-wide ready devices on std threads, NBD_POOL_SIZE)
  src/network/         slot.rs (netns, veth, tap, iptables per slot; HostSlots), pool.rs (NetworkPool: 32 new,
                       100 reused, 3s return delay), storage.rs (SlotStorage: index allocation, leaked namespaces)
  src/template.rs      Template: snapfile + headers fetched once per build (OnceCell); template/cache.rs (TemplateCache:
                       25h sliding TTL, Arc pins, 1 min grace)
  src/phases.rs        PhaseTimer: resume phase durations and notes
  src/cgroup.rs        cgroup v2 per sandbox
  src/firecracker/     config (VmConfig builder), client (API over unix socket), launch.rs (unshare -m script, /fc-vm paths),
                       memory_info.rs (GET /memory, /memory/mappings), mmds.rs, process
  src/memory/          userfaultfd: region.rs (handshake regions, address <-> memfile offset), uffd.rs (safe uffd fd),
                       handshake.rs (SCM_RIGHTS listener), server.rs (page-fault loop), snapshot.rs (memfile export)
  src/envd.rs          envd POST /init client (upstream retry semantics)
  src/storage/build.rs Build: memfile + rootfs headers of a snapshot
  src/sandbox.rs       Sandbox lifecycle: cold_boot, stop; sandbox/config.rs builder, resources.rs (teardown, Drop),
                       snapshot.rs (Snapshot, snapshot/pause: dirty pages via /memory/dirty or resident-empty, rootfs diff, upload),
                       resume.rs (snapshot load over uffd)
  src/server/          gRPC node: config.rs (env), node.rs (Node: storage, store, registry, network pool, template cache),
                       lifecycle.rs (task per sandbox: commands, end_time expiry, death watch), service.rs (SandboxService),
                       registry.rs
  src/proto.rs         tonic::include_proto!("_") of proto/orchestrator/orchestrator.proto (no package upstream)
  src/main.rs          orchestrator binary: NodeConfig::from_env + tonic server on GRPC_PORT (5008)
  examples/sandbox.rs  dev runner: cold or (E2B_RESUME=1) resumed sandbox, 127.0.0.1:49983 forwarded to envd
  tests/*_contract.rs  Linux-only (root, KVM, nbd) integration tests, run in CI
docker/guest.Dockerfile + envd.service  test microVM rootfs (Debian, envd under systemd)
scripts/ci/            CI helpers: vm-assets.sh, guest-rootfs.sh, vm-sdk-tests.sh, s3.sh
proto/envd/            envd .proto copied from upstream E2B (never edit)
proto/orchestrator/    orchestrator.proto copied from upstream E2B (never edit), UPSTREAM_COMMIT
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
