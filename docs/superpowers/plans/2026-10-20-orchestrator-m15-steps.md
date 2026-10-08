# Orchestrator M15: the steps phase and the layer executor

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Template steps (RUN, ENV/ARG, WORKDIR, USER, COPY/ADD) build layers on top of the base, as upstream `phases/steps`, `phases/user`, `commands` and `layer`:
- each step resumes the previous layer, or cold-boots it when that layer came from the cache;
- it runs the command through envd, syncs, pauses, caches and uploads the layer in the background;
- the layer is indexed after the upload lands.

**Spec:** `docs/superpowers/specs/2026-10-17-template-builder-design.md`, sections "Phases" and "Layer executor (M15)".

**Upstream digest:** `docs/superpowers/research/2026-10-20-m15-upstream-digest.md`. It is the source of every text, command and ordering below.

**Spec corrections (Go wins):**
- a missing `filesHash` hashes as `<nil>`;
- a layer built in this build is resumed, and only a cached source cold-boots with an envd update;
- the default USER phase reports failures as `steps`/`1`;
- uploads run in the background, the index is written after the upload lands, and the build waits for all uploads.

## Global Constraints

- **Every command error is a user error** with the exact Go text (digest §1). Internal errors use Go's wrapping.
- **Command output reaches the build log** as `[<prefix>] [stdout|stderr]: <line>`, split per event, trimmed, blank lines dropped. The level is Info for steps and Debug for the user phase, sudoers, the COPY move script and the envd replace.
- **`copy_script.sh`** is copied byte for byte from upstream and embedded.
- **Feature-flag defaults:** reserved blocks are 256 MB; the user phase applies to versions ≥ v2.1.0.
- **Code style:** per CLAUDE.md.

## Review Focus

- A step that fails in the middle of a chain: the uploads of earlier layers still finish and the build reports the user error.
- ENV values with `"`, backticks, `$(`, backslashes and newlines.
- COPY with globs, a trailing slash, an owner without a colon and permissions.
- A cached step followed by an uncached one: the cold boot path with the envd update.
- Cancelling during a step: the VM, slot, uploads and pins are all released.

## Tasks

### Task 1: layer executor foundation

- **Upload group:**
  - `builder/uploads.rs`: a per-build `LayerUploads` group;
  - `Node::upload_layer` spawns the upload, then `index.save`, then logs `Saved: <id>`;
  - errors are `error uploading snapshot: …` and `error saving UUID to hash mapping: …`;
  - `wait()` returns the first error;
  - after the phases, the pipeline waits with `error waiting for layers upload: …`;
  - on a failed build it waits too and joins `error uploading template layers: …`;
  - the base phase moves to this path.
- **Metadata:** `TemplateMetadata::new_version(TemplateVersions)` (version 2; drops prefetch and the other non-kept fields).
- **Memfile header:** the cold-boot memfile parent header is `Metadata::template(source_build_id, page, size)` with a nil mapping.
- **Pre-boot hook:** `SandboxBuilder::pre_boot(fn(&Path) -> Result)` runs on the exposed rootfs device; its error is `create sandbox: pre-boot hook failed: …`.
- **Layer source:** `Node::layer_source(build_id)` pins the cached template. On top of it:
  - `Node::resume_layer(...)`;
  - `Node::cold_boot_layer(...)`: an overlay over the cached rootfs, a fresh memory, `/sbin/init`, kvm-clock gated on envd ≥ 0.2.11, `Sync` io, reserved blocks.
- **envd files client:** `envd/files.rs` sends `POST /files?path=&username=` as a multipart `file` part, with the Go error texts.
- **`update_envd`:** copies the host envd to `/tmp/envd_updated`, runs the replace script at Debug with id `update-envd-replace`, runs the restart script with its error ignored, then waits for envd again.

**Tests:**
- metadata `new_version`;
- the upload group's first error;
- the files client against a fake envd;
- the memfile header base id.

### Task 2: commands

- **Module:** `builder/commands.rs` + `commands/{run,env,workdir,user,copy}.rs` + `assets/template/copy_script.sh`.
- **Log adapter:** the `logStream` adapter over `run_command`.
- **Helpers:** Go `filepath.Join`/`IsAbs`, doublestar `SplitPattern` and `parseCopyArgs`.
- **COPY:** downloads `{scope}/files/{hash}.tar` from build storage to a temp file, uploads it to `/tmp/<hash>.tar`, extracts it, then runs the rendered move script.

**Tests:**
- the `parseCopyArgs` table;
- `SplitPattern` edges;
- the ENV escaping;
- the WORKDIR join rules;
- the logStream split;
- a byte-compare of `copy_script.sh`;
- the copy script behaviour cases on Linux.

### Task 3: steps and user phases

- **`StepPhase`:**
  - prefix `builder n/N`;
  - describe `TYPE args`;
  - meta `steps`/type/n;
  - hash `HashKeys(prev, type, args, Sprintp(filesHash))`;
  - layer with force and the cache rules;
  - build via the layer executor (resume or cold boot by `source.cached`, `update_envd` when cold);
  - executes the command, syncs, pauses and uploads.
- **`UserPhase`:**
  - prefix `base`;
  - describe `DEFAULT USER user`;
  - meta `base`;
  - `user_hash`;
  - runs at Debug with the `steps`/`1` error quirk.
- **Pipeline:**
  - base;
  - user, if the version ≥ v2.1.0 (`error checking build version: …`);
  - the steps, with sticky force.

**Tests:**
- forceSteps;
- the texts;
- the version gate;
- the cache rules with a stub index.

### Task 4: CI contract and docs

The template contract runs these steps on top of the base and checks:
- RUN `echo 'Hello, World!'`;
- the ENV escaping cases, checked by RUN `[[ … ]]` assertions;
- WORKDIR `/app` with `[[ "$(pwd)" == "/app" ]]`;
- USER `root`/`user`;
- COPY of a local tar uploaded through `InitLayerFileUpload`;
- a rebuild that logs `CACHED [builder 1/N] …`.

Docs: README, PARITY, BACKLOG and the project-structure skill.

## Order and gates

Each task is unit-tested and gated, then committed. Push after tasks 1, 3 and 4. A fresh review of the whole milestone runs at the end.
