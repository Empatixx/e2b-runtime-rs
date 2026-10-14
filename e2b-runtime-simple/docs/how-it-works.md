# How e2b-runtime-simple works

Everything is built on one idea: a **checkpoint** is the CPU state plus lists of layers (disk, memory, volumes). Layers are named by their sha256, live in S3 and are cached on the node. Create, pause, resume and snapshot only make a checkpoint or start a VM from one.

Czech version: [how-it-works.cs.md](how-it-works.cs.md).

## 1. The big picture

```
                        ┌──────────────┐
                        │   E2B SDK    │
                        └──┬────────┬──┘
           API (create,    │        │   sandbox traffic (envd: commands, files, ports)
           pause, connect) │        │   http://49983-<id>.<node domain>
                           ▼        ▼
          ┌─────────────────┐    ┌─────────────────┐
          │     node A      │◀──▶│     node B      │   the same e2b-node binary
          │ API  :3000      │    │ API  :3100      │   forwards calls for sandboxes it does not own
          │ proxy:3001      │    │ proxy:3101      │
          │ VM, VM, VM      │    │ VM, VM          │
          │ local cache     │    │ local cache     │
          └────────┬────────┘    └────────┬────────┘
                   └───────────┬──────────┘
                               ▼
                    ┌─────────────────────┐
                    │  S3 (the only thing │  layers + JSON records
                    │   nodes share)      │
                    └─────────────────────┘
```

## 2. Inside one node

```
 e2b-node (root)
 ├─ api/            E2B REST: /v2/sandboxes, /pause, /connect, /snapshots, /volumes
 │   └─ forward.rs  "runs on another node?" then send the request to the owner
 ├─ proxy.rs        traffic into the guest: E2b-Sandbox-Id header, slot IP, envd
 ├─ sandbox.rs      map of RUNNING sandboxes (paused ones live only in S3)
 ├─ warm_pool       4 Firecracker processes started ahead of time, each with its network slot
 ├─ shared_memory   1 read-only memory device per checkpoint, shared
 └─ starts ─▶ uvm-ublk-daemon (from AgentENV)
                 └─ /dev/ublkbN = disks and memory made of OverlayBD layers
```

## 3. Layers: how a disk is put together

OverlayBD reads from the top down and returns the first layer that has the block.

```
 the guest sees /dev/vda (ext4)
        │
        ▼
 /dev/ublkb7 ── OverlayBD ──┐
                            │  reads from the top:
   ┌────────────────────────┴───────────────┐
   │ runtime upper  (local, WRITABLE)        │ ← every write of the sandbox goes here
   ├─────────────────────────────────────────┤
   │ sha256:d623…  changes from pause #1     │ ┐
   ├─────────────────────────────────────────┤ │ read only, shared
   │ sha256:9629…  ext4 from a Docker image  │ ┘ by all sandboxes
   └─────────────────────────────────────────┘
          │ layer in the local cache?
          ├─ yes: read the file data/layers/sha256:….commit
          └─ no:  read s3://bucket/layers/sha256:…  (block by block, then cached)
```

**Memory** works the same way, read only. Firecracker maps the memory device as a file:

```
 guest RAM (1 GiB)
   │ reading a page  → page cache of /dev/ublkbM  (shared by every sandbox of the checkpoint)
   │ writing a page  → private copy in the Firecracker process (copy on write)
   ▼
 /dev/ublkbM ── OverlayBD ── [ memory from pause #1 ] [ template memory (74 MB of 1 GiB) ]
```

## 4. Create

```
 SDK            node A                         S3                 Firecracker + guest
  │ POST /v2/sandboxes {templateID: base}
  │──────────────▶│
  │               │ templates/base.json ──────▶│ → checkpoint ckpt-x
  │               │ checkpoints/ckpt-x.json ──▶│ → layer lists, vmstate
  │               │ take a Firecracker from the warm pool (running, slot ready)
  │               │ rootfs: writable ublk device from the daemon pool (~1 ms)
  │               │ memory: the shared device of the checkpoint
  │               │ PUT /snapshot/load ─────────────────────────────▶│ VM runs (~5 ms)
  │               │ MMDS: sandbox id, token hash ───────────────────▶│
  │               │ GET /health, POST /init ────────────────────────▶│ envd (~50 ms, page faults)
  │               │ sandboxes/<id>.json {owner: A} ─▶│
  │◀──────────────│ 201 {sandboxID, domain: "node-a…", envdAccessToken}
  │                                                         about 58 ms in total
```

## 5. Pause

```
 SDK           node A                                        S3
  │ POST /sandboxes/<id>/pause
  │─────────────▶│ PATCH /vm Paused            (vCPUs stop)
  │              │ PUT /snapshot/create        → vmstate (25 KB)
  │              │ GET /vm/dirty-memory-ranges → only written pages
  │              │   process_vm_readv → memory layer ──────────────▶│ layers/sha256:…
  │              │ fsync + restack rootfs      → the upper layer is sealed ▶│ layers/sha256:…
  │              │ (volumes: the same)                              │
  │              │ checkpoints/ckpt-y.json + vmstate ──────────────▶│
  │              │ sandboxes/<id>.json {owner: none, ckpt-y} ──────▶│
  │◀─────────────│ 204                                about 34 ms
  │              │ in the background: kill Firecracker, return devices and slot
```

After a pause the sandbox belongs to no node and its whole state is in S3.

## 6. Resume on another node

```
 SDK           node B                                         S3
  │ POST /v2/sandboxes/<id>/connect
  │─────────────▶│ GET sandboxes/<id>.json  (etag "e1") ──────▶│ {owner: none, ckpt-y}
  │              │ PUT sandboxes/<id>.json  If-Match: "e1" ───▶│ {owner: B}   ✅
  │              │      (had node C written in between: 412, "being resumed elsewhere")
  │              │ GET checkpoints/ckpt-y/vmstate ────────────▶│ (25 KB)
  │              │ layers: whatever B has not cached, OverlayBD reads from S3 on demand
  │              │ load snapshot, /init
  │◀─────────────│ 201 {domain: "node-b…"}            about 125 ms
  │
  │ from now on the SDK talks straight to node B (49983-<id>.node-b…)
```

An API call for a sandbox that runs elsewhere:

```
 SDK ── DELETE /sandboxes/<id> ──▶ node A
                                    │ not mine, the record says owner B
                                    │ forward (header x-e2b-simple-forwarded)
                                    └──────────────▶ node B ── kill ── 204
 SDK ◀──────────────── 204 ─────────┘                         about 3 ms
```

## 7. Network of one sandbox

```
 ┌─ netns e2bs-7 ─────────────────────────────────────────────┐
 │                                                            │
 │  guest eth0 169.254.0.21 ═══ tap0 169.254.0.22             │  every guest has the SAME IP
 │                                   │                        │  (because of snapshots)
 │        NAT: 169.254.0.21 ⇄ 10.11.0.7  (slot IP)             │
 │                                   │                        │
 │                              eth0 10.12.0.30 ══ veth ══╗   │
 └────────────────────────────────────────────────────────╫───┘
                                                          ║
  host:  e2bv7 10.12.0.29 ◀════════════════════════════════╝
         route 10.11.0.7 via the veth of slot 7
         node proxy ──▶ 10.11.0.7:49983 ──▶ envd in the guest
         out: MASQUERADE to the internet ✅
         blocked: other sandboxes (10.11/16), the host (INPUT from e2bv+), private networks, Azure 168.63.129.16 ❌
```

## 8. Volumes (for example `/home/user/data`)

```
 a template has 2 reserved drive slots: vdb, vdc, normally pointing at an empty placeholder

 create with a volume:
   volumes/<vol>.json  If-Match: attached_to = <sandbox>   (exclusive, one sandbox at a time)
   vol0.dev ──symlink──▶ /dev/ublkb9 (the layers of the volume)
   load snapshot, PATCH /drives/vol0 (the guest learns the new size), envd: mount /dev/vdb

 kill:
   sync in the guest, pause, seal the upper layer of the volume ─▶ S3 layers/sha256:…
   volumes/<vol>.json { layers += the new one, attached_to: none }
   the next sandbox mounts the volume on any node
```

## 9. Template build

```
 Docker image ubuntu:24.04
   + useradd user, sudo, python3, envd, /sbin/e2b-init
   │ docker build, docker export
   ▼
 directory → mkfs.ext4 -d → rootfs.ext4 → OverlayBD layer ─────────────▶ S3 layers/sha256:9629…
   │
   ▼ cold boot of Firecracker from that layer, wait for envd, 2 s of quiet
   │ capture (same as pause): memory + disk changes + vmstate ──────────▶ S3
   ▼
 templates/base.json → ckpt-x                         about 7 s with a warm Docker cache
```

## 10. What is in S3

```
 s3://e2b-simple/
 ├─ layers/sha256:9629…            160 MB  ext4 from the image   ┐
 ├─ layers/sha256:e646…             74 MB  template memory       │ immutable,
 ├─ layers/sha256:d623…             20 KB  disk changes          │ named by content
 ├─ layers/sha256:…                        every pause, volume   ┘
 ├─ checkpoints/ckpt-x.json                { rootfs_layers, memory_layers, drives, vcpu, mem }
 ├─ checkpoints/ckpt-x/vmstate      25 KB  CPU and device state
 ├─ templates/base.json                    → ckpt-x
 ├─ volumes/vol-….json                     { layers, attached_to }
 └─ sandboxes/i….json                      { info, owner: {node, api_url, domain} | none, checkpoint }
```

The local disk of a node is only a cache, so losing it breaks nothing:

```
 data-a/layers/sha256:….commit   layer cache (hard links to the sealed files)
 data-a/checkpoints/<id>/vmstate vmstate cache
 data-a/sandboxes/vm-…/          running VMs: upper disk layer, fc.sock, symlinks
 data-a/memory/<ckpt>/           image.json of the shared memory device
```

## 11. Housekeeping

### A node that dies

```
 every node, every 5 s:   PUT nodes/node-a.json { alive_until: now + 30 s }

 node A reads sandboxes/<id>.json { owner: node-b }
    └─ nodes/node-b.json alive_until < now ?
          no  → forward the call to B (as before)
          yes → B is gone:
                 volumes attached to the sandbox → attached_to: none (If-Match)
                 delete the checkpoint it was resumed from
                 delete the record → the SDK gets 404

 node B restarts → it forgets its own records first (its VMs did not survive)
```

Measured: right after `kill -9` of node B, calls for its sandbox fail with 502. After 37 s they get 404, the sandbox is gone from the list, and its volume mounts on node A.

### Merging layers

```
 budget for own layers = 32 - shared / 4   (minimum 1, the AgentENV rule)

 41st pause, 3 shared template layers:     after merging:
 ┌───────────────────────┐                 ┌───────────────────────┐
 │ pause #33             │ ┐               │ merged sha256:86cf…   │ ← one new layer in S3
 │ …                     │ │ 33 own > 32   ├───────────────────────┤
 │ pause #1              │ ┘               │ template layers       │ ┐ shared,
 ├───────────────────────┤                 │ (never merged)        │ ┘ unchanged
 │ template layers       │                 └───────────────────────┘
 └───────────────────────┘
```

Own layers that are only in S3 (after a resume on another node) are downloaded into the cache first. `examples/sdk-smoke/many-pauses.mjs` runs 40 rounds of write, pause and resume; at the end the root disk has 11 layers and every file is there.

### Old checkpoints and the local cache

```
 pause:  new checkpoint saved → delete the checkpoint the sandbox was resumed from
 kill:   delete the last checkpoint of a paused sandbox
 layers: stay in S3 (templates and other checkpoints share them), like AgentENV

 local cache, every 30 min:  above 95 % of E2B_SIMPLE_CACHE_GB?
                             → delete least recently used layers idle for 10 min, down to 70 %
                             (every layer is also in S3, so nothing is lost)
```
