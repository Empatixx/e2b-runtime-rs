# e2b-runtime-rs

**The [E2B](https://e2b.dev) sandbox runtime, rewritten in Rust.** Firecracker microVMs that resume from a snapshot, run untrusted agent code and pause when the agent stops, with the same APIs, file formats and behaviour as the [Go original](https://github.com/e2b-dev/runtime). The official E2B SDKs (JS, Python) work against it unchanged.

**[Documentation](website/)** (English and Czech) | [Performance](docs/performance.md) | [Go parity](docs/PARITY.md) | [Known gaps](docs/BACKLOG.md)

## What is it?

[E2B Runtime](https://github.com/e2b-dev/runtime) is the open-source backend behind E2B: a control-plane API, an orchestrator on every node that drives Firecracker, `envd` inside every VM, an edge proxy and a template builder, all written in Go.

This project rewrites **the node side** of it in Rust: **envd** and the **orchestrator** (including the template-manager role). Everything at the edges stays exactly as in Go: routes, gRPC services, status codes, error texts, snapshot and header formats, metric names, feature flags. The rest of E2B (API, client proxy, dashboard) talks to a Rust node exactly as it talks to a Go one.

## Why it's fast

The design is upstream's; the implementation is Rust.

**A sandbox is a resumed snapshot.** Templates are pre-booted VMs (memory, disk and machine state) stored in object storage. Creating a sandbox restores one instead of booting a kernel. Memory pages are served lazily on page fault through `userfaultfd`, and the root filesystem is a copy-on-write overlay over a read-only image, so only the data a sandbox actually touches is fetched. Fresh creates and resumes after a pause take the same path.

**Less runtime under the hood.** No garbage collector, no goroutine scheduler: envd and the orchestrator run on tokio with small, predictable memory, and tail latencies stay low under load.

## Key numbers

envd, Rust vs Go, under load (8 CPUs, 100 connections; full method in [docs/performance.md](docs/performance.md) and the [blog post](website/content/blog/envd-rust-vs-go.mdx)):

| Test | Rust req/s | Go req/s | Rust p99 | Go p99 | Rust RSS | Go RSS |
|---|---|---|---|---|---|---|
| health | **361 285** | 257 347 | **0.72 ms** | 2.34 ms | **9 MB** | 20 MB |
| stat | **101 106** | 50 669 | **2.3 ms** | 15.7 ms | **10 MB** | 60 MB |
| list dir | **1 012** | 466 | **417 ms** | 785 ms | **105 MB** | 130 MB |
| download 1 MiB | 492 | 501 | **515 ms** | 1 016 ms | **116 MB** | 141 MB |
| upload 1 MiB multipart | **10 409** | 7 080 | **25 ms** | 103 ms | 59 MB | **23 MB** |
| upload 1 MiB raw | **13 687** | 9 076 | **19 ms** | 67 ms | 32 MB | **24 MB** |
| process start | **6 057** | 5 450 | **47 ms** | 59 ms | **51 MB** | 126 MB |

| envd idle | Rust | Go |
|---|---|---|
| binary size | **6.3 MB** | 10.3 MB |
| RSS after `/init` | **~6 MB** | ~11 MB |
| threads | 3 | 5 |

- **p99 latency is lower in all seven tests**, often several times lower (stat 2.3 vs 15.7 ms, multipart upload 25 vs 103 ms).
- **Throughput is higher or equal in all seven** on 8 CPUs; CPU per request is lower everywhere except download (Go uses `sendfile`).
- **SDK compatibility:** the official E2B JS SDK sandbox tests pass against envd-rs (105 passed, 9 skipped by the SDK itself in debug mode).
- **Sandbox start:** the target is upstream's ~150 ms create/resume on a bare-metal node. CI runners (nested virtualisation) measure 0.8 to 2.4 s for a resume; bare-metal numbers will be published here.

## What you get

- **Hardware-isolated sandboxes.** One Firecracker microVM per sandbox, in its own cgroup and network namespace, with a per-sandbox nftables egress firewall and SNI/Host-inspecting domain allow and deny lists.
- **Pause, resume and checkpoint.** Pause diffs memory and disk against the parent build and uploads the diff in the background; a resume on the same node reads only local files.
- **Templates from your recipe.** Layered builds from Docker images and steps (RUN, ENV, WORKDIR, USER, COPY), each layer hashed and cached, with an optimize pass that records which pages a resume touches.
- **envd inside every VM.** Processes, PTYs, filesystem, watchers, uploads and port forwarding over Connect RPC and REST, live-upgradable without dropping processes.
- **Upstream's operations story.** LaunchDarkly feature flags, OpenTelemetry metrics, traces and logs, ClickHouse events and host stats, Redis routing, GCS/S3/Azure storage with the same credential chains as the Go SDKs.

## Status

| Component | What it does | State |
|---|---|---|
| **envd** | Agent inside every sandbox: processes, PTY, files, watchers, `/files` upload/download | Done, passes the official E2B JS SDK tests |
| **orchestrator** | Firecracker microVMs on a node: snapshot resume over `userfaultfd`, copy-on-write rootfs over NBD, pause with diffs to object storage | Done: create, resume, pause, checkpoint, `SandboxService`, `InfoService`, egress firewall, routing, events and stats at upstream parity. In progress (wave 2): NBD/network over netlink, in-place pause, huge pages, `VolumeService`/`ChunkService` |
| **template build** | Builds snapshots from an OCI image and steps (template-manager role) | Done: base, steps, finalize and optimize phases with the layer cache |
| api, client-proxy | REST API for the SDK, `https://<port>-<sandbox>.<domain>` routing | not part of this project: use upstream's Go services |

## How it fits together

```
SDK / CLI ──REST──▶ API (Go, upstream) ──gRPC──▶ orchestrator (Rust) ──▶ Firecracker microVM ──▶ envd (Rust) ──▶ your processes
                                                     │
                                                     ├── object storage (templates, snapshots)
                                                     ├── Redis (routing, events)
                                                     └── ClickHouse (events, host stats)
```

## Quick start (docker compose)

```bash
docker compose up -d --build
```

This starts three services:

- `envd` on `localhost:49983`.
- `envd-init`, a one-shot `POST /init` that sets the default user `user` and workdir `/home/user`, as the orchestrator would.
- `s3`, a [RustFS](https://github.com/rustfs/rustfs) S3-compatible store on `localhost:9000` (credentials `minio` / `minio123`), used by the orchestrator.

Try it with the official SDK:

```bash
cd examples/sdk-playground
npm install
npm run demo
```

Or interactively:

```bash
E2B_DEBUG=true node
> const { Sandbox } = await import('e2b')
> const sandbox = await Sandbox.create()
> (await sandbox.commands.run('uname -a')).stdout
```

Stop and remove everything with `docker compose down -v`.

## Kubernetes / k3s

`deploy/k8s/` is a kustomize base with the same services: an envd Deployment and Service, the init Job, and a RustFS StatefulSet and Service, all in namespace `e2b-runtime`. It is meant for trying envd on a cluster. The real runtime deploys the orchestrator as a privileged DaemonSet on KVM nodes, which is not part of the base yet.

**k3s locally (k3d):**

```bash
docker build -f docker/Dockerfile -t e2b-runtime-rs/envd:dev .
k3d cluster create e2b-dev --no-lb --wait --kubeconfig-update-default=false
export KUBECONFIG=$(k3d kubeconfig write e2b-dev)
k3d image import e2b-runtime-rs/envd:dev -c e2b-dev
kubectl apply -k deploy/k8s
kubectl -n e2b-runtime port-forward svc/envd 49983:49983
```

**Any Kubernetes cluster:** push the image to your registry, point `images:` in `deploy/k8s/kustomization.yaml` at it, then `kubectl apply -k deploy/k8s`. Check `kubectl config current-context` first.

The manifests are validated with `kubectl kustomize` but have not been run on a cluster yet. The compose setup has been run end to end with the SDK demo.

## Documentation

`website/` is a [Fumadocs](https://fumadocs.dev) site that explains the concepts, envd and the orchestrator, plus a blog, in English and Czech: `cd website && npm install && npm run dev`.

## Development

```bash
cargo test --workspace
scripts/test-linux.sh
scripts/sdk-tests.sh
```

- `cargo test --workspace` runs on macOS: unit and contract tests, minus the Linux-only ones.
- `scripts/test-linux.sh` runs the full suite in a Linux container (PTY, inotify, setuid).
- `scripts/sdk-tests.sh` runs the official E2B JS SDK tests against envd in Docker.

### Orchestrator gRPC server (Linux, KVM, root)

```bash
TEMPLATE_STORAGE_URL=file:///srv/e2b/builds GRPC_PORT=5008 \
sudo -E cargo run --release -p orchestrator
```

Firecracker and the kernel are resolved per version as upstream: `FIRECRACKER_VERSIONS_DIR/<version>/<arch>/firecracker` (default `/fc-versions`) and `HOST_KERNELS_DIR/<version>/<arch>/vmlinux.bin` (default `/fc-kernels`); `E2B_FIRECRACKER` and `E2B_KERNEL` override them with fixed paths. The host must have `net.ipv4.ip_forward=1`; it is checked at startup only with `NETWORK_VERSION=2`, as upstream. Firecracker's output goes to the service log. It serves upstream's `SandboxService` (`/SandboxService/Create`, `Update`, `List`, `Delete`, `Pause`). Create resumes a build from template storage. Storage is configured as upstream: `TEMPLATE_STORAGE_URL` and `BUILD_CACHE_STORAGE_URL` take `gs://bucket`, `s3://bucket?endpoint=…&s3ForcePathStyle=true&region=…`, `azblob://container` or `file:///path`; without them the legacy `STORAGE_PROVIDER` (`GCPBucket` default, `AWSBucket`, `AzureBucket`, `Local`) with `TEMPLATE_BUCKET_NAME` / `BUILD_CACHE_BUCKET_NAME`, `LOCAL_TEMPLATE_STORAGE_BASE_PATH` (`/tmp/templates`), `LOCAL_BUILD_CACHE_STORAGE_BASE_PATH` (`/tmp/build-cache`) and `S3_USE_PATH_STYLE` apply. Credentials are resolved like upstream's SDKs: GCS through Application Default Credentials (`GOOGLE_APPLICATION_CREDENTIALS`, the gcloud well-known file, external accounts, impersonation, the GCE metadata server) with `GOOGLE_SERVICE_ACCOUNT_BASE64` signing uploads, S3 and ECR through the AWS default chain (env, profiles, SSO, web identity, ECS and EC2 roles), Azure through `AZURE_STORAGE_*` or DefaultAzureCredential (`AZURE_CLIENT_ID`/`AZURE_TENANT_ID` with a secret, certificate or username/password, workload identity, managed identity, Azure CLI). With a local build cache the template manager signs layer uploads with an HMAC token and serves `PUT /upload` on the gRPC port (`LOCAL_UPLOAD_BASE_URL`, default `http://localhost:<GRPC_PORT>`). Expiry belongs to the API, as upstream; set `E2B_LOCAL_EXPIRY=1` to remove sandboxes at `end_time` without one. `NODE_ID` names the node (default `local`), `E2B_WORK_DIR` and `E2B_CACHE_DIR` hold per-sandbox files and fetched chunks, `E2B_TEMPLATE_CACHE_DIR` (default `$E2B_CACHE_DIR/template`) holds cached snapfiles. The node keeps 32 network slots and `NBD_POOL_SIZE` (default 64) NBD devices ready, as upstream. The LaunchDarkly flag `envd-timeout-milliseconds` bounds envd `/init` after a resume (fallback: `ENVD_TIMEOUT` as a Go duration, else 10 s). The same port serves upstream's `InfoService`, gRPC health and `GET /health` (`{"status":"healthy","version":"<commit>"}`, 503 only when unhealthy). Create is admitted as upstream: at most `max-sandboxes-per-node` (200) sandboxes and `max-starting-instances-per-node` (3) starting at once, waiting up to 15 s; both flags are re-read every 30 s. Pause returns once the snapshot is in the node's template cache; the upload to storage runs in the background (20 min per attempt, 2 h budget), and a resume of that build on the same node reads only local files. With the `peer-to-peer-chunk-transfer` flag, Redis and a `NODE_IP` other than `localhost`, a node that needs a build another node still uploads reads its snapfile, headers and chunks from that node over `ChunkService` on the gRPC port, and switches to storage when the upload lands; `peer-to-peer-async-checkpoint` lets a checkpoint return before its upload ends. Cached diffs expire after 25 h or earlier when the cache disk passes the `build-cache-max-usage-percentage` flag (85). A resume prefetches the pages recorded in the build's `metadata.json` trace (the `resume-prefetch-source` flag picks `init`, `last-cycle`, `both` or `off`; worker counts come from the `memory-prefetch-*` flags; `resume-prefetch-harvest` records a fresh trace after each pause with a throwaway resume), chunks stream into the cache so a fault waits only for its own pages, and Firecracker's guest memory is a memfd that pause copies dirty pages from (the `use-memfd` flag off falls back to `process_vm_readv`). SIGTERM shuts down as upstream: status ShuttingDown, a 15 s pause unless `ENVIRONMENT=local`, then it waits for running template builds (plus 15 s for consumers to read their status), then until the API has removed every sandbox and every upload has finished; `FORCE_STOP=true` skips both waits. At startup the binary sweeps leftovers of a previous run (`ns-*` namespaces, `/sys/fs/cgroup/e2b/*`, the work dir), so it expects a dedicated node. `crates/orchestrator/tests/grpc_contract.rs` shows the whole lifecycle. Feature flags come from LaunchDarkly with `LAUNCH_DARKLY_API_KEY`; without it every flag uses upstream's offline fallback. `OTEL_COLLECTOR_GRPC_ENDPOINT` enables OTLP metrics, traces and logs (gzip; the service name joins `ORCHESTRATOR_SERVICES` with underscores, as upstream); `E2B_DEBUG=true` exports debug logs too. `DOMAIN_NAME` and `INSTANCE_GROUP_NAME` go into the flag contexts as upstream.

With `ORCHESTRATOR_SERVICES=template-manager` the node also builds templates as upstream's base phase: it pulls `fromImage` (Docker Hub through `DOCKERHUB_REMOTE_REPOSITORY_URL`/`_PROVIDER` when set, `AZURE_ACR` exchanging a DefaultAzureCredential token; general, GCP or ECR credentials from the request), builds an ext4 (`mkfs.ext4`, `rsync`, `e2fsck`, `resize2fs`, `tune2fs` and `debugfs` must be installed; overlayfs needs kernel 6.8+), injects envd (`HOST_ENVD_PATH`, default `/fc-envd/envd`; a static musl build, as upstream's, so it runs in any image) and a static busybox (`HOST_BUSYBOX_DIR/BUSYBOX_VERSION/<arch>/busybox`, defaults `/fc-busybox`, `1.36.1`), provisions it in a busybox-init VM with internet access, boots it with systemd until envd answers, and pauses and uploads it as the base layer. Template steps then run through envd in a VM resumed from the previous layer (or cold-booted from a cached one, with envd updated), and each step is paused and uploaded as its own layer. Finalize cold-boots the last layer under the requested build id, runs the configuration script, the start command and the ready command (`sleep 20` by default when only a start command is set) and packs the CA cert bundle; optimize resumes the final template twice and records the pages both runs touched as the prefetch in `metadata.json`. Work files live in `TEMPLATES_DIR` (default `${ORCHESTRATOR_BASE_PATH}/build-templates`, base `/orchestrator`); new builds record `DEFAULT_KERNEL_VERSION` (`vmlinux-6.1.158`) and `DEFAULT_FIRECRACKER_VERSION` (`v1.14-0.2.0`). `crates/orchestrator/tests/template_contract.rs` builds one.

### SDK against a microVM (Linux, KVM)

On a Linux host with `/dev/kvm`, Docker and the `nbd` module available, run as a user with sudo:

```bash
scripts/ci/nbd-host.sh                     # udev nowatch rule for /dev/nbd*, then modprobe nbd nbds_max=128
scripts/ci/vm-assets.sh                    # Firecracker fork + guest kernel into /opt/e2b
scripts/ci/guest-rootfs.sh                 # Debian guest with envd under systemd -> /opt/e2b/rootfs.ext4
export E2B_FIRECRACKER=/opt/e2b/firecracker E2B_KERNEL=/opt/e2b/vmlinux E2B_ROOTFS=/opt/e2b/rootfs.ext4
scripts/ci/vm-sdk-tests.sh                 # egress check + SDK tests against a cold-booted and a resumed sandbox
```

`scripts/ci/nbd-host.sh` installs the udev rule upstream requires on every node (`ACTION=="add|change", KERNEL=="nbd*", OPTIONS:="nowatch"` in `/etc/udev/rules.d/97-nbd-device.rules`). Without it systemd-udevd watches each NBD device with inotify and probes it with blkid whenever it is closed; the probe races the next connect and disconnect, and requests can get stuck ([LKML](https://lore.kernel.org/lkml/20220422054224.19527-1-matthew.ruffell@canonical.com/)). Production nodes also need enough devices: one per running sandbox plus the `NBD_POOL_SIZE` warm pool.

`cargo run -p orchestrator --example sandbox` (as root) boots one sandbox and forwards `127.0.0.1:49983` to envd in the VM until Ctrl-C, so `examples/sdk-playground` works against it. With `E2B_RESUME=1` it cold-boots, snapshots, and serves a sandbox resumed from that snapshot over userfaultfd.

**Firecracker needs Linux with `/dev/kvm`, so it does not run on macOS.** Everything that boots VMs is tested in GitHub Actions (`.github/workflows/ci.yml`, job `kvm-spike` and later integration jobs) and on bare-metal nodes with host kernel 6.12+.

Repository layout:

```
Cargo.toml             workspace (members = crates/*)
crates/envd/           envd: src/, tests/, build.rs, reference/ (upstream OpenAPI)
crates/orchestrator/   orchestrator
proto/envd/            envd protobuf contracts (from upstream E2B)
docker/                envd image; guest.Dockerfile + envd.service for the test microVM rootfs
deploy/k8s/            kustomize base
docs/superpowers/      specs and implementation plans
```

Code style rules live in [CLAUDE.md](CLAUDE.md): readability first, small methods, method chains, builders, no comments.
