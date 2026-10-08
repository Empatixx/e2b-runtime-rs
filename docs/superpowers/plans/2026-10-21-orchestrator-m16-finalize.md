# Orchestrator M16: finalize, optimize and the build result

**Goal:** the build tail after the steps works as upstream `phases/finalize`, `phases/optimize` and the end of `Builder.Build`:
- finalize cold-boots the last layer, runs `configure.sh`, the start and ready commands and the CA cert bundle, then pauses and uploads the layer under the **requested build id**;
- optimize resumes that template twice, intersects the page traces and rewrites `metadata.json` with the prefetch mapping;
- the result carries the rootfs size and scheduling metadata.

**Spec:** `docs/superpowers/specs/2026-10-17-template-builder-design.md`, section "Finalize, optimize and result (M16)".

**Upstream digest:** `docs/superpowers/research/2026-10-21-m16-upstream-digest.md`. Every text, script and ordering comes from it.

**Rulings (Go wins, except where Go is nondeterministic):**
- `ensurefreedisk` is behind `build-ensure-free-disk-space=false`, so it does not run by default. It goes to the backlog with the digest's section 1.
- **Go's random `select` on build cancel during the ready loop** always picks `waiting for start command failed: context canceled`. This is recorded in PARITY.
- **The optimize/finalize metadata race:** optimize waits for the finalize upload of that build (Go's unused `Upload.Wait`) before it puts the object, so the prefetch always lands, which is Go's intent. This is recorded in PARITY.

## Global Constraints
- **Finalize user errors** use step `finalize`, with the exact Go texts.
- **Optimize** only fails with `failed to get template from cache: …`. Every other failure is a service-log warning.
- **`configure.sh` and the cert bundle command** are byte-exact, embedded as assets.
- **Feature-flag defaults:**
  - IO engine `Sync`;
  - reserved blocks 256 MB, set only when the source layer is cached;
  - balloon `{reporting:false, hinting:false}`;
  - no cmdline args or CPU template.
- **Code style:** per CLAUDE.md.

## Tasks

### Task 1: foundations
- Typed `Start { start_command, ready_command, context }` in `metadata.json`, written without omitting empty fields.
- `LayerBuild` gains:
  - envd defaults (user, workdir);
  - reserved blocks only when the source is cached;
  - a per-layer build origin (`template_build` for finalize).
- `run_command` gains a started hook, fired after the Start response headers arrive (Go `confirmCh`).
- A full Go `Duration.String()` formatter.
- The result carries scheduling metadata from the final template, the `rootfs size` service log, and Go's rootfs-header error texts.

### Task 2: finalize phase
- `builder/phases/finalize.rs` with the embedded `configure.sh` and `pack_cert_bundle.sh`.
- Texts `finalize` / `Finalizing template build`, the finalize hash, and the layer rules: Start, requested build id, cmdline args, balloon, CPU template.
- The action, in this order:
  1. configure, with a 5 min timeout;
  2. the start command in the background;
  3. the ready loop: 2 s interval, 10 min budget, default ready commands;
  4. confirm, cancel, wait;
  5. cert bundle, then sync.
- Always a cold boot; the envd update only when the source is cached.

### Task 3: optimize phase
- `[optimize] Optimizing template` with the optimize hash. Two resumes of the final template through `collect_prefetch`, with the 1-based Go run texts.
- `with_prefetch` (a `None` mapping too), then the local and remote `metadata.json` writes with `build_origin=template_build`, then the template cache update.

### Task 4: CI contract and docs
- The template contract adds a start/ready build:
  - check the log lines and their order;
  - a start command failing with `exit 3` gives Go's reason;
  - create a sandbox from the final build and check `/.e2b`.
- README, PARITY, BACKLOG (ensurefreedisk, envVars omitempty) and the project-structure skill.

## Order and gates
Each task is unit-tested, gated and committed. Push after tasks 2 and 4. An opus review of the whole milestone runs at the end.
