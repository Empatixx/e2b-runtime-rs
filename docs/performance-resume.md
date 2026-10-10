# Performance: sandbox resume, Rust orchestrator vs Go

Resume latency of the Rust orchestrator against the upstream Go orchestrator (`e2b-dev/runtime`, measured on 2026-10-10), on the same node and the same template.

## Setup

- Node: Azure `Standard_E4ds_v4`, 4 vCPUs, 32 GB RAM, Ubuntu 24.04, host kernel 6.17. It is a nested KVM guest, not bare metal.
- Host setup as upstream requires: the udev `nowatch` rule for `nbd*`, `nbd nbds_max=128`, 8192 hugepages (16 GiB).
- Firecracker `v1.14-0.2.0` (E2B fork), guest kernel `vmlinux-6.1.158`.
- Template: `e2bdev/base`, built by Go `create-build` with its optimize phase (a prefetch mapping of 71 blocks of 2 MiB). Two sizes: 1 GiB and 512 MiB of guest RAM, 2 vCPUs.
- Rust: `crates/orchestrator/examples/resume_bench.rs`. It starts the real node and gRPC server, calls `SandboxService/Create` with `snapshot: true`, deletes the sandbox, and waits until Firecracker and the NBD device are gone before the next wave.
- Go: `packages/orchestrator/cmd/resume-build -iterations N`, which calls the sandbox factory directly. A parallel mode (waves of N goroutines) was added for this test only.
- A run is the time from the start of the resume to envd answering `/init`. The first run of every set fills the caches and is left out. P99 comes from 40 to 48 runs, so it is close to the maximum.

## 1 GiB template

| Parallel | Implementation | Runs | min | p50 | p90 | p99 | max | Failed |
|---|---|---|---|---|---|---|---|---|
| 1 | Rust | 40 | 33 | 51 | 58 | 63 | 63 | 0 |
| 1 | Go | 40 | 40 | 321 | 336 | 347 | 347 | 0 |
| 4 | Rust | 48 | 58 | 121 | 223 | 661 | 661 | 0 |
| 4 | Go | 48 | 108 | 430 | 606 | 746 | 746 | 0 |
| 8 | Rust | 48 | 58 | 277 | 713 | 878 | 878 | 0 |
| 8 | Go | 48 | 338 | 747 | 1362 | 1956 | 1956 | 0 |

All times in milliseconds.

## 512 MiB template

| Parallel | Implementation | Runs | min | p50 | p90 | p99 | max | Failed |
|---|---|---|---|---|---|---|---|---|
| 1 | Rust | 40 | 38 | 57 | 74 | 121 | 121 | 0 |
| 1 | Go | 40 | 43 | 328 | 340 | 360 | 360 | 0 |
| 4 | Rust | 48 | 84 | 142 | 239 | 597 | 597 | 0 |
| 4 | Go | 48 | 103 | 417 | 732 | 1123 | 1123 | 0 |
| 8 | Rust | 48 | 59 | 323 | 479 | 714 | 714 | 0 |
| 8 | Go | 48 | 319 | 717 | 1213 | 2169 | 2169 | 0 |

Guest RAM size barely changes the result, because memory is loaded on demand and the prefetch covers the same 71 blocks.

## Where the time goes (Rust, one sandbox)

| Step | Time |
|---|---|
| Firecracker spawn and NBD exposure, in parallel | 13 to 23 ms |
| Snapshot load, with prefetch copies running | 6 to 16 ms |
| vCPU resume and MMDS | 1 to 8 ms |
| envd `/init` | 6 to 28 ms |

Most Go runs spend about 300 ms in `nbdnl.Connect` before the snapshot load starts.

## What made the difference

- **2 MiB huge pages** for build sandboxes, like Go: about 60 page faults per resume instead of about 7000.
- **The chunk cache is read through an mmap**, like Go's `Cache.Slice`. Faults and prefetch install pages straight from the mapping instead of copying each 2 MiB block up to five times.
- **Prefetch copies start at the userfaultfd handshake**, while the snapshot loads, like Go's copy workers waiting on `uffd.Ready()`.
- **Firecracker is spawned with `clone3(CLONE_INTO_CGROUP)`**, like Go's `UseCgroupFD`. Moving the process into its cgroup through `cgroup.procs` cost 3 to 7 ms and sometimes 100 to 250 ms.
- **`TCP_NODELAY` on the gRPC listener**, as Go sets on every connection. Without it every `Create` reply waited about 40 ms for a delayed ACK.
- **The udev `nowatch` rule** that upstream requires on its nodes.

## Tried and dropped

- **NBD over generic netlink**, like Go: a connect right after a device is released waits 100 to 250 ms in the kernel (`blk_mq_freeze_queue_wait` inside `nbd_add_socket`). Go shows the same wait in its slow runs. The ioctl connect takes about 17 ms, so Rust keeps it.

## Known gaps

- The node is a nested KVM guest with 4 vCPUs. Bare metal should be faster for both.
- With 8 parallel resumes most of the Rust tail is the admission queue: the node starts at most `max-starting-instances-per-node` (3, as upstream) sandboxes at once, and Go's `resume-build` skips that queue. The resume itself takes p50 111 ms and p90 194 ms at 8 in parallel. `BENCH_MAX_STARTING` lifts the limit in the benchmark; on 4 vCPUs that does not help, because 8 microVMs then compete for the CPUs.
- Go is measured without its gRPC layer, Rust with it, so Rust carries a small extra cost.
- Go's resume maps the whole guest memory from hugepages up front. With 4 GiB of hugepages, 8 parallel 1 GiB sandboxes failed in 15 of 48 Go runs; Rust did not fail.
