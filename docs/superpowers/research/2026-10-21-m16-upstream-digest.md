# M16 research digest: the build tail after the steps (ensurefreedisk, finalize, optimize, result) (Go upstream to envd-rs)

Paths: `B` = `~/IdeaProjects/e2b-runtime-origin-main-review/packages/orchestrator/pkg/template/build`, `O` = `.../packages/orchestrator`, `S` = `.../packages/shared`, `T` = `~/IdeaProjects/e2b-runtime-origin-main-review/tests/integration/internal/tests`, `R` = `~/IdeaProjects/envd-rs/crates/orchestrator/src`.

## Key findings up front

1. **The requested build id is the finalize layer's id.** `finalize.Layer` sets `result.Template.BuildID = ppb.Template.BuildID`, and `ppb.Template` is the `storage.Paths{BuildID: cfg.GetBuildID()}` built in `O/pkg/template/server/create_template.go:62-64`. Every artifact of the final layer (memfile, rootfs, headers, snapfile, metadata.json) is paused, cached and uploaded under the requested id. Optimize creates no layer; it only rewrites `<requested>/metadata.json`. There is no copy or alias step anywhere (section 4).
2. **Finalize always cold boots** (`layer.NewCreateSandbox`, `B/phases/finalize/builder.go:222-227`), even from a source layer built earlier in the same build. `UpdateEnvd` and the reserved-blocks pre-boot hook apply **only when the source layer is cached** (`:217-219`, `:240`). Rust `LayerExecutor::cold_boot` always attaches `reserved_blocks()` (`R/builder/layer.rs:207`), so that has to become conditional.
3. **Finalize is never cached.** `Layer` always returns `Cached: false` (`:150-154`). Its hash `HashKeys(src, "config-run-cmd", <raw configure.sh>, <packCertBundleCmd>)` is still written to the index after upload, but nothing ever reads it.
4. **All finalize user errors carry step `"finalize"`.** Their message is the inner text. The outer `error running start and ready commands in sandbox: %w` wrap is dropped by `UnwrapUserError`. The wrap only shows up for internal errors, and those are hidden.
5. **Optimize can never fail the build** except on `failed to get template from cache: %w`. Every collection or upload failure is a service-log Warn, and the phase returns the source metadata unchanged.
6. **Go race:** optimize writes `<id>/metadata.json` directly while the finalize upload (async, in `UploadErrGroup`) also writes `<id>/metadata.json` from the local metafile, without prefetch. Nothing orders the two (section 3).
7. **`ensurefreedisk` is off by default.** Its flag is `build-ensure-free-disk-space=false`. When it is on, it is a VM-less, host-side ext4 grow over NBD.

---

## 1. Phase order and the ensurefreedisk phase (`B/builder.go:409-433`, `B/phases/ensurefreedisk/*.go`)

### Order
After the steps, `runBuild` appends phases in this order:
1. `ensurefreedisk.New(bc, sandboxFactory, layerExecutor, templateCache, index)`, only if `featureFlags.BoolFlag(ctx, featureflags.BuildEnsureFreeDiskSpace)` (`builder.go:422-431`). The flag is `"build-ensure-free-disk-space"`, default **`false`** (`S/pkg/featureflags/flags.go:467-468`). The LD context is template plus team (`builder.go:128-132`).
2. `postProcessingBuilder` (finalize) (`:432`).
3. `optimizeBuilder` (`:433`).

Both finalize and optimize always run. No version gate applies to either.

Construction (`builder.go:388-407`):
- finalize gets `templateStorage`, which is the **NFS-wrapped** one when `use-nfs-for-building-templates` is on (`:318-324`);
- optimize gets `builder.templateStorage`, which is the **raw** one, and `builder.templateCache`, `builder.sandboxes` and `builder.logger`.

### Texts (`ensurefreedisk/builder.go:31-82`)
- Constants: `resizeDiskName = "resize-disk"` and `resizeDiskTimeout = time.Hour`.
- `Prefix()` = `"resize-disk"`, and `String()` = `"Resizing disk"`.
- `Metadata()` = `{Phase: "resize-disk" (metrics.PhaseResizeDisk, B/metrics/*.go:20), StepType: "resize-disk"}`. StepNumber is nil, so `step` = `"resize-disk"`.
- The runPhase line is `[resize-disk] Resizing disk [<hash>]`, or `CACHED [resize-disk] Resizing disk [<hash>]` on a hit.
- **Hash:** `HashKeys(sourceLayer.Hash, "resize-disk", strconv.FormatInt(Config.FreeDiskSizeMB, 10))`.
- **`FreeDiskSizeMB`** comes from proto field 17, `optional int32 freeDiskSizeMB`. If it is absent, it falls back to `diskSizeMB` (`create_template.go:189`, `:314-320`).

### Layer (`ensurefreedisk/builder.go:84-134`)
1. `finalMetadata = sourceLayer.Metadata` with `.Template = {BuildID: uuid.NewString(), Config.KernelVersion, Config.FirecrackerVersion}`.
2. If `!sourceLayer.Cached || (Config.Force != nil && *Config.Force)`, return the not-cached result. A source that was freshly built or forced is never reconnected to an older ensure artifact.
3. Otherwise `index.LayerMetaFromHash(hash)`. On error, service Info `resize-disk layer unavailable in cache, building it` (error, hash), then not cached.
4. `index.Cached(buildID)`. On error, service Info `resize-disk artifact unavailable in cache, building it` (error, hash, build_id), then not cached.
5. Hit: `{Metadata: meta, Cached: true, Hash}`.

Note: this phase uses `Config.Force` only, never the per-step force.

### Build (`ensurefreedisk/builder.go:136-254`, `grow.go`, `sizing.go`)
VM-less. The flow under `ctx` with a 1h timeout:

1. **Source:** `templateCache.GetTemplatePinned(ctx, source.BuildID, false, true)`, error `get source template: %w`. Then `sourceTemplate.Rootfs()`, error `get source rootfs: %w`. The layer id comes from `uuid.Parse(currentLayer...BuildID)`, error `parse resize-disk layer build id: %w`.
2. **`growAndExport`** (`grow.go:39-113`). Any error is wrapped as `resize disk: %w`.
   - **Guards:**
     - `targetFreeMB < 0 || > MaxInt64>>20` fails with `disk target overflows: %d MiB`;
     - a nil header fails with `source rootfs header metadata is missing`;
     - `Size` errors with `get source rootfs size: %w`;
     - `validateSourceGeometry` (`sizing.go:57-75`) checks block size == `header.RootfsBlockSize`, and that size and block size equal the header's, with texts `invalid source size: %d`, `unsupported source block size: %d`, `source header geometry overflows`, `source geometry mismatch: device=%d/%d header=%d/%d` and `source size %d is not block aligned`.
   - **`measureFree`:**
     - a sparse cache at `<DefaultCacheDir>/resize-disk-<layerID>-measure.cache` (error `create measurement cache: %w`);
     - an overlay exposed through an NBD `DirectPathMount` (error `open NBD device: %w`);
     - `e2fsck -p -E journal_only <dev>` (error `replay source journal: %w`);
     - `mnt.Flush` (error `flush recovered source journal: %w`);
     - free bytes from `debugfs -R stats` (error `measure source free space: %w`).
   - **`freeBefore >= target`:** no resize. The result is an empty-diff header: `NewDiffMetadata(blockSize,nil,nil).ToDiffHeader(ctx, sourceHeader, buildID)` (error `build unchanged rootfs header: %w`), mapping validation (error `validate unchanged rootfs mapping: %w`), and `build.NoDiff{}`.
   - **Otherwise `computeGrownSize`** (`sizing.go:15-55`):
     - `shortage = target - freeBefore`;
     - `extra = ceil(shortage/10)`;
     - `size = current + shortage + extra`, rounded **up** to whole MiB.
     - Errors: `invalid current size: %d`, `invalid target free space: %d`, `free space before (%d) is already at or above target (%d)`, `free-space shortage overflows`, `free-space growth overflows`, `grown size overflows`, `aligned grown size overflows`.
     - Test vectors (`sizing_test.go:13-35`), with current = 1 GiB:
       - (target 512 MiB, free 512 MiB-1) gives +1 MiB;
       - (512 MiB, -1 MiB) gives +565 MiB;
       - (15666 MiB, 0) gives +17233 MiB.
   - **`resizeAndExport`** (`grow.go:157-220`):
     - a grow cache at `resize-disk-<id>-grow.cache` sized `newSize` (error `create grow cache: %w`);
     - zero the tail (error `zero-fill grown tail: %w`);
     - `resizeOffline`, which runs `e2fsck -pfv` (error `pre-resize e2fsck: %w`), `resize2fs <dev> <MB>M` (error `resize rootfs: %w`), `e2fsck -pfv` again (error `post-resize e2fsck: %w`), free space (error `measure grown free space: %w`), then flush (error `flush grown device: %w`);
     - export the diff into a `LocalDiffFile` (errors `create rootfs diff file: %w` and `export grown overlay: %w`);
     - `ToResizedDiffHeader(..., newSize)` (error `build grown rootfs header: %w`);
     - close the cache (error `close grow cache: %w`), then `CloseToDiff` (error `convert rootfs diff file: %w`).
3. **User log, Info** (`builder.go:183-203`):
   - `resize-disk: %d MiB free already >= target %d MiB, no resize needed`, or
   - `resize-disk: free %d -> %d MiB`, with `units.BytesToMB` = bytes>>20.
   - An undershoot is only a service Warn: `resize-disk target undershot after ext4 resize`.
4. **Metadata:** `meta := currentLayer.Metadata.MarkFilesystemOnly(true)`. This sets `filesystem_only: true` and raises `Version` to at least `FilesystemOnlyVersion` = 2 (`O/pkg/template/metadata/template_metadata.go:25-32`, `:238-245`).
   - It is written to the cache path's metadata file. Errors: `create cache paths: %w` and `write resize-disk layer metadata: %w`.
5. **Snapshot:** `sandbox.NewFilesystemOnlySnapshot(...)`, with memory `NoDiff`, a nil memory header and a `NoopFile` snapfile (`O/pkg/sandbox/snapshot.go:83-108`). Then `layerExecutor.UploadSnapshot(ctx, userLogger, snapshot, currentLayer.Hash, meta, ObjectOriginTemplateBuildCache)` runs, which is the same async path as M15. It emits the Debug lines `Adding layer to cache:`, `Uploading layer:` and `Saved:`. Error: `persist resized rootfs layer: %w`.
6. **Result:** `{Metadata: meta, Cached: sourceLayer.Cached /* pass-through */, Hash}`. A built ensure layer therefore tells finalize whether the last *VM* layer was cached, and that decides finalize's envd update and reserved blocks.

Every ensurefreedisk error is internal; none is a PhaseBuildError.

---

## 2. Finalize, the "postProcessingBuilder" (`B/phases/finalize/*.go`)

### Constants
| Name | Value | Where |
|---|---|---|
| `configurationTimeout` | `5 * time.Minute` | `configure.go:22` |
| `defaultReadyWait` | `20 * time.Second` | `ready.go:19` |
| `readyCommandRetryInterval` | `2 * time.Second` | `ready.go:21` |
| `readyCommandTimeout` | `10 * time.Minute` | `ready.go:22` |
| `finalizeTimeout` | `configurationTimeout + readyCommandTimeout + 5min` = **20 min** | `builder.go:37` |

`finalizeTimeout` is the **sandbox lifetime** passed to `NewCreateSandbox`. It is not a ctx deadline.

### Texts (`builder.go:77-97`)
- `Prefix()` = `"finalize"`.
- `String()` = `"Finalizing template build"`.
- `Metadata()` = `{Phase: "finalize", StepType: "finalize"}`, so `step` = `"finalize"` and `step_number` = `<nil>`.
- The runPhase line is `[finalize] Finalizing template build [<hash>]`. It is never `CACHED`.
- **Hash:** `cache.HashKeys(sourceLayer.Hash, "config-run-cmd", configureScriptFile, packCertBundleCmd)`. `configureScriptFile` is the **raw, unrendered** embedded `configure.sh`, with `{{ .TemplateID }}` still in it. `packCertBundleCmd` is the constant below. Rust `hash::finalize_hash(previous, configure_script, cert_bundle_command)` already has the right key order (`R/builder/hash.rs:47-49`).

### Layer (`builder.go:99-155`)
`result := sourceLayer.Metadata`, then:
1. If `Config.StartCmd != "" || Config.ReadyCmd != ""`, set `result.Start = &Start{StartCmd, ReadyCmd, Context: result.Context}`. That is the **current accumulated context** (user, workdir, env) after all steps. Otherwise the inherited `Start` is kept unchanged, including its old context. For example, FROM TEMPLATE `BasedOn` keeps the parent's `Start` (`template_metadata.go:270-284`), and step layers copy it along.
2. `result.Template = {BuildID: ppb.Template.BuildID /* the REQUESTED build id */, Config.KernelVersion, Config.FirecrackerVersion}`.
3. `result.CmdlineArgs`: `nil`, then `maps.Clone(Config.CmdlineArgs)` if it is non-empty. It is always overwritten, so an inherited value never stands. The flag `build-kernel-cmdline-args` defaults to `""`, which gives nil (`flags.go:995`).
4. `result.Balloon = &Balloon{Reporting: Config.FreePageReporting, Hinting: Config.FreePageHinting}`, always set. With flag defaults (`free-page-reporting=false` at `flags.go:334`; `free-page-hinting-config=null`, i.e. disabled, at `flags.go:715-721`) this is `{"reporting":false,"hinting":false}`. `Sandbox.Pause` re-stamps it from device truth (`O/pkg/sandbox/sandbox.go:2138-2142`). A VM without a balloon has mode `None`, which gives `(false,false)`, so the JSON comes out the same.
5. `result.CPUTemplate = nil` and `BuildCPUTemplate = nil`. Both become clones if `Config.CPUTemplate` is non-nil and non-empty. The flag `build-cpu-template` defaults to `null` (`flags.go:1001`).
6. Return `{Metadata: result, Cached: false, Hash: hash}`.

The JSON for `Start` is `{"start_command": "...", "ready_command": "...", "context": {...}}`. **No `omitempty` applies inside Start**, so empty strings are still written, and the `context` keys follow `Context`'s own omitempty rules (`template_metadata.go:60-64`, `:109-113`).

Upstream unit vectors:
- `cmdline_args_test.go:26-78`: this build's args win; no args clears inherited ones; nothing set stays empty.
- `cpu_template_test.go:21-108`: the same rules, plus "a template that changes nothing is not recorded", plus the stored template being a copy.

### Build (`builder.go:157-255`)
1. **Default user and workdir sent to envd `/init`** (`:170-182`):
   - `defaultUser := &currentLayer.Metadata.Context.User` and `defaultWorkdir := currentLayer.Metadata.Context.WorkDir`.
   - `utils.IsGTEVersion(ppb.Version, "v2.1.0")`. Error: `error checking build version: %w`, which is unreachable because the pipeline already checked.
   - If the version is lower: `defaultUser = &"user"` (consts.TemplateDefaultUser) and `defaultWorkdir = nil`.
2. **sbxConfig** (`:185-202`):
   - `Vcpu`, `RamMB`, `HugePages`, `FreePageReporting`, `FreePageHinting`;
   - `Envd{Version: EnvdVersion, DefaultUser, DefaultWorkdir}`. There are **no env vars and no access token**;
   - `FirecrackerConfig{KernelVersion, FirecrackerVersion}`.
   - `/init` sends `defaultUser`/`defaultWorkdir` with `omitempty` (`O/pkg/sandbox/envd/envd.gen.go:330-333`, `O/pkg/sandbox/envd.go:85-93`), so an empty user or a nil workdir sends no key.
3. **IO engine:** `featureFlags.StringFlag(ctx, BuildIoEngine)`, i.e. `"build-io-engine"`, default `"Sync"` (`flags.go:975`). Service Debug: `using io engine`.
4. **Options:** `[WithIoEngine(ioEngine)]`. Only `if sourceLayer.Cached` is `layer.ReservedBlocksOptions(ctx, ff, RootfsBlockSize())...` appended. That hook sets `tune2fs -r` on the host before boot when `build-reserved-disk-space-mb` > 0 (default 256, `flags.go:660`; `B/layer/create_sandbox.go:79-90`).
5. **`layer.NewCreateSandbox(sbxConfig, factory, finalizeTimeout, options...)`**. "Always restart the sandbox for the final layer to properly wire the rootfs path for the final template" (`:221`).
   - The cold boot is the same as M15 `CreateSandbox`: a fresh `block.NewEmpty` memfile over RAM with base build id = **source build id**, the mask template, `kvmClock` for envd at least `0.2.11`, `/sbin/init`, CmdlineArgs/CPUTemplate from config, then `WaitForEnvd(StartTypeCreate, 60s)` (`create_sandbox.go:111-188`).
   - RuntimeMetadata `BuildID = layerExecutor.Template.BuildID`, the requested id.
6. **`BuildLayer`** (`:233-245`) with:
   - `SourceTemplate: NewCacheSourceTemplateProvider(sourceLayer.Metadata.Template.BuildID)`;
   - `CurrentLayer: currentLayer.Metadata`;
   - `Hash: currentLayer.Hash`;
   - `UpdateEnvd: sourceLayer.Cached`;
   - `ActionExecutor: NewFunctionAction(postProcessingFn(userLogger))`;
   - **`BuildOrigin: storage.ObjectOriginTemplateBuild` = `"template_build"`**, where steps use `"template_build_cache"` (`S/pkg/storage/storageopts/storageopts.go:48`).
   - Inside, the same M15 steps run (`B/layer/layer_executor.go:79-166`, `:267-405`): envd update (`update envd: %w`), the action, `meta.NewVersionTemplate(...)`, `Pause`, `AddSnapshot`, then the async upload with `index.SaveLayerMeta(finalizeHash -> requested build id)`. Error: `pause and upload: %w`.
7. **Error wrap:** `error running start and ready commands in sandbox: %w`. This is internal unless a PhaseBuildError is inside.
8. **Result:** `{Metadata: finalLayer, Cached: false, Hash: currentLayer.Hash}`.

### postProcessingFn, in exact order (`builder.go:257-381`)
**Deferred tail, registered first and so run last.** It runs only when `e == nil`, and it runs **after** `commandsCancel` and `configCancel` (LIFO), meaning after the start command has been cancelled:
1. `packCertBundle(ctx, userLogger, proxy, sandboxID)` (`configure.go:86-112`):
   - `RunCommandWithLogger(DebugLevel, id "certs", packCertBundleCmd, Context{User: "root"})`, with no env and no workdir;
   - error `error packing CA cert bundle: %w`;
   - wrapped as **`PhaseBuildError(finalize)`**, i.e. a user error with step `finalize`.
2. `SyncChangesToDisk`, which runs `/usr/bin/busybox sync` as root. Error: `error running sync command: %w`, internal.

It runs **even when `meta.Start == nil`**.

**Body:**
1. **Configuration script.**
   - `configCtx` gets a 5 min timeout.
   - The script is rendered by `text/template` into `"provisioning-finish-script"` with `{EnvID: TemplateID, TemplateID: TemplateID, BuildID: bc.Template.BuildID /* requested */}`. Render error: `error executing provision script: %w`.
   - It runs through `RunCommandWithLogger(DebugLevel, id "config", script, Context{User:"root"})`. Error: `error running configuration script: %w`.
   - It is wrapped as **`PhaseBuildError(finalize, "configuration script failed: %w")`**.
   - Example user message: `configuration script failed: error running configuration script: exit status 1`.
   - On timeout: `configuration script failed: error running configuration script: context: context deadline exceeded`.
2. If `meta.Start == nil`, **return meta**. No start, ready or "Waiting" log appears; the cert bundle and sync still run in the defer.
3. `commandsCtx, commandsCancel := context.WithCancel(ctx)`; `var startCmdRun errgroup.Group`; `startCmdConfirm := make(chan struct{})`.
4. **Start command**, if `meta.Start.StartCmd != ""`:
   - User log **Info** `Running start command: <StartCmd>`.
   - `startCmdRun.Go` runs `RunCommandWithConfirmation(commandsCtx, proxy, userLogger, InfoLevel, "start", sandboxID, StartCmd, meta.Start.Context, startCmdConfirm)`. Its output lines are `[start] [stdout]: ...` and `[start] [stderr]: ...` at **Info**.
   - It runs with `Start.Context`: that context's user (Basic auth header), workdir as cwd, and env vars with the PATH suffix.
   - If `err != nil && !errors.Is(err, context.Canceled)`: call `commandsCancel()` and return `error running start command: %w`. Otherwise return nil.
   - **Confirmation:** `runCommandWithAllOptions` calls `close(confirmCh)` right after `processC.Start(...)` returns, whether or not it erred (`B/sandboxtools/command.go:171-177`). That is the moment the response headers arrive.
   - **Without a start command,** `close(startCmdConfirm)` is called immediately.
5. **Ready command:** `readyCmd := Start.ReadyCmd`. If it is empty:
   - `"sleep 0"` when `StartCmd` is empty too;
   - otherwise `GetDefaultReadyCommand(TemplateID)`, which is `fmt.Sprintf("sleep %d", 20)` = **`sleep 20`**. For the hard-coded template IDs `zegbt9dl3l2ixqem82mm`, `ot5bidkk3j2so2j02uuz` and `0zeou1s7agaytqitvmzc` it is `sleep 120` (`ready.go:77-86`).
6. **`runReadyCommand`** (`ready.go:25-75`) with `commandsCtx`:
   - User log **Info** `Waiting for template to be ready: <readyCmd>`.
   - `ctx = WithTimeout(commandsCtx, 10min)`.
   - Loop:
     - `RunCommandWithLogger(DebugLevel, id "ready", readyCmd, Start.Context)`. Output is `[ready] [stdout]: ...` at Debug;
     - success: Info `Template is ready`, then return nil;
     - failure: Debug `Template is not ready: <err>`, then `select`:
       - ctx done with `DeadlineExceeded`: return `ready command timed out after <time.Since(start)>`, a Go `Duration.String()` such as `10m0.003118234s`;
       - ctx done otherwise (cancelled, because the start command failed or the build was cancelled): Info **`Template is ready`**, then return **nil**;
       - `time.After(2s)`: retry.
   - Any error is wrapped as **`PhaseBuildError(finalize, "ready command failed: %w")`**, e.g. `ready command failed: ready command timed out after 10m0.5s`.
7. **Wait for confirmation:**
   - `select { case <-ctx.Done(): PhaseBuildError(finalize, "waiting for start command failed: %w", commandsCtx.Err()); case <-startCmdConfirm: }`;
   - the message is `waiting for start command failed: context canceled`.
8. `commandsCancel()`. This cancels the start command's **stream** only. The guest process keeps running and is captured in the snapshot, which is the point of a start command.
9. `startCmdRun.Wait()`. An error becomes **`PhaseBuildError(finalize, "start command failed: %w")`**, e.g. `start command failed: error running start command: exit status 1`.
10. Return meta. The defer then runs the cert bundle and sync.

**Behaviour edges worth contract tests:**
- **Start command exits non-zero before the ready command succeeds.** `commandsCancel` fires, the in-flight ready run fails with `context: context canceled`, Debug `Template is not ready: context: context canceled` appears, the select sees `Canceled`, and Info **`Template is ready`** is logged. The confirm channel is already closed, so `Wait` gives the failure message above. The log shows "Template is ready" right before the build fails.
- **Start command exits 0 quickly** (e.g. `exit 0`, `echo hi`). This is not an error and causes no cancel. The ready command runs normally.
- **Build cancelled during the ready loop.** The ready loop returns nil with "Template is ready". Then Go's `select` picks randomly between `ctx.Done()` (giving `waiting for start command failed: context canceled`) and the closed confirm. With the confirm, the start command is cancelled, then the cert bundle runs on a cancelled ctx and gives `error packing CA cert bundle: context: context canceled`. Both are PhaseBuildErrors, so the user-visible reason is **nondeterministic** in Go.
- The **integration test notes** that `[start] [stdout]` lines can be lost when the ready command returns in milliseconds, because the stream is cancelled at once (`T/api/templates/distro_build_test.go:83-90`).

### configure.sh, embedded verbatim (`configure.go:24-26`; the file is `B/phases/finalize/configure.sh`, 102 lines, ending in `\n`)
The rendered script runs as root at Debug level with id `config`. Its echo lines show up as `[config] [stdout]: Starting configuration script` and so on. `BASH_XTRACEFD=1` is set but there is no `set -x`.

```bash
#!/bin/bash
export BASH_XTRACEFD=1
set -euo pipefail

echo "Starting configuration script"

cat <<EOF > /.e2b
ENV_ID={{ .TemplateID }}
TEMPLATE_ID={{ .TemplateID }}
BUILD_ID={{ .BuildID }}
EOF

# Create default user. useradd is in shadow(-utils) on every supported distro
# family; -m creates the home dir, -s the shell. A creation failure is a real
# error — a template whose default user silently doesn't exist fails much more
# confusingly later.
echo "Create default user 'user' (if doesn't exist yet)"
if ! id -u user >/dev/null 2>&1; then
    useradd -m -s /bin/bash user
fi
# useradd -m skips skeleton files when /home/user already exists, so copy them
# explicitly (no-clobber). Not every image ships /etc/skel — say so instead of
# hiding it. Walked entry by entry because `cp -n` exits non-zero when it skips
# an existing file on coreutils >= 9.2 (Fedora 40+), which is a skip, not a failure.
if [ -d /home/user ]; then
    if [ -d /etc/skel ]; then
        echo "Copy skeleton files to /home/user (keeping existing files)"
        find /etc/skel/ -mindepth 1 \( -type f -o -type d -o -type l \) -print0 | while IFS= read -r -d '' src; do
            rel="${src#/etc/skel/}"
            if [ -e "/home/user/$rel" ] || [ -L "/home/user/$rel" ]; then
                echo "skel: keeping existing $rel"
                continue
            fi
            parent="${rel%/*}"
            if [ "$parent" != "$rel" ]; then
                if [ -L "/home/user/$parent" ]; then
                    echo "skel: skipping $rel (parent is a symlink)"
                    continue
                fi
                if [ ! -d "/home/user/$parent" ]; then
                    echo "skel: skipping $rel (parent is not a directory)"
                    continue
                fi
            fi
            if [ -d "$src" ] && [ ! -L "$src" ]; then
                # Created, not copied: cp -a on a dir would drag specials past the type filter.
                mkdir "/home/user/$rel"
                chmod "$(stat -c %a "$src")" "/home/user/$rel"
            else
                cp -a "$src" "/home/user/$rel"
            fi
        done
    else
        echo "No /etc/skel on this image; skipping skeleton copy"
    fi
fi

echo "Add sudo to 'user' with no password"
# Admin group differs by distro (sudo on Debian/Ubuntu, wheel elsewhere); the
# NOPASSWD sudoers entry below is what actually grants privileges. The group
# comes from the distro profile, resolved and persisted by provisioning —
# defined once in distro.go, not re-probed here. Templates built before
# distro.env existed (FROM-template parents reuse their rootfs without
# re-provisioning) fall back to probing the two known groups.
if [ -f /usr/local/share/e2b/distro.env ]; then
    . /usr/local/share/e2b/distro.env
fi
if [ -n "${E2B_ADMIN_GROUP:-}" ]; then
    if ! getent group "$E2B_ADMIN_GROUP" >/dev/null; then
        echo "ERROR: the '$E2B_ADMIN_GROUP' group from the distro profile does not exist on this image" >&2
        exit 1
    fi
    usermod -aG "$E2B_ADMIN_GROUP" user
elif getent group sudo >/dev/null; then
    usermod -aG sudo user
elif getent group wheel >/dev/null; then
    usermod -aG wheel user
else
    echo "ERROR: neither the sudo nor the wheel group exists on this image" >&2
    exit 1
fi
passwd -d user
# Skip the append when the entry is already present.
if grep -q '^user ALL=(ALL:ALL) NOPASSWD: ALL' /etc/sudoers; then
    echo "sudoers entry already present"
else
    echo "user ALL=(ALL:ALL) NOPASSWD: ALL" >>/etc/sudoers
fi

echo "Give 'user' ownership to /home/user"
mkdir -p /home/user
chown -R user:user /home/user

echo "Give 777 permission to /usr/local"
chmod -R 777 /usr/local

echo "Create /code directory"
mkdir -p /code
echo "Give 777 permission to /code"
chmod -R 777 /code

echo "Finished configuration script"
```

Hash input: the bytes above with `{{ .TemplateID }}` and `{{ .BuildID }}` **unrendered**. The rendered `/.e2b` contains `ENV_ID=<templateID>`, `TEMPLATE_ID=<templateID>` and `BUILD_ID=<requested build id>`.

Upstream tests in `configure_test.go:30-340`:
- `bash -n` validity for both scripts;
- no `cd` calls;
- the skeleton copy cases: login-shell cd hook under nounset, symlinked skel, dangling home symlink, files under a non-dir parent, preserved skel dir modes, symlinked dotfiles, newline filenames, a symlinked parent, special files skipped.

### packCertBundleCmd, verbatim (`configure.go:45-83`; a Go raw string, tab-indented, ending in `\n`)
```sh
set -e
# Recreate dpkg-recorded paths under /etc/ssl/certs lost to this fresh boot's
# tmpfs re-seed (build-layer dpkg writes never reach the rootfs), then settle
# dpkg before update-ca-certificates fires the packages' triggers against them.
if command -v dpkg-query >/dev/null 2>&1; then
	dpkg-query -W -f='${db:Status-Abbrev} ${binary:Package}\n' 2>/dev/null \
		| awk '/^.[iUFHWt]/{print $2}' \
		| xargs -r dpkg -L 2>/dev/null \
		| grep '^/etc/ssl/certs/' | sort -u > /run/e2b-cert-paths
	# dpkg's file db records no entry types; md5sums+conffiles enumerate its files.
	{ sed -n 's|^[0-9a-f]\{32\}  |/|p' /var/lib/dpkg/info/*.md5sums; cat /var/lib/dpkg/info/*.conffiles; } 2>/dev/null \
		| grep '^/etc/ssl/certs/' | sort -u > /run/e2b-cert-files
	comm -23 /run/e2b-cert-paths /run/e2b-cert-files \
		| while IFS= read -r path; do
			[ -e "$path" ] || mkdir -p "$path"
		done
	rm -f /run/e2b-cert-paths /run/e2b-cert-files
	# start_cmd may still hold the dpkg lock; retry instead of failing instantly.
	n=0
	until dpkg --configure -a; do
		n=$((n+1))
		[ "$n" -ge 5 ] && exit 1
		sleep 5
	done
fi
if command -v update-ca-certificates >/dev/null 2>&1; then
	update-ca-certificates
fi
# update-ca-certificates ignores hook failures; never snapshot half-configured state.
if command -v dpkg-query >/dev/null 2>&1; then
	broken=$(dpkg-query -W -f='${db:Status-Abbrev} ${Package}\n' 2>/dev/null | awk '/^iF/{print $2}' | tr '\n' ' ')
	if [ -n "$broken" ]; then
		printf 'cert bundle: packages left half-configured: %s\n' "$broken" >&2
		exit 1
	fi
fi
mkdir -p /usr/local/share/e2b
tar -C /etc/ssl/certs -chf /usr/local/share/e2b/ssl-certs.tar .
```
- It runs as root (`Context{User:"root"}`) at Debug level with id `certs`.
- Its stderr text `cert bundle: packages left half-configured: <pkgs> ` (trailing space from `tr`) appears as `[certs] [stderr]: cert bundle: packages left half-configured: ...`.
- A non-zero exit gives the **user error** `error packing CA cert bundle: exit status 1`, step `finalize`.

### Log lines a finalize phase produces
All go to the step user logger, with fields `phase=finalize, step_type=finalize, step_number=<nil>, step=finalize`. In order:

1. Info `[finalize] Finalizing template build [<hash>]`.
2. If source is cached:
   - Debug `Updating envd to version v<ver>`;
   - Debug `[update-envd-replace] [...]` lines.
3. Debug `[config] [stdout]: Starting configuration script` ... `[config] [stdout]: Finished configuration script`.
4. Info `Running start command: <cmd>`.
5. Info `Waiting for template to be ready: <ready>`.
6. Info `[start] [stdout]: ...` lines, interleaved with the following.
7. Debug `[ready] [...]` and `Template is not ready: <err>`.
8. Info `Template is ready`.
9. Debug `[certs] [stdout]: ...` (update-ca-certificates output).
10. Debug `Adding layer to cache: <requested id>`.
11. Debug `Uploading layer: <requested id>`.
12. Debug `Saved: <requested id>`, asynchronously, possibly after the optimize line.

---

## 3. Optimize (`B/phases/optimize/*.go`)

### Constants
- `prefetchTimeout = 5 * time.Minute` is the resumed sandbox's **lifetime** (`builder.go:29`).
- `prefetchIterations = 2` (`:35`).

### Texts
- `Prefix()` = `"optimize"` and `String()` = `"Optimizing template"`.
- `Metadata()` = `{Phase: "optimize", StepType: "optimize"}`.
- The runPhase line is `[optimize] Optimizing template [<hash>]`. It is the **only user-visible line**, because `Build` ignores `userLogger` (`builder.go:119`).
- **Hash:** `HashKeys(sourceLayer.Hash, "optimize")` (`:91-95`). Rust `hash::optimize_hash` matches.
- **Layer:** `{Metadata: sourceLayer.Metadata, Cached: false, Hash}`. It never hits the cache and never reads the index.

### Build (`builder.go:117-183`)
1. Service Info: `Collecting prefetch mapping from template resume`.
2. `templateCache.GetTemplatePinned(ctx, sourceLayer.Metadata.Template.BuildID /* requested */, false, true)`. Error: **`failed to get template from cache: %w`**. This is the phase's **only** returned error, and it is internal.
3. **`collectMemoryPrefetchMapping`** (`:185-238`):
   - sbxConfig `{Vcpu, RamMB, HugePages, Envd{Version}}` plus FirecrackerConfig, with **no** default user, workdir, env or token, and no FreePage fields;
   - `layer.NewResumeSandbox(sbxConfig, factory, 5min)`.
   - The resume path itself resolves the default user from metadata. `resolveEnvdDefaultUser` gives `"user"` only when `meta.Context.User == "user"`, and the workdir is never sent (`O/pkg/sandbox/sandbox.go:1518-1521`, `envd.go:680-692`).
   - For `i` in `0..2`:
     - service Debug `starting sandbox run %d/%d for prefetch collection` (1-based);
     - `runSandboxAndCollectPrefetch` resumes the sandbox. Error: `failed to resume sandbox: resume sandbox: %w`. The resume waits for envd `/init` (StartTypeResume);
     - **immediately** `sbx.MemoryPrefetchData(ctx)`, which stops tracking and copies the tracked block map. Error: `failed to get prefetch data: failed to get prefetch data: %w`;
     - `defer sbx.Close(ctx)`, with no pause;
     - a run error becomes `failed to collect prefetch data from run %d: %w` (1-based).
   - `computeCommonPrefetchEntries` (`prefetch.go:12-56`):
     - take each index of run 0 that is present in **every** run;
     - order = `sum(orders)/len(runs)`, integer division;
     - the access type is the first run's, or `Read` if any run differs.
   - No common entries: service Debug `no common blocks found for prefetch mapping`, then return `nil, nil`.
   - Otherwise `metadata.PrefetchEntriesToMapping(entries, blockSize)` (`O/pkg/template/metadata/prefetch.go:15-44`) sorts by `Order` (unstable, Order only), then builds `Indices`, `AccessTypes` (`"r"`/`"w"`/`"p"`) and `BlockSize`.
4. **A collection error** gives service Warn `failed to collect prefetch mapping, continuing without prefetch` (error), and returns `{Metadata: sourceLayer.Metadata, Cached:false, Hash: currentLayer.Hash}`. No metadata write happens.
5. **`updatedMetadata := sourceLayer.Metadata.WithPrefetch(&Prefetch{Memory: mapping})`.**
   - `WithPrefetch` (`template_metadata.go:325-339`) keeps Version, Template, Context, Start, From*, CmdlineArgs, Balloon and the CPU templates, and **drops** FilesystemOnly and FsQuiesced.
   - **When nothing was common, `mapping == nil`, so the JSON gets `"prefetch":{"memory":null}`, and it is still uploaded.**
6. **`updateMetadata`** (`:261-271`):
   - `metadata.UploadMetadata(ctx, pb.templateStorage /* raw, not NFS-wrapped */, t, Config.ObjectMetadata("template_build"))` (`prefetch.go:48-75`);
   - this is `OpenBlob(<buildID>/metadata.json)` (error `failed to open metadata object: %w`), then `json.Marshal(t)` (error `failed to serialize metadata: %w`), then `Put(bytes, WithMetadata{team_id, template_id, build_origin:"template_build"})` (error `failed to write metadata: %w`);
   - then `templateCache.Invalidate(t.Template.BuildID)`, which deletes and retires the pinned entry so the next lookup re-fetches from storage (`O/pkg/sandbox/template/cache.go:777-795`).
   - An error gives service Warn `failed to upload prefetch metadata, continuing without prefetch`, and returns the source metadata.
7. Service Info `Collected prefetch mapping with memory blocks` (block_count = `len(Indices)`, or 0). Returns `{Metadata: updatedMetadata, Cached:false, Hash}`.

### The race (Go behaviour, documented)
- The finalize upload's `upload.Run` writes `<id>/metadata.json` from the **local metafile**, which has no prefetch (`O/pkg/sandbox/build_upload_v4.go:77`). It runs inside `UploadErrGroup` and waits for parent uploads first.
- Optimize `Put`s the prefetch version **without** calling `upload.Wait`. `Upload.Wait` exists, with the doc comment "re-uploading the metadata object without racing the upload's own metadata write" (`build_upload.go:189-199`), but no caller uses it (grep).
- So the final `metadata.json` may lose the prefetch when the finalize upload's metadata write lands after optimize's `Put`. That is likely when earlier layers are still uploading, because the child upload waits on its parents.

### Flags
No flag gates optimize. Prefetch *consumption* flags (`resume-prefetch-source="init"`, `memory-prefetch-*`, `pause-resume-prefetch-*`, `flags.go:580-641`) belong to resume, not to this phase.

Unit vectors are in `prefetch_test.go:13-297`:
- empty input gives nil;
- a single run gives all entries;
- complete overlap;
- partial overlap;
- no overlap gives empty;
- differing access types give read;
- same types are preserved;
- the three-run intersection;
- the average order rounds down;
- three runs with mixed types.

---

## 4. The tail of `Build` and how the requested build id becomes the final template

### runBuild after `phases.Run` (`B/builder.go:435-461`)
1. `phases.Run` error: return it (the M13 defers then run).
2. `bc.UploadErrGroup.Wait()`. An error is internal: `error waiting for layers upload: %w`.
3. `getRootfsSize(ctx, builder.templateStorage /* raw */, Paths{BuildID: lastLayerResult.Metadata.Template.BuildID})` (`:502-518`):
   - `OpenBlob(<id>/rootfs.ext4.header)` (error `error opening rootfs header object: %w`);
   - `header.Deserialize` (error `error deserializing rootfs header: %w`);
   - returns `Metadata.Size`;
   - wrapped as `error getting rootfs size: %w`.
   - This is the final layer's rootfs device size: the base size, or the grown size when ensurefreedisk ran.
4. Service Info `rootfs size` with `size`.
5. **Result** (`:455-461`):
   - `EnvdVersion: bc.EnvdVersion`;
   - `KernelVersion` and `FirecrackerVersion` from Config;
   - `RootfsSizeMB: units.BytesToMB(size)` (>>20);
   - `SchedulingMetadata: templateSchedulingMetadata(ctx, templateCache, lastBuildID)`.
6. **`templateSchedulingMetadata`** (`:464-480`) calls `cache.GetTemplatePinned(ctx, buildID, false, false)`, which re-fetches because optimize invalidated the entry.
   - Any error gives `nil`, silently.
   - If the template implements `SchedulingMetadata(ctx)`, it returns `scheduling.FromHeaders(rootfsHeader.Metadata.BuildId, memfileDurableHeader /* nil if not ready or absent */, rootfsHeader, 0)` (`O/pkg/sandbox/template/storage_template.go:386-430`).
7. `create_template.go:285-293` maps this into `TemplateBuildMetadata{RootfsSizeKey: int32(RootfsSizeMB), EnvdVersionKey, KernelVersion, FirecrackerVersion, SchedulingMetadata}`.
8. **Outer defers** (`builder.go:139-264`), already in Rust from M13/M15:
   - the deferred upload Wait;
   - removal of the `paths.BuildID` prefix on error, which removes only the **requested-id** objects, i.e. the finalize layer's files, never the intermediate layers;
   - the context-to-user-error wrap;
   - the panic recovery;
   - `Build finished, took <dur truncated to s>` or `Build failed: <reason message>`.

### How `TemplateConfig.BuildID` ends up pointing at the final artifacts
1. `create_template.go:62-64` builds `paths := storage.Paths{BuildID: cfg.GetBuildID()}` and passes it as `Builder.Build(ctx, paths, ...)`, which becomes `bc.Template` (`builder.go:269-278`).
2. Base, user, step and resize-disk layers each mint `uuid.NewString()` build ids (M14/M15). Every build sandbox's `RuntimeMetadata.BuildID` is `bc.Template.BuildID`, but that is only identity and logging.
3. **`finalize.Layer` stamps `Template.BuildID = ppb.Template.BuildID`** (`finalize/builder.go:116-121`).
4. `BuildLayer` keeps it:
   - `meta.NewVersionTemplate({BuildID: cmd.CurrentLayer.Template.BuildID, ...})`;
   - `sbx.Pause(ctx, meta, SnapshotUseCaseBuild)` writes a snapshot whose diff headers carry **that** build id;
   - `templateCache.AddSnapshot(meta.Template.BuildID, ...)` registers it locally;
   - `sandbox.NewUpload` uploads `<requested>/memfile`, `<requested>/rootfs.ext4` (diffs) plus `.header` files, `snapfile` and `metadata.json`.
   - The headers map unchanged blocks to the **earlier layers' build ids**: the rootfs to the last step layer's chain, the memfile to `uuid.Nil` with base build id = the source layer.
5. After the upload lands, `index.SaveLayerMeta(finalizeHash, {build_id: requested})` runs.
6. Optimize keeps the same `Template.BuildID` and overwrites only `<requested>/metadata.json`.
7. Therefore `lastLayerResult.Metadata.Template.BuildID == cfg.BuildID`, and the API's `env_builds` row for `cfg.BuildID` resolves directly. Nothing is copied or aliased.

### FROM TEMPLATE later
A later `FROM TEMPLATE` build uses `index.Cached(fromTemplate.BuildID)`, which reads `<requested>/metadata.json` (M14). So the final metadata (Start, Context, Prefetch) is what child templates inherit; `BasedOn` keeps Start but not Prefetch.

---

## 5. Integration tests that exercise finalize (CI contract vectors)

All of these use `buildTemplate` (`T/api/templates/build_template_test.go:43-145`): POST `/v3/templates` (2 CPU, 1024 MB), POST `/v2/templates/{id}/builds/{buildID}`, then poll status with `logsOffset` and `level` (Info, or Debug with `EnableDebugLogs`) until `ready` or `error` (`reason.message`).

1. **ENV visible to the start command** (`:225-240`):
   - steps `ENV ENV_VAR "Hello, World!"`;
   - StartCmd `: "${ENV_VAR:?ENV_VAR is not set or empty}"; echo "$ENV_VAR"`;
   - ReadyCmd `sleep 5`;
   - the build must succeed.
2. **WORKDIR in the start command** (`:432-447`): `WORKDIR /app`, StartCmd `[[ "$(pwd)" == "/app" ]] || exit 1`, ReadyCmd `sleep 5`.
3. **Start command override in a derived template** (`:555-623`):
   - base: ENV `BASE_VAR=base_value`, WORKDIR `/app/base`, StartCmd `[[ ! -f /override_check.txt ]] || exit 97; echo 'base_command_executed'`, Ready `sleep 5`;
   - derived FROM base: ENV, WORKDIR `/app/derived`, RUN `echo 'override_expected' > /override_check.txt` as `root`, StartCmd `exit 0`, Ready `sleep 5`;
   - both succeed. This proves that `Start` is replaced and that the start command runs in the derived context.
4. **Start inherited from the base, run with the base's context** (`:732-790`):
   - base StartCmd `: "${BASE_VAR:?BASE_VAR not set}"; [[ "$BASE_VAR" == "base_value" ]] || exit 1; [[ "$(pwd)" == "/app/base" ]] || exit 2; echo "Base start command runs with original base context"`;
   - the derived template only adds `WORKDIR /app/derived` and no start or ready;
   - it must still succeed. This proves the inherited `Start.Context` is used, not the current one.
5. **FROM TEMPLATE start command uses the inherited env and workdir** (`:686-730`): StartCmd `: "${START_ENV:?...}"; [[ "$START_ENV" == "start_value" ]] || exit 1; [[ "$(pwd)" == "/start/workdir" ]] || exit 2; echo '...'`.
6. **Start/ready execution and log order** (`:881-988`):
   - (a) RUN `echo 'Setting up template'`, StartCmd `echo 'Hello, World!'`, ReadyCmd `sleep 2`. Expected substrings: `Running start command`, `[start] [stdout]: Hello, World!`, `Waiting for template to be ready: sleep 2`, `Template is ready`;
   - (b) ENV `TEST_VAR=test_value`, StartCmd `echo "Starting with TEST_VAR=$TEST_VAR"; echo 'Initialization complete'`, ReadyCmd `echo 'Checking readiness...'; sleep 1; echo 'Ready check complete'`. Expected: `[start] [stdout]: Starting with TEST_VAR=test_value`, `[start] [stdout]: Initialization complete`, `Waiting for template to be ready`, `Template is ready`;
   - order: last index of `Running start command` < last index of `Waiting for template to be ready` < last index of `Template is ready`.
7. **Distro families** (`T/api/templates/distro_build_test.go:21-104`): no steps, StartCmd `echo 'Sandbox started'`, ReadyCmd `command -v ss && command -v curl && sleep 1`. Expected: `[start] [stdout]: Sandbox started` and `Template is ready`. The SELinux image uses ReadyCmd `grep -qw selinux=0 /proc/cmdline && ! test -e /sys/fs/selinux/enforce && sleep 1` (`:131-155`).
8. **Cert bundle** (`T/envd/ca_cert_build_test.go:65-136`): the CA added in a RUN as root, or by a start command (`... && sleep infinity`, ready polls for the file), must appear in `/usr/local/share/e2b/ssl-certs.tar` inside a sandbox built from the template.
9. **Suggested extra vectors** that are cheap and pin the edge texts:
   - StartCmd `exit 3` with ReadyCmd `sleep 5`: reason `start command failed: error running start command: exit status 3`, and the log contains `Template is ready` first;
   - ReadyCmd `exit 1` alone: it retries every 2 s for 10 min, too slow for CI;
   - a start command only, with no ready command: the log has `Waiting for template to be ready: sleep 20`;
   - a ready command only: `Waiting for template to be ready: <ready>` and no `Running start command`;
   - no start and no ready from an image: no `Waiting...` line at all;
   - `[finalize] Finalizing template build [` and `[optimize] Optimizing template [` appear in the Info log;
   - `/.e2b` contains `BUILD_ID=<the requested build id>`, checked from a sandbox.

---

## 6. Feature flags involved (`S/pkg/featureflags/flags.go`)

| Flag | Key | Default | Effect in M16 |
|---|---|---|---|
| `BuildEnsureFreeDiskSpace` | `build-ensure-free-disk-space` | `false` (`:468`) | adds the resize-disk phase |
| `BuildIoEngine` | `build-io-engine` | `"Sync"` (`:975`) | finalize drive IO engine |
| `BuildReservedDiskSpaceMB` | `build-reserved-disk-space-mb` | `256` (`:660`) | pre-boot `tune2fs -r`, only when the source is cached |
| `UseNFSCacheForBuildingTemplatesFlag` | `use-nfs-for-building-templates` | `env.IsDevelopment()` (`:161`) | finalize's storage wrapper (no visible effect) |
| `BuildEnvdVersion` | `build-envd-version` | `DEFAULT_ENVD_VERSION` env or `DefaultEnvdVersion` (`:974`) | the envd binary copied by UpdateEnvd |
| `FreePageReportingFlag` | `free-page-reporting` | `false` (`:334`) | `balloon.reporting` |
| `FreePageHintingConfig` | `free-page-hinting-config` | `null`, i.e. disabled (`:715`) | `balloon.hinting` |
| `BuildKernelCmdlineArgs` | `build-kernel-cmdline-args` | `""` (`:995`) | `cmdline_args`, nil by default |
| `BuildCPUTemplate` | `build-cpu-template` | `null` (`:1001`) | `cpu_template` and `build_cpu_template`, nil by default |

---

## 7. Rust side: what to reuse and the gaps

**Already in place:**
- `hash::finalize_hash(previous, configure_script, cert_bundle_command)` and `hash::optimize_hash` (`R/builder/hash.rs:47-53`), with correct key order and tests.
- `phase.rs`:
  - `Phase` trait, `run_phases`, `layer_info`;
  - `BuildError::user(&meta, msg)`, where `meta.step()` gives `"finalize"` for a nil step number;
  - `internal_context`, which keeps user errors, so the `error running start and ready commands in sandbox:` wrap only reaches internal errors, as in Go.
- `layer.rs` `LayerExecutor::build_layer`:
  - source → sandbox → optional `update_envd` → action → `new_version` → `pause_and_upload`;
  - `cold_boot` with kvm clock, `/sbin/init`, `Sync`, envd wait, and `memory_base(source build id)` (`R/server/node/layers.rs:69-75`);
  - `sync_changes_to_disk`.
- `uploads.rs` `LayerUploads`: async upload, then index save, then `Saved: <id>`; plus `wait` (first error), already awaited by `pipeline.rs:113-116` and `service.rs:94-98`.
- `sandbox_command.rs`: `run_logged` (logStream parity, `[id] [stdout]: line`), `run_quiet` and `ROOT`.
- `metadata.rs`:
  - `TemplateMetadata` with `prefetch`, `cmdline_args`, `balloon`, `cpu_template`, `build_cpu_template`, `filesystem_only`;
  - `new_version` (= Go `NewVersionTemplate`);
  - `with_prefetch(None)`, which already serialises `"prefetch":{"memory":null}` (test at `R/template/metadata.rs:181-193`).
- `template/optimize.rs` `collect_prefetch` (2 runs, intersection, `None` when disjoint) over `memory/trace.rs` `Trace::intersect` and `to_mapping`: the average order uses integer division, and a differing access type gives Read, as in Go.
- `sandbox/resume.rs:99` `Sandbox::memory_trace()`.
- `template.rs:103` `Template::update_metadata`.
- `server/scheduling.rs` `from_headers(build_id, memfile, rootfs, new_bytes)`.
- `node/layers.rs` `resume_layer` (default user from `metadata.context.user == "user"`, matching Go resume).
- `service.rs`: the outcome lines, prefix removal and cancel mapping.

**To build or fix:**
1. **`pipeline.rs` phases:**
   - append `FinalizePhase`, then `OptimizePhase`, after the steps;
   - leave out the resize-disk phase (flag default false) and record it in `docs/BACKLOG.md` with the full section 1 spec;
   - pass `config.build_id`, `start_command`, `ready_command`, `version`, `force` and the TemplateID into the finalize phase.
2. **Typed `Start`:** replace `start: Option<Value>` (`R/template/metadata.rs:17`) with `Option<Start { start_command: String, ready_command: String, context: Context }>`. Serialize all three keys with no skip; the context keeps its own omit rules. Keep accepting legacy JSON.
3. **`FinalizePhase::layer`:**
   - Start only when start or ready is non-empty, with the current context;
   - `template.build_id = <requested build id>`;
   - `cmdline_args = {}`;
   - `balloon = Some({"reporting":false,"hinting":false})`, since Rust has no balloon, which matches Go's defaults and the `None` device mode;
   - `cpu_template = build_cpu_template = None`;
   - `cached: false`.
4. **The finalize cold boot needs things `LayerExecutor` cannot express yet:**
   - (a) **EnvdInit with `default_user` and `default_workdir`.** `cold_boot_layer` uses `build_sandbox`, which uses `build_envd(None)` (`R/server/node.rs:241-250`). Add e.g. `LayerBuild.envd_defaults` or a `SandboxShape.default_user/default_workdir`. The values are `context.user` / `context.workdir` for version ≥ v2.1.0, else `Some("user")` / `None`. Rust omits empty strings, which matches Go `omitempty`.
   - (b) **Reserved blocks only when `source.cached`.** `cold_boot` always calls `.pre_boot(reserved_blocks())` (`R/builder/layer.rs:207`), so make it a `LayerBuild` field. Steps pass `true` because they always cold boot only when cached.
   - (c) **Build origin per layer.** `upload` hardcodes `BUILD_ORIGIN = "template_build_cache"` (`R/builder/layer.rs:38`, `:281`); finalize needs `"template_build"`.
   - (d) Optional: `SandboxCreator::ColdBoot` regardless of `source.cached`. That already works by passing `ColdBoot` explicitly.
5. **postProcessing action** (e.g. `builder/phases/finalize.rs` plus `finalize/configure.sh` via `include_str!`, byte-exact):
   - render with plain `replace("{{ .TemplateID }}", ..)` and `replace("{{ .BuildID }}", ..)`. The script has no other template syntax;
   - the run order and level/id table from section 2: config Debug `config`, start Info `start`, ready Debug `ready`, certs Debug `certs`;
   - all user errors via `BuildError::user(&finalize_meta, ...)` with the exact texts.
   - The configuration and ready timeouts are `tokio::time::timeout`, and they map to `context: context deadline exceeded` inside the wrapped text, the same as a Go ctx deadline in `runCommandWithAllOptions`.
6. **Start command in the background:**
   - `envd::process::run_command` awaits `.send()` (headers) before streaming (`R/envd/process.rs:92-103`). Add a confirmation hook, e.g. an `on_started` oneshot fired after `send()` returns (success or error), to mirror `close(confirmCh)` at `B/sandboxtools/command.go:174`.
   - Run it with a child `CancellationToken`. Map cancellation to `context: context canceled` and skip it like `errors.Is(err, context.Canceled)`.
   - Spawn it, or `select!` it alongside the ready loop. Cancel on a start failure so the ready loop takes the "cancelled, log `Template is ready`, return Ok" branch.
   - Dropping the reqwest stream must **not** kill the guest process. Verify that envd-rs keeps the process running when the Start stream is cancelled, which Go envd does. This needs a contract test: the start command `sleep infinity` must survive into the snapshot (`ca_cert_build_test` relies on it).
7. **Ready loop:**
   - a 2 s interval and a 10 min budget;
   - Debug `Template is not ready: <err>`;
   - the timeout text needs the **full Go `Duration.String()`** (e.g. `10m0.003118234s`). Only a whole-seconds `go_duration_seconds` exists (`R/builder/service.rs:169-177`), so add a full formatter (h/m/s with up to 9 fractional digits, trailing zeros trimmed; `ms`/`µs`/`ns` units below 1 s).
   - **Rust divergence:** Go's random `select` on build cancel. Choose `waiting for start command failed: context canceled` as the deterministic branch and note it in PARITY.
8. **OptimizePhase:**
   - `layer` returns the source metadata, not cached;
   - `build`:
     - (a) `node.layer_source(requested id)`. An error is the only returned one: `failed to get template from cache: <e>`;
     - (b) `collect_prefetch(|run| ...)`: lease a slot, `resume_layer`, `.resume(snapfile)`, `sandbox.memory_trace()` (treat `None` as an error), `stop()`;
     - (c) errors are only `tracing::warn!` with the Go texts. Run numbers are 1-based in `failed to collect prefetch data from run %d`. The current `collect_prefetch` is 0-based (`prefetch run {run} timed out`) and adds a per-run tokio timeout that Go lacks. Go's 5 min is a sandbox lifetime, so align the texts;
     - (d) `with_prefetch(mapping)`, a storage put of `<id>/metadata.json` with object metadata `{team_id, template_id, build_origin:"template_build"}`, then `Template::update_metadata` (or invalidate).
   - **Race decision:** to land Go's intended result deterministically, also rewrite the local `metadata.json` that `upload_attempt` reads (`R/server/node.rs:664-680`, `metadata_path`) before the put, or await the finalize upload first. Go's order is not guaranteed either way; note this in PARITY.
9. **Result:**
   - `result_metadata` returns `scheduling_metadata: None` (`R/builder/pipeline.rs:159-167`). Compute it from the final template, i.e. `node.templates.get(requested).build()` then `from_headers(build.rootfs.metadata.build_id, Some(&build.memfile), &build.rootfs, 0)`, mapping any error to `None` as Go does;
   - `rootfs_size` collapses Go's two texts (`error opening rootfs header object` / `error deserializing rootfs header`) into one `error getting rootfs size: {e}` (`:148-157`). That is invisible (internal), but match it anyway;
   - add the service log `rootfs size`.
10. **Proto:** `freeDiskSizeMB` (field 17) is already in `proto/orchestrator/template-manager.proto:93-95`. Only resize-disk uses it, so wire it in when that phase is built.
11. **Smaller observed gap (not M16-specific):** `EnvdInit::body` always sends `"envVars": {}` (`R/envd.rs:44-47`), where Go's `EnvVars` is `omitempty` and sends nothing for a nil or empty map. Add it to the backlog unless envd treats them differently.
