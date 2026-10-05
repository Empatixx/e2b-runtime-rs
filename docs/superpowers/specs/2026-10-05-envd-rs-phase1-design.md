# envd-rs — fáze 1: jádro

**Datum:** 2026-10-05
**Upstream:** `e2b-dev/infra` `packages/envd` @ `92197909` (2026-10-03), viz `reference/UPSTREAM_COMMIT`

## Cíl

1:1 Rust replika E2B `envd` (daemon uvnitř sandbox VM), použitelná jako **drop-in** náhrada Go binárky
v E2B infra. Wire kompatibilita (Connect RPC + HTTP) s jejich SDK a orchestrátorem.

Projekt je rozdělen do fází, každá má vlastní spec → plán → implementaci:

| Fáze | Obsah |
|---|---|
| **1 (tento spec)** | Process + PTY, Filesystem + watchers, `/files` (+compose), auth + signing, `/init`, `/envs`, `/health` |
| 2 | `/fsfreeze`, `/fsthaw`, `/freeze`, `/unfreeze`, `/collapse`, `/metrics`, cgroups (zařazení procesů), OOM |
| 3 | live upgrade/handover, NFS `volumeMounts`, `caBundle`, port forwarding, log exporter, legacy RPC |

**Hotovo (fáze 1) =** testy JS SDK `packages/js-sdk/tests/sandbox/{commands,files,pty}` v debug režimu
proti Rust envd (`-isnotfc`, Docker) procházejí + unit/kontraktové testy níže.

## Stack

- tokio, axum (HTTP/1.1 + h2c), tower-http (CORS)
- `connectrpc` (github.com/connectrpc/connect-rust) + `buffa` (protobuf runtime, binární i JSON)
- codegen přes `buf generate` (`protoc-gen-buffa`, `protoc-gen-buffa-packaging`, `protoc-gen-connect-rust`), výstup checked-in v `src/generated/`
- rustix (signály, uid/gid, `copy_file_range`, `statfs`, `clock_settime`), `pty-process` (async), `notify`
- `sha2`, `base64`, `subtle` (signing), `dashmap`, `tracing`
- build: statický musl, x86_64 + aarch64

## Struktura

```
envd-rs/
├── proto/                 # 1:1 kopie upstream process.proto, filesystem.proto
├── reference/envd.yaml    # upstream OpenAPI (reference, ne codegen)
├── buf.gen.yaml
├── src/
│   ├── main.rs            # CLI flagy: -isnotfc -port(49983) -cgroup-root -no-cgroups -verbose -version -commit
│   ├── server.rs          # jeden axum Router: Connect služby + HTTP routy, CORS, auth layer
│   ├── state.rs           # AppState: env vars, access token, defaults (user, workdir), registry
│   ├── generated/
│   ├── api/               # init, envs, health, files (download/upload/compose), auth, signing, encoding
│   ├── services/process/  # služba + handler/ (spawn, multiplex, pty)
│   ├── services/filesystem/
│   ├── permissions/       # user lookup, path expand/resolve, Basic-auth username
│   ├── execcontext/       # default user + workdir
│   └── host/              # MMDS klient (jen !isnotfc)
├── tests/
└── docker/                # Dockerfile podle upstream debug.Dockerfile
```

Fáze 2/3 routy existují a vrací `501 Not Implemented`.

## Process služba

**Registry:** běžící procesy `pid → Arc<ProcessHandle>`; ukončené v `terminated` cache s TTL **30 s**
(pozdní `Connect` dostane `EndEvent`). Start s tagem smaže retained exit pod stejným tagem.
Selektor `pid | tag`, tag = první shoda. Tag `_system` = systémový typ procesu.

**Spawn** (shodně s Go):
- `/bin/sh -c 'echo <oom_score> > /proc/$$/oom_score_adj && exec <ionice/nice prefix>"${@}"' -- cmd args…`
- uid, gid + supplementary groups uživatele
- cwd = `expand_and_resolve(cwd, user, default_workdir)`; neexistuje → `InvalidArgument` `cwd '<path>' does not exist`
- env v pořadí: `PATH` (z envd) → `HOME`, `USER`, `LOGNAME` → globální env z `/init` → env z requestu (vyhrává poslední)
- bez PTY: pipes stdout/stderr, stdin pipe jen při `stdin=true`
- s PTY: `pty-process` s velikostí z requestu; výstup jde do `DataEvent.pty`

**Multiplex:** fan-out výstupu do N odběratelů (Start + Connecty) přes `mpsc` per subscriber,
**s backpressure jako Go** (pomalý odběratel brzdí čtení, data se neztrácí). Zrušený odběratel se přeskočí.

**Streamy Start/Connect:** `StartEvent{pid}` → `DataEvent*` → `EndEvent{exit_code, exited, status, error?}`.
Keepalive: interval z hlavičky `Keepalive-Ping-Interval` (sekundy), default 90 s, reset po každém odeslání.

**Ostatní:** `SendSignal` (SIGTERM/SIGKILL nejdřív zruší čtení výstupu), `Update` (resize PTY),
`SendInput`, `StreamInput` (client stream), `CloseStdin`, `List`.

## Filesystem služba

- `Stat`, `MakeDir` (s parenty, chown na uživatele), `Move`, `ListDir` (`depth`), `Remove`
- `EntryInfo`: name, type, path, size, mode, permissions string (`-rwxr-xr-x`), owner, group, mtime,
  `symlink_target`, `is_symlink`
- chyby: neexistuje → `NotFound`, existuje → `AlreadyExists`, není adresář → `InvalidArgument`
  (přesné texty převzít z upstream)
- `WatchDir` (stream): `StartEvent`, pak `FilesystemEvent{name (relativní), type, entry?}`, keepalive jako u procesů
- síťový mount (NFS/FUSE dle `statfs` magic) + `allow_network_mounts=false` → chyba jako upstream
- `CreateWatcher` / `GetWatcherEvents` (vyprázdní buffer) / `RemoveWatcher`: watcher pod UUID

## HTTP API a auth

**Auth layer** (tower, nad HTTP i Connect):
- `X-Access-Token` porovnaný v konstantním čase, jen pokud je token nastaven
- výjimky: `GET /health`, `GET /files`, `POST /files`, `POST /init`
- Connect uživatel: HTTP Basic auth username; bez něj default user z `/init`; neznámý → `Unauthenticated`

**Signing** (`/files`): `"v1_" + base64_raw_std(sha256("path:operation:username:token[:expiration]"))`,
`operation ∈ {read, write}`; token v hlavičce má přednost; expirace v unix sekundách.

**`POST /init`** (204):
- token se přijme, pokud odpovídá stávajícímu nebo MMDS hashi; jinak 401 (mismatch / reset not authorized)
- nastaví env vars, `defaultUser`, `defaultWorkdir`
- `timestamp` → `clock_settime` (jen `!isnotfc`)
- `hyperloopIP` → `/etc/hosts` záznam `events.e2b.local`
- `volumeMounts`, `caBundle`: parsovat, warning log, přeskočit (fáze 3)

**Files:**
- `GET /files`: stream, `Content-Disposition`, kódování dle `Accept-Encoding` (port upstream `encoding.go`)
- `POST /files`: multipart, víc souborů, vytvoří parenty, chown, přepíše existující; odpověď pole `EntryInfo` dle OpenAPI
- `POST /files/compose`: `copy_file_range` do cíle, pak smazat zdroje

**Ostatní:** `GET /envs`, `GET /health` (204, hlavičky jako upstream).

## Testování

- **Unit:** signing (vektory spočítané z Go implementace), env layering, path resolve,
  `/init` token validace s mock MMDS, mapování chyb, multiplex backpressure
- **Kontraktové:** `/init`, `/files`, auth výjimky proti běžící binárce (`tests/`)
- **SDK:** Docker image s Rust envd (`-isnotfc`), JS SDK testy commands/files/pty v debug režimu
  (přesný mechanismus debug režimu ověřit v SDK při plánování)

## Mimo rozsah fáze 1

cgroups (kromě respektování `-no-cgroups`), freeze/collapse/metrics, handover, NFS, CA bundle,
port forward, OOM, log exporter, legacy RPC.
