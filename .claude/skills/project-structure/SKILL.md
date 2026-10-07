---
name: project-structure
description: Use when adding or moving code in envd-rs — new endpoint, Connect RPC, middleware, helper, module or test. Says which file/folder it belongs in and the code style rules (crates.io-style layout, no mod.rs, no comments, KISS, Go-compatible API).
---

# envd-rs project structure

Layout follows rust-lang/crates.io: `foo.rs` declares submodules that live in `foo/`. Never create `mod.rs`.

```
Cargo.toml             workspace only: members = ["crates/*"], release profile
crates/orchestrator/   orchestrator crate (same layout rules as envd below)
proto/envd/            envd .proto copied from upstream E2B (never edit)
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
