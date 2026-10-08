# Orchestrator M13: TemplateService skeleton, phases and layer cache

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The `TemplateService` gRPC with upstream semantics: create, status with logs, delete with cancellation, and `InitLayerFileUpload`. It sits on top of a phase pipeline with the layer hash chain and the cache index. Real phases (OCI, provision, steps, finalize) come in M14–M16. M13 runs a stub phase list in its tests.

**Spec:** `docs/superpowers/specs/2026-10-17-template-builder-design.md` (sections Service, Phases).

**Architecture (`src/build.rs` + `src/build/`):**
- **`proto`.** `template-manager.proto` is copied verbatim and compiled with the other two.
- **`hash.rs`.** `hash_keys(base, keys) = hex(sha256(base ; k1 ; k2 …))` and the per-phase key builders.
- **`index.rs`.** `LayerIndex` over `Storage`:
  - `{scope}/index/{hash}` = `{"template":{"build_id":"…"}}`;
  - `cached(hash)` requires `metadata.json` with version ≥ 2;
  - `save(hash, build_id)`.
- **`logs.rs`.** `BuildLog` (a ring of `LogEntry{timestamp, message, level, fields}`) and `query(level, start, end, direction, offset, limit ≤ 100)`. A `tracing` layer routes the events of a build span into its ring.
- **`builds.rs`.** `BuildCache`:
  - `build_id → BuildInfo{ result: watch<Option<BuildResult>>, log, cancel }`;
  - a 10 min TTL after the build ends;
  - `register` fails for a duplicate id.
- **`phase.rs`:**
  - the `Phase` trait, with `hash`, `layer` (cache lookup) and `build`;
  - `run_phases` with the force rule and the `[CACHED ]` log line;
  - `PhaseBuildError{phase, step, message}`, the internal-error text and cancel/timeout texts.
- **`service.rs`.** `TemplateServer` (TemplateCreate / TemplateBuildStatus / TemplateBuildDelete / InitLayerFileUpload) with upstream texts. It is registered in `App`.

**Global Constraints:**
- **TemplateCreate:**
  - no source → `InvalidArgument "template build requires either fromImage or fromTemplate"`;
  - `version` defaults to `v2.0.0` and `cacheScope` defaults to templateID;
  - it returns `Empty` immediately.
- **TemplateBuildStatus:**
  - an unknown build → `Status::not_found("build {id} not found, maybe already expired")`; check the exact upstream text in `template_status.go`;
  - default window now−24h..now, limit at most 100.
- **TemplateBuildDelete:**
  - both ids are required;
  - a running build is failed with `"build was cancelled"`;
  - then the `{buildID}` prefix is deleted.
- **InitLayerFileUpload:** S3 presigns PUT for 30 min; local storage returns `file://<path>` (ruling).

## Tasks

### Task 1: proto and hashes

- [ ] Add `template-manager.proto` to `build.rs`.
- [ ] **Unit tests:**
  - `hash_keys` matches upstream vectors. Compute them in Go-equivalent form in the test: sha256 of `base;k1;k2`.
  - the step key joins args with spaces and uses `""` when there is no `filesHash`.
- [ ] **Commit.**

### Task 2: index, logs, build cache

- [ ] **Unit tests:**
  - the index round trip on local storage; a metadata version of 1 is not a hit;
  - log query filtering by level, window, direction, offset and limit;
  - build cache: duplicate register, TTL after finish, cancel.
- [ ] **Commit.**

### Task 3: phase runner

- [ ] **Unit tests** with stub phases:
  - a cached phase is skipped and logged `[CACHED ]`;
  - force propagates to later phases;
  - a user error becomes `reason{message, step}`;
  - an internal error becomes the internal text;
  - cancel becomes `"build was cancelled"`.
- [ ] **Commit.**

### Task 4: TemplateService

- [ ] **Unit tests:** all four RPCs, with a stub builder injected into `TemplateServer`.
- [ ] **Contract:** `grpc_contract` calls `TemplateCreate` with an empty source and gets `InvalidArgument` over the real port. Status of an unknown build gives the not-found text.
- [ ] Docs, skill, backlog, review, CI green.
