# Backlog: deferred review findings

Every final review ends with minors and deferred rulings. They are collected here so they don't get lost.

- When an item is fixed, delete its line in the same commit.
- "Target" is when the item starts to matter: `API` before the node serves the real E2B API, `perf` with performance work, `template build` with the template builder, `any` whenever convenient, `later` after that.

## Orchestrator: cold boot (M5)

| Item | Target |
|---|---|
| udev/blkid probing a just-disconnected `/dev/nbdN` can still reset the next owner's size. The `nowatch` udev rule from `scripts/ci/nbd-host.sh` stops the inotify probes; the kernel's own change events are still probed | perf |

## Orchestrator: resume (M6)

| Item | Target |
|---|---|
| The uffd handshake socket is `0777` (upstream parity); `0600` would do, since Firecracker runs as root | API |

## Orchestrator: pause (M7)

| Item | Target |
|---|---|
| If write-protect is unavailable the resumed pause falls back to resident−empty (correct but large); surface it as a metric | API |
| No KVM test with multiple memory regions (guests above the MMIO gap) | API |

## Orchestrator: gRPC (M8)

| Item | Target |
|---|---|
| Local expiry is opt-in (`E2B_LOCAL_EXPIRY=1`); default leaves expiry and auto-pause to the API evictor like upstream | API |
| Startup sweep removes every `ns-<N>` netns and all of `/sys/fs/cgroup/e2b/*` and `E2B_WORK_DIR`; assumes a dedicated node | API |

## Orchestrator: fast resume (M9)

| Item | Matters from |
|---|---|
| Slot links, addresses and routes use `ip` execs; upstream uses netlink in-process (only matters for pool refill speed) | perf |
| NBD connects with ioctls and one socket; upstream uses generic netlink and can do multi-conn. A netlink port was measured and reverted, see [performance-resume.md](performance-resume.md#tried-and-dropped); the fixed port is kept outside the repository on the maintainer's machine | perf |

## Orchestrator: async pause (M11)

| Item | Matters from |
|---|---|
| A process that exits during forced shutdown before starting sandboxes finish leaves their slots and cgroups to the next startup sweep | API |

## Orchestrator: template builder (M13 to M16)

| Item | Matters from |
|---|---|

## Orchestrator: upstream sync (bfd917908 to 2a78eba4b)

| Item | Target |
|---|---|
| Delta counters and histograms keep up to 20 000 series per export interval; upstream lifts the cap entirely, but the Rust SDK preallocates its maps by the limit and rejects an unbounded one | later |
| No per-sandbox LaunchDarkly log routing (`logs-write-config`), mutual TLS, gRPC server credentials or connection age | later |

## Orchestrator: vCPU hotplug and filesystem boot

| Item | Target |
|---|---|
| The `resume-build` dev tool (`defaultRunVcpu`) has no Rust counterpart | later |

## Orchestrator: peer chunk sharing

| Item | Target |
|---|---|
| A peer transition re-reads the diff from storage with the header it already holds; the chunker reads only uncompressed objects, so a build a Go peer uploads compressed is unreadable after the transition (upstream reloads the header and its frame table) | later |
| Nothing produces a provisional memfile header: the pause exports and dedups the memfile before the template is served, so the durable header API (`Template::durable_memfile_header`) always sees the final header | perf |

## Orchestrator: volumes

| Item | Target |
|---|---|
| `Chrooted` has only what the volume service uses; upstream also has rename, remove, symlink, readlink, chtimes, temp files and the trailing-slash rules for them, which only the NFS proxy needs | NFS proxy |
| `chmod` goes through `/proc/self/fd` (upstream tries `fchmodat2` first); a host without procfs cannot chmod | later |
| `CreateVolume` reports the full volume path in a mkdir failure; Go reports the first path component that failed | later |

## Orchestrator: in-place checkpoint

| Item | Target |
|---|---|
| The memory in-place checkpoint is tested only with the synchronous copy. The deferred export needs a template built with `free-page-reporting` on, and no test builds one yet | perf |
| A snapshot has no lineage (`AbandonsPredecessor`) in the template cache | later |
| No end-to-end test of the filesystem-only in-place checkpoint over gRPC (needs KVM and a cluster with `filesystem-only-checkpoint` on) | API |

