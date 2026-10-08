# M15 research digest: steps phase and layer executor (Go upstream to envd-rs)

Paths: `B` = `~/IdeaProjects/e2b-runtime-origin-main-review/packages/orchestrator/pkg/template/build`, `O` = `.../packages/orchestrator`, `S` = `.../packages/shared`, `R` = `~/IdeaProjects/envd-rs/crates/orchestrator/src`.

## Spec corrections found (spec "Layer executor (M15)" and M13 sections against Go)

1. **Step hash `filesHash`:** spec says `filesHash or ""`. Go uses `utils.Sprintp(step.FilesHash)`, which gives `"<nil>"` for nil and the value otherwise, so an empty string stays `""` (`B/phases/steps/hash.go:15-20`, `S/pkg/utils/ptr.go:15-21`). Rust `hash::step_hash` already does this correctly. Fix the spec.
2. **Cold boot vs resume:** the rule is `sourceLayer.Cached` (`B/phases/steps/builder.go:187-196`). A cached source (from the index, the base phase hit, FROM TEMPLATE, or a cached previous step) gets a cold boot plus `UpdateEnvd`. A source **built in this build** gets a resume, with no envd update. "From the base phase" is true only when the base was a cache hit. A freshly built base is resumed.
3. **The default USER phase reuses StepBuilder.** It is `steps.New(... TemplateStep{Type:"USER", Args:[user,"true"], Force: cfg.Force}, stepNumber 1, zapcore.DebugLevel)`. Only `Prefix/String/Metadata/Hash` are overridden (`B/phases/user/builder.go:44-81`). Because Go has no virtual dispatch through embedding, `StepBuilder.Build` and `Layer` still call `sb.Metadata()` of the **StepBuilder**. So a failure inside the default user phase becomes `PhaseBuildError{Phase:"steps", Step:"1"}`, not `"base"`. The log fields still come from `UserBuilder.Metadata()` (`phase=base, step_type=base, step_number=<nil>`), because `runPhase` calls it through the interface.
4. **Uploads are asynchronous.** `UploadSnapshot` returns nil right after `AddSnapshot` and `NewUpload`. The upload and the index write run in `UploadErrGroup` (see section 2). Rust M14 `BaseBuild::upload` uses the synchronous `upload_now`, which M15 must change for every layer, base included.
5. **ENV values are evaluated as root with no context env and no workdir** (`B/commands/env.go:83-85`). `$VAR` therefore expands against root's login-shell env, not the template's accumulated env vars.

---

## 1. Steps phase (`B/phases/steps/*.go`, `B/commands/*.go`)

### Construction
- `CreateStepPhases` (`factory.go:22-55`) creates one `StepBuilder` per `bc.Config.Steps[i]`, with `stepNumber = i+1` and `defaultLoggingLevel = zapcore.InfoLevel` (`factory.go:20`).
- **Phase order** (`B/builder.go:410-433`):
  1. `base`;
  2. `user`, only if `utils.IsGTEVersion(bc.Version, "v2.1.0")`. A version error is internal: `error checking build version: %w` (`builder.go:414-420`);
  3. the steps;
  4. `ensurefreedisk`, only with the flag `BuildEnsureFreeDiskSpace`;
  5. `finalize`;
  6. `optimize`.
- `forceSteps` (`builder.go:482-500`) starts with `shouldRebuild = cfg.Force != nil && *cfg.Force`. For each step in order, `step.Force != nil && *step.Force` sets `shouldRebuild = true`. After that, every step gets `Force = &true`, so force is sticky from the first forced step. `cfg.Force` itself feeds base and user. Force on a step does **not** force base or user.

### Texts (`steps/builder.go:90-104`)
- `Prefix()` = `fmt.Sprintf("builder %d/%d", stepNumber, len(Config.Steps))`.
- `String()` = `fmt.Sprintf("%s %s", strings.ToUpper(type), strings.Join(args, " "))`. RUN with a user arg prints both, e.g. `RUN echo hi root`. No args gives `"ENV "` (trailing space).
- `Metadata()` = `PhaseMeta{Phase: "steps", StepType: step.GetType() /*raw case*/, StepNumber: &n}`.
- **User phase** (`user/builder.go:68-81`, `user/hash.go:12-20`): `Prefix()`=`"base"`, `String()`=`"DEFAULT USER user"`, `Metadata()`=`{Phase: "base", StepType: "base"}`, hash `HashKeys(prev, "DEFAULT USER", "user")`.
- The runPhase line is `layerInfo`: `"%s[%s] %s [%s]"` with an optional `"CACHED "` prefix (`phases/phase.go:48-60`). Examples: `[builder 1/2] ENV ENV_VAR Hello, World! [<hash>]` and `CACHED [builder 1/2] ENV ENV_VAR Hello, World! [<hash>]`. The integration test greps for exactly `CACHED [builder 1/2] ENV ENV_VAR Hello, World!` (`tests/integration/internal/tests/api/templates/build_template_test.go:486`).

### Hash (`steps/hash.go:14-20`)
`cache.HashKeys(sourceLayer.Hash, step.GetType() /*raw, not upper-cased*/, strings.Join(args, " "), Sprintp(FilesHash))`. `HashKeys` = sha256 of `base` followed by `";"+key` for each key, as lowercase hex (`B/storage/cache/cache.go:112-121`). The `force` flag is **not** part of the hash.

### Layer / cache lookup (`steps/builder.go:106-150`)
```
forceBuild := step.Force != nil && step.GetForce()
if !forceBuild {
  m, err := index.LayerMetaFromHash(hash)
  if err != nil { sb.logger.Info("layer not found in cache, building new step layer", error, hash) }
  else {
    meta, err := index.Cached(m.Template.BuildID)
    if err == nil { return {Metadata: meta, Cached: true, Hash: hash} }
    logger.L().Info("layer not cached, building new layer", error, hash)
  }
}
final := sourceLayer.Metadata
final.Template = TemplateMetadata{BuildID: uuid.NewString(), KernelVersion: Config.KernelVersion, FirecrackerVersion: Config.FirecrackerVersion}
return {Metadata: final, Cached: false, Hash: hash}
```
- The source metadata is copied, including `Version`, Context, Start, FromImage, FromTemplate and Prefetch. Only `.Template` is replaced.
- Neither Info line is user-facing; both go to the service logger.
- `Cached` errors: `error reading template metadata: %w`, and `outdated template metadata: expected %d, got %d` (`cache.go:130-138`). Rust `LayerIndex::cached` already covers this.

### Build (`steps/builder.go:152-250`)
- **sbxConfig:** `Vcpu = Config.VCpuCount`, `RamMB = MemoryMB`, `HugePages`, `FreePageReporting`, `FreePageHinting`, `Envd.Version = bc.EnvdVersion`, `FirecrackerConfig{KernelVersion, FirecrackerVersion}`. There are no env vars and no access token. `Envd.DefaultUser` is nil; on resume it is filled from the metadata (section 4).
- **SandboxCreator:**
  - if `sourceLayer.Cached`: `layer.NewCreateSandbox(sbxConfig, factory, layerTimeout /*1h*/, layer.ReservedBlocksOptions(ctx, ff, Config.RootfsBlockSize())...)`;
  - otherwise: `layer.NewResumeSandbox(sbxConfig, factory, layerTimeout)`.
- **ActionExecutor** (`builder.go:198-224`):
  1. `cmdMeta, err := commandExecutor.Execute(ctx, userLogger, sb.defaultLoggingLevel, sbx, prefix, step, meta.Context)`. Any error becomes **`phases.NewPhaseBuildError(sb.Metadata(), err)`**, so **every command error is a user error** whose message is `err.Error()` and whose step is `"<n>"`.
  2. `sandboxtools.SyncChangesToDisk` runs `"/usr/bin/busybox sync"` as root with no logger. An error is internal: `error running sync command: %w`.
  3. `meta.Context = cmdMeta`.
- **BuildLayer call:** `LayerBuildCommand{SourceTemplate: NewCacheSourceTemplateProvider(sourceLayer.Metadata.Template.BuildID), CurrentLayer: currentLayer.Metadata, Hash, UpdateEnvd: sourceLayer.Cached, SandboxCreator, ActionExecutor, BuildOrigin: storage.ObjectOriginTemplateBuildCache /* "template_build_cache" */}`.
- **Error wrap:** `error building step %d: %w`. A PhaseBuildError inside it keeps its own message, because `UnwrapPhaseBuildError` uses `errors.As` and `Message = phaseBuildError.Error()`.
- **Result:** `{Metadata: meta (NewVersionTemplate result), Cached: false, Hash: currentLayer.Hash}`.

### CommandExecutor (`B/commands/executor.go:47-114`)
`getCommand` switches on `strings.ToUpper(type)`:
- `ADD` and `COPY` give `Copy{FilesStorage: buildStorage, CacheScope}`;
- `RUN` gives `Run`;
- `USER` gives `User`;
- `WORKDIR` gives `Workdir`;
- `ENV` and `ARG` give `Env`.

An unknown type fails with `failed to get command for step %s: command type %s is not implemented`. The first `%s` is the raw type and the second is the upper-cased one. A command error is returned unwrapped.

### RUN (`commands/run.go`)
- No args: `RUN requires command argument`.
- If `len(args) >= 2`, then `cmdMetadata.User = args[1]`, for this command only.
- It runs `RunCommandWithLogger(ctx, proxy, logger, lvl, prefix, sandboxID, args[0], cmdMetadata)` with the context's workdir, env and user.
- Error: `failed to run command '%s': %w`. For a non-zero exit, `%w` is the envd End `status`, for example `failed to run command 'exit 3': exit status 3`.
- It returns the **original** metadata, so RUN never changes the context.

### ENV / ARG (`commands/env.go`)
- Errors: `%s does not support passing no arguments` (upper-cased type), `%s requires both a key and value arguments` (odd number of args), and `failed to evaluate environment variable %s: %w`.
- `envVars := maps.Clone(cmdMetadata.EnvVars)`, then pairs are assigned in order. **Parity edge:** `maps.Clone(nil)` is nil, and assigning to it panics. Context `EnvVars` is nil after JSON load when `env_vars` was omitted (`omitempty`), e.g. a cached or FROM TEMPLATE layer with no env. `Build` recovers the panic as `fatal error occurred during template build, please contact us` (`builder.go:177-182`). That is not a user error, so the reason is the InternalErrorMessage. The defers run in LIFO order, so the "remove build files" defer sees `e == nil` and does **not** delete the prefix. Rust `BTreeMap` cannot tell nil from empty. Exact parity would need `Option<BTreeMap>` or a flag recording the field's absence in the JSON.
- `evaluateValue` (`env.go:61-95`):
  - escaping, applied in this order: `\` becomes `\\`, `"` becomes `\"`, backtick becomes ``\` ``, `$(` becomes `\$(`;
  - command: ``printf "%s" "<escaped>"``;
  - it runs `RunCommandWithOutput` with `metadata.Context{User: "root"}`, so no env, no workdir and no logging;
  - the value is the concatenated stdout, **not trimmed**.
- **Integration vectors** (`build_template_test.go:197-400`):
  - `PATH="${PATH}:/my/path"`;
  - `-----BEGIN DATA-----`;
  - `say "hello"`;
  - `it's working`;
  - `path\to\file` stays literal;
  - `"line1\nline2\nline3"` (real newlines) keeps 3 lines;
  - `` `echo pwned` `` stays literal;
  - `$(echo pwned)` stays literal.

### WORKDIR (`commands/workdir.go`)
- No args: `WORKDIR requires a path argument`.
- The base is `*cmdMetadata.WorkDir`, or `"/"` when it is nil. If `!filepath.IsAbs(arg)`, then `arg = filepath.Join(base, arg)`; Join also cleans. An absolute arg is **not** cleaned.
- Script, run with `RunCommandWithLogger(lvl, prefix)` under `Context{User: "root", EnvVars: ctx.EnvVars}` and **no workdir**:
```
			target="%s"

			# Exit early if target already exists
			if [ -d "$target" ]; then
			    exit 0
			fi

			# Find the first non-existent parent
			first_new=""
			check="$target"
			while [ ! -d "$check" ]; do
			    first_new="$check"
			    check=$(dirname "$check")
			done

			# Create and chown from the top (only if we found new directories to create)
			if [ -n "$first_new" ]; then
			    mkdir -p "$target" && chown -R %s:%s "$first_new"
			fi
    
```
  The exact bytes are at `workdir.go:64-84`: three tabs of indent, and four trailing spaces before the closing backtick. The `%s:%s` is `cmdMetadata.User` twice. Error: `failed to create workdir: %w`.
- `saveWorkdirMeta` (`workdir.go:98-135`) runs `` cd "%s" && pwd `` as root with no env and no workdir, through RunCommandWithOutput. On each output event, a non-empty stderr sets `outputErr = error getting absolute path of workdir: <stderr chunk>` and returns; otherwise `workdir = TrimSpace(stdout)`. A set `outputErr` wins. Otherwise a run error is `failed to save workdir %s: %w`, where `%s` is the current `workdir` variable. Success sets `cmdMetadata.WorkDir = &workdir`.

### USER (`commands/user.go`)
- No args: `USER requires a username argument`.
- `id -u <user>` runs through RunCommand as root with the context env. An error means the user does not exist, and the error is discarded.
- If the user does not exist: `useradd --create-home --shell /bin/bash <user>` through `RunCommandWithLogger(lvl, prefix)` as root with the env. Error: `failed to create user: %w`.
- If `len(args) > 1 && args[1] == "true"`, `addToSudoers` runs with **`zapcore.DebugLevel`** and the same prefix:
  1. The admin group script (`user.go:108-124`, verbatim: distro.env, then `E2B_ADMIN_GROUP`, then sudo, then wheel; stderr texts `the '$E2B_ADMIN_GROUP' group from the distro profile does not exist on this image` and `neither the sudo nor the wheel group exists on this image`). Error: `failed to add user to sudo group: %w`.
  2. `passwd -d <user>`. Error: `failed to remove user password: %w`.
  3. `grep -q '^<u> ALL=(ALL:ALL) NOPASSWD: ALL' /etc/sudoers || echo '<u> ALL=(ALL:ALL) NOPASSWD: ALL' >>/etc/sudoers`. Error: `failed to configure sudoers: %w`.
- `saveUserMeta` runs `printf "<user>"` (``printf "%s"`` formatted in Go, so the guest sees `printf "user"`) as root with no env, and `user = stdout`. Note the assignment, not an append: the last data event wins. It then sets `cmdMetadata.User = user` and returns **the run error unwrapped**.
- All commands are interpolated into the shell string unquoted, exactly as Go does.

### COPY / ADD (`commands/copy.go`, `commands/copy_script.sh`)
- `parseCopyArgs(args, cmdMetadata.User)` (`copy.go:186-222`):
  - fewer than 2 args: `COPY requires a local path and a container path argument`, always with "COPY", even for ADD;
  - `sourcePath, _ := doublestar.SplitPattern(ensureTrailingSlash(args[0]))`. SplitPattern v4.9.1: scan bytes; `\` skips the next byte, `/` records `splitIdx`, and `* ? [ {` stops the scan. `splitIdx == 0` gives `"/"`; `> 0` gives `unescapeMeta(p[:splitIdx])`, which replaces `\* \? \[ \] \{ \}` with the bare character; otherwise `"."`;
  - owner defaults to `user:user` from the context user. A non-empty `args[2]` becomes the owner, and one without `:` becomes `x:x`;
  - permissions are `args[3]` or `""`.
- `FilesHash == nil || == ""` fails with `%s requires files hash to be set` (upper-cased type). This check runs **after** parsing.
- **Download:** `FilesStorage.OpenBlob(paths.GetLayerFilesCachePath(CacheScope, hash))`, with path `path.Join(scope, "files", hash+".tar")` (`B/storage/paths/paths.go:7-13`). Error: `failed to open files object from storage: %w`. The object is streamed to a host temp file (`os.CreateTemp("", "layer-file-*.tar")`; error `failed to create temporary file for layer tar: %w`). The copy error is `failed to copy layer tar data to temporary file: %w`, and that also covers download errors coming through the pipe. The temp file is removed afterwards.
- **Upload:** `CopyFile(proxy, sbx, "root", tmp, "/tmp/<hash>.tar")`. Error: `failed to copy layer tar data to sandbox: %w`.
- **Extract:** `mkdir -p "/tmp/<hash>/unpack" && tar -xzvf "/tmp/<hash>.tar" -C "/tmp/<hash>/unpack"` through RunCommand (no logging) with `cmdMetadata.WithUser("root")`, so it keeps the env and workdir. Error: `failed to extract files: %w`. The tar is gzip.
- **Move:** `copy_script.sh` is rendered with text/template (no escaping) from `{Workdir: deref(WorkDir, ""), User: ctx.User, SourcePath: filepath.Join("/tmp/<hash>/unpack", args.SourcePath), TargetPath, Owner, Permissions}`. It runs through **`RunCommandWithLogger(DebugLevel, id "unpack")`** with `WithUser("root")`. Error: `failed to move files in sandbox: %w`. A template error, practically unreachable, is `failed to execute copy script template: %w`.
- Script user-visible stdout texts (logged at Debug as `[unpack] [stdout]: ...`): `Error: source path does not exist: $sourcePath` and `Error: entry is neither file, directory, nor symlink`. Embed the script byte for byte; it is 92 lines.
- COPY returns `cmdMetadata` unchanged.

---

## 2. Layer executor (`B/layer/*.go`)

### Interfaces (`interfaces.go`)
- `waitEnvdTimeout = 60 * time.Second`.
- `SandboxCreator.Sandbox(ctx, *LayerExecutor, template)`.
- `ActionExecutor.Execute(ctx, sbx, meta) (meta, error)`.
- `SourceTemplateProvider.Get(ctx, cache) (tmpl, release, err)`.
- `LayerBuildCommand{SourceTemplate, CurrentLayer, Hash, UpdateEnvd, SandboxCreator, ActionExecutor, BuildOrigin}`.

### Source providers (`source_template.go`)
- `CacheSourceTemplateProvider.Get` calls `templateCache.GetTemplatePinned(ctx, buildID, false /*isSnapshot*/, true /*isBuilding: NFS cache choice only*/)`. Its error is `get template snapshot: %w`, and BuildLayer wraps it **again**, so the internal text is `get template snapshot: get template snapshot: ...`.
- `DirectSourceTemplateProvider` returns the given template; finalize and optimize use it.

### BuildLayer (`layer_executor.go:79-166`)
1. `Get` the source template. The release is deferred first, so it runs last.
2. `sbx, err := cmd.SandboxCreator.Sandbox(...)`. The error is returned unwrapped (`create sandbox: ...`, `wait for envd: ...`, `resume sandbox: ...`). Then `defer sbx.Close(ctx)`, and a deferred `proxy.RemoveFromPool(sbx.LifecycleID)`, which logs Warn `errors when manually closing connections to sandbox` or Debug `removed proxy from pool`.
3. If `UpdateEnvd`, call `updateEnvdInSandbox`. On error: service logger Error `error updating envd`, then return `update envd: %w`.
4. `meta, err := ActionExecutor.Execute(ctx, sbx, cmd.CurrentLayer)`. On error: service logger Error `error executing action`, then return the error **unwrapped**, so a PhaseBuildError is preserved.
5. `meta = meta.NewVersionTemplate(TemplateMetadata{BuildID: cmd.CurrentLayer.Template.BuildID, KernelVersion: sbx.Config.FirecrackerConfig.KernelVersion, FirecrackerVersion: ...})`. NewVersionTemplate (`metadata/template_metadata.go:286-299`) sets `Version = CurrentVersion (2)` and keeps Context, Start, FromTemplate, FromImage, CmdlineArgs, Balloon, CPUTemplate and BuildCPUTemplate. It **drops** Prefetch and every other field (FilesystemOnly, FsQuiesced). Rust has no `new_version`, only `same_version`, which keeps the version, so M15 needs one.
6. `PauseAndUpload(ctx, userLogger, sbx, cmd.Hash, meta, cmd.BuildOrigin)`. Error: `pause and upload: %w`.

### CreateSandbox, the cold boot (`create_sandbox.go`)
- Constants: `minEnvdVersionForKVMClock = "0.2.11"` and IoEngine `Sync`.
- **Memfile:** `block.NewEmpty(MBToBytes(RamMB), MemfilePageSize(HugePages), uuid(sourceTemplate.BuildID))`. Error: `create memfile: %w`. The header is `NewTemplateMetadata(sourceBuildID, ...)`, so `BuildId = BaseBuildId = source build id` and generation 0, with **one mapping to `uuid.Nil`** over the whole size (`O/pkg/sandbox/block/empty.go:21-33`). The child memfile diff therefore has generation 1 and base build id = **source build id**. Rust `memory_parent` uses `Uuid::nil()` as the base: a gap for header parity.
- **Template:** `NewMaskTemplate(source, WithMemfile(memfile))`, so the rootfs comes from the source and the memory starts fresh.
- `kvmClock := IsGTEVersion(Envd.Version, "0.2.11")`. Error: `error comparing envd version: %w`.
- **CreateSandbox call:**
  - RuntimeMetadata `{TemplateID, SandboxID: "b"+id.Generate(), ExecutionID: uuid, TeamID, BuildID: layerExecutor.Template.BuildID /*the final build id*/, SandboxType: build}`;
  - `timeout = 1h`;
  - `rootfsCachePath ""`;
  - ProcessOptions `{InitScriptPath: "/sbin/init", KernelLogs: env.IsDevelopment(), SystemdToKernelLogs: false, KvmClock, IoEngine: Sync, CmdlineArgs: Config.CmdlineArgs, CPUTemplate: Config.CPUTemplate}`;
  - `preBootFn`.
  Error: `create sandbox: %w`.
- `WaitForEnvd(StartTypeCreate, 60s)`. Error: `wait for envd: %w`. On error after creation, `sbx.Close` is joined.
- **Pre-boot hook:** `ReservedBlocksOptions` adds `WithPreBootFn(SetReservedBlocksOnHost(rootfsPath, reservedMB, blockSize))` only when flag `BuildReservedDiskSpaceMB` > 0 (default 256, see M14). The hook gets the **rootfs device path** (the NBD device of the overlay) before Firecracker boots. Rust `SandboxBuilder::cold_boot` has no hook between `expose(overlay)` and the boot. This is a gap.

### ResumeSandbox (`resume_sandbox.go`)
`factory.ResumeSandbox(ctx, template, config, RuntimeMetadata{same as above}, time.Now(), time.Now().Add(1h), nil)`. Error: `resume sandbox: %w`. No envd update follows.

### updateEnvdInSandbox (`layer_executor.go:169-265`)
1. `envd.GetEnvdVersion(HostEnvdPath)` runs `<envd> -version` and trims the output; its error is `error while getting envd version: %w`. The update wraps that as `error getting envd version: %w`. It then logs the user log line **Debug** `Updating envd to version v<ver>`.
2. `CopyFile(sbx, "root", HostEnvdPath, "/tmp/envd_updated")`. Error: `failed to copy envd binary to sandbox: %w`.
3. `RunCommandWithLogger(DebugLevel, id "update-envd-replace", ...)` as root runs:
   ```
   \n\t\t# Replace the binary and set permissions\n\t\tchmod +x /tmp/envd_updated\n\t\tmv -f /tmp/envd_updated /usr/bin/envd\n\t
   ```
   Error: `failed to replace envd binary: %w`.
4. RunCommand as root, error ignored:
   ```
   if command -v systemctl >/dev/null 2>&1; then
       systemctl restart envd
   else
       kill -TERM "$(pidof envd)"
   fi
   ```
5. `proxy.RemoveFromPool`, which logs Warn `errors when manually closing connections to sandbox after restarting envd`, then Debug `removed proxy from pool after restarting envd`.
6. `WaitForEnvd(StartTypeCreate, 60s)`, which re-sends `/init`. Error: `failed to wait for envd initialization after update: %w`.

### PauseAndUpload / UploadSnapshot (`layer_executor.go:267-405`)
- `sbx.Pause(ctx, meta, SnapshotUseCaseBuild)`. Error: `error processing vm: %w`.
- `UploadSnapshot`:
  1. Debug user log `Adding layer to cache: <buildID>`.
  2. `templateCache.AddSnapshot(WithoutCancel(ctx), buildID, SnapshotLineage{Origin}, headers, snapfile, metafile, diffs...)`. Error: `error adding snapshot to template cache: %w`, joined with `snapshot.Close` errors.
  3. Debug `Uploading layer: <buildID>`.
  4. `objectMetadata := Config.ObjectMetadata(buildOrigin)`. Rust uses team_id, template_id and build_origin.
  5. `sandbox.NewUpload(...)`. Error: `register upload: %w`.
  6. `UploadErrGroup.Go(func)` with a **WithoutCancel ctx**:
     - `upload.Run(ctx)` (error `error uploading snapshot: %w`);
     - **then** `index.SaveLayerMeta(hash, {template:{build_id}})` (error `error saving UUID to hash mapping: %w`). `storage.ErrObjectRateLimited` is skipped with Warn `rate limited writing layer metadata to object, skipping`;
     - then the Debug user log `Saved: <buildID>`;
     - a deferred `endLayerUpload`: on error `upload.Abandon(ctx, err)`, otherwise `upload.Finish(ctx, nil)`. This returns the template cache pin and wakes child-layer waiters with the error;
     - on error, the service log line `template layer snapshot upload failed` with layer_build_id and layer_hash.
  7. It returns nil immediately.
- The `Saved:` and index lines run in the background, so their log lines can appear after later step lines.

### The upload error group and waiting (`B/builder.go:236-264`, `440-444`)
- `uploadErrGroup := &errgroup.Group{}` is created per build, with **no** context and no limit. `errgroup.Wait` returns the **first** error.
- **After `phases.Run` succeeds:** `bc.UploadErrGroup.Wait()`; an error becomes the internal `error waiting for layers upload: %w`. Only then does `getRootfsSize` read `<lastBuildID>/rootfs.ext4.header` from storage.
- **A deferred Wait always runs**, even on failure: it waits for every upload; an error logs `template layer upload wait failed` and is joined into `e` as `error uploading template layers: %w` (`errors.Join`, so newline-separated).
- **Order of uploads:** the next layer runs from the template cache while its parent uploads. A child's `upload.Run` waits for its parents (the uploads registry), and a parent failure aborts the child. Rust `Uploads::run`/`start` already wait for `parents(&build)`.

### Timeouts summary
- `layerTimeout = 1h` is the sandbox lifetime, for create and resume.
- `waitEnvdTimeout = 60s`.
- `commandHardTimeout = 1h` is the HTTP client timeout for `process.Start`.
- `fileCopyTimeout = 10min` is the HTTP client timeout for `POST /files`.
- No other per-step timeout exists; the build ctx bounds everything.

---

## 3. sandboxtools (`B/sandboxtools/command.go`, `file.go`)

### runCommandWithAllOptions (`command.go:121-224`)
- **Env:** `envs := maps.Clone(ctx.EnvVars)`. If the `PATH` key exists, append `":/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"`.
- **Request:** `process.Start{Process{Cmd: "/bin/bash", Cwd: ctx.WorkDir /*nil = unset*/, Args: ["-l","-c",command], Envs}}`, with the user header `SetUserHeader(header, ctx.User)`, i.e. Basic `user:`.
- **Errors:**
  - a Start error: `error starting process: %w`;
  - a stream error: `command failed: %w`;
  - ctx done: `context: %w`;
  - a nil event: Error log `received nil command event`, and the call returns nil (success);
  - an End with `exitCode != 0`: `errors.New(end.GetStatus())`, e.g. `exit status 1`;
  - the stream closes: drain `msgErrCh`; an error there is `command failed: %w`, otherwise nil.
- Rust `envd::process::run_command` already mirrors all of this, including PATH and the user header.

### logStream (`command.go:226-242`)
Called for **each Data event**: first stdout with name `"stdout"`, then stderr with `"stderr"`.
```
for line := range strings.SplitSeq(content, "\n") {
    line = strings.TrimSpace(line); if line == "" { continue }
    logger.Log(ctx, lvl, fmt.Sprintf("[%s] [%s]: %s", id, name, line))
}
```
- There is no buffering across events: a line split between two chunks becomes two log lines. Blank lines are dropped, and leading or trailing whitespace is trimmed.
- The logger is the step-scoped userLogger, so lines carry the phase fields.
- Levels:

| Case | Level |
| --- | --- |
| steps | Info |
| default user phase (all its commands) | Debug |
| addToSudoers | always Debug |
| the COPY move script, id `unpack` | Debug |
| the envd replace, id `update-envd-replace` | Debug |

- `id` is the phase prefix, e.g. `[builder 2/5] [stdout]: hello`, or `[base] [stderr]: ...` for the user phase.
- The commands that never log are RunCommand and RunCommandWithOutput: `id -u`, ENV evaluation, `pwd`, `printf user`, `tar -xzvf`, sync and the envd restart.

### CopyFile (`file.go:29-132`)
- `POST http://<proxy>/files?path=<target>&username=<user>`. The query comes from `url.Values.Encode`, so keys are sorted and escaped with QueryEscape.
- Body: multipart with a single part from `CreateFormFile("file", filepath.Base(source))`, streamed; Content-Type is `writer.FormDataContentType()`. There is no Authorization header; the user comes from the `username` query. The timeout is 10 min.
- Errors:
  - `failed to open source file: %w`;
  - `failed to create request: %w`;
  - `failed to send request: %w`;
  - `file upload failed: %w`, wrapping `failed to create form file: %w` or `failed to copy file: %w`;
  - a non-200 status: `failed to upload file (%d): %s` with the body.

Rust has no `POST /files` client yet. Add one beside `envd/process.rs`, e.g. `envd/files.rs`. Rust `go_url` has an `Encode` equivalent.

### SyncChangesToDisk
RunCommand of `"/usr/bin/busybox sync"` (`rootfs.SandboxBusyBoxPath + " sync"`) as root. Rust `SYNC_COMMAND` exists in `base/build.rs`.

---

## 4. Rust side: what to reuse and the gaps

**Already in place:**
- `phase.rs` (Phase trait, runPhase, `layer_info`, validation, `BuildError::user`);
- `hash.rs` (`step_hash`, `user_hash`);
- `index.rs` (`LayerIndex::cached`/`save`/`join_path`);
- `envd/process.rs` (`run_command`, Go-compatible errors);
- `logs.rs` (`ScopedLog::push`);
- `service.rs` `init_layer_file_upload` (`{scope}/files/{hash}.tar`, M13).

**To build:**
1. **Steps pipeline:**
   - in `pipeline.rs`, gate the user phase with `is_at_least(version, "v2.1.0")` (Go `IsGTEVersion`; error `error checking build version: %w`);
   - add `force_steps`;
   - add one `StepPhase` per step, plus a `UserPhase` that wraps StepPhase with the overrides from section 1, the `steps`/`1` PhaseBuildError quirk and the Debug level;
   - the prefix is needed inside build (Rust `build()` can call `self.prefix()`).
2. **Commands module** (e.g. `builder/commands.rs` + `commands/{run,env,workdir,user,copy}.rs` + embedded `copy_script.sh`):
   - a `CommandContext` <-> `metadata::Context` conversion (`user: Option<String>` vs `String`);
   - a log adapter (`logStream`) over `run_command` with a `(level, id)`;
   - a Go `filepath.Join`/`IsAbs` (reuse `index::join_path`);
   - doublestar `SplitPattern`.
3. **The layer executor** (e.g. `builder/layer.rs`), shared later by finalize and optimize:
   - **Source template:** `node.templates.get(Uuid)` returns an `Arc<Template>` with `build()`, `snapfile()` and `metadata()`. The Arc acts as the pin, because eviction skips entries with `strong_count > 1`. Hold the Arc for the whole layer.
   - **Resume:** `Node::resume` (`server/node.rs:529-569`) already builds the rootfs `Overlay` over `BuildFile(build.rootfs, Artifact::Rootfs, store)` and the memfile `BuildFile`, then calls `Sandbox::builder()...resume(snapfile)`. It is private and tied to `SandboxConfig` (proto) and `Lifecycle`. Extract a `pub` helper, e.g. `Node::resume_layer(build_id, sandbox_id, template_id, slot, init) -> Sandbox`, or move the overlay and memfile construction into a small function used by both. The build passes:
     - `pooled_slot(slot)` from `network.lease(None)` plus `assign_build_network` (as `BaseBuild::build_slot`);
     - `envd_timeout(60s)`;
     - `use_memfd(...)` and `hyperloop_ip`;
     - `EnvdInit`, with no token, no env vars and `default_user` = `Some("user")` only when `metadata.context.user == "user"`. This is Go `EnvdDefaultUser`, which reads **`meta.Context.User`** (`O/pkg/sandbox/sandbox.go:1477-1521`, `metadata/template_metadata.go:90-96`). **Parity flag:** Rust `node.rs::default_user` (line 645) reads `start.context.user`, not `context.user`. Verify that customer resumes match Go too.
     - Workdir: Go never re-sends it.
   - **Cold boot from a cached layer:** build the same overlay over the cached template's rootfs `BuildFile` and call `.rootfs(Arc<Overlay>)...cold_boot()`, which Rust already supports (`attach_disk_and_slot` exposes it over NBD). Set:
     - `init_path("/sbin/init")`;
     - `kernel_logs(development)`;
     - `kvm_clock(is_at_least(envd_version,"0.2.11"))`;
     - `io_engine("Sync")`;
     - `vcpus` and `memory_mib` from the config;
     - `without_mmds()`? Check Go. Go CreateSandbox passes no special MMDS opt. M14 base used `without_mmds`; keep it consistent with what M14 decided.
     - `envd_timeout(60s)`;
     - `skip_envd()` then `wait_for_envd()` (error `wait for envd: ...`).
   - Gaps in the cold boot:
     - (a) a **pre-boot hook** on the exposed device for `tune2fs -r` (`ext4.set_reserved_blocks` against the NBD device path), where an error is `create sandbox: pre-boot hook failed: ...` as in M14;
     - (b) memfile **parent header** = `Metadata::template(source_build_id, page_size, ram)` with one `Uuid::nil()` mapping, instead of `Uuid::nil()` as the base build id;
     - (c) the memfile page size follows HugePages (Go `MemfilePageSize`).
   - **updateEnvd** (cold boot only):
     - copy via the new `POST /files` client;
     - the replace script;
     - the restart script, error ignored;
     - `sandbox.wait_for_envd()` again, which re-inits envd. The pooled connection to drop does not exist in Rust, because `run_command` builds a fresh reqwest client per call.
   - **Pause:**
     - `sandbox.pause_into(&node.layer_dir(build_id), build_id)` already handles both overlay (resume or cold boot) and file rootfs, and stops the VM;
     - release the slot right after the pause, as in M14;
     - write `metadata.new_version(...)`;
     - `node.cache_layer(&local, metadata, dir)` (AddSnapshot);
     - then **`node.upload(local, object_metadata)`, the async one**, instead of `upload_now`.
   - **Index write after landing:** Rust `node.upload` takes no completion hook for the index. Add a variant that runs `uploads.run(build_id, parents, attempt)` in a task held by a per-build group, chained with `index.save(hash, build_id)` and logging `Saved: <id>`. Errors: `error uploading snapshot: ...` / `error saving UUID to hash mapping: ...`.
   - **Per-build upload group:** for example a `JoinSet<Result<(),String>>` or a `Vec<JoinHandle>` in `BuildEnv` or a build context.
     - After `run_phases` succeeds, wait for all of them, failing on the first error with `error waiting for layers upload: <e>`.
     - On failure, still wait for all of them, joining `error uploading template layers: <e>` (errgroup keeps the first error only).
     - Convert M14 `BaseBuild::upload` to the same path.
4. **Build-file deletion on failure** and the panic edge come from M13; mind the ENV nil-map behaviour above.
5. **`metadata.Context.user`:** steps after base always have a user, so `validate_layer` passes. `RUN`'s user override must not leak.

---

## 5. Upstream unit tests and vectors to port

- **`B/commands/copy_test.go` `TestParseCopyArgs`** (`:204-422`). Each vector is (args, defaultUser) → (Source, Target, Owner, Perms):
  - `["/local/path","/container/path"]`, `user` → `/local/path`, `/container/path`, `user:user`, `""`
  - `[..., "root"]` → `root:root`
  - `[..., "www-data:www-data"]` → `www-data:www-data`
  - `[..., "user:staff"]`, `defaultuser` → `user:staff`
  - `[..., ""]`, `ubuntu` → `ubuntu:ubuntu`
  - `[..., "root", "755"]` → `root:root`, `755`
  - `[..., "www-data:www-data", "644"]` → perms `644`
  - `["/app/*.js","/dest/"]` → `/app`
  - `["/app/**/*.ts","/dest/"]` → `/app`
  - `["/app/file?.txt",...]` → `/app`
  - `["/app/[abc].txt",...]` → `/app`
  - `["/app/src/",...]` → `/app/src`
  - `["/app/src",...]` → `/app/src`
  - `["./local/path","container/path"]` → `./local/path`, `container/path`
  - `["/src/components/**/*.{ts,tsx}","/dest/"]`, `developer` → `/src/components`, `developer:developer`
  - `["/source/*.go","/target/","admin:sudo","700"]` → `/source`, `/target/`, `admin:sudo`, `700`
  - `[]` and `["/local/path"]` → `COPY requires a local path and a container path argument`
- **`TestCopyScriptBehavior`** (`:424-993`) renders the script with Owner = current uid:gid and runs it under `/bin/bash` on the host. Port it as a Linux-only test in `scripts/test-linux.sh`. Cases:
  - `single_file_root_level`
  - `single_file_to_new_nested_directory_trailing_slash`
  - `single_file_to_preexisting_directory_no_slash`
  - `symlink_to_new_directory_trailing_slash`
  - `multiple_files_root_level`
  - `directory`
  - `nested_directory_structure_to_target`
  - `copy_from_deeply_nested_subfolder`
  - `copy_from_nested_folder_with_subdirs`
  - `with_permissions_755` / `with_permissions_644`
  - `directory_with_permissions_recursive`
  - `symlink_preservation`
  - `nested_folder_with_symlinks`
  - `hidden_files_and_directories`
  - `deeply_nested_file`
  - `merge_directory_into_existing_tree`
  - `merge_root_directory_into_existing_root`
  - `overwrite_existing_file_in_target`
  - `preserve_target_directory_metadata`
  - `directory_with_readonly_permissions`
  - `replace_target_symlink_to_file`
  - `follow_target_symlink_to_directory`
  - `copy_directory_that_is_not_first_alphabetically`
  - `copy_file_that_is_not_first_alphabetically`
  - `deeply_nested_folder`

  Since the script is embedded verbatim, the cheapest parity check is a byte-compare test against the upstream file plus a handful of these cases.
- **`B/layer/layer_executor_test.go` `TestEndLayerUpload`:** a landed upload gives `UploadLanded`; a failed one (`storage down`) gives `UploadAbandoned`, and `upload.Wait` returns the upload error. The Rust analogue: a failed layer upload is reported to waiting children, and `Uploads::wait` gives `Failed`.
- **No upstream unit tests exist** for the step and user hashes, forceSteps, logStream, the ENV escaping, WORKDIR or USER. Hash vectors follow from `HashKeys`; Rust `hash.rs` tests already pin `p;RUN;echo hi;<nil>` and `p;COPY;;f`. Suggested new Rust unit tests:
  - forceSteps stickiness;
  - the `String()` text with a trailing space for no args;
  - the logStream split, trim and skip;
  - the ENV escape of `` \ " ` $( ``;
  - the WORKDIR join rules (`rel` + nil workdir gives `/rel`; `../x` from `/a/b` gives `/a/x`; an absolute path is not cleaned);
  - the SplitPattern edge cases (`/` gives `/`, `*.txt/` gives `.`, `a\*b/` gives `a*b`).
- **Integration vectors** (`tests/integration/internal/tests/api/templates/build_template_test.go`) are good contract tests for M15 CI:
  - RUN `echo 'Hello, World!'`;
  - the ENV escaping cases (lines 197-400);
  - WORKDIR `/app` followed by `[[ "$(pwd)" == "/app" ]]`;
  - a cache rebuild expecting `CACHED [builder 1/2] ENV ENV_VAR Hello, World!`;
  - FROM TEMPLATE inheritance of env and workdir;
  - RUN with user override `root`;
  - COPY `[".", "/app/"]` with filesHash.
