# Backlog: gaps to Go parity

Each line is a place where this runtime still differs from upstream Go or where parity is not yet verified. Deliberate differences and features upstream does not have are in [new-features.md](new-features.md).

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `later` after that.

## envd

| Item | Target |
|---|---|
| Filesystem RPC internals beyond the spot checks in `scripts/parity/envd-diff.sh` are not compared with Go | API |
| hyper rejects a request with more than 100 headers with 431; Go has no count limit, only the 1 MiB size limit. A higher hyper limit allocates that many slots on every request | later |
| A 431 from hyper has an empty body and no `Content-Type`; Go writes `431 Request Header Fields Too Large` as a `text/plain; charset=utf-8` body | later |
| Connect unary answers are chunked; Go sends `Content-Length` when the answer fits its 2 KiB buffer. Connect error JSON keeps `<`, `>` and `&`, which Go writes as `<`, `>` and `&`. Both decode to the same values | later |
| A unary RPC that runs past a valid `Connect-Timeout-Ms` ends with connectrpc's `request timeout`; in Go a handler that ignores its context finishes and one that watches it fails with `context deadline exceeded` | later |
| A malformed `grpc-timeout` on the gRPC protocol is ignored; Go answers `invalid_argument` | later |
| `GET /files` copies through user space; Go serves a file with one `sendfile`, about 12 % faster on 2 CPUs | perf |
| 50 to 100 concurrent 1 MiB uploads peak at higher RSS than Go (the hyper read buffer grows up to 1 MiB per connection, the Go header limit, plus chunks in flight) | perf |

## Orchestrator: volumes over NFS

| Item | Target |
|---|---|
| A rename moves the handles under the old path to the new one; Go makes the renamed entry's handle stale, and handles inside a renamed directory keep their old paths | later |
| The portmapper answers 2049 for every program; Go answers 2049 only for NFS and mountd over TCP and 0 otherwise | later |
| `NFS_PROXY_LOG_LEVEL` (go-nfs's own log level) and `NFS_PROXY_DIR_VERIFIER_LIMIT` are not read, because nfsserve pages READDIR by the handle of the last entry instead of a cached listing | later |
| Call metrics, logs and spans use go-nfs's operation names but follow the Rust calls, so the count per NFS request differs (go-nfs adds `FS.Stat` checks and opens files once per read or write) | later |

## Orchestrator: shared chunk cache

| Item | Target |
|---|---|
| `SHARED_CHUNK_CACHE_PATH` and the `use-nfs-for-snapshots`, `use-nfs-for-templates` and `use-nfs-for-building-templates` flags (a chunk cache on a shared filestore) are not ported | perf |

## Orchestrator: pause and checkpoint

| Item | Target |
|---|---|
| No KVM test with multiple memory regions (guests above the MMIO gap) | API |
| No end-to-end test of the filesystem-only in-place checkpoint over gRPC (needs KVM and a cluster with `filesystem-only-checkpoint` on) | API |
| The memory in-place checkpoint is tested only with the synchronous copy. The deferred export needs a template built with `free-page-reporting` on, and no test builds one yet | perf |

## Orchestrator: network

| Item | Target |
|---|---|
| Slot links, addresses and routes use `ip` execs; upstream uses netlink in-process (only matters for pool refill speed) | perf |

## Orchestrator: telemetry and gRPC

| Item | Target |
|---|---|
| Delta counters and histograms keep up to 20 000 series per export interval; upstream lifts the cap entirely, but the Rust SDK preallocates its maps by the limit and rejects an unbounded one | later |
| The template cache has no residency gauges (`orchestrator.templates.cache.entries`, pinned refs, oldest pin age, mapping bytes, layer entries by kind and generation band, unlanded layers) and no `evictions` counter by reason | later |
| The gRPC server pings every 15 s like upstream, but hyper has no keepalive enforcement policy, so a client that pings more often than every 5 s is not sent `too_many_pings` | later |

## Orchestrator: peer chunk sharing

| Item | Target |
|---|---|
| A peer transition re-reads the diff from storage with the header it already holds; the chunker reads only uncompressed objects, so a build a Go peer uploads compressed is unreadable after the transition (upstream reloads the header and its frame table) | later |
| `snapshot-cache-drop-provisional-header` is not read and the `orchestrator.sandbox.snapshot.dead_structure_outcome` and `dead_structure_bytes` counters are not recorded; the provisional header stays in the template until the entry goes | later |
