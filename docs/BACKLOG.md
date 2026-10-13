# Backlog: gaps to Go parity

Each line is a place where this runtime still differs from upstream Go or where parity is not yet verified. Deliberate differences and features upstream does not have are in [new-features.md](new-features.md).

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `later` after that.

## envd

| Item | Target |
|---|---|
| A request target longer than 64 KiB gets 414 from hyper; Go accepts any target that fits in the 1 MiB header limit | later |
| A request with a transfer coding other than `chunked` gets Go's `400 Bad Request` page; Go answers `501 Not Implemented` with the body `Unsupported transfer encoding`. hyper does not say which parse error it hit | later |
| An `HTTP/1.2` request gets Go's `400 Bad Request` page; Go answers `505 HTTP Version Not Supported: unsupported protocol version` | later |
| `GET /files` copies through user space; Go serves a file with one `sendfile`, about 12 % faster on 2 CPUs | perf |
| 50 to 100 concurrent 1 MiB uploads peak at higher RSS than Go (the hyper read buffer grows up to 1 MiB per connection, the Go header limit, plus chunks in flight) | perf |

## Orchestrator: volumes over NFS

| Item | Target |
|---|---|
| `NFS_PROXY_DIR_VERIFIER_LIMIT` is not read, because nfsserve pages READDIR by the handle of the last entry instead of a cached listing | later |
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
| `snapshot-cache-drop-provisional-header` is not read and `dead_structure_outcome` and `dead_structure_bytes` are recorded only for `dedup_index`, not for `template_provisional_header` and `upload_provisional_header`. The template keeps its build, provisional memfile header included, until the entry goes, because every resume reads it from there | later |
