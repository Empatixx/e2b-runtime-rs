# Parity: orchestrator vs upstream Go

An opus audit compared our Rust orchestrator with upstream E2B Go, covering RPCs, resume, pause, lifecycle, pools, node services and the hot path.

- **G** = `e2b-runtime-origin-main-review/packages/orchestrator/pkg`
- **R** = `crates/orchestrator/src`

## Fixed right after the audit

- `/init` sent `"accessToken": ""`, `"defaultUser": ""` and `"defaultWorkdir": ""`. Go envd rejects the empty token with 400. Empty fields are now omitted (R `envd.rs`).
- MMDS `accessTokenHash` was omitted without a token. It is now always `hash(token or "")`, as G `fc/process.go:668`.
- The build cache disk threshold default is now 85 %, as G `featureflags/flags.go:550` (was 90 %).
- `Update` with `egress` silently returned OK. It now applies the egress rules to the slot firewall (see below).
- Answered: admission `Count()` upstream counts live sandboxes only, so ours matches.

## Closed by the parity milestone

Plan: `docs/superpowers/plans/2026-10-18-orchestrator-parity.md`. Every row of the original audit table is now done:

| # | Difference | Now |
|---|---|---|
| 1 | Network egress on Create and Update | nftables `slot-firewall` with upstream sets and DSCP, host REDIRECTs, TCP firewall proxy on 5016/5017/5018, Update egress with rollback, BYOP gate |
| 2 | Redis routing record | `sandbox:routing:{id}` on insert and stopping, tombstones, `fred` client |
| 3 | Pause reply timing | reply after snapshot and cache, teardown in the background |
| 4 | Create deadline | 60 s over the whole Create |
| 5 | Lifecycle events and host stats | Redis stream (`wh:{team}` gate), ClickHouse `sandbox_events` and `sandbox_host_stats`, OOM watermark |
| 6 | Checkpoint | resume-fresh with upstream texts, synchronous upload, `checkpointed` event |
| 7 | `/init` fields | lifecycleID, hyperloopIP, volumeMounts, defaultUser rule; hyperloop server on 5010 |
| 8 | `SchedulingMetadata`, `ResolvedFirecrackerVersion` | filled on Create, Pause and Checkpoint |
| 9 | memfd background copy on pause | metadata at once, copy in the background, readers and upload wait for it |
| 10 | Template pinning and TTL | pinned for the sandbox's life, TTL `max(25h, max_sandbox_length + 1h)` |
| 11 | Error texts | upstream texts for AlreadyExists, snapshot, lost races, Delete, Pause, Checkpoint |
| 12 | Same-id Create while stopping | allowed, Go map semantics with lifecycle ids |
| 13 | Missing `start_time` / `end_time` | Unix epoch |
| 14 | Metadata versions on pause | from the sandbox config |
| 15 | Upload object metadata, empty diffs | `team_id`, `template_id`, `build_origin`; empty diffs skip the body |
| 16 | Storage configuration | `TEMPLATE_STORAGE_URL` / `BUILD_CACHE_STORAGE_URL` with upstream URL parsing and texts, legacy `STORAGE_PROVIDER` envs, separate build-cache storage, GCS/S3/Azure/local providers |
| 17 | Layer file uploads | `InitLayerFileUpload` on the build cache with signed-URL headers (Azure `x-ms-blob-type`), local HMAC `PUT /upload` |
| 18 | Shutdown | 15 s pause unless `ENVIRONMENT=local`, template builds drained with a 15 s consumer grace, `FORCE_STOP` |

Also: cgroups are named `sbx-<id>-<random>` as upstream, so a same-id resume or checkpoint never shares the old sandbox's cgroup.

## Still different (flag-gated or not ported)

These paths are off by default upstream. We follow the default:

- in-place checkpoint (`in-place-checkpoint`, needs sync write-protect and FC >= 0.2.0);
- filesystem-only checkpoint (`filesystem-only-checkpoint`), refused with the upstream `UserError`;
- pause/checkpoint admission grace (`pause-admission-grace-milliseconds` = -1);
- async peer-to-peer checkpoint upload (`peer-to-peer-async-checkpoint`);
- ClickHouse alternate endpoints are parsed and logged only;
- resume-time envd live upgrade.

Not ported yet: the template builder phases (M14–M16) and the envd `oom_kills` metric in envd-rs.

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
