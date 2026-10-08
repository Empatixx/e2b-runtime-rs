# Orchestrator M14: OCI → ext4, provisioning and the base layer

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The base phase of a template build, as upstream `phases/base`:
1. pull the image;
2. build an ext4 with the injected files;
3. provision it in a busybox-init VM;
4. grow it;
5. boot it with systemd until envd answers;
6. sync, pause, upload and index it as the base layer.

**Spec:** `docs/superpowers/specs/2026-10-17-template-builder-design.md`, section "OCI → ext4 and provisioning (M14)", with the corrections below.

**Upstream digest:** `docs/superpowers/research/2026-10-19-m14-upstream-digest.md`. It holds exact commands, texts, log lines and test vectors with Go file:line references; every task reads its section first.

**Spec corrections (Go wins, the spec is updated in Task 1):**
- ToExt4 is mkfs → mount → extract → umount → `e2fsck -pfv` → `resize2fs -M` → `e2fsck -pfv`; `tune2fs -O ^read-only`, free space, enlarge and `e2fsck -pfv` follow in `CreateExt4Filesystem`.
- The base `e2fsck` + grow to `diskSizeMB` runs after the provisioning boot (`enlargeDiskAfterProvisioning`).
- Go does not enforce the 5 min provisioning timeout; the wait ends on the marker, EOF or build cancellation. We do the same.
- Busybox is `HOST_BUSYBOX_DIR/BUSYBOX_VERSION/<arch>/busybox` (defaults `/fc-busybox`, `1.36.1`), envd is `HOST_ENVD_PATH` (default `/fc-envd/envd`).
- 21 injected files plus one symlink (22 with `build-envd-memory-protection`).
- The base systemd boot has a pre-boot `tune2fs -r 65536` (256 MB reserved).
- The base hash appends `cmdline:`, `cpu-template:` and `envd-memory-protection:128:256` when present; the provision version is `HashKeys(provision.sh, filesHash, distroFingerprint)`.
- Provisioning uses `init=usr/bin/init` (no leading slash).

## Global Constraints

- Every user-visible text, log line and command argv is copied from the digest; nothing is paraphrased.
- Feature flags take their upstream defaults as constants: `build-base-rootfs-size-limit-mb` = 25000, `build-reserved-disk-space-mb` = 256, `build-provision-version` = 0, `build-envd-memory-protection` = false.
- Raw templates (`assets/template/files/*.tpl`) and `provision.sh` are byte copies of upstream, so `filesHash` and the provision version equal upstream's (`83b23b6a510b214cdfc5bce2a379219610df77a4e4fecc1c7598fcd17caa08f7`).
- Rendered outputs in `tests/golden/template/` (made with the real Go templates) are the test oracle.
- Linux-only code is `#[cfg(target_os = "linux")]`; root-needing tests run in `docker run --privileged` and on CI.
- Code style per CLAUDE.md: small methods, chains, builders, no comments, syscalls behind small safe types.

## Review Focus

- Whiteouts: an opaque dir and a `.wh.` file in the same layer; a whiteout for a path the lower layer has as a dir.
- A build cancelled during provisioning: the VM, loop mounts and temp dirs are all released.
- `e2fsck` exit codes 1 and 2 with fix are success; signals are errors.
- `No space left on device` from rsync becomes the image-too-large user error with both sizes.
- A registry 401 without a diagnostics body gives `registry returned status code 401`.

## Tasks

### Task 1: config, assets and hashes

**Files:**
- `src/server/config.rs`: `HOST_ENVD_PATH`, `HOST_BUSYBOX_DIR`, `BUSYBOX_VERSION`, `TEMPLATES_DIR` (default `${ORCHESTRATOR_BASE_PATH}/build-templates`), `PROVIDER` (default `gcp`), `TARGET_ARCH` (normalised `amd64`/`arm64`), `DOCKERHUB_REMOTE_REPOSITORY_PROVIDER`/`_URL`.
- `src/builder/template_files.rs` + `template_files/`:
  - `render.rs`: the small Go-template subset the `.tpl` files use (`WriteFile`, `.Field`, `if`, `MemoryLimit`), output trimmed;
  - `layers.rs`: the files layer (sorted tar, name/size/mode only) and the symlink layer;
  - `hash.rs`: `files_hash`, `distro_fingerprint`, `provision_version`.
- `src/builder/hash.rs`: `base_hash` with the optional keys.
- Spec update with the corrections above.

**Tests:**
- every rendered file equals its golden file (MemoryMB=1024);
- 21 files at MemoryMB=100 with `GOMEMLIMIT=50MiB`;
- protection on adds the slice drop-in with `MemoryMin=128M`/`MemoryLow=256M`;
- `provision_version()` = `83b23b6a…`;
- base hash vectors from digest 7.5;
- `TARGET_ARCH` normalisation.

### Task 2: ext4 tools

**Files:** `src/builder/ext4.rs` + `ext4/stats.rs`. It holds mkfs, mount/umount loop, `check_integrity(fix)` with the exit-bit rules, shrink, resize, enlarge, make_writable (one argv element `-O ^read-only`), reserved blocks, `debugfs` stats parse, `remove_file`, `log_metadata`. Programs are named fields, so tests can swap a fake script.

**Tests:**
- 7.1 vectors;
- 7.2 exit table with fake `e2fsck` scripts;
- on Linux, mkfs keeps `dir_index` (privileged docker).

### Task 3: layer export with whiteouts and the overlay copy

**Files:**
- `src/builder/oci/untar.rs`: a moby-like untar with overlay whiteouts, chown, modes, xattrs, mtimes, hardlinks, devices and confined paths. Symlink targets stay verbatim.
- `src/builder/oci/export.rs`: parallel layers in reversed order, the per-layer log lines.
- `src/host/overlay_mount.rs`: fsopen/fsconfig/fsmount/move_mount behind `OverlayMount`.
- `src/builder/oci/copy.rs`: rsync, `du -sb`, `ImageTooLarge` with `humanize_bytes`.

**Tests:** 7.3 order, whiteout (char device 0:0), escaping symlink (Linux, root); `humanize_bytes` vectors; image-too-large texts.

### Task 4: OCI pull

**Files:** `src/builder/oci/pull.rs`, `oci/auth.rs`, `oci/dockerhub.rs`. Uses `oci-client`:
- platform `linux/<arch>`;
- the config arch check;
- general Basic, GCP `_json_key`, ECR token via a SigV4-signed `GetAuthorizationToken`;
- the Docker Hub proxy (GCP/AWS/Azure/noop), with `removeRegistryFromTag`;
- `wrap_image_pull_error` on distribution error codes and status;
- `parse_envs`, `image_size`.

**Tests:**
- the error mapping (7.3 vectors);
- `removeRegistryFromTag`;
- `parse_envs`;
- a contract test against a local `registry:2` with Basic auth (CI).

### Task 5: rootfs creation

**Files:** `src/builder/rootfs.rs`. `create_ext4_filesystem` runs:
1. pull;
2. the size log;
3. the injected layers;
4. ToExt4;
5. writable;
6. free space;
7. enlarge;
8. integrity.

It returns the image config and env. Every user log line and error text comes from digest §2.

**Tests:** a Linux contract building an ext4 from `alpine` and listing `/usr/bin/envd` and `/etc/hostname` with `debugfs`.

### Task 6: direct-file rootfs sandbox and boot options

**Files:**
- `src/sandbox/config.rs`: `Rootfs::{Overlay, File}`;
- `src/sandbox.rs`: skip NBD for `File`;
- `src/firecracker/config.rs`: Go's sorted kernel args (`kernel_args.rs`) with `KernelLogs`, `KvmClock` and `init`;
- `src/firecracker/client.rs`: `io_engine: Sync`, optional MMDS;
- `src/firecracker/process.rs`: a console line stream (tee to `console.log`) with the FlushMetrics filter;
- a `shutdown` that pauses, snapshots to a throwaway path, kills and fsyncs the file;
- `export_file_diff`: a full-file 4 KiB scan, zero blocks marked empty.

**Tests:**
- kernel args string vectors (digest §4);
- the FlushMetrics filter;
- the full-file diff on a sparse file.

### Task 7: provisioning boot

**Files:** `src/builder/provision.rs`:
- the console scan (marker at line start, `\r` stripped, 30-line tail, the EOF text);
- the `[external] ` user log at Debug;
- the success logs, shutdown and `debugfs rm "/provision.result"`;
- every error wrapping from digest §4.

**Tests:** marker vectors (digest end of §7).

### Task 8: base phase

**Files:**
- `src/builder/phases/base.rs`: implements `Phase`.
  - `prefix` is `base`.
  - `describe` is `FROM <image>` / `FROM TEMPLATE <alias>`.
  - `hash` follows digest §5; `layer` does the cache rules; `build` runs digest §5 steps 1–12.
- `src/envd.rs`: a `busybox sync` process run as root over the envd Connect API.
- The base layer upload uses the existing pause/upload with `export_file_diff`, then `LayerIndex::save`.
- `PhasePipeline::phases()` returns the base phase.

**Tests:** cache rules with a stub index; the FromTemplate missing-template user error.

### Task 9: CI contract and docs

- **CI provides:** static busybox under `/fc-busybox/1.36.1/amd64/busybox`, and our envd at `/fc-envd/envd`.
- **Contract `tests/template_contract.rs`:**
  1. `TemplateCreate` from `debian:bookworm-slim`;
  2. the status ends `Completed` with the base layer only;
  3. logs contain `Provisioning was successful, cleaning up` and `Sandbox template provisioned`;
  4. a `SandboxService.Create` of the build answers envd `/health`.
- **Docs:** README, PARITY, BACKLOG and the project-structure skill.

## Order and gates

Each task:
1. its unit tests first;
2. the gate (fmt, clippy on Mac and Linux, tests);
3. a commit.

Push after Tasks 4, 6 and 9 for CI. An opus review of the whole milestone runs at the end, as in M9–M13.
