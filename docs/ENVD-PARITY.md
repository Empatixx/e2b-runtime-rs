# Parity audit: envd-rs vs upstream Go envd (2026-10-08)

An audit compared `crates/envd` with upstream `packages/envd` at commit `bfd9179`. The `.proto` files and `reference/envd.yaml` are byte-identical to that commit, so no RPC or field is missing at the proto level. `crates/envd/reference/UPSTREAM_COMMIT` still names `92197909…` and should be bumped.

File RPCs, `/files`, URL signing and the process lifecycle are ported closely. The pause/resume control plane, cgroups, `/metrics`, port forwarding and live upgrade are mostly missing.

- **G** = `e2b-runtime-origin-main-review/packages/envd`
- **R** = `crates/envd/src`

## 1. `/metrics` (missing; R `router.rs:45` answers 501)

### Response (G `internal/api/store.go:174-200`, `internal/host/metrics.go:12-26`)

- Headers: `Cache-Control: no-store`, `Content-Type: application/json`. The body comes from `json.NewEncoder`, so it ends with `\n`.
- If reading the metrics fails, the response is 500 with an empty body.
- Fields:

  | Field | Type | Value |
  |---|---|---|
  | `ts` | i64 | Unix seconds |
  | `cpu_count` | u32 | logical CPUs |
  | `cpu_used_pct` | f32 | gopsutil `cpu.Percent(0,false)[0]`: delta since the previous call, rounded to 2 decimals only when > 0 |
  | `mem_total`, `mem_used`, `mem_cache` | u64 | `Used = Total − Free − Buffers − Cached`; `Cached` includes `SReclaimable` |
  | `disk_used`, `disk_total` | u64 | `statfs("/")`: `total = Blocks*Bsize`, `used = total − Bavail*Bsize` |
  | `oom_kills` | `*[]OOMKill` | `omitempty`, see below |

- `oom_kills` appears only when the watcher exists and `Kills()` returns ok. `nil` omits the key; an empty list is `[]`.

### OOM watcher (G `internal/host/oom.go`)

The watcher starts only when the `-isnotfc` flag is not set.

**Constants:**

| Name | Value |
|---|---|
| kmsg path | `/dev/kmsg` |
| marker | `"Killed process "` |
| unknown name | `Unknown` |
| open attempts | 5, 1 s apart |
| max read failures | 5 |
| healthy read | 1 min; resets the failure count |
| recovery delay | 30 min |
| kills kept | 32; the oldest is dropped |
| record buffer | 8192 B |
| poll timeout | 1000 ms |

**Kill JSON:** `{"seq": i64, "process": string}`.

**Reading the log:**
1. Open with `O_RDONLY|O_NONBLOCK|O_CLOEXEC`. Each `read` returns one record.
2. EAGAIN means the reader has caught up: set `caughtUp`, then `poll(POLLIN, 1000 ms)`.
3. EPIPE and EINTR are skipped. Any other error ends the round.

**Closing (`markStopped`):**
- Clears `caughtUp` and drops the kill list.
- A reopen rebuilds the list from the start of the log.
- `Kills()` returns `(nil, false)` until the reader catches up again.

**Retries:**
- After a closed round, reopen after 1 s.
- After 5 read failures, or when the open itself fails:
  1. Log at Error.
  2. Sleep 30 min.
  3. Retry quietly.

**Log texts:**
- `Failed to open the kernel log, out-of-memory kills will not be reported`
- `Stopped reading the kernel log, out-of-memory kills will not be reported`
- `Failed to read the kernel log, reopening it` (Warn)
- `Reading the kernel log again, out-of-memory kills are reported` (Info, only on recovery)

**Parsing a record (G `:286-363`):**
1. Skip the record unless it contains the marker.
2. Split at the first `;`. Split the prefix by `,` into at most 3 fields:
   - the priority must be an integer in 0..=7;
   - the second field is the seq (i64).
3. Keep the message up to the first `\n`. It must start with one of these, followed by the marker:
   - `Out of memory: `
   - `Out of memory (oom_kill_allocating_task): `
   - `Memory cgroup out of memory: `
4. The victim is `<pid> (`, where pid passes `Atoi`. The name ends at the last `") total-vm:"`.
5. If the name is unreadable:
   - keep the kill with the name `Unknown`;
   - log `Failed to find the process name of an out-of-memory kill`, once per seq above `lastUnnamed`.
   - The error texts are `no process ID followed by a name` and `no end to the process name`.

## 2. Other HTTP routes

### `/freeze` and `/unfreeze` (missing; G `init.go:418-614`, `internal/services/cgroups/freeze.go`)

**Query:**
- `mode`: `legacy` or `hierarchy`; the default is `legacy`.
- `maxCgroups`: a value ≤ 0 means 512; the cap is 8192.
- `maxWaitMs`.

**Responses:**
- Without `maxWaitMs`: 204.
- With `maxWaitMs`: 200 and a `FreezeResult` with these camelCase optional fields: `mode`, `visited`, `allowlisted`, `truncated`, `preFrozen`, `requested`, `frozen`, `notFrozen`, `failed`, `vanished`, `unobservable`, `sweepMs`, `waitMs`.
- 503 when the lock is busy or the context is cancelled.

**Timing:** logs are flushed before the reply within min(2 s, the remaining wait). Thaw watchdog 10 min, poll every 2 ms, handover wait 2 s.

**`/unfreeze`:** 204, or a 500 jsonError.

### `/fsfreeze` and `/fsthaw` (missing; G `internal/api/fsfreeze.go`, `internal/services/fsfreeze/fsfreeze_linux.go`)

- Both act on the root `/` with ioctls `FIFREEZE 0xC0045877` and `FITHAW 0xC0045878`.
- EBUSY on freeze and EINVAL on thaw count as success.
- Errors are a 500 jsonError with one of these texts: `open %s: %w`, `FIFREEZE %s: %w`, `FITHAW %s: %w`.
- 503 when the semaphore is busy. The thaw ignores cancellation.
- Success: 204 with `Cache-Control: no-store`.

### `/collapse` (missing; G `internal/api/collapse.go`, `internal/services/memory/collapse_linux.go`)

- Works on anonymous read-write mappings: `MADV_HUGEPAGE`, then `MADV_COLLAPSE` on each 2 MiB chunk.
- The `AnonHugePages` delta in `smaps_rollup` splits the chunks into collapsed and already-huge.
- 200 with `{regions, chunks, collapsed, alreadyHuge, skipped, elapsedMs}`; 500 on error.

### `POST /upgrade` and handover (missing; G `main.go:408-456`)

- 409 `envd not initialized`.
- 400 `unsupported upgrade target`, based on the header `X-Envd-Upgrade-Bin`.
- Otherwise: write the body to the fixed upgrade path, freeze, export, then `execve`.
- Also missing:
  - the `-resume-handover` flag, which R `config.rs` rejects;
  - re-adoption after a handover;
  - the 60 s fallback thaw.

### `/envs` (text differs)

G's encoder appends `\n`; R's axum `Json` does not.

## 3. `/init` (G `init.go:178-411`, R `controllers/init.rs`)

### Version (text differs, high impact)

- R sends `X-Envd-Version` and `-version` from `CARGO_PKG_VERSION`, which is `0.1.0`.
- G `pkg/version.go` is `0.9.0`.
- The orchestrator gates features on this version (snapshots ≥ 0.5.0, cgroup freeze ≥ 0.6.3, `/collapse` ≥ 0.6.5). The version can only be raised once those endpoints exist.

### Missing headers

- **`X-Envd-Memory`**, on every response including errors:
  - Shape: `{request, low, floor, partial}`.
  - Built from `memory.min` along the cgroup chain and from this cgroup's own `memory.low`.
  - `max` reads as MaxInt64. An unreadable value sets `partial: true`. Without cgroups the header is `{partial: true}`.
  - Source: G `internal/services/cgroups/memory.go`.
- **`X-Envd-Freeze-Audit`** `{visited, frozen, escaped, violations, truncated}`, sent only after a freeze.
- **`X-Envd-Handover`**.

### Behaviour

- **Deferred workload thaw:** G thaws the workload on every authenticated `/init`. R does not.
- **Token decoding:** G's `SecureToken.UnmarshalJSON` rejects `"accessToken": ""` and any backslash escape with 400. R treats an empty token as unset.
- **Timestamp ordering:**
  - G's `AtomicMax` starts at 0 and accepts `>=`. An identical timestamp is applied again, and pre-1970 timestamps are rejected.
  - R starts at `i64::MIN` and requires strictly greater.

### MMDS self-heal

When the hash lookup fails, G calls `PinMMDSRoute` and retries once (`init.go:83-95`):

```
iptables -w 5 -t nat -D/-I <PREROUTING|OUTPUT> 1 -d 169.254.169.254 -p tcp --dport 80 -j RETURN
```

It logs `failed to pin MMDS iptables route` at Warn, at most once per 10 s.

### `caBundle` (missing; G `internal/host/cacerts.go`)

1. Normalise the bundle to one trailing `\n`; skip it if unchanged.
2. Append it to `/etc/ssl/certs/ca-certificates.crt`.
3. In the background, write `/usr/local/share/ca-certificates/e2b-ca.crt` and remove the previous cert from the bundle.

Errors: 400 `failed to install CA bundle: %w`; 503 `CA install already in progress`.

### `volumeMounts` and `lifecycleID` (missing; G `init.go:640-800`)

- Each item is `{nfs_target, path}`.
- Mount options, with a 10 s timeout:

  ```
  fg,hard,sync,rsize=1048576,wsize=1048576,mountproto=tcp,mountport=2049,proto=tcp,port=2049,nfsvers=3,noacl,noac,lookupcache=none
  ```

- Remount only when the stored lifecycle differs.
- Clear stale mounts:
  1. Find them with `findmnt`.
  2. `umount --force`.
  3. Fall back to `umount --lazy` (2 s).
- Error texts: `failed to setup NFS volumes: %w` and `` `%s` failed: %w\n%s ``.

### Hyperloop (G `init.go:802-859`)

- G validates the IP (`failed to get ip family: …`) and holds a mutex while applying it.
- On success, G stores `E2B_EVENTS_ADDRESS=http://<ip>`. R does not set it.
- G rewrites `/etc/hosts` with the `txeh` library. Its exact output was not compared.

### JSON keys

G matches keys case-insensitively; serde does not.

## 4. Startup, flags, environment

### Flags

`-commit` is parsed but ignored: R starts the server instead of printing the SHA.

### Missing at startup (G `main.go:179-197`)

- `mkdir -p /run/e2b`.
- The env var `E2B_SANDBOX`, set to `true` or `false` from `!isnotfc`, and the file `/run/e2b/.E2B_SANDBOX` with mode 0444.
- An MMDS poll at startup with no deadline. R polls only after `/init`.

### MMDS poll texts

- G logs `<4>gave up polling for mmds opts: <ctx err> (last error: get mmds token: …)`. R drops the context error and the `get mmds token:` / `get mmds opts:` prefixes.
- G writes the ID files with mode 0666.
- R calls `error_for_status` on the token request; G does not.

### HTTP log exporter (missing; G `internal/logs/exporter/exporter.go`)

- POSTs JSON to the MMDS `address` with a 10 s timeout.
- Max line 192 KiB, buffer 8 MiB.
- Flushes and purges before a freeze.

### Server

- No `IdleTimeout` of 640 s.
- CORS: R uses `Any`; G uses an explicit list with MaxAge 7200 (also in the backlog).

## 5. Cgroups and processes

### Cgroup manager (missing; G `main.go:466-527`, `internal/services/cgroups/cgroup2.go`)

R parses `cgroup_root` and `no_cgroups` but ignores them. G:

1. Checks `statfs` for `CGROUP2_SUPER_MAGIC`.
2. Creates three cgroups:

   | Cgroup | `cpu.weight` | `io.weight` | memory |
   |---|---|---|---|
   | `ptys` | 200 | `default 50` | `memory.high = memory.max = MemTotal − min(MemTotal/8, 128 MiB)` |
   | `socats` | 150 | `default 50` | `memory.min 5 MiB`, `memory.low 8 MiB` |
   | `user` | 50 | `default 10` | same as `ptys` |

3. Spawns with `CLONE_INTO_CGROUP` (G `handler.go:453-463`):
   - tag `_system` stays in the root cgroup;
   - PTY processes go to `ptys`;
   - everything else goes to `user`.

### Port forwarder (missing; G `internal/port/forward.go:172`)

- Every 1 s, scan for LISTEN sockets on `127.0.0.1`, `localhost` or `::1`.
- For each, run in the `socats` cgroup with Setpgid:

  ```
  socat -d -d -d TCP4-LISTEN:<p>,bind=169.254.0.21,reuseaddr,fork TCP<family>:localhost:<p>
  ```

### Other process differences

- **Start error codes:** DeadlineExceeded and Canceled should map to those Connect codes (G `start_error.go:26-29`). R maps both to InvalidArgument.
- **Legacy SDK (missing; G `internal/services/legacy/*`):** for User-Agent exactly `connect-python`:
  - set `X-E2B-Legacy-SDK: true`;
  - down-convert filesystem responses to the legacy `EntryInfo{name,type,path}`.
- **Keepalive (low):** G's `Atoi` of 0 or a negative value panics in `NewTicker`; R falls back to 90 s.
- **StreamInput data before start (low):** G dereferences nil; R returns FailedPrecondition `process not selected`.

## 6. `/files`

Ported closely. One possible difference: `Content-Type`.
- G `http.ServeContent` uses `mime.TypeByExtension` (with charset, plus `/etc/mime.types`) and falls back to content sniffing.
- R uses `mime_guess`.

Not verified.

## Not verified

- gopsutil formulas.
- `txeh` hosts output.
- `Content-Type: ""` on 204.
- `invalid signal` Debug text.
- Invalid `Connect-Timeout-Ms` handling.
- Filesystem RPC internals beyond spot checks.
