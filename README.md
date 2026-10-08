# e2b-runtime-rs

A self-hosted [E2B](https://e2b.dev) runtime written in Rust, aimed at bare-metal Kubernetes. The official E2B SDKs (JS, Python) work against it unchanged.

## Status

| Component | What it does | State |
|---|---|---|
| **envd** | Agent inside every sandbox: processes, PTY, files, watchers, `/files` upload/download | ✅ done, passes the official E2B JS SDK tests |
| **orchestrator** | Firecracker microVMs on a node: snapshot resume over `userfaultfd`, copy-on-write rootfs (ublk / NBD), pause with diffs to S3 | 🚧 cold boot, resume and pause work end to end in CI (M0–M7); gRPC server next ([spec](docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md)) |
| template build | OCI image → ext4 → snapshot | planned |
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
