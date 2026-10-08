# Orchestrator parity: every open difference from docs/PARITY.md, exactly as Go

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close every open difference in `docs/PARITY.md` the way upstream Go behaves. No triage by impact. The internals stay in our Rust style.

`G` = `~/IdeaProjects/e2b-runtime-origin-main-review/packages`. The facts below were read from the code; keep the constants exact.

## Upstream facts

### Create, ids, texts (`G/orchestrator/pkg/server/sandboxes.go`, `sandbox/map.go`)

**Deadline:**
- 60 s over the whole Create (`WithTimeoutCause "request timed out"`): reserve, admission, template, slot, resume, envd.
- The running VM uses an execution context that is not cancelled with the request.
- A resume error answers `Internal "failed to create sandbox: <err>"`. When the deadline hit, `<err>` is the inner error, then `\n`, then `request timed out`.

**Reserve** refuses only an id that is live or reserved. A stopping sandbox has already left live, so a same-id Create is allowed while the old VM tears down. Lifecycles are keyed `sandboxID/lifecycleID`. The texts are:
- `AlreadyExists "sandbox '<id>' is already running on this node: sandbox is already running on this node: lifecycle <L> (execution <E>) is live"`;
- the same text ending `…: another create is in flight`.

**Lost races:**
- Delete: a missing sandbox gives `NotFound "sandbox '<id>' not found"`; losing to another stop gives `Internal "failed to delete sandbox '<id>'"`.
- Pause: a missing sandbox gives `NotFound "sandbox not found"`; losing gives `Internal "failed to pause sandbox"`.

**Pause snapshot error:** `Internal "error snapshotting sandbox '<id>': error snapshotting sandbox: <err>"`. The other inner forms are `error adding snapshot to template cache: …` and `no metadata found in template: …`.

**Missing `start_time` / `end_time`:** both are the Unix epoch.

### Pause (`sandboxes.go:893-1061`)

**Order:**
1. Get.
2. MarkStopping.
3. `EnsurePausable`. On failure: kill async, `killed` event with `seal_failed`, then `Internal "sandbox '<id>' cannot be persisted, killing it: <e>"`.
4. Stop reason Paused.
5. `defer stopSandboxAsync`: the reply does not wait for teardown.
6. Snapshot and cache.
7. Async upload.
8. `paused` event.
9. Reply with SchedulingMetadata.

**Metadata versions:** `metadata.json` takes its kernel and Firecracker versions from the sandbox config (`SameVersionTemplate{BuildID, KernelVersion, FirecrackerVersion}`).

**Object metadata:**
- every object gets `team_id`, `template_id` (from the pause request) and `build_origin` (`pause`, `snapshot_template` or `template_build`);
- data objects also get `logical-size` (header size), `mapped-size` (sum of bytes by build) and `diff-size` (bytes of this build).

**Empty diffs:** a diff with no dirty blocks uploads no body object, but its header is still uploaded. The metadata object is always uploaded.

**memfd background copy** (`block/memfd.go:186-269`):
- pause returns while a task copies the dirty memfd ranges into the cache file;
- readers (the upload and a same-node resume) wait for that task;
- `Close` cancels the copy;
- an empty dirty set resolves immediately.

### /init and MMDS (`sandbox/envd.go:86-95`, `fc/process.go:668`)

**/init fields:**
- `lifecycleID`: a new uuid per start or resume.
- `hyperloopIP`: `SANDBOX_ORCHESTRATOR_IP`, default `192.0.2.1`.
- `caBundle`: `""`, omitted.
- `volumeMounts`: `[{"nfs_target":"192.0.2.1:/<name>","path":<path>}]`, omitted when empty. A volume id that is not a uuid fails Create with `"failed to convert volume mounts: invalid volume id %q: …"`.
- `defaultUser`: the configured value, else `"user"` when the template metadata `start.context.user == "user"`, else omitted. The resolved value is written back into the config.

**MMDS:** `address` = `http://<hyperloopIP>/logs`.

**Hyperloop server** (`G/orchestrator/pkg/hyperloopserver`):
- listens on `SANDBOX_HYPERLOOP_PROXY_PORT` (5010) and always starts;
- `GET /me` returns `{"sandboxID": …}`, looked up by source IP;
- `POST /logs` forwards to the logs collector (`LOGS_COLLECTOR_ADDRESS`).

### Slot network (`sandbox/network/*.go`, v1)

**nftables inside the netns:** table `inet slot-firewall`, chain `PREROUTE_FILTER` (filter, prerouting, priority -150, policy accept).

**Sets** (ipv4_addr, interval):
- `filtered_always_denylist`;
- `filtered_always_allowlist`;
- `filtered_denylist`;
- `filtered_allowlist`.

**Rules,** each starting with `meta iifname "tap0"`:
1. `ct state established,related accept`.
2. Destination in always-allow → accept.
3. Destination in always-deny → drop. With BYOP only for `l4proto != tcp`.
4. `l4proto != tcp` and destination in allow → accept.
5. `l4proto != tcp` and destination in deny → drop.

The policy accepts. TCP is decided by the TCP firewall proxy.

**Set contents:**
- always-deny: `10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16, 172.16.0.0/12, 192.168.0.0/16` (IPv6 entries too).
- always-allow: `<SANDBOX_ORCHESTRATOR_IP>/32` plus `ALLOW_SANDBOX_INTERNAL_CIDRS`.

A CIDR that ends at 255.255.255.255 is stored as a start element only, and later CIDRs that start after it are dropped. Every apply flushes the chain, reinstalls the rules and refills all sets in one batch.

**Egress config:**
- `SandboxConfig.network.egress` holds `allowed_cidrs`, `denied_cidrs`, `allowed_domains`, `rules`, `egress_proxy_*`.
- **ConfigureInternet** at slot `get`: a no-op when everything is empty; otherwise `ApplyRules(byop, allowed, denied)`. Error: `"error setting slot internet access: failed execution in network namespace 'ns-N': …"`.
- **ResetInternet** on return: only when custom rules were set.

**DSCP:**
- `iptables -t mangle -A POSTROUTING -o eth0 -j DSCP --set-dscp N` in the netns;
- `SANDBOX_EGRESS_DSCP` defaults to 0, which means no rule, and the range is 0..63;
- a re-stamp appends the new rule before deleting the old one.

**Host rules,** in order:
1. In the netns: SNAT and DNAT, as today.
2. `FORWARD -i veth-N -o gw ACCEPT`.
3. `FORWARD -i gw -o veth-N ACCEPT`.
4. `MASQUERADE -s hostIP/32 -o gw`.
5. `nat PREROUTING -i veth-N -p tcp -d 192.0.2.1 --dport 80 -j REDIRECT --to-port 5010`.
6. The same for `111 → 5012` and `2049 → 5011`.
7. TCP firewall: `nat PREROUTING -i veth-N -p tcp --dport 80 -j REDIRECT --to-port 5016`, `--dport 443 → 5017`, and `-p tcp → 5018` (ports from `SANDBOX_TCP_FIREWALL_{HTTP,TLS,OTHER}_PORT`).

Our extra rules (`INPUT NEW DROP`, the restricted FORWARD and our private-range FORWARD drops) are replaced by these.

**TCP firewall proxy** (`G/orchestrator/pkg/tcpfirewall`):
- it finds the sandbox by source IP and reads the original destination with `SO_ORIGINAL_DST`;
- it allows an allowed domain (HTTP Host or TLS SNI, with `*` and `*.suffix`), then dials the hostname and refuses it if the name resolved into the always-deny CIDRs;
- an allowed CIDR is allowed, a denied CIDR is closed, everything else is allowed;
- the dial timeout is 30 s.

**BYOP:** a non-empty proxy address on Create or Update gives `PermissionDenied "egress proxy is not enabled for this team"` when the flag is off, else `Unimplemented "egress proxy is not supported by this orchestrator build"`.

**Update egress** (`sandboxes.go:480-653`):
- updates apply in order (end_time, egress), all or none, rolled back in reverse;
- the kernel firewall changes first, then the published config;
- errors: `"failed to update sandbox network: …"`, and the outer `Internal "failed to update sandbox: %s"`.

### Redis routing (`G/shared/pkg/sandbox-catalog`, `orchestrator/pkg/routing`)

- **Key** `sandbox:routing:{id}`.
- **Value** JSON `{"orchestrator_id","orchestrator_ip","execution_id","sandbox_started_at","sandbox_max_length_in_hours"}`:
  - `orchestrator_id` is a uuid per process;
  - `orchestrator_ip` is `NODE_IP`, default `localhost`;
  - `sandbox_started_at` is RFC3339Nano.
- **TTL** is `max_sandbox_length` hours; 0 means no expiry.
- **Writes:**
  - a `SET` when the sandbox becomes running (build sandboxes are skipped);
  - on stopping, a compare-and-delete on `execution_id` (Lua; results 0 absent, 1 deleted, 2 mismatch, 3 unreadable);
  - a stop that arrives before the insert leaves a tombstone.
  - Every call has a 1 s timeout and is best-effort.
- **Env:**
  - `REDIS_CLUSTER_URL` wins over `REDIS_URL`;
  - `REDIS_PASSWORD`, `REDIS_TLS_ENABLED`, `REDIS_TLS_CA_BASE64`;
  - `REDIS_POOL_SIZE` 5, `REDIS_MIN_IDLE_CONNS` 2;
  - a password without TLS is a startup error: `"a Redis password is set but TLS is not enabled: …"`;
  - without either URL, Redis is disabled with a warning.

### Lifecycle events (`G/shared/pkg/events/sandbox.go`)

**JSON fields:**
- `id`, `version` `"v2"`, `type`, `timestamp` (UTC);
- `event_category` and `event_label` (both `""`);
- `event_data` (omitempty);
- `sandbox_id`, `sandbox_execution_id`, `sandbox_template_id` (= base template id), `sandbox_build_id`, `sandbox_team_id`;
- `events_ttl_days` (omitempty).

**Types:** `sandbox.lifecycle.{created,resumed,updated,paused,killed,checkpointed}`.

**`event_data`:**
- `sandbox_metadata`;
- paused and killed add `execution{started_at, vcpu_count, memory_mb, execution_time ms}`;
- killed adds `kill_reason` (from the request, else `unknown`);
- updated adds `set_timeout` and `network_egress`.

**Transport:**
- Redis `XADD sandbox.events.stream * payload <json>`, only when the key `wh:{teamID}` exists;
- ClickHouse `INSERT INTO sandbox_events` when configured;
- publishing is async.

### Host stats and OOM

**Host stats:**
- every 1 s, read cgroup `cpu.stat` (`usage_usec`, `user_usec`, `system_usec`), `memory.current` and `memory.peak` (reset after each read);
- the first row is a zero baseline, then deltas plus `interval_us`, then a final sample on stop;
- rows go to `INSERT INTO sandbox_host_stats` (ClickHouse only).

**OOM:**
- the watermark is seeded from envd `/metrics` `oom_kills`: 5 attempts, 2 s apart, 2 s timeout;
- every 5 s, each new kill is logged as `"Out of memory: process was killed"` (process name cut to 64 runes);
- at most 10 kills per poll, then `"Out of memory: at least this many more processes were killed"`.

### Checkpoint (`sandboxes.go:1156-1312, 1502-1648`)

**Checks first:**
- `NotFound "sandbox '<id>' not found"`;
- envd older than 0.5.0 → `FailedPrecondition "sandbox envd version must be at least 0.5.0 to create snapshots, current version: X"`;
- the semaphore waits 15 s;
- `filesystem_only` (flag off) → `FailedPrecondition "filesystem-only checkpoint of sandbox '<id>' is disabled"`.

**Resume-fresh:**
1. MarkStoppingReserved (busy → `FailedPrecondition "an operation is already in progress for sandbox '<id>'"`).
2. Stop the old sandbox async.
3. Snapshot and cache it (origin `snapshot_template`).
4. Pin the template.
5. Resume with the same config, execution id and start/end times, and a new lifecycle id.
6. Mark it live.
7. Upload synchronously (20 min per attempt). On failure, kill the new sandbox and return `Internal "error uploading snapshot for checkpoint '<id>': …"`.
8. `checkpointed` event; reply with SchedulingMetadata.

**Other errors:**
- `error snapshotting sandbox '<id>': …`;
- `error getting template for resume: …`;
- `error resuming sandbox after checkpoint: …`;
- `error registering resumed sandbox after checkpoint: …`.

### SchedulingMetadata (`G/orchestrator/pkg/scheduling/metadata.go`)

- **`FromHeaders(build, memfile, rootfs, newBytes)`:**
  - base build ids;
  - per-artifact build ids: the keys of bytes-by-build, plus base and build, sorted by string;
  - a cap of 128: keep base and build, then the largest by bytes, then by id;
  - `*_dropped_builds`;
  - `*_build_bytes` aligned with the ids.
- **Create** fills it from the template headers; **Pause** fills it from the new headers with `dirty × block` bytes.
- **`ResolvedFirecrackerVersion`** echoes the request without a flag override.

### Template pinning

- A template is pinned from Create until the lifecycle ends.
- Its TTL is `max(25h, max_sandbox_length hours + 1h)` when `max_sandbox_length > 0`.

## Tasks

Each task: tests first (RED), implement, gate, commit.

1. **Create and ids:** whole 60 s deadline with the joined error text; Reserve semantics allowing a stopping id; lifecycle ids; exact AlreadyExists, Delete, Pause and lost-race texts; epoch timestamps.
2. **Pause:** reply before teardown (stop async); snapshot error inner prefix; metadata versions from the sandbox config; object metadata on uploads; empty diffs skip the body.
3. **/init and MMDS:** lifecycleID, hyperloopIP, volumeMounts, defaultUser rule, MMDS address; the hyperloop server (`/me`, `/logs`) on 5010.
4. **Slot firewall:** nftables `slot-firewall` with the exact sets and rules; DSCP; host REDIRECTs; our extra rules removed; ConfigureInternet and ResetInternet in the pool; Update egress with rollback; BYOP gates.
5. **TCP firewall proxy:** 5016/5017/5018 with domain, CIDR and deny logic, `SO_ORIGINAL_DST`, 30 s dial.
6. **Redis routing record:** the `fred`/`redis` crate behind a small type; env config; tombstones.
7. **Lifecycle events:** Redis stream (`wh:{team}` gate) and the ClickHouse sink (HTTP insert) when configured.
8. **Host stats and OOM watermark.**
9. **SchedulingMetadata** (`FromHeaders`) on Create and Pause, and `ResolvedFirecrackerVersion`.
10. **Template pinned for the sandbox's life,** TTL from `max_sandbox_length`.
11. **memfd background copy on pause.**
12. **Checkpoint RPC,** resume-fresh.
13. **Docs, PARITY.md, review, CI green.**
