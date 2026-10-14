# e2b-runtime-rs

**The node side of the [E2B](https://e2b.dev) sandbox runtime, rewritten in Rust.** Firecracker microVMs that resume from a snapshot, run untrusted agent code and pause when the agent stops. Same APIs, file formats and behaviour as the [Go original](https://github.com/e2b-dev/runtime), so the official E2B SDKs work unchanged.

[Documentation](https://empatixx.github.io/e2b-runtime-rs/) | [Blog](https://empatixx.github.io/e2b-runtime-rs/en/blog/) | [Deploying](deploy/README.md) | [Known gaps](docs/BACKLOG.md) | [New features](docs/new-features.md)

## What it is

- **envd** runs inside every sandbox: processes, PTYs, files, watchers, uploads and port forwarding.
- **The orchestrator** runs on every node: it resumes Firecracker microVMs from snapshots, pauses them with memory and disk diffs, and builds templates (the template-manager role).
- The rest of E2B (API, client proxy, dashboard) stays upstream's Go and talks to a Rust node the same way it talks to a Go one.
- **[e2b-runtime-simple](e2b-runtime-simple/README.md)** is a separate, much smaller runtime for the same SDK. One binary per node, disks and memory as OverlayBD layers over ublk, and S3 as the only thing nodes share. It does not follow the Go internals.

## Differences from E2B Runtime

| | E2B Runtime | e2b-runtime-rs |
|---|---|---|
| Language | Go | Rust (tokio, axum, tonic) |
| Scope | the whole stack | envd and the orchestrator; API and proxy stay upstream's |
| Infrastructure | Terraform for GCP, AWS and Azure | Terraform for Azure (AKS) |
| Deployment | Nomad, plus Kubernetes and Docker Compose | Kubernetes (one orchestrator per node, Envoy Gateway) with Terraform for Azure, no Nomad |
| NBD connect | generic netlink | ioctl, about 17 ms against netlink's 100 to 250 ms stall |
| Platform | Linux | Linux only |
| Edges | | identical: routes, gRPC services, errors, snapshot formats, metrics, feature flags |

## Numbers

- **Sandbox resume:** p50 **51 ms** against Go's 321 ms on the same node and template. See the [blog post](website/content/blog/orchestrator-resume-rust-vs-go.mdx).
- **envd under load:** lower p99 latency in all seven tests and higher or equal throughput, idle RSS about 6 MB against Go's 11 MB. See the [blog post](website/content/blog/envd-rust-vs-go.mdx).
- **Compatibility:** the official E2B JS SDK sandbox tests pass against envd (105 passed, 9 skipped by the SDK).

## Status

| Component | State |
|---|---|
| envd | done, passes the official E2B JS SDK tests |
| orchestrator | create, resume, pause, checkpoint, egress firewall, routing, events and stats at upstream parity; open items in [the backlog](docs/BACKLOG.md) |
| template build | base, steps, finalize and optimize phases with the layer cache |
| API, client proxy | not part of this project, use upstream's |

## Quick start

envd and an S3 store in Docker, then the official SDK against it:

```bash
docker compose up -d --build
cd examples/sdk-playground && npm install && npm run demo
```

Running the orchestrator and real microVMs needs Linux with KVM. To run a whole cluster, see [Deploying](deploy/README.md): one orchestrator per node on Kubernetes, with Terraform for Azure.

## Development

```bash
cargo test --workspace     # unit and contract tests
scripts/test-linux.sh      # the Linux-only suite in a container
scripts/sdk-tests.sh       # the official E2B JS SDK tests against envd
```

Code style and conventions are in [CLAUDE.md](CLAUDE.md). The documentation site is in [`website/`](website): `cd website && npm install && npm run dev`.

## License

Apache License 2.0, the same license as E2B. See [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).

This project rewrites parts of [E2B](https://github.com/e2b-dev/infra) (Copyright 2023 FoundryLabs, Inc.) in Rust. It is not affiliated with, endorsed by or sponsored by FoundryLabs, Inc. E2B is their trademark.
