# Backlog: gaps to Go parity

Each line is a place where this runtime still differs from upstream Go or where parity is not yet verified. Deliberate differences and features upstream does not have are in [new-features.md](new-features.md).

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `later` after that.

## envd

| Item | Target |
|---|---|
| `GET /files` copies through user space; Go serves a file with one `sendfile`, about 12 % faster on 2 CPUs | perf |
| 50 to 100 concurrent 1 MiB uploads peak at higher RSS than Go (the hyper read buffer grows up to 1 MiB per connection, the Go header limit, plus chunks in flight) | perf |

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
| The template cache has no residency gauges (`orchestrator.templates.cache.entries`, pinned refs, oldest pin age, mapping bytes, layer entries by kind and generation band, unlanded layers) and no `evictions` counter by reason | later |

## Orchestrator: reading builds

| Item | Target |
|---|---|
| The chunker reads only uncompressed objects, so a build that a Go node uploaded compressed (`.zstd` or `.lz4` with a frame table in its header) cannot be read, neither from storage nor after a peer transition | API |
