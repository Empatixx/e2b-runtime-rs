# e2b-runtime-rs — orchestrator design

## Goal

Self-hosted E2B on bare-metal Kubernetes, written entirely in Rust. The official E2B SDKs (JS, Python) work unchanged against the cluster: sandbox lifecycle, pause/resume and template builds.

Success means the E2B SDK test suites for sandbox, commands, files, pty, pause/resume and template build pass against the cluster, and a sandbox starts from a template snapshot in well under a second.

## Programme

The runtime is split into sub-projects. Each gets its own spec, plan and implementation.

| # | Sub-project | Upstream (Go) | State |
|---|---|---|---|
| 1 | envd — agent inside the VM | `packages/envd` | done (PR #1) |
| 2 | orchestrator — Firecracker VMs on one node | `packages/orchestrator` | this spec |
| 3 | template build — OCI image → snapshot | `orchestrator/pkg/template` | part of the orchestrator binary, own spec later |
| 4 | api — REST API for the SDK, node selection, Postgres | `packages/api` | own spec later |
| 5 | client-proxy — `<port>-<id>.<domain>` routing | `packages/client-proxy` | own spec later |
| 6 | Kubernetes deployment — Helm, DaemonSet, MinIO, Postgres | Nomad jobs | own spec later |

Kubernetes runs only long-lived services: API and proxy as Deployments, the orchestrator as a privileged DaemonSet with `/dev/kvm`. Sandboxes are never Pods. The API picks a node in memory and calls the orchestrator over gRPC, and the orchestrator resumes a Firecracker snapshot in tens of milliseconds. Pod scheduling latency never touches sandbox start.

## Decisions

- **Faithful to upstream.** Memory is served lazily over `userfaultfd` from storage chunks, the root filesystem is a copy-on-write overlay over the template, and pause uploads only memory and disk diffs.
- **gRPC contract** is upstream `orchestrator.proto` (`SandboxService`), unchanged, served with `tonic`.
- **Storage format** is upstream header V3 (64-byte metadata, `BuildMap` entries) with files `memfile`, `memfile.header`, `rootfs.ext4`, `rootfs.ext4.header`, `snapfile`, `metadata.json` per build. Compression and header V4/V5 come later.
- **Firecracker** is the E2B fork, which adds `GET /memory/dirty`, `GET /memory/mappings` and memfd memory. Binaries come from `github.com/e2b-dev/fc-versions` (pinned `v1.14.1_*`). Guest kernels come from `github.com/e2b-dev/fc-kernels` (default `vmlinux-6.1.158`).
- **Host kernel** on our nodes is 6.12 LTS or newer.
- **Block device exposure** is separate from the block layer. The block layer only knows `read_at` / `write_at` on an overlay. **ublk** (`libublk`, io_uring, ~10 ms attach) is the primary way to expose it to Firecracker. **NBD** (kernel nbd over generic netlink, ~300 ms attach) is a fallback for hosts without `ublk_drv`.
- **Object storage** is S3-compatible (MinIO on the cluster) through `object_store`, with a local filesystem backend for tests.
- **Code style** follows the envd crate: crates.io module layout, no comments, method chains, builders, named fields.

## Repository layout

```
Cargo.toml                    [workspace] members = ["crates/*"]
crates/envd/                  existing envd crate (moved from the root)
crates/orchestrator/          new crate, one binary
proto/envd/                   existing
proto/orchestrator/           orchestrator.proto, info.proto (upstream, unchanged)
firecracker/firecracker.yml   E2B fork swagger, source of the API types
```

The envd move to `crates/envd/` happens after the in-flight `phase1` work lands on `main`.

```
crates/orchestrator/src/
  main.rs, config.rs
  server.rs + server/            gRPC SandboxService
  sandbox.rs + sandbox/          lifecycle: resume, cold boot, pause, sandbox map
  firecracker.rs + firecracker/  process launch, API client over the unix socket
  memory.rs + memory/            uffd handshake, page-fault serving, prefetch
  block.rs + block/              cache + dirty tracker, chunker, overlay, build mapping
  exposure.rs + exposure/        ublk (primary), nbd (fallback), device pool
  storage.rs + storage/          object_store backends, paths, header V3, diff metadata
  network.rs + network/          slots: netns, veth, tap, iptables, slot pool
  cgroup.rs                      cgroup v2 per sandbox
  template.rs                    node-local template cache
  envd.rs                        POST /init and health checks into the VM
```

## How a sandbox runs

**Create (resume from a template snapshot)** starts these in parallel:

- a uffd listener for the template memfile;
- a network slot from the pool;
- the rootfs overlay exposed as `/dev/ublkbN`, or as `/dev/nbdN` on the fallback.

Then:

1. Create the cgroup.
2. Start Firecracker inside the slot's netns.
3. `PUT /snapshot/load` with `mem_backend: Uffd`.
4. Serve page faults with `UFFDIO_COPY` from 4 MiB chunks fetched by range and cached locally.
5. `PATCH /vm Resumed`, then `PUT /mmds`.
6. envd `POST /init`.

**Pause** does the following:

1. `PATCH /vm Paused`, then `PUT /snapshot/create` (writes the snapfile).
2. Read the dirty pages from `GET /memory/dirty` and the dirty disk blocks from the overlay tracker.
3. Write the memory and rootfs diffs, and merge the new headers over the parent headers.
4. Upload all build files in parallel and register the build in the node-local cache.

**Resume** is the create path pointed at the paused build.

**Cold boot** is only used for template builds. It uses a plain rootfs file and `PUT /boot-source` … `InstanceStart`.

## Network

Each sandbox has a slot:

- a netns `ns-N` and a veth pair on a /31 from 10.12.0.0/16;
- a tap `tap0` in the netns at 169.254.0.22, with the guest always at 169.254.0.21;
- NAT to the unique host IP 10.11.0.N, a host route and MASQUERADE out.

envd answers on `10.11.0.N:49983`. Slots are pre-created in a pool. Egress firewalling (nftables sets, SNI inspection) is out of the first version.

## Milestones

| # | Milestone | Upstream packages | Size | Proof in CI (GitHub Actions, KVM) |
|---|---|---|---|---|
| 0 | Workspace, CI with KVM, spike: `ublk_drv` and E2B FC fork on the runner | — | small | runner boots the FC fork; ublk device attaches |
| 1 | Storage and header V3: paths, local FS + S3, header encode/decode, mapping merge, diff metadata | `shared/pkg/storage` | ~2.5k | unit tests, MinIO container |
| 2 | Block layer: mmap cache + roaring tracker, chunker (4 MiB range fetch), build file mapping, overlay | `sandbox/block`, `sandbox/build`, template cache | ~3.5k | unit tests |
| 3 | Exposure: ublk device backed by the overlay, device pool | (new; upstream `nbd`) | ~1.5k | ext4 on `/dev/ublkbN` mounts and round-trips data |
| 4 | Host plumbing: network slot pool, cgroup v2, Firecracker process and API client | `sandbox/network`, `sandbox/cgroup`, `sandbox/fc` | ~3k | VM network reachable from the host |
| 5 | Cold boot: VM from a hand-made ext4 with envd-rs, `/init` | `CreateSandbox`, `rootfs/direct`, `envd.go` | ~1.5k | SDK `commands.run` against a real VM |
| 6 | Resume: uffd handshake and serve loop, snapshot load, prefetch | `sandbox/uffd`, `ResumeSandbox` | ~3k | sandbox from snapshot, start latency measured |
| 7 | Pause: dirty bitmap, memory and rootfs export, headers, upload | `Sandbox.Pause`, `fc/memory`, `build_upload*` | ~2.5k | pause → resume keeps files and processes |
| 8 | gRPC server: `SandboxService` Create/Delete/Pause/List/Update, sandbox map, timeouts | `server` | ~2.5k | gRPC lifecycle end to end |
| 9 | NBD fallback: generic netlink connect, protocol dispatch, device pool | `sandbox/nbd`, `rootfs/nbd` | ~1.5k | milestone 3 test passes on NBD |

Template build (OCI → ext4 → provision boot → layers → snapshot), the edge proxy and the API follow as their own specs.

## Testing

- Unit tests sit next to the code. They cover the header format, mapping merge, cache/tracker, chunker, slot math and API types.
- Integration tests run on GitHub Actions Linux runners with `/dev/kvm`, as root. They use real Firecracker (E2B fork), real network namespaces and a MinIO container.
- From milestone 5 on, the existing envd SDK playground and SDK tests run against a real VM.

## Out of scope for this spec

NFS chunk cache, peer-to-peer chunk routing, volumes, hyperloop server, egress firewall and SNI proxy, compression and header V4/V5, memfile dedup, balloon and free-page reporting, reclaim, OOM handling, live envd upgrade, in-place checkpoint, filesystem-only boot, optimize/prefetch recording, metrics and events, multi-tenant teams.

## Risks

- **The ublk and tokio runtime models differ.** `libublk` brings its own io_uring executor, so ublk queues run on dedicated threads that hand work to the shared block layer.
- **GitHub runners may lack `ublk_drv` or the needed kernel.** Milestone 0 checks this first. If ublk is missing, CI uses the NBD fallback while nodes use ublk.
- **We depend on the E2B Firecracker fork.** We pin a release and keep the swagger file in the repo so API changes show up in review.
- **There is much `unsafe` systems code** (uffd copy, mmap, ioctl). It stays in small functions inside `memory/`, `block/` and `exposure/`, each with a focused test.
