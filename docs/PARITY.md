# Parity audit: orchestrator vs upstream Go (2026-10-08)

An opus audit compared our Rust orchestrator with upstream E2B Go, covering RPCs, resume, pause, lifecycle, pools, node services and the hot path.

- **G** = `e2b-runtime-origin-main-review/packages/orchestrator/pkg`
- **R** = `crates/orchestrator/src`

## Fixed right after the audit

- `/init` sent `"accessToken": ""`, `"defaultUser": ""` and `"defaultWorkdir": ""`. Go envd rejects the empty token with 400. Empty fields are now omitted (R `envd.rs`).
- MMDS `accessTokenHash` was omitted without a token. It is now always `hash(token or "")`, as G `fc/process.go:668`.
- The build cache disk threshold default is now 85 %, as G `featureflags/flags.go:550` (was 90 %).
- `Update` with `egress` silently returned OK. It now returns `Unimplemented` until the slot firewall exists.
- Answered: admission `Count()` upstream counts live sandboxes only, so ours matches.

## Open, by impact

| # | Difference | Upstream | Ours | Impact |
|---|---|---|---|---|
| 1 | Network egress / internet access in `SandboxConfig` on Create | applied to the slot firewall | ignored | **High** (security) |
| 2 | Redis routing record `sandbox:routing:{id}` for client-proxy | published on running and stopping | absent | **High** if client-proxy relies on it |
| 3 | Pause reply timing | after snapshot + cache; VM stopped in the background | after the full teardown | Med |
| 4 | Create deadline | 60 s over the whole Create | 60 s on the slot wait only | Med |
| 5 | Lifecycle events (created/resumed/updated/paused/killed + kill_reason) and per-sandbox host stats | Redis streams, ClickHouse | absent | Med–High |
| 6 | Checkpoint | implemented (resume-fresh default) | `Unimplemented` | Med–High if the API uses snapshots |
| 7 | `/init` lifecycleID, hyperloopIP, caBundle, volumeMounts; defaultUser from template metadata | sent | not sent | Med |
| 8 | Create response `SchedulingMetadata`, `ResolvedFirecrackerVersion` | filled | empty | Med |
| 9 | memfd background copy on pause | on | synchronous | Med (pause latency) |
| 10 | Template pinned while a sandbox runs; TTL `max(25h, max_sandbox_length + 1h)` | yes | `Arc` only during start, fixed 25 h | Low–Med |
| 11 | Error texts: AlreadyExists inner phrase, `error snapshotting sandbox:` inner prefix, lost Delete/Pause race → Internal | Go texts | shorter / NotFound | Low |
| 12 | Same-id Create while the old one is still stopping | allowed | `AlreadyExists "sandbox is still stopping"` | Low–Med |
| 13 | Missing `start_time` / `end_time` | Unix epoch | now | Low |
| 14 | Metadata kernel/firecracker versions on pause | from the sandbox config | copied from the parent | Low |
| 15 | Upload object metadata (team/template/origin); empty diffs skipped | yes | no | Low |

## Same as upstream (checked)

- Resume order;
- rate limiters;
- envd attempt timing (50 ms / 5 ms / 10 s);
- the retry policy;
- network pool sizes and return delay, NBD pool size;
- DiffStore TTL and eviction delay;
- InfoService fields and override rules;
- admission texts;
- shutdown order;
- List of live sandboxes only;
- Delete running async.
