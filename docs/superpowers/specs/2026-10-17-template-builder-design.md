# Template builder (M13–M16)

## Goal

Port upstream's template manager, `orchestrator/pkg/template/**`, so that `TemplateService` builds a template from an OCI image or from another template. The result is a snapshot build that `SandboxService.Create` can resume at once, with a prefetch trace from optimize.

The port keeps upstream's business logic exactly where a client or the API can observe it: the proto, status codes, error texts, log format, layer hashes, cache index and `metadata.json`. The code underneath follows our conventions. The upstream reference is `~/IdeaProjects/e2b-runtime-origin-main-review/packages/orchestrator` (written `O/` below).

## What we reuse

- **Storage:** paths, header V3 and `metadata.json` (`template/metadata.rs`).
- **The pause path as one layer:**
  - `Sandbox::pause_into`;
  - `Node::cache_snapshot` (local diffs, `AddSnapshot`);
  - `Uploads` (async upload with retry, parents land first).
- **Cold boot and resume:** `SandboxBuilder::cold_boot` and `resume`, the network pool, the NBD pool, the template cache.
- **Optimize:** `template/optimize.rs` `collect_prefetch`.
- **Node and services:** the gRPC server (`App` router), `NodeStatus`, shutdown.

## Service: `TemplateService` (M13)

- **Proto.** `proto/orchestrator/template-manager.proto` is copied verbatim and compiled with the other two. It has no `package`, so its paths are `/TemplateService/{TemplateCreate, TemplateBuildStatus, TemplateBuildDelete, InitLayerFileUpload}`. It imports `orchestrator.proto` for `SchedulingMetadata`.
- **TemplateCreate:**
  - Validation errors:
    - `InvalidArgument "template build requires either fromImage or fromTemplate"`;
    - a duplicate build id is an error (`O/pkg/template/cache/build_cache.go`).
  - Defaults:
    - `cacheScope` = templateID;
    - `version` = `v2.0.0`; the user phase runs only when the version is ≥ `v2.1.0`;
    - kernel and Firecracker versions come from node config (`E2B_KERNEL_VERSION`, `E2B_FIRECRACKER_VERSION`), never from the request;
    - `freeDiskSizeMB` falls back to `diskSizeMB`.
  - It registers a `BuildInfo`, starts the build in a background task and returns `Empty` at once.
- **TemplateBuildStatus:**
  - status is `Building` until the build ends, then `Completed` with metadata or `Failed` with a reason;
  - log entries are filtered by level ≥ the requested level and by a time window (default the last 24 h), ordered by direction, then `offset` and `limit` (at most 100) are applied;
  - an unknown build gives the error text `"maybe already expired"`;
  - builds stay in memory with a TTL of 10 min.
- **TemplateBuildDelete:** both ids are required. A running build is failed with `"build was cancelled"` and its cancellation token fires. Then every object with prefix `{buildID}` is deleted.
- **InitLayerFileUpload:**
  - object `{cacheScope}/files/{hash}.tar` in the build storage;
  - returns `present = exists`, plus a presigned PUT URL valid for 30 min with its headers;
  - for local storage it returns a `file://` path and no headers (ruling: local storage cannot presign).
- **Build logs:** a `tracing` layer captures the entries of one build (timestamp, message, level, fields) into that build's ring. The user-facing lines use upstream's text, for example `[CACHED ][prefix] text [hash]`.
- **Errors:**
  - a user error is `PhaseBuildError{phase, step}`, which becomes `reason{message, step}`;
  - anything else becomes `"An internal error occurred. Please try again or contact support with the build ID."`;
  - cancellation becomes `"build was cancelled"` and a timeout `"build timed out"`.
- **On failure** the build deletes the `{buildID}` prefix and waits for its pending uploads.
- **Shutdown** waits for running builds, then gives readers a 15 s grace period to collect the final status.

## Phases, hashes and layer cache (M13)

- **Phases:** `base` → `user` (if version ≥ 2.1.0) → one phase per step → `finalize` → `optimize`.
- **The `Phase` trait:**
  - `hash(source)`;
  - `layer(source, hash) → LayerMeta { metadata, cached, hash }`;
  - `build(...)`.
- **Running a phase:**
  1. compute the hash;
  2. look up the cache;
  3. log `[CACHED ]` or build;
  4. validate the metadata: build id, kernel and Firecracker versions, and a non-empty user.
- **Hashes:** `sha256(join(keys, ";"))` in hex. Keys:
  - **base:** `"v2"`, the provision version (a hash of `provision.sh` and the injected files), `diskSizeMB`, and the source (the image ref, or `template:{buildID}`);
  - **user:** `prev`, `"DEFAULT USER"`, `"user"`;
  - **step:** `prev`, `type`, `args` joined with spaces, `filesHash` or `""`;
  - **finalize:** `prev`, `"config-run-cmd"`, `configure.sh`, the cert-bundle command;
  - **optimize:** `prev`, `"optimize"`.
- **Cache index:**
  - `{cacheScope}/index/{hash}` holds `{"template":{"build_id":"…"}}`;
  - a hit requires `{build_id}/metadata.json` with version ≥ 2;
  - the index is written only after the layer's upload has landed.
- **Force:** `force` on the config or on a step forces every later phase. FromTemplate is always a hit and uses its parent's metadata. Finalize and optimize are never cached.
- **Build ids:** every built layer gets a fresh UUID build id, except finalize, which uses the requested build id.

## Layer executor (M15)

**Each layer:**
1. take the source template;
2. cold-boot (when the source came from the cache or from the base phase) or resume the previous layer of this build;
3. run the action;
4. `NewVersionTemplate` metadata;
5. pause into the template cache (`AddSnapshot`), so the layer is usable immediately;
6. upload in the background;
7. write the cache index after the upload lands.

**Steps run through envd** (Connect `process.Start` and `POST /files`), using our envd's proto:
- **RUN** `[cmd, user?]`: `/bin/bash -l -c <cmd>` in the context's workdir, with its env and user, timeout 1 h, streaming `[prefix] [stdout|stderr]: line`; a non-zero exit fails the step.
- **ENV/ARG:** each value is evaluated in the guest with `printf "%s" "<escaped>"`.
- **WORKDIR:** `mkdir -p`, then `chown -R` from the first new parent.
- **USER:** `useradd`; with `sudo`, the admin group and NOPASSWD.
- **COPY/ADD:** download the files tar, upload it to `/tmp/{hash}.tar`, unpack it and run `copy_script.sh` as root.
- **After each step:** `busybox sync`.

## OCI → ext4 and provisioning (M14)

**Pull** with the `oci-client` crate:
- platform `linux/<arch>`, and the image config architecture is checked;
- Basic auth for general registries;
- ECR and GCP auth go to the backlog (ruling).

**Injected files** are copied verbatim into `crates/orchestrator/assets/template/`. Source: `O/pkg/template/build/core/rootfs/files/*.tpl` and `phases/base/provision.sh`, rendered with the same values. The full list:
- `/usr/bin/envd` (0777, our envd) and the envd systemd preset;
- `provision.sh`;
- busybox as `/usr/bin/busybox` and `/usr/bin/init`;
- `envd.service`, hostname `e2b.local`, hosts, resolv.conf, inittab, rcS, provision runner;
- chrony source, journald, watchdog-off, serial-getty, seed-certs;
- the symlink `multi-user.target.wants/envd.service`.

**Root filesystem:**
1. Flatten the layers in-process into a staging dir. Tar entries apply in layer order and whiteouts are honoured (`.wh.` and `.wh..wh..opq`).
2. `mkfs.ext4 -d <staging>` with upstream's `-O` features (`^64bit,^dir_nlink,ext_attr,extent,extra_isize,filetype,flex_bg,huge_file,inline_data,large_file,sparse_super2 -b 4096 -m 0 -i 4096`) and a 25000 MB maximum. Ruling: `-d` instead of loop mount, overlayfs and rsync. It gives the same filesystem tree without mounts.
3. `e2fsck -pfv` → `resize2fs -M` → `e2fsck` → `tune2fs -O ^read-only`.
4. Read the free space via `debugfs -R stats`, `resize2fs` to add `diskSizeMB − free`, then `e2fsck`.

**Provision boot:**
- a cold boot over a **direct file rootfs** (a new rootfs mode, no NBD) with `init=/usr/bin/init` (busybox), kernel logs on, and a 5 min timeout;
- the host scans the serial console for `E2B_PROVISIONING_EXIT:<code>`; `0` is success, anything else fails with the last 30 `[external]` lines; EOF without the marker means "exit code not detected";
- then shut down and run `debugfs -w -R 'rm "/provision.result"'`.

**Base layer:**
1. `e2fsck` and grow the filesystem to `diskSizeMB`;
2. cold boot with systemd (`init=/sbin/init`, 10 min timeout);
3. wait for envd for 60 s;
4. `busybox sync`;
5. pause and upload; this is the base layer.

**Node needs** `e2fsprogs` (mkfs.ext4, e2fsck, resize2fs, tune2fs, debugfs) and a static busybox (`E2B_BUSYBOX`). The guest needs internet while it provisions.

## Finalize, optimize and result (M16)

**Finalize** (always a cold boot, 20 min):
1. `configure.sh` (5 min):
   - writes `/.e2b`;
   - creates `user` and the sudo/admin group;
   - `chown /home/user`, `chmod -R 777 /usr/local`, `/code`.
2. If a start command is set, it runs in the background, and the ready command runs every 2 s for up to 10 min (default `sleep 20`, or `sleep 0` without a start command). An error in the start command cancels the ready loop.
3. Deferred cert-bundle command, then `sync`.
4. Pause with the requested build id.
5. `metadata.json`:
   - `start{start_command, ready_command, context}`;
   - `template{build_id, kernel_version, firecracker_version}`;
   - `context` from the steps.

**Optimize:** two resumes of the finalize build → intersect → `with_prefetch` → upload `metadata.json` → invalidate the cache entry. Any failure is only a warning.

**Result:**
- `TemplateBuildMetadata{rootfsSizeKey (MB from the final rootfs header), envdVersionKey, kernelVersion, firecrackerVersion, schedulingMetadata}`;
- it waits for every upload first.

## Testing

**Unit tests:**
- hashes against upstream's test vectors (`hash_test.go`);
- the index JSON;
- log filtering and paging;
- the status mapping;
- the whiteout flattening on synthetic tars;
- the provisioning marker scan;
- each step's command text against upstream.

**Contracts on CI:**
- **M13:** `TemplateService` over gRPC with a stub phase list: create, status with logs, delete cancels.
- **M14:** build an ext4 from a small public image (`alpine` or `debian:bookworm-slim`), provision it, and boot it with systemd until envd answers.
- **M15:** RUN, ENV, WORKDIR, USER and COPY steps, a cached rebuild that logs `[CACHED ]`, and a resume between layers.
- **M16:** a full build from `debian:bookworm-slim` with a start command → `SandboxService.Create` of the build → the SDK `commands.run` works and the prefetch counter is above 0.

## Out of scope

- ECR and GCP registry auth.
- The Docker Hub proxy cache.
- `ensurefreedisk`.
- CPU templates and cmdline args in the hash.
- envd memory protection.
- Huge pages.
- The artifact-registry image delete.
- External log shipping (Loki).

## Order

M13 → M14 → M15 → M16, each with its own plan, CI proof and opus review, the same way as M9–M12.
