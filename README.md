# e2b-runtime-simple

A small E2B sandbox runtime in Rust. One binary per node serves the E2B API and the sandbox proxy, so the official E2B SDK works against it. Sandboxes are Firecracker microVMs. Disks and memory are layers in the [OverlayBD](https://github.com/containerd/overlaybd) format, exposed through ublk, using the storage crates from [AgentENV](https://github.com/kvcache-ai/AgentENV).

This is a proof of concept.

## One idea: the checkpoint

E2B has templates, build layers, snapshots and paused sandboxes. Here they are all one thing, a **checkpoint**:

```
Checkpoint {
  vm_state        Firecracker device and CPU state
  memory_layers   OverlayBD layers, oldest first
  rootfs_layers   OverlayBD layers, oldest first
  drives          volumes mounted in the guest, with their layers
}
```

- **Template**: a name that points to a checkpoint.
- **Pause**: capture a checkpoint, stop the VM.
- **Resume or create**: start a VM from a checkpoint.
- **Snapshot**: capture a checkpoint, keep the VM running, save it as a template.

Capturing adds one new layer on top of each stack:

- **rootfs**: the writable upper layer is sealed and becomes the newest lower layer.
- **memory**: only the pages the guest wrote since the last checkpoint, read with `process_vm_readv` from the Firecracker process.
- **volumes**: the same as rootfs, the new layer belongs to the volume.

On resume, the memory layers become one read-only ublk device. Firecracker maps it as its memory file and copies pages on write, so there is no userfaultfd handler. Sandboxes from the same checkpoint share the device, and the node keeps it open for 60 seconds after the last one stops, so its page cache stays warm.

## Layout

```
crates/envd            the in-guest daemon, from e2b-runtime-rs
crates/node            the node binary
  api/                 E2B REST API (sandboxes, snapshots, volumes)
  proxy.rs             sandbox traffic, by E2b-Sandbox-Id/Port headers or {port}-{id}.{domain}
  sandbox.rs           lifecycle: create, pause, connect, snapshot, kill, timeouts
  sandbox/vm.rs        one VM: boot, restore, capture, shutdown
  sandbox/envd.rs      calls into envd: health, /init, sync
  firecracker/         process and API client
  storage.rs           OverlayBD devices through the ublk daemon
  storage/memory.rs    dirty guest memory into a layer
  network.rs           one network namespace per slot, NAT to a per-slot host IP
  checkpoint.rs        checkpoint records
  template.rs          template build: Docker image, ext4, first layer, boot, checkpoint
  volume.rs            volumes: an ext4 disk made of layers
```

## Requirements

- Linux 6.8 or newer with `ublk_drv`, and `/dev/kvm`
- The AgentENV Firecracker build (`1.15.1-patch-v1`), which adds the dirty memory ranges endpoint
- Docker, for template builds
- Root, for ublk, network namespaces and iptables

`scripts/setup-node.sh` installs the packages and downloads Firecracker, the guest kernel and the ublk daemon.

## Run

```bash
cargo build --release
sudo target/release/e2b-node build-template --name base --envd target/release/envd
sudo E2B_API_KEY=e2b_local target/release/e2b-node serve
```

Point the SDK at it:

```bash
export E2B_API_URL=http://localhost:3000
export E2B_SANDBOX_URL=http://localhost:3001
export E2B_API_KEY=e2b_local
cd examples/sdk-smoke && npm install && node smoke.mjs
```

## Numbers

Azure Standard_E4ds_v4 with nested KVM, kernel 7.0, template on Ubuntu 24.04 (2 vCPU, 1 GiB), measured with curl on the node. AgentENV ran on the same machine (Docker image `aenv-server:latest`) with the same calls:

| Step | e2b-runtime-simple | AgentENV |
|---|---|---|
| create from template | about 60 ms | about 110 ms |
| pause | about 8 ms | about 44 ms |
| resume | about 82 ms | about 130 ms |

What made the difference:

- **Warm pools, like AgentENV.** Firecracker processes with their network slot are started ahead of time, and the ublk daemon keeps a pool of writable devices. The VM restore takes about 6 ms.
- **One memory device per checkpoint.** Sandboxes from the same template share it. The node keeps the device open, because Linux drops the page cache of a block device when its last user closes it.
- **Fewer page faults after a restore.** envd asked hyper for room for about 263,000 headers on every connection, which allocated and zeroed about 17 MB per request. After a restore every one of those pages is a fault in the host. With hyper's default the guest touches about 1,500 pages after a restore instead of 9,800 (AgentENV: about 2,800).
- **Pause does not wait.** The guest is not synced (memory and disk are captured together), and the VM shuts down in the background.

There are no huge pages, like in AgentENV: the memory file is shared between sandboxes through the page cache, and huge pages would end that sharing.

## What works

The SDK smoke test passes: create, commands, files, pause, connect (resume), snapshot, create from a snapshot, and a volume mounted at `/home/user/data` that keeps its files after the sandbox is killed.

## Not done yet

- Layers stay on the local disk. Upload to S3 and lazy loading from S3 are not wired up.
- No garbage collection of checkpoints and layers.
- Snapshots of a sandbox with a volume share the volume's layer files. Deleting the volume breaks the snapshot.
- Sandbox state lives in memory. A node restart forgets running and paused sandboxes.
- Template build through the SDK (`Template.build`), fork, metrics, logs and the volume content API.
- Network rules (`allowOut`, `denyOut`), traffic access tokens, auto resume.
- Multi node.
