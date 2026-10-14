# e2b-runtime-simple

A small E2B sandbox runtime in Rust. One binary per node serves the E2B API and the sandbox proxy, so the official E2B SDK works against it. Sandboxes are Firecracker microVMs. Disks and memory are layers in the [OverlayBD](https://github.com/containerd/overlaybd) format, exposed through ublk, using the storage crates from [AgentENV](https://github.com/kvcache-ai/AgentENV).

Nodes share one S3 bucket and nothing else. A sandbox paused on one node can be resumed on any other.

This is a proof of concept. [How it works](docs/how-it-works.md) explains it step by step with diagrams ([česky](docs/how-it-works.cs.md)).

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

## Many nodes, one bucket

```
   SDK ──API──▶ any node ──(forwards to the owner if needed)──▶ owner node
   SDK ──sandbox traffic──────────────────────────────────────▶ owner node ({port}-{id}.{node domain})

   node A        node B        node C        local disk = cache only
      └─────────────┼─────────────┘
                    ▼
                    S3   layers/sha256:…        rootfs, memory and volume layers, by content
                         checkpoints/<id>.json  + checkpoints/<id>/vmstate
                         templates/<name>.json, volumes/<id>.json
                         sandboxes/<id>.json    { owner node or none, checkpoint }
```

- **Layers** are named by their sha256. A sealed layer goes into the local cache and to S3 under the same name. When a node opens a disk, a layer it has in its cache is read locally, any other layer is read by OverlayBD straight from S3, block by block, and cached.
- **A running sandbox** belongs to one node. Its record says which one, and every node forwards API calls for it to that node. The SDK sends sandbox traffic straight to the owner, because each node puts its own domain into the `domain` field of the API answer.
- **A paused sandbox** belongs to no node. `connect` on any node takes it with a conditional write to its record (`If-Match`), so two nodes can never resume it at the same time.

## Layout

```
crates/envd            the in-guest daemon, from e2b-runtime-rs
crates/node            the node binary
  api/                 E2B REST API (sandboxes, snapshots, volumes)
  api/forward.rs       calls for a sandbox that runs on another node go to that node
  proxy.rs             sandbox traffic, by E2b-Sandbox-Id/Port headers or {port}-{id}.{domain}
  sandbox.rs           lifecycle: create, pause, connect, snapshot, kill, timeouts
  sandbox/vm.rs        one VM: boot, restore, capture, shutdown
  sandbox/envd.rs      calls into envd: health, /init, sync
  sandbox/record.rs    the sandbox record in S3: owner node, checkpoint, claim
  sandbox/warm_pool.rs Firecracker processes started ahead of time
  firecracker/         process and API client
  storage.rs           OverlayBD devices through the ublk daemon
  storage/memory.rs    dirty guest memory into a layer
  storage/shared_memory.rs  one read-only memory device per checkpoint
  layers.rs            content addressed layers: local cache, S3, image configs
  store.rs             S3: JSON records with conditional writes, file upload and download
  network.rs           one network namespace per slot, NAT to a per-slot host IP
  checkpoint.rs        checkpoints in S3
  template.rs          template build: Docker image, ext4, first layer, boot, checkpoint
  volume.rs            volumes: an ext4 disk made of layers
```

## Requirements

- Linux 6.8 or newer with `ublk_drv`, and `/dev/kvm`
- The AgentENV Firecracker build (`1.15.1-patch-v1`), which adds the dirty memory ranges endpoint
- Docker, for template builds
- Root, for ublk, network namespaces and iptables
- An S3 bucket. Locally, [Silo](https://hub.docker.com/r/pgsty/silo) (a MinIO fork) works.

`scripts/setup-node.sh` installs the packages and downloads Firecracker, the guest kernel and the ublk daemon.

## Run

```bash
cargo build --release
export E2B_SIMPLE_S3_ENDPOINT=http://127.0.0.1:9000 E2B_SIMPLE_S3_BUCKET=e2b-simple
export E2B_SIMPLE_S3_ACCESS_KEY=… E2B_SIMPLE_S3_SECRET_KEY=…
sudo -E target/release/e2b-node build-template --name base --envd target/release/envd
sudo -E E2B_API_KEY=e2b_local target/release/e2b-node serve
```

`scripts/two-nodes.sh` starts two nodes on one machine (API on 3000 and 3100) and `examples/sdk-smoke/two-nodes.mjs` moves a sandbox between them: create and pause on A, resume on B with its files and running processes, back to A.

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
| create from template | about 58 ms | about 110 ms |
| pause, layers uploaded to S3 | about 34 ms | about 44 ms |
| resume on the same node | about 93 ms | about 130 ms |
| resume on another node | about 125 ms | |
| kill sent to a node that does not own the sandbox | about 3 ms | |

What made the difference:

- **Warm pools, like AgentENV.** Firecracker processes with their network slot are started ahead of time, and the ublk daemon keeps a pool of writable devices. The VM restore takes about 6 ms.
- **One memory device per checkpoint.** Sandboxes from the same template share it. The node keeps the device open, because Linux drops the page cache of a block device when its last user closes it.
- **Fewer page faults after a restore.** envd asked hyper for room for about 263,000 headers on every connection, which allocated and zeroed about 17 MB per request. After a restore every one of those pages is a fault in the host. With hyper's default the guest touches about 1,500 pages after a restore instead of 9,800 (AgentENV: about 2,800).
- **Pause does not wait for the VM.** The guest is not synced (memory and disk are captured together), and the VM shuts down in the background. Pause does wait for the upload to S3, so a paused sandbox survives the loss of its node.

There are no huge pages, like in AgentENV: the memory file is shared between sandboxes through the page cache, and huge pages would end that sharing.

## What works

The SDK smoke test passes: create, commands, files, pause, connect (resume), snapshot, create from a snapshot, and a volume mounted at `/home/user/data` that keeps its files after the sandbox is killed.

Sandboxes reach the internet, but not each other, the node or private networks (`examples/sdk-smoke/isolation.mjs`).

## Housekeeping, the AgentENV way

- **A node that dies.** Every node writes `nodes/<id>.json` with `alive_until` every 5 seconds. A record whose owner has not written for 30 seconds counts as gone: the next node that reads it releases its volumes and deletes it, and the SDK gets a 404. A node that restarts forgets its own records first. AgentENV does the same with a 30 second binding TTL in its scheduler.
- **Merging layers.** The layers of the template are shared and never merged. When a sandbox has more own layers than `32 - shared / 4`, the pause merges them into one with OverlayBD's `merge_files_ro`, like AgentENV. `examples/sdk-smoke/many-pauses.mjs` pauses a sandbox 40 times and checks every file.
- **Old checkpoints.** A pause deletes the checkpoint the sandbox was resumed from, and a kill deletes the last one. Layers stay in S3, as in AgentENV, because templates and other checkpoints share them.
- **Local cache.** Sealed layers move into the cache. Every 30 minutes, when the cache is above 95 % of `E2B_SIMPLE_CACHE_GB`, the node deletes the least recently used layers that were idle for 10 minutes, down to 70 %. Every layer is also in S3, so a deleted one is read from there again.

## Not done yet

- Layers are never deleted from S3. A mark and sweep from templates, sandboxes and volumes would free them.
- `GET /v2/sandboxes` lists the records in S3, one request per sandbox.
- Template build through the SDK (`Template.build`), fork, metrics, logs and the volume content API.
- Network rules (`allowOut`, `denyOut`), traffic access tokens, auto resume.
- The memory of a sandbox resumed on a new node is read from S3 on demand. AgentENV prefetches it with a startup pack; that would help with a remote bucket.
