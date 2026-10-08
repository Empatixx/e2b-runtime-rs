# e2b-runtime-rs

A self-hosted [E2B](https://e2b.dev) runtime written in Rust, aimed at bare-metal Kubernetes. The official E2B SDKs (JS, Python) work against it unchanged.

## Status

| Component | What it does | State |
|---|---|---|
| **envd** | Agent inside every sandbox: processes, PTY, files, watchers, `/files` upload/download | ✅ done, passes the official E2B JS SDK tests |
| **orchestrator** | Firecracker microVMs on a node: snapshot resume over `userfaultfd`, copy-on-write rootfs (ublk / NBD), pause with diffs to S3 | 🚧 cold boot, resume (~0.5 s with prefetch), pause, checkpoint and the gRPC `SandboxService` and `InfoService` work end to end in CI; egress firewall, Redis routing, events, ClickHouse stats at upstream parity ([parity](docs/PARITY.md), [spec](docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md)) |
| template build | OCI image → ext4 → snapshot (template-manager role of the orchestrator) | 🚧 `TemplateService`, build cache, the base phase (pull, ext4, provisioning, base layer) and the steps (RUN, ENV, WORKDIR, USER, COPY), finalize (configure, start and ready commands, CA bundle) and optimize (prefetch) done ([spec](docs/superpowers/specs/2026-10-17-template-builder-design.md)) |
| api | REST API for the SDK, node selection | planned |
| client-proxy | `https://<port>-<sandbox>.<domain>` routing | planned |
| Kubernetes deployment | Helm chart: orchestrator DaemonSet, API, proxy, S3, Postgres | planned |

Until the orchestrator lands, envd runs as a plain container and the SDK talks to it in debug mode (`E2B_DEBUG=true`).

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

Firecracker and the kernel are resolved per version as upstream: `FIRECRACKER_VERSIONS_DIR/<version>/<arch>/firecracker` (default `/fc-versions`) and `HOST_KERNELS_DIR/<version>/<arch>/vmlinux.bin` (default `/fc-kernels`); `E2B_FIRECRACKER` and `E2B_KERNEL` override them with fixed paths. The host must have `net.ipv4.ip_forward=1`; it is checked at startup only with `NETWORK_VERSION=2`, as upstream. Firecracker's output goes to the service log. It serves upstream's `SandboxService` (`/SandboxService/Create`, `Update`, `List`, `Delete`, `Pause`). Create resumes a build from template storage. Storage is configured as upstream: `TEMPLATE_STORAGE_URL` and `BUILD_CACHE_STORAGE_URL` take `gs://bucket`, `s3://bucket?endpoint=…&s3ForcePathStyle=true&region=…`, `azblob://container` or `file:///path`; without them the legacy `STORAGE_PROVIDER` (`GCPBucket` default, `AWSBucket`, `AzureBucket`, `Local`) with `TEMPLATE_BUCKET_NAME` / `BUILD_CACHE_BUCKET_NAME`, `LOCAL_TEMPLATE_STORAGE_BASE_PATH` (`/tmp/templates`), `LOCAL_BUILD_CACHE_STORAGE_BASE_PATH` (`/tmp/build-cache`) and `S3_USE_PATH_STYLE` apply. Credentials come from the provider's environment (`AWS_*`, `GOOGLE_*`, `AZURE_STORAGE_*`). With a local build cache the template manager signs layer uploads with an HMAC token and serves `PUT /upload` on the gRPC port (`LOCAL_UPLOAD_BASE_URL`, default `http://localhost:<GRPC_PORT>`). Expiry belongs to the API, as upstream; set `E2B_LOCAL_EXPIRY=1` to remove sandboxes at `end_time` without one. `NODE_ID` names the node (default `local`), `E2B_WORK_DIR` and `E2B_CACHE_DIR` hold per-sandbox files and fetched chunks, `E2B_TEMPLATE_CACHE_DIR` (default `$E2B_CACHE_DIR/template`) holds cached snapfiles. The node keeps 32 network slots and `NBD_POOL_SIZE` (default 64) NBD devices ready, as upstream. `ENVD_TIMEOUT_MS` (default 10000) bounds envd `/init` after a resume. The same port serves upstream's `InfoService`, gRPC health and `GET /health` (`{"status":"healthy","version":"<commit>"}`, 503 only when unhealthy). Create is admitted as upstream: at most `MAX_SANDBOXES_PER_NODE` (200) sandboxes and `MAX_STARTING_INSTANCES_PER_NODE` (3) starting at once, waiting up to 15 s. Pause returns once the snapshot is in the node's template cache; the upload to storage runs in the background (20 min per attempt, 2 h budget), and a resume of that build on the same node reads only local files. Cached diffs expire after 25 h or earlier when the cache disk passes `BUILD_CACHE_MAX_USAGE_PERCENTAGE` (85). A resume prefetches the pages recorded in the build's `metadata.json` trace (upstream's init prefetch: 16 fetch and 8 copy workers; `RESUME_PREFETCH_SOURCE=off` disables it), chunks stream into the cache so a fault waits only for its own pages, and Firecracker's guest memory is a memfd that pause copies dirty pages from (`USE_MEMFD=0` falls back to `process_vm_readv`). SIGTERM shuts down as upstream: status ShuttingDown, a 15 s pause unless `ENVIRONMENT=local`, then it waits for running template builds (plus 15 s for consumers to read their status), then until the API has removed every sandbox and every upload has finished; `FORCE_STOP=true` skips both waits. At startup the binary sweeps leftovers of a previous run (`ns-*` namespaces, `/sys/fs/cgroup/e2b/*`, the work dir), so it expects a dedicated node. `crates/orchestrator/tests/grpc_contract.rs` shows the whole lifecycle.

With `ORCHESTRATOR_SERVICES=template-manager` the node also builds templates as upstream's base phase: it pulls `fromImage` (Docker Hub through `DOCKERHUB_REMOTE_REPOSITORY_URL`/`_PROVIDER` when set; general, GCP or ECR credentials from the request), builds an ext4 (`mkfs.ext4`, `rsync`, `e2fsck`, `resize2fs`, `tune2fs` and `debugfs` must be installed; overlayfs needs kernel 6.8+), injects envd (`HOST_ENVD_PATH`, default `/fc-envd/envd`) and a static busybox (`HOST_BUSYBOX_DIR/BUSYBOX_VERSION/<arch>/busybox`, defaults `/fc-busybox`, `1.36.1`), provisions it in a busybox-init VM with internet access, boots it with systemd until envd answers, and pauses and uploads it as the base layer. Template steps then run through envd in a VM resumed from the previous layer (or cold-booted from a cached one, with envd updated), and each step is paused and uploaded as its own layer. Finalize cold-boots the last layer under the requested build id, runs the configuration script, the start command and the ready command (`sleep 20` by default when only a start command is set) and packs the CA cert bundle; optimize resumes the final template twice and records the pages both runs touched as the prefetch in `metadata.json`. Work files live in `TEMPLATES_DIR` (default `${ORCHESTRATOR_BASE_PATH}/build-templates`, base `/orchestrator`); new builds record `DEFAULT_KERNEL_VERSION` (`vmlinux-6.1.158`) and `DEFAULT_FIRECRACKER_VERSION` (`v1.14-0.2.0`). `crates/orchestrator/tests/template_contract.rs` builds one.

### SDK against a microVM (Linux, KVM)

On a Linux host with `/dev/kvm`, Docker and the `nbd` module loaded, run as a user with sudo:

```bash
sudo modprobe nbd nbds_max=16
scripts/ci/vm-assets.sh                    # Firecracker fork + guest kernel into /opt/e2b
scripts/ci/guest-rootfs.sh                 # Debian guest with envd under systemd -> /opt/e2b/rootfs.ext4
export E2B_FIRECRACKER=/opt/e2b/firecracker E2B_KERNEL=/opt/e2b/vmlinux E2B_ROOTFS=/opt/e2b/rootfs.ext4
scripts/ci/vm-sdk-tests.sh                 # egress check + SDK tests against a cold-booted and a resumed sandbox
```

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
