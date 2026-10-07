# Performance: envd-rs vs Go envd

Load test of envd-rs against the upstream Go envd (`e2b-dev/infra` commit `92197909`, `packages/envd`).

## Setup

- Both daemons run in the same Debian trixie image as `docker/Dockerfile`, started with `envd -isnotfc -verbose` and initialised with `POST /init {"defaultUser":"user"}`.
- Go is built like upstream `make build`: `CGO_ENABLED=0 go build -trimpath -ldflags "-s -w"`. Rust is `cargo build --release` (LTO, `codegen-units = 1`, stripped).
- Load comes from [oha](https://github.com/hatoo/oha) in a separate container on the same Docker network, 15 s per test. Each test starts a fresh container.
- Memory is the peak RSS of envd (`VmHWM` of PID 1). CPU is the container cgroup `cpu.stat` divided by the number of requests, so process start includes the CPU of `/bin/sh` and `/bin/true`.
- Docker Desktop on Apple Silicon. Numbers move by ±10 % between runs, differences below that are a tie.

| Test | Request |
|---|---|
| health | `GET /health` |
| stat | `POST /filesystem.Filesystem/Stat` on `/home/user` (Connect JSON) |
| listdir | `POST /filesystem.Filesystem/ListDir` on `/usr/bin`, depth 1 (~625 entries) |
| download | `GET /files` of a 1 MiB file |
| upload multipart | `POST /files`, 1 MiB `multipart/form-data`, 1000 rotating paths on tmpfs |
| upload raw | `POST /files`, 1 MiB `application/octet-stream`, 1000 rotating paths on tmpfs |
| process start | `POST /process.Process/Start` running `/bin/true` (Connect server stream) |

## 8 CPUs, 100 connections

| Test | Rust req/s | Go req/s | Rust p99 | Go p99 | Rust CPU/req | Go CPU/req | Rust RSS | Go RSS |
|---|---|---|---|---|---|---|---|---|
| health | **361 285** | 257 347 | **0.72 ms** | 2.34 ms | **15 µs** | 21 µs | **9 MB** | 20 MB |
| stat | **101 106** | 50 669 | **2.3 ms** | 15.7 ms | **74 µs** | 123 µs | **10 MB** | 60 MB |
| listdir | **1 012** | 466 | **417 ms** | 785 ms | **8.0 ms** | 16.2 ms | **105 MB** | 130 MB |
| download | 492 | 501 | **515 ms** | 1 016 ms | 16.4 ms | **15.7 ms** | **116 MB** | 141 MB |
| upload multipart | **10 409** | 7 080 | **25 ms** | 103 ms | **733 µs** | 908 µs | 59 MB | **23 MB** |
| upload raw | **13 687** | 9 076 | **19 ms** | 67 ms | **554 µs** | 622 µs | 32 MB | **24 MB** |
| process start | **6 057** | 5 450 | **47 ms** | 59 ms | **1.32 ms** | 1.47 ms | **51 MB** | 126 MB |

health and stat are limited by the load generator (4 CPUs): both servers used about 5.5 of their 8 cores.

## 2 CPUs, 50 connections

| Test | Rust req/s | Go req/s | Rust p99 | Go p99 | Rust RSS | Go RSS |
|---|---|---|---|---|---|---|
| health | **233 460** | 193 503 | **0.45 ms** | 0.87 ms | **6.7 MB** | 17 MB |
| stat | **49 892** | 25 371 | **2.1 ms** | 5.8 ms | **7.5 MB** | 25 MB |
| listdir | 233 | 253 | **587 ms** | 664 ms | **56 MB** | 67 MB |
| download | 172 | **195** | **506 ms** | 778 ms | **58 MB** | 64 MB |
| upload multipart | **5 084** | 3 643 | **17 ms** | 36 ms | 30 MB | **16 MB** |
| upload raw | 6 358 | **6 736** | **13 ms** | 20 ms | 17 MB | 17 MB |
| process start | ~1 700 | ~1 430–1 880 | 83–87 ms | 82–93 ms | **18 MB** | 42–50 MB |

listdir and process start are a tie within run-to-run noise.

## Idle

| | Rust | Go |
|---|---|---|
| Binary size | **6.3 MB** | 10.3 MB |
| RSS after `/init` | **~6 MB** | ~11 MB |
| Threads | 3 | 5 |

## What made the difference

| Change | Effect |
|---|---|
| `TCP_NODELAY` on every connection (Go sets it by default) | Streaming responses no longer wait 40 ms for a delayed ACK; process start p50 was a flat 41 ms before |
| Uploads write chunks straight into a `std::fs::File` instead of `tokio::fs` + `BufWriter` | No thread-pool hop per 8 KiB write; multipart upload 713 → 3 489 req/s on 2 CPUs |
| Request bodies flow as `Bytes` streams (no `AsyncRead` round trips) | Fewer copies; multipart upload 3 489 → 4 728 req/s on 2 CPUs |
| hyper `max_buf_size` 64 KiB (custom serve loop in `main.rs`, `axum::serve` cannot set it) | Upload RSS 137 MB → 30 MB and higher throughput |
| `OwnerNames` cache of uid/gid names within one ListDir | One `/etc/passwd` and `/etc/group` lookup per owner instead of per entry; ListDir 82 → 239 req/s on 2 CPUs |
| Spawn under per-thread credentials (`rustix::thread`) instead of `pre_exec` | std uses `posix_spawn` (vfork) instead of `fork`; one process start 0.83 → 0.59 ms (Go 0.71 ms) |
| Exited-process retention as a pid map plus an expiry queue | Start and exit were scanning ~120 000 retained exits under one lock; process start on 8 CPUs 4 341 → 6 057 req/s |

## Tried and dropped

- **mimalloc**: 5–10 % more req/s, but ListDir RSS grew to 454 MB.
- **jemalloc**: slower overall, download fell to 41 req/s.
- **Synchronous reads for download**: no change, the gap to Go is `sendfile`.
- **`spawn_blocking` around process spawn**: slower (1 971 → 1 365 req/s on 2 CPUs), the thread hop costs more than the vfork pause.

## Known gaps

- **Download**: Go serves files with a single `sendfile` per request; hyper copies through user space. Equal on 8 CPUs, about 12 % behind on 2 CPUs.
- **Upload memory under load**: with 50–100 concurrent 1 MiB uploads Rust peaks higher than Go (hyper buffers up to 64 KiB per connection plus in-flight chunks).
- **Header size**: the 64 KiB hyper buffer also caps request headers at 64 KiB; Go allows 1 MiB (`MaxHeaderBytes`).
