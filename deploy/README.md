# Deploying to Kubernetes

The stack runs on two node pools. The control pool runs upstream's API and client proxy, the stores and the Envoy gateway. The sandbox pool runs one orchestrator per node as a DaemonSet. envd is not deployed on its own: the orchestrator copies it into every template, and it runs inside each microVM.

| Part | Kind | Where | Scales by |
|---|---|---|---|
| orchestrator (Rust) | DaemonSet | every sandbox node | adding sandbox nodes |
| API, client proxy (upstream Go images) | Deployment with HPA | control pool | CPU, 2 to 10 and 2 to 20 replicas |
| Envoy gateway | Envoy Gateway with HPA | control pool | CPU, 2 to 10 replicas |
| PostgreSQL, Redis, ClickHouse | StatefulSet | control pool | one replica each |
| Vector (sandbox logs into ClickHouse) | Deployment | control pool | 2 replicas |
| migrations and the team seed | Job | control pool | runs once |

## How a request flows

```
SDK  --https://api.<domain>-->               Envoy --> api:3000 --gRPC--> orchestrator :5008 on the node IP
SDK  --https://<port>-<sandbox>.<domain>-->  Envoy --> client-proxy:3002 --> orchestrator proxy :5007 --> envd in the VM
```

The API finds the orchestrators through the Kubernetes API (`SERVICE_DISCOVERY_PROVIDER=kubernetes`). It lists the pods labelled `app.kubernetes.io/name=orchestrator` and dials their host IP.

## Azure with Terraform

`deploy/terraform/azure` creates everything:

- an AKS cluster with Azure CNI overlay, an autoscaled control pool and a sandbox pool of Ubuntu 24.04 nodes with nested virtualization;
- a storage account with the `templates` and `build-cache` containers, read through the kubelet managed identity;
- a container registry, where `az acr build` builds `docker/orchestrator.Dockerfile`;
- Envoy Gateway, cert-manager, a static public IP and the wildcard DNS record;
- the `e2b-secrets` Secret with generated passwords and the team API key;
- the overlay `deploy/k8s/overlays/<name>` that the next step applies.

You need `az`, `terraform` and `kubectl`, and an Azure DNS zone that holds your domain.

```bash
cd deploy/terraform/azure
cp terraform.tfvars.example terraform.tfvars    # fill in the subscription, domain and DNS zone
terraform init
terraform apply                                 # the orchestrator image build takes about 20 minutes
$(terraform output -raw kubeconfig_command)
cd ../../..
kubectl apply --server-side -k deploy/k8s/overlays/e2b-rs
```

Then point the SDK at it:

```bash
export E2B_DOMAIN=$(terraform -chdir=deploy/terraform/azure output -raw e2b_domain)
export E2B_API_KEY=$(terraform -chdir=deploy/terraform/azure output -raw e2b_api_key)
```

## Scaling

- **Sandbox nodes:** change `sandbox_node_count` and run `terraform apply`. Each new node gets an orchestrator, and the API starts placing sandboxes there. The cluster autoscaler does not add sandbox nodes, because sandboxes are microVMs, not pods.
- **Removing a sandbox node:** the orchestrator gets `SIGTERM`, reports itself as draining and waits until the API has removed every sandbox (up to one hour).
- **Control pool:** the API, the client proxy and Envoy scale on CPU. The cluster autoscaler adds control nodes between `control_min_count` and `control_max_count`.
- **Huge pages:** each sandbox node reserves `hugepages` pages of 2 MiB (default 8192, 16 GiB). Keep it near half of the node's RAM.
- **Rootfs transport:** `rootfs_exposure` is `nbd` by default, as upstream. `ublk` attaches a disk in about 2 ms instead of 12 ms and serves more IOPS, but upstream E2B does not have it. A node image upgrade changes the kernel, and `host-setup` installs the matching `linux-modules-extra` again on the next start.

## Other clusters

The base in `deploy/k8s/base` does not depend on Azure. Copy `deploy/k8s/overlays/example`, then:

1. Label the sandbox nodes `e2b.dev/role=sandbox`, taint them `e2b.dev/role=sandbox:NoSchedule`, and label the other nodes `e2b.dev/role=control`. Sandbox nodes need x86-64, `/dev/kvm`, cgroup v2, kernel 6.8 or newer and `apt` (Ubuntu 24.04).
2. Install Envoy Gateway and cert-manager, and create a `ClusterIssuer` named `e2b` that can issue a wildcard certificate.
3. Create the Secret: `kubectl -n e2b create secret generic e2b-secrets --from-literal=POSTGRES_PASSWORD=... --from-literal=CLICKHOUSE_PASSWORD=... --from-literal=ADMIN_TOKEN=... --from-literal=SANDBOX_ACCESS_TOKEN_HASH_SEED=... --from-literal=TEAM_API_KEY=e2b_<40 hex characters>`.
4. Set the domain, the huge pages, the storage and `ROOTFS_EXPOSURE` in `cluster.env`. Storage takes the same variables as the orchestrator: `TEMPLATE_STORAGE_URL`, `BUILD_CACHE_STORAGE_URL` and the credentials of the provider.
5. Build and push `docker/orchestrator.Dockerfile` and set its name under `images`.
6. Run `kubectl apply --server-side -k` on your overlay.

## What runs on a sandbox node

The orchestrator pod has three steps:

1. `host-setup` enters the host namespaces and installs `rsync`, `e2fsprogs`, `iproute2`, `iptables` and `nftables`. It loads `nbd` with the udev `nowatch` rule, and with `ROOTFS_EXPOSURE=ublk` also `ublk_drv`, installing `linux-modules-extra` for the running kernel when the module is missing. It reserves the huge pages and restarts the kubelet once so it advertises them.
2. `install` copies the orchestrator, envd, Firecracker `v1.14-0.2.0`, the `vmlinux-6.1.177_5008931` kernel and busybox from the image to the host.
3. `orchestrator` turns off `memory.oom.group` for its container, so an out of memory kill takes one process and not the whole pod. Then it runs the binary in the host mount namespace, like upstream's launcher. It needs the host's systemd, `/dev`, cgroups and network namespaces. Service names in its environment are resolved to cluster IPs first, because the host does not use the cluster DNS.

## Upgrades

- **Orchestrator:** change `orchestrator_version`, run `terraform apply` and apply the overlay again. The DaemonSet replaces one node at a time, and each old orchestrator drains first.
- **API and client proxy:** change the upstream tags under `images` in `deploy/k8s/base/kustomization.yaml`. Delete the finished `migrate` Job before you apply, so the new migrations run: `kubectl -n e2b delete job migrate`.

## Known limits

- PostgreSQL, Redis and ClickHouse run as single in-cluster replicas. For production, replace them with managed services and change the connection strings in `api.yaml`, `client-proxy.yaml` and `orchestrator.yaml`.
- The orchestrator resolves the store addresses once at start. If you recreate a store Service, restart the orchestrators.
- Only x86-64 sandbox nodes are supported.
