# M14 research digest: OCI to ext4 and provisioning (Go upstream to envd-rs)

Paths: `O` = `~/IdeaProjects/e2b-runtime-origin-main-review/packages/orchestrator`, `S` = `.../packages/shared`, `B` = `O/pkg/template/build`, `R` = `~/IdeaProjects/envd-rs/crates/orchestrator`.
Rendered artefacts (made with a scratch Go program running the real templates, distro.go and init.go): `/Users/jirka/.claude/jobs/1089bd34/tmp/render/out/`
- `rendered/<guest path>`: every injected template file, rendered with MemoryMB=1024 (GOMEMLIMIT=512MiB) and memory protection off. `envd.service` matches `testdata/envd.service.golden` byte for byte.
- `provision.rendered.sh`: provision.sh with BusyBox=/usr/bin/busybox, ResultPath=/provision.result, Provider=gcp and the generated distro selector (521 lines).
- `distro-selector.sh`: the `distro.ShellSelector()` output alone.

Hashes at today's upstream: `distro.Fingerprint()` = `f6af3e80...391a169e`, `rootfs.FilesHash()` = `e5641b1d...45d79339`, fallback provision version = `83b23b6a510b214cdfc5bce2a379219610df77a4e4fecc1c7598fcd17caa08f7` (formulas in section 5).

## Spec corrections found (spec lines 95-133 against Go)

1. **Order of ToExt4 steps.** Go runs: mkfs, mount, extract, unmount, `e2fsck -pfv`, `resize2fs -M`, `e2fsck -pfv`. `tune2fs -O ^read-only` runs **after** ToExt4, in `CreateExt4Filesystem`. Then debugfs stats, enlarge, and `e2fsck -pfv`. No integrity check runs between the unmount and the first e2fsck.
2. **Base layer step 1.** "e2fsck and grow to diskSizeMB" happens **after the provisioning boot**, not before. Go order: provision boot, `e2fsck -pfv`, `enlargeDiskAfterProvisioning` (debugfs free space, then `resize2fs <size>M`, then `e2fsck -nfv` with a retry using `-pfv`, then a size check, then a header size update), then the systemd boot.
3. **The 5 min provisioning timeout is not enforced by Go.** `provisionTimeout` only becomes the sandbox `endAt`. Nothing in `provisionSandbox` waits on it. The wait ends on the marker or on build ctx cancellation. In Go, "exit code not detected" can only come from scanner EOF, and the pipe is closed only when the function returns. If you implement a 5 min deadline, that is a Rust addition, so decide what text it produces.
4. **Busybox location.** Go reads `HOST_BUSYBOX_DIR`/`BUSYBOX_VERSION`/`runtime.GOARCH`/`busybox` (defaults `/fc-busybox`, `1.36.1`). The spec says `E2B_BUSYBOX`. envd comes from `HOST_ENVD_PATH` (default `/fc-envd/envd`).
5. **The injected file list is larger than the spec's.** 21 files plus one symlink with the option off, 22 files with `build-envd-memory-protection` on. It also includes the OpenRC files (`envd.openrc`, `chrony-source.openrc`), `e2b-chrony-source.service`, the slice drop-in and the second watchdog override. Full list in section 3.
6. **Extra pre-boot hook on the base systemd boot.** `tune2fs -r <blocks>` with `build-reserved-disk-space-mb` = 256 (default) runs before the base-layer boot, not the provisioning boot.
7. **Base hash keys.** Go also appends conditional keys: `cmdline:<sorted args>`, `cpu-template:<..>`, and `envd-memory-protection:128:256`. Rust `base_hash` has only the three fixed keys. The fallback provision version is the hash above, not a number.

---

## 1. OCI pull (`B/core/oci/oci.go`, `B/core/oci/auth/*.go`, `S/pkg/dockerhub/*.go`)

### Platform
`oci.go:62-67`: `Platform{OS: "linux", Architecture: utils.TargetArch()}`. `TargetArch()` (`S/pkg/utils/env.go:24`) reads env `TARGET_ARCH`, normalised with `amd64|x86_64 -> amd64` and `arm64|aarch64 -> arm64`. An unknown value prints `WARNING: unrecognized TARGET_ARCH=%q, falling back to %s\n` to stderr once and uses the host GOARCH. Unset means the host GOARCH. The value uses Go arch names (`amd64`, `arm64`), which is also what OCI configs use.

`remote.Image(ref, remote.WithPlatform(p))`: for an index or manifest list, go-containerregistry picks the child that matches the platform. A missing match is a non-transport error, so it gets the generic text below. A single manifest is returned as is, so it is then checked:

```go
// oci.go:469-481
config, err := img.ConfigFile()
if err != nil { ... return fmt.Errorf("failed to inspect image '%s': unable to retrieve image metadata from registry", imageRef) }
if config.Architecture != platform.Architecture {
    return fmt.Errorf("image architecture %q does not match expected %q", config.Architecture, platform.Architecture)
}
```
The check runs after every successful pull, through the proxy or directly. Go `%q` gives double-quoted strings, for example `image architecture "arm64" does not match expected "amd64"`. Before the check, Go logs Warn `failed to get image config file` with fields `image_ref` and `error`.

### GetPublicImage flow (`oci.go:100-156`)
1. `name.ParseReference(tag)` fails: `invalid image reference '%s': %w`. The `%w` is the go-containerregistry text, for example `could not parse reference: ...`.
2. **Docker Hub proxy.** It is used when `authProvider == nil && ref.Context().RegistryStr() == name.DefaultRegistry`, where `name.DefaultRegistry` = `"index.docker.io"`. `docker.io/...` and bare `ubuntu:22.04` both resolve to it. The call is `dockerhubRepository.GetImage(ctx, tag, platform)`. An error goes through `wrapImagePullError`. On success the telemetry event is `pulled public image through docker remote repository proxy`, followed by verifyImagePlatform.
3. Otherwise the options are `[WithPlatform]`. If authProvider is set, `GetAuthOption` runs. Its error becomes `error getting auth option: %w`, unwrapped, so the raw ECR text reaches the user (see below). Then `remote.Image`. An error goes through `wrapImagePullError`. On success the event is `pulled public image`, followed by verifyImagePlatform.
4. In `rootfs.go:134-137`, **every** GetPublicImage error is wrapped as `phases.NewPhaseBuildError(phaseMetadata, err)`. It is a user error for phase `base`, step `base`, and the message is `err.Error()` exactly.

### Docker Hub remote repository (`S/pkg/dockerhub/repository.go`)
- Env `DOCKERHUB_REMOTE_REPOSITORY_PROVIDER`, default `GCP_REMOTE_REPOSITORY`. Values: `GCP_REMOTE_REPOSITORY`, `AWS_ECR`, `AZURE_ACR`, `Local`.
- Env `DOCKERHUB_REMOTE_REPOSITORY_URL`. When it is empty, `NoopRemoteRepository` pulls `tag` directly from Docker Hub with the platform and **no auth**. Errors: `invalid image reference: %w` and `error pulling image: %w`.
- An unknown provider fails startup with `unknown dockerhub remote repository provider: %s`. `Local` means noop. Setup timeout is 10 s.
- Each proxy computes `removeRegistryFromTag(tag)` = `ref.Name()` without the `RegistryStr()` prefix and without a leading `/`. For example `ubuntu:22.04` gives `library/ubuntu:22.04`, because `ref.Name()` = `index.docker.io/library/ubuntu:22.04`. It then pulls `repositoryURL + "/" + that`.
  - **GCP** (`repository_gcp.go`): Basic auth. If env `DOCKER_AUTH_BASE64` is set, it is base64 **URL** encoding of the JSON `{"username","password"}`. Otherwise `{Username: "_json_key_base64", Password: <env GOOGLE_SERVICE_ACCOUNT_BASE64>}`. Errors: `failed to decode auth config: %w` and `invalid JSON auth config: %w`, wrapped in `error getting auth token: %w`.
  - **AWS** (`repository_aws.go`): default AWS config, then `ecr.GetAuthorizationToken`, then base64 std decode, then split on `:` into Basic. Errors: `failed to get aws ecr auth token: %w`, `no aws ecr auth token found`, `failed to decode aws ecr auth token: %w`, `invalid aws ecr auth token`, wrapped in `error getting auth token: %w`.
  - **Azure** (`repository_azure.go`): the URL must not contain `://`, otherwise `invalid Azure remote repository URL %q: want registry.host/repository without a scheme`. It also needs strict repository parsing, otherwise `invalid Azure remote repository URL %q (want registry.host/repository): %w`. It uses an ACR token exchange from DefaultAzureCredential and retries once with a fresh token on unauthorized.
  - Every proxy wraps the pull error as `error pulling image: %w`, so a `transport.Error` is still found by `errors.As` inside wrapImagePullError.

### Auth providers (proto `O/template-manager.proto:38-62`; Rust copy is `proto/orchestrator/template-manager.proto`)
`TemplateConfig.fromImageRegistry` (field 15, optional) is a `FromImageRegistry` with `oneof type { AWSRegistry aws = 1; GCPRegistry gcp = 2; GeneralRegistry general = 3; }`. `NewAuthProvider` (`auth/auth.go`) returns nil for nil, or for a oneof that is not set, so the proxy path is possible.
- **AWS** (`auth/aws.go`): region `awsRegion`, static creds `awsAccessKeyId`/`awsSecretAccessKey` (no session token), `GetAuthorizationToken{}`, first entry, base64 std decode, `SplitN(":", 2)`, Basic. Errors in order: `failed to load AWS config: %w`, `failed to get ECR authorization token: %w`, `no ECR authorization data returned`, `failed to decode ECR token: %w`, `invalid ECR token format`. The user sees them prefixed with `error getting auth option: `.
- **GCP** (`auth/gcp.go`): `google.NewJSONKeyAuthenticator(serviceAccountJson)` = Basic `{Username: "_json_key", Password: <json verbatim>}`. It never errors.
- **General** (`auth/general.go`): Basic `{username, password}`. It never errors, and empty strings are still sent as Basic.
- In go-containerregistry, Basic auth is used for the registry's token exchange (Bearer realm with basic credentials) or sent directly when the challenge is `Basic`. `oci-client`'s `RegistryAuth::Basic` behaves the same way.

### wrapImagePullError (`oci.go:70-98`): user-facing texts
It logs Warn `failed to pull image` with `image_ref` and `error`, then:
```
if transport.Error: for each diagnostic e in order (first match wins):
  MANIFEST_UNKNOWN -> "image '%s' not found: the image or tag does not exist in the registry"
  NAME_UNKNOWN     -> "repository '%s' not found: verify the image name is correct"
  UNAUTHORIZED     -> "access denied to '%s': authentication required or insufficient permissions"
  DENIED           -> "access denied to '%s': you don't have permission to pull this image"
  then if StatusCode != 0 -> "failed to pull image '%s': registry returned status code %d"
otherwise / fallback -> "failed to pull image '%s': unable to retrieve image from registry"
```
`%s` is the original `tag` string as the user gave it, not the proxy-rewritten one. Registry message and detail never leak. A 401 with no diagnostics body gives `registry returned status code 401`. Network, DNS, missing-platform and digest errors give the generic text.

### Image size and env
- `GetImageSize` sums `layer.Size()`, the **compressed** descriptor sizes. Errors: `error getting image layers: %w`, `error getting layer (%d) size: %w`. The caller wraps them as `error getting image size: %w`, which is internal.
- `ParseEnvs(cfg.Env)` (`oci.go:250-268`): skip blank entries, `SplitN("=",2)`, require 2 parts, TrimSpace key and value, **drop when key or value is empty** (so `FOO=` is dropped). The result becomes `baseMetadata.Context.EnvVars` (`base/builder.go:186`).

---

## 2. Root filesystem creation

### Call chain and user log lines (`B/core/rootfs/rootfs.go:112-212`)
```
l.Debug "Requesting Docker Image"
GetPublicImage                      (any err -> PhaseBuildError)
GetImageSize                        err -> "error getting image size: %w"
l.Info  "Base Docker image size: %s"          humanize.Bytes(compressed sum)
l.Debug "Setting up system files"
additionalOCILayers                 err -> "error populating filesystem: %w"
mutate.AppendLayers(files layer, symlink layer)  err -> "error appending layers: %w"
l.Info  "Creating file system and pulling Docker image"
maxRootfsSize = MBToBytes(FF build-base-rootfs-size-limit-mb, default 25000)   (MB = 1<<20)
oci.ToExt4(ctx, l, img, rootfsPath, maxRootfsSize, 4096)
    *ImageTooLargeError -> PhaseBuildError(imgErr)   (user-visible)
    other -> "error converting oci to ext4: %w"
l.Debug "Filesystem cleanup"
filesystem.MakeWritable               err -> "error making rootfs file writable: %w"
GetFreeSpace(rootfsPath, 4096)        err -> "error getting free space: %w"
diskAdd = MBToBytes(DiskSizeMB) - free
logger.L().Debug "adding disk size diff to rootfs" {size_current, size_add, size_free}
if diskAdd > 0: Enlarge(rootfsPath, diskAdd)   err -> "error enlarging rootfs: %w"
CheckIntegrity(fix=true)              Debug "filesystem ext4 integrity" {result, error}
                                      err -> "error checking ext4 filesystem integrity: %w"
img.ConfigFile()                      err -> "error getting image config file: %w"
return config.Config                  (used for Env)
```
`rootfsPath` = `TEMPLATES_DIR/<base buildID>/rootfs.filesystem.build` (`base/builder.go:42,156,169`). The dir is created 0777 and removed by `RemoveAll` when buildLayerFromOCI returns, with Error `Error while removing template build directory` on failure. Default `TEMPLATES_DIR` = `${ORCHESTRATOR_BASE_PATH}/build-templates`.

`humanize.Bytes` (go-humanize, SI): below 10 it prints `"%d B"`. Otherwise e = floor(log1000(s)), units `B kB MB GB TB PB EB`, val = floor(s/1000^e*10+0.5)/10, format `"%.1f %s"` if val < 10, else `"%.0f %s"`. Examples: 25000 MiB = 26214400000 B gives `26 GB`, 2_500_000 gives `2.5 MB`.

### ToExt4 (`oci.go:177-210`)
```
filesystem.Make(rootfsPath, BytesToMB(maxSize), blockSize)   err -> "error creating ext4 file: %w"
ExtractToExt4(...)                    err -> "error extracting image to ext4 filesystem: %w"
CheckIntegrity(fix=true)              err -> "error checking filesystem integrity after ext4 creation: %w"
Shrink                                err -> "error shrinking ext4 filesystem: %w"
CheckIntegrity(fix=true)              err -> "error checking filesystem integrity after shrinking: %w"
return size (stat size after shrink)
```

### mkfs (`B/core/filesystem/ext4.go:50-93`)
If `blockSize < 4096` the error is `block size must be greater than inodes ratio`. The command is exactly:
```
mkfs.ext4 -O ^64bit,^dir_nlink,ext_attr,extent,extra_isize,filetype,flex_bg,huge_file,inline_data,large_file,sparse_super2 -b 4096 -m 0 -i 4096 <rootfsPath> 25000M
```
mkfs creates the file itself, and it is sparse. stdout and stderr go to telemetry event writers (span events), not logs. The error is the bare `exec` error, for example `exit status 1`.

### ExtractToExt4 (`oci.go:212-248`)
- `tmpMount = os.MkdirTemp(dir(rootfsPath), ".ext4-mount-")`. Error: `error creating temporary mount point: %w`. RemoveAll in defer, logging Error `error removing temporary mount point`.
- `mount -o loop <rootfsPath> <tmpMount>`. Error: `error mounting ext4 filesystem: %w`. Deferred `umount <tmpMount>` runs with `context.WithoutCancel`. Unmount errors are wrapped as `error unmounting ext4 filesystem: %w`, logged as Error `error unmounting ext4 filesystem` and **not returned**.
- Debug `extracting image to ext4 filesystem` {rootfs_path, tmp_mount}.
- `unpackRootfs` error becomes `error extracting tar to directory: %w`.

### unpackRootfs (`oci.go:270-331`)
- `ociPath = MkdirTemp(tempRoot, ".oci-image-")`. Error: `while creating temporary file for squashed image: %w`. Removed in defer, with Error `error removing temporary OCI image`.
- `createExport`. Error: `while creating export of source image: %w`.
- `mountPath = MkdirTemp(tempRoot, ".overlayfs-mount-")`. Error: same text `while creating temporary file for squashed image: %w`. Removed in defer, with Error `error removing temporary overlayfs mount point`.
- `MountOverlayFS(layers, mountPath)`. Error: `while mounting overlayfs with layers: %w`. Deferred `umount mountPath` (WithoutCancel), with Error `error unmounting overlayfs mount point`.
- `os.ReadDir(mountPath)`. Error: `while listing files in overlayfs: %w` with inner `while reading directory %s: %w`. **User Info log:** `"Root filesystem structure: " + strings.Join(names, ", ")`. ReadDir returns names sorted by filename.
- `copyFiles(mountPath, tmpMount)`. If the error text contains `No space left on device`, it computes `du -sb mountPath` (error ignored, 0 on failure) and returns `&ImageTooLargeError{ImageSize, MaxSize}`. Otherwise `while copying files from overlayfs to destination directory: %w`.
- Defer order (LIFO): overlay umount, then rm mountPath, then rm ociPath. The ext4 umount happens later in ExtractToExt4.

### createExport, layer untar (`oci.go:382-446`)
- For each layer i (base first), it reads the digest and size, failing with `failed to get digest of layer %d: %w` or `failed to get size of layer %d: %w`. These run sequentially before the goroutines start.
- **User Info log per layer:** `fmt.Sprintf("Uncompressing layer %s %s", digest, humanize.Bytes(size))`. The digest prints as `sha256:abc...`, and the size is the compressed one.
- `layerPath = ociPath/layer-<i>-<digest with ':' replaced by '-'>`, for example `layer-0-sha256-ab12...`. The paths are stored **reversed**: `layerPaths[len-1-i]`, so index 0 is the top layer. That is overlayfs lowerdir order (first = uppermost).
- In parallel (errgroup, no limit): `MkdirAll(layerPath, 0755)` (`failed to create directory for layer %d: %w`), then `l.Uncompressed()` (`failed to get uncompressed layer %d: %w`), then `chrootarchive.UntarUncompressed(rc, layerPath, &archive.TarOptions{WhiteoutFormat: archive.OverlayWhiteoutFormat})` (`failed to untar layer %d: %w`).
- The group error is wrapped as `while extracting layers: %w`. On success it logs **user Info** `Layers extracted`.
- moby `OverlayWhiteoutFormat` semantics, which a Rust untar must reproduce:
  - an entry whose basename is `.wh..wh..opq`: set xattr `trusted.overlay.opaque` = `y` on its parent dir and do not create the entry;
  - an entry whose basename is `.wh.<name>`: `mknod(<dir>/<name>, S_IFCHR|0, dev 0)` (a char device 0:0), chown it to the header uid/gid, and do not create the `.wh.` file.
- Other moby untar behaviour that matters: running as root, it chowns to the header uid/gid. It restores mode including setuid, setgid and sticky. It applies `SCHILY.xattr.*` PAX records (non-fatal for `security.*` in some modes), mtimes and atimes (lutimes for symlinks), hardlinks resolved relative to the layer root, and device nodes and FIFOs. Absolute and `..` paths are confined to the root, but symlink targets are kept **verbatim**, even when they escape (test 7.3). Nested whiteouts such as `.wh.foo` plus `foo/.wh.bar` fail with ENOTDIR, which upstream accepts.
- Media types: `Uncompressed()` handles gzip, zstd and plain tar.

### MountOverlayFS (`ext4.go:331-371`), new mount API, needs kernel 6.8 or later
```
fsfd = fsopen("overlay", FSOPEN_CLOEXEC)                    err "fsopen failed: %w"
for layer in layers (top first): fsconfig(fsfd, FSCONFIG_SET_STRING, "lowerdir+", layer)
                                                            err "fsconfig lowerdir failed: %w"
fsconfig(fsfd, FSCONFIG_CMD_CREATE)                         err "fsconfig create failed: %w"
mfd = fsmount(fsfd, 0, 0)                                   err "fsmount failed: %w"
move_mount(mfd, "", AT_FDCWD(-1 passed), mountPoint, MOVE_MOUNT_F_EMPTY_PATH)
                                                            err "move mount failed: %w"
close both fds
```
The mount is read-only overlay because there is no upperdir. `nix` 0.29 has no fsopen wrapper, so this needs `libc::syscall(SYS_fsopen, ...)` behind a small safe type.

### rsync (`oci.go:348-376`)
```
rsync -aH --whole-file --inplace <src>/ <dest>      (cwd = "/")
err -> fmt.Errorf("while copying files from %s to %s: %w: %s", src, dest, err, combinedOutput)
```
ENOSPC detection is `strings.Contains(err.Error(), "No space left on device")`, so it matches rsync's own output text.

ImageTooLargeError texts (`oci.go:44-59`, verbatim):
```
ImageSize>0: "the uncompressed Docker image size (%s) exceeds the maximum filesystem size (%s). Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)"   (humanize.Bytes(ImageSize), humanize.Bytes(MaxSize))
else:        "the Docker image is too large for the maximum filesystem size of %s. Please reduce your Docker image size (e.g., use a smaller base image, multi-stage builds, or remove unnecessary files)"
```
`du -sb dir` gives the first field of the output, parsed as i64. Errors (all ignored by the caller): `error running du: %w`, `unexpected du output: %s`, `error parsing du output: %w`.

### e2fsprogs helpers (`ext4.go`)
| Fn | Command | Error text |
|---|---|---|
| `Mount` | `mount -o loop <file> <mp>` | bare exec err |
| `Unmount` | `umount <path>` | `error unmounting ext4 filesystem: %w` |
| `MakeWritable` | `tune2fs "-O ^read-only" <file>`. **One argv element** `-O ^read-only`, which getopt reads as `-O` with optarg ` ^read-only` (leading space) | bare exec err |
| `Shrink` | `resize2fs -M <file>`, returns stat size | `error shrinking rootfs file: %w`, `error stating rootfs file after resize: %w` |
| `Resize` | `resize2fs <file> <BytesToMB(target)>M` | `error resizing rootfs file: %w`, `error stating rootfs file after resize: %w` |
| `Enlarge` | stat, then `Resize(stat.size + add)` | `error stating rootfs file: %w` |
| `CheckIntegrity(fix)` | `e2fsck -pfv <file>` (fix) / `e2fsck -nfv <file>`, CombinedOutput | see below |
| `RemoveFile` | `debugfs -w -R 'rm "<path>"' <file>`, CombinedOutput | logs Error `error removing file` {error, output}; returns `error removing file: %w` |
| `GetFreeSpace` | `debugfs -R stats <file>` with env `LC_ALL=C`, stdout only | logs Error `Error getting free space` {error, output}; `error statting ext4: %w`; `could not parse free blocks: %w`; `could not parse reserved blocks: %w` |
| `SetReservedBlocksOnHost` | no-op if MB <= 0, else `tune2fs -r <MB*2^20/4096> <file>` | `error setting reserved blocks: %w` |
| `LogMetadata` | `tune2fs -l <file>`, logged at Debug `tune2fs -l output` {path, output, error} | runs before every CheckIntegrity and on every resize/shrink failure |
| `ReplayJournal` | `e2fsck -p -E journal_only <file>` | only ensurefreedisk uses it, not M14 |

stdout and stderr of mkfs, mount, umount, tune2fs and resize2fs go to telemetry span events, not to the build log.

**CheckIntegrity exit handling** (`ext4.go:241-284`): with fix, the accepted bits are `1|2`. Without fix, nothing is accepted.
- The process never completed (exit -1: failed to start or killed by a signal): `"error running e2fsck: %w\n%s"` (err, output).
- `exitCode &^ accepted != 0`: `"error running e2fsck [exit %d]\n%s"`.
- Otherwise it returns `TrimSpace(output)` and no error.

**debugfs stats parsing** (`ext4.go:30-34, 415-448`), regexes verbatim:
```
groupHeaderPattern     = (?m)^[ \t]*Group[ \t]+\d+:
groupFreeBlocksPattern = (?m)^[ \t]+(\d+)[ \t]+free blocks?,
reservedBlocksPattern  = Reserved block count:\s+(\d+)
```
- free = sum of all group counters. It fails if groups == 0 or counts differ: `could not parse free blocks for every block group: found %d groups and %d counters`, and `could not parse block-group free blocks: %w`.
- reserved fails with `could not find reserved blocks in debugfs output` or `could not parse reserved blocks: %w`.
- freeBytes = `(free - reserved) * blockSize`.

### Feature flags involved (`S/pkg/featureflags/flags.go`)
- `build-base-rootfs-size-limit-mb` = 25000 (line 652)
- `build-reserved-disk-space-mb` = 256 (line 660)
- `build-provision-version` = 0 (line 551)
- `build-envd-memory-protection` = false (line 567)

Rust has no feature-flag client, so use these defaults as constants.

---
## 3. Injected files (`B/core/rootfs/rootfs.go:214-282`, `templates.go`, `files/*.tpl`)

There are two extra OCI layers on top of the image, appended in this order: the **files layer**, then the **symlink layer**.

**Files layer** (`oci/layer_file.go:22-53`). It is a tar with entries sorted by name. Each header has only `Name`, `Size` and `Mode`: typeflag regular, uid/gid 0, mtime 0, no parent dir entries. When untarred, moby creates the missing parents as 0755.

| tar name (as written) | mode | content |
|---|---|---|
| `/usr/bin/envd` (`storage.GuestEnvdPath`, leading slash kept in the tar name) | 0777 | `HOST_ENVD_PATH` file. Error: `error reading envd file: %w` |
| `etc/systemd/system-preset/00-e2b.preset` | 0644 | `enable envd.service\n` |
| `usr/local/bin/provision.sh` | 0777 | rendered provision.sh (section 3b) |
| `usr/bin/busybox` | 0755 | `HOST_BUSYBOX_DIR/BUSYBOX_VERSION/<GOARCH>/busybox`. Error: `error reading busybox file %s: %w` |
| `usr/bin/init` | 0755 | the same busybox bytes |
| one entry per `WriteFile` path of each template, listed below | per template | rendered with `bytes.TrimSpace`, so there is **no trailing newline** and no leading blank line |

Template rendering (`templates.go:21-37`): a template that renders no text and no path is skipped. Text without a path fails with `error generating file from %q: template %q did not set path`, wrapped as `error populating filesystem: ...`. Execute errors are `error executing template %q: %w`. Paths have their leading `/` stripped.

**Symlink layer** (`layer_file.go:56-85`): `etc/systemd/system/multi-user.target.wants/envd.service`, a symlink with mode 0777 to `/etc/systemd/system/envd.service` (absolute target).

**Template model values** (`templates.go:54-92`):
- `Hostname` = `e2b.local`
- `ProvisionLogPrefix` = `[external] ` (trailing space)
- `ProvisionResultPath` = `/provision.result`
- `ProvisionExitPrefix` = `E2B_PROVISIONING_EXIT:`
- `Nameserver` = `8.8.8.8`
- `MemoryLimit()` = `int(min(MemoryMB/2.0, 512))`, a float that is truncated, so 100 MB gives 50
- `EnvdMemoryProtection()` = flag `build-envd-memory-protection` (default false)
- `EnvdMemoryMinMiB()` = 128 and `EnvdMemoryLowMiB()` = 256

| template | guest path(s) | mode | variables |
|---|---|---|---|
| disable-watchdog.service.tpl | `etc/systemd/system/systemd-journald.service.d/override.conf`, `etc/systemd/system/systemd-networkd.service.d/override.conf` (same bytes) | 0644 | none |
| envd.openrc.tpl | `usr/local/share/e2b/envd.openrc` | 0755 | MemoryLimit |
| envd.service.tpl | `etc/systemd/system/envd.service` | 0644 | MemoryLimit, EnvdMemoryProtection, Min/Low |
| hostname.tpl | `etc/hostname` | 0644 | Hostname, giving `e2b.local` |
| hosts.tpl | `etc/hosts` | 0644 | Hostname |
| chrony-source.openrc.tpl | `usr/local/share/e2b/chrony-source.openrc` | 0755 | none |
| chrony-source.service.tpl | `etc/systemd/system/e2b-chrony-source.service` | 0644 | none |
| chrony-source.sh.tpl | `usr/local/bin/e2b-chrony-source` | 0755 | none |
| inittab.tpl | `etc/inittab` | 0777 | none |
| journald.conf.tpl | `etc/systemd/journald.conf.d/e2b.conf` | 0644 | none |
| provision-runner.sh.tpl | `usr/local/bin/e2b-provision-runner` | 0755 | ProvisionLogPrefix, ProvisionResultPath, ProvisionExitPrefix |
| rcS.sh.tpl | `etc/init.d/rcS` | 0777 | none |
| resolv.conf.tpl | `etc/resolv.conf` | 0644 | Nameserver, giving `nameserver 8.8.8.8` |
| seed-certs.sh.tpl | `usr/local/bin/e2b-seed-certs` | 0755 | none |
| serial-getty-autologin.service.tpl | `etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf` | 0644 | none |
| system.slice.d-envd.conf.tpl | `etc/systemd/system/system.slice.d/10-e2b-envd.conf`, **only when protection is on** | 0644 | Min/Low |

That gives 16 template paths plus 5 fixed files = **21 files** (the upstream test asserts 21 at MemoryMB=100). With protection on there are 22.

The raw templates are in **Appendix A**, and the rendered outputs in `tmp/render/out/rendered/`. In Rust, write the files exactly as rendered: trimmed, with no trailing newline. The `{{- /*gotype...*/ -}}` header line and the `WriteFile` line render to whitespace that is trimmed away.

**filesHash** (`rootfs.go:62-79`): `sha256` over, for each `files/*` entry sorted by name, `fmt.Fprintf(h, "%s\x00%x\x00", name, rawTemplateBytes)` (lower-hex of the raw template bytes). Then, for each symlink sorted, `"%s\x00%s\x00"` name and target. Output is hex. It covers the **raw templates**, not the rendered output, and not provision.sh, envd or busybox.

### 3b. provision.sh (`B/phases/base/provision.sh`, `provision.go:43-74`, `files.go:45-50`)
It is a Go text/template named `provisioning-script` with params:
- `BusyBox` = `/usr/bin/busybox`
- `ResultPath` = `/provision.result`
- `Provider` = env `PROVIDER` (default `gcp`). `{{ if eq .Provider "gcp" }}` only adds a comment line, `# GCP Specific logic`. Any other value leaves blank lines.
- `DistroSelector` = `distro.ShellSelector()` (`distro/distro.go:202-268` plus the init blocks in `distro/init.go`). It is generated from the profile registry: debian|ubuntu, fedora|centos|rocky|almalinux, arch|archarm, alpine, nixos. Rejected: rhel|ol|amzn. Unknown IDs fall back to ID_LIKE with a WARNING.

The rendered script is **not** trimmed, so it is used as is. The raw template is in **Appendix B**. The full rendered output is in `tmp/render/out/provision.rendered.sh`, and the easiest path is to embed that file verbatim, with `Provider=gcp` baked in, plus a one-line variant if the provider can differ. Template errors: `error executing provision script: %w`, wrapped as `error getting provision script: %w`.

Key script behaviour:
- `set -eu`
- `chattr +i /etc/resolv.conf`
- distro detection from `/etc/os-release` `ID` and `ID_LIKE`
- writes `/usr/local/share/e2b/distro.env` (`E2B_DISTRO_ID`, `E2B_INIT_SYSTEM`, `E2B_ADMIN_GROUP`)
- package install of the missing packages and the CA bundle
- `/etc/tmpfiles.d/fuse.conf`
- profile.d shell and PS1 setup
- `passwd -d root`
- chrony config (`include /run/chrony-e2b/source.conf`, `makestep 1.0 3`) and `/etc/chrony.conf`
- sshd drop-in or append
- sysctl `fs.inotify.max_user_watches=65536` and `vm.compaction_proactiveness=0`
- `e2b_init_setup`
- `rm -rf /etc/machine-id`
- `ln -sf $E2B_INIT_BIN /usr/sbin/init` (and `/sbin/init` unless `/sbin` is a symlink)
- `chattr -i /etc/resolv.conf`
- `rm -rf /etc/init.d/rcS /usr/local/bin/provision.sh`
- `printf "0" > /provision.result`

Distro fingerprint: `sha256hex("1" + "\x00" + ShellSelector())`, where `distro.Version = "1"`.

---

## 4. Provisioning boot (`B/phases/base/provision.go:76-214`)

### Guest side
1. The kernel boots `init=/usr/bin/init`, which is busybox init.
2. busybox reads `/etc/inittab`:
   - `::sysinit:/etc/init.d/rcS` mounts proc, sysfs, devtmpfs and the tmpfs dirs and echoes `Mounting essential filesystems` and `System Init`;
   - `::wait:/usr/bin/busybox ash /usr/local/bin/e2b-provision-runner`;
   - `::wait:/usr/bin/busybox sleep infinity`.
3. The runner runs `$BB sh /usr/local/bin/provision.sh 2>&1 | $BB sed "s/^/[external] /"`, then `sync`, then `fsfreeze --freeze /` if available. Otherwise it echoes `[external] fsfreeze not available on this image; using sync-only flush` and runs `sync` again.
4. It then echoes `E2B_PROVISIONING_EXIT:<content of /provision.result>`, or `E2B_PROVISIONING_EXIT:1` if the file cannot be read.
5. All of this reaches ttyS0, which is Firecracker's stdout.

### Host side: sandbox creation
- `sandbox.Config`: Vcpu, RamMB, HugePages, FreePageReporting, FreePageHinting from the template config. `Envd.Version` = bb.EnvdVersion. `FirecrackerConfig{KernelVersion, FirecrackerVersion}`. Network is nil, which means **unrestricted internet** ("Allow sandbox internet access during provisioning").
- `RuntimeMetadata`: `SandboxID = "b" + id.Generate()` (`config.InstanceBuildPrefix = "b"`), `ExecutionID = uuid`, `SandboxType = build`, TemplateID, TeamID, BuildID.
- `sandboxFactory.CreateSandbox(ctx, cfg, runtime, localTemplate, provisionTimeout=5m, rootfsPath, fc.ProcessOptions{...}, nil, nil)`, with options:
  - `IoEngine: "Sync"` (`layer.DefaultIoEngine = models.DriveIoEngineSync`)
  - `InitScriptPath: "usr/bin/init"`. The constant has **no leading slash** (`rootfs.BusyBoxInitPath`), so Go passes `init=usr/bin/init`. The kernel resolves it relative to `/` at boot, so it works. For exact parity, emit `init=usr/bin/init`.
  - `KernelLogs: true`
  - `KvmClock: true`
  - `Stdout` and `Stderr` both go to `logsWriter`
  - no MMDS (AccessToken nil), no pre-boot hook, no CmdlineArgs, no CPU template
- **Rootfs mode.** `rootfsCachePath != ""` means `rootfs.NewDirectProvider` (`O/pkg/sandbox/rootfs/direct.go`):
  - `OpenFile(path, O_RDWR|O_CREATE, 0644)` and an mmap of the whole size (`error opening file: %w`, `error mapping region: %w`);
  - `Path()` is the file itself, so Firecracker writes **straight into rootfsPath**;
  - `Close` flushes the mmap, fsyncs and unmaps.

  The memfile is `block.NewEmpty(MBToBytes(MemoryMB), pageSize, buildID)`. The rootfs is `block.NewLocal(rootfsPath, 4096, buildID)` (`files.go:64-80`; errors `failed to parse build id: %w`, `error reading rootfs blocks: %w`, `error creating memfile: %w`).

### Kernel command line (`O/pkg/sandbox/fc/kernel_args.go:96-159`)
The args are sorted and joined with spaces. Keys with an empty value print bare.
```
base: quiet loglevel=1 init=<InitScriptPath> ip=<ipv4> ipv6.disable=0 ipv6.autoconf=1 panic=1 reboot=k pci=off random.trust_cpu=on rootflags=discard selinux=0
amd64 only: i8042.nokbd i8042.noaux, and clocksource=kvm-clock if KvmClock
SystemdToKernelLogs: systemd.journald.forward_to_console
KernelLogs || SystemdToKernelLogs: delete quiet; console=ttyS0; loglevel=5
then CmdlineArgs overlay (validated)
```
For provisioning on amd64 this gives:

`clocksource=kvm-clock console=ttyS0 i8042.noaux i8042.nokbd init=usr/bin/init ip=<..> ipv6.autoconf=1 ipv6.disable=0 loglevel=5 panic=1 pci=off random.trust_cpu=on reboot=k rootflags=discard selinux=0`

Rust's `BASE_BOOT_ARGS` (`R/src/firecracker/config.rs:6`) is a different, simplified line (`console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw ip=... init=...`). Because it keeps `console=ttyS0`, serial output already reaches console.log, but the line is not at parity.

Firecracker stdout passes through `fcLogFilter` (`fc/process.go:47-80`). It drops lines containing `FlushMetrics` and the next line containing `The request was executed successfully`. stderr is not filtered.

### Host side: console scan (`provision.go:88-146`)
- `prefixedLogsWriter = PrefixFilteredWriter{Writer: zapio(userLogger, Debug), PrefixFilter: "[external] "}`. Only lines that **start with** `[external] ` are forwarded to the **user build log at Debug level**, with the prefix removed. Other lines (kernel and rcS output) are dropped from the user log. Lines are split on `\n` only, so a trailing `\r` from the tty stays in the text.
- In parallel, a `bufio.Scanner` over a pipe copy of the same stream. ScanLines drops a trailing `\r`, and the max token is 64 KiB. If a line exceeds it, `scanner.Err()` is ErrTooLong, which is returned as is.
```go
const failureTailLines = 30
for scanner.Scan() {
    line := scanner.Text()
    if after, ok := strings.CutPrefix(line, "E2B_PROVISIONING_EXIT:"); ok {
        if after == "0" { return nil }
        if len(tail) > 0 { return fmt.Errorf("exit status: %s; provisioning output tail:\n%s", after, strings.Join(tail, "\n")) }
        return fmt.Errorf("exit status: %s", after)
    }
    if after, ok := strings.CutPrefix(line, "[external] "); ok {
        if trimmed := strings.TrimSpace(after); trimmed != "" {
            tail = append(tail, trimmed); if len(tail) > 30 { tail = tail[1:] }
        }
    }
}
if err := scanner.Err(); err != nil && !errors.Is(err, io.EOF) { return err }
return errors.New("exit code not detected")
```
- The marker must be at the **start of the line**, and the comparison is exact (`"0"`).
- `done.WaitWithContext(ctx)` errors become `phases.NewPhaseBuildError(base meta, fmt.Errorf("error waiting for provisioning sandbox: %w", err))`. The user sees, for example, `error waiting for provisioning sandbox: exit status: 1; provisioning output tail:\n[provision] ERROR: unsupported base image distribution: ID='...'.\n...`. On ctx cancel the error is `ctx.Err()`, also wrapped as a PhaseBuildError.
- CreateSandbox failure: `error creating sandbox: %w` (internal).

### After success
1. user Info `Provisioning was successful, cleaning up`.
2. `sbx.Shutdown(ctx)` (`O/pkg/sandbox/sandbox.go:1891-1931`): **pause the VM**, then `CreateSnapshot` into a throwaway cache path to flush the drive, then `Close` (kill FC, then DirectProvider Close, which runs msync and fsync). Errors: `failed to pause VM: %w`, `error creating snapshot: %w`, `error stopping sandbox: %w`, all wrapped as `error shutting down provisioned sandbox: %w`.
3. `filesystem.RemoveFile(rootfsPath, "/provision.result")` runs `debugfs -w -R 'rm "/provision.result"' <rootfsPath>`. Error: `result file cleanup failed: %w`.
4. user Info `Sandbox template provisioned`.
5. Deferred: `sbx.Close`, then `proxy.RemoveFromPool(LifecycleID)`, with Warn `errors when manually closing connections to sandbox` or Debug `removed proxy from pool`.

The outer error from `provisionSandbox` is wrapped as `error provisioning sandbox: %w`, and the user message stays the inner PhaseBuildError text.

---

## 5. Base layer phase (`B/phases/base/*.go`)

### Phase interface (Go `phases.BuilderPhase`; Rust `R/src/builder/phase.rs` `Phase`)
| Go | value |
|---|---|
| `Prefix()` | `"base"` |
| `String()` | `"FROM TEMPLATE " + alias` / `"FROM " + fromImage` / err `build has no base image or template` |
| `Metadata()` | `PhaseMeta{Phase: "base", StepType: "base"}`, so the step string is `base` |

### Hash (`hash.go:22-158`)
- `baseSource` = `"template:" + FromTemplate.BuildID`, or else `FromImage` (raw string).
- `provisionVersion` = `cache.HashKeys(provisionScriptFile /*raw template text*/, rootfs.FilesHash(), distro.Fingerprint())`, unless flag `build-provision-version` (with template and team context) differs from its fallback 0, in which case it is `strconv(val)`.
  - `HashKeys(base, k...)` = `sha256hex(base + ";" + k1 + ";" + k2 ...)`, already `hash_keys` in Rust.
  - Today the value is `83b23b6a510b214cdfc5bce2a379219610df77a4e4fecc1c7598fcd17caa08f7`.
  - This needs our own envd-rs copies of the files to be byte-identical to upstream for parity. Otherwise the hash differs, which only matters for sharing a cache with Go nodes.
- key = `HashKeys(indexVersion, provisionVersion, str(DiskSizeMB), baseSource, [cmdline:<KernelArgs sorted string>], [cpu-template:<..>], [envd-memory-protection:128:256 when FromTemplate==nil && protection on])`. The bracketed keys are appended only when present.

### Layer (`builder.go:297-381`)
- **FromTemplate:** `index.Cached(buildID)`. On `ErrObjectNotExist` it returns PhaseBuildError `error getting base template, you may need to rebuild it first`. Other errors give `error getting base template: %w`. On success it returns `tm.BasedOn({alias, buildID})` with Cached true.
- **FromImage:**
  - The fresh meta is: `Version: metadata.CurrentVersion`, `Template{BuildID: uuid.New(), KernelVersion, FirecrackerVersion}`, `Context{User: "root", WorkDir: nil, EnvVars: {}}`, `FromImage: &FromImage`, FromTemplate nil, Start nil.
  - `Force` returns not cached.
  - `index.LayerMetaFromHash(hash)` failing logs (internal logger) Info `base layer not found in cache, building new base layer` {error, hash} and returns not cached.
  - `index.Cached(bm.BuildID)` failing logs Info `base layer metadata not found in cache, building new base layer` and returns not cached.
  - Otherwise it returns cached.

### Build: buildLayerFromOCI (`builder.go:121-295`), in order
1. `MkdirAll(TEMPLATES_DIR/<buildID>, 0777)`, error `error creating template build directory: %w`, with `RemoveAll` in defer.
2. `constructLayerFilesFromOCI`: render provision.sh, then CreateExt4Filesystem (section 2), then block.NewLocal and an empty memfile. Error: `error building environment: %w`.
3. Cache paths. Error: `error creating template files: %w`. Then a LocalTemplate.
4. `baseMetadata.Context.EnvVars = ParseEnvs(imgConfig.Env)`.
5. user Info `Provisioning sandbox template`. Then **provisionSandbox** (section 4), with error `error provisioning sandbox: %w`.
6. `CheckIntegrity(rootfsPath, fix=true)`. On error, Error log `provisioned filesystem ext4 integrity` {result, error} and return `error checking provisioned filesystem integrity: %w`. On success, Debug with the same message.
7. `enlargeDiskAfterProvisioning` (`provision.go:216-288`). Its errors are wrapped as `error enlarging disk after provisioning: %w`.
   - `GetFreeSpace` fails with `error getting free space: %w`.
   - `sizeDiff = MBToBytes(DiskSizeMB) - free`, then Debug `adding provision size diff to rootfs` {size_add, size_free, size_target}.
   - If `sizeDiff <= 0`, Debug `no need to enlarge rootfs, skipping` and return.
   - `Enlarge(rootfsPath, sizeDiff)`. On error it runs `tune2fs -l` and logs Error with the output as message, then returns `error enlarging rootfs: %w`.
   - `CheckIntegrity(fix=false)`. On error, Error `final enlarge filesystem ext4 integrity`, then retry `CheckIntegrity(fix=true)` with Error `final enlarge filesystem ext4 integrity - retry with fix`. If that fails too: `error checking final enlarge filesystem integrity: %w`. On success, Debug `final enlarge filesystem ext4 integrity`.
   - `stat` fails with `error getting rootfs file info: %w`. If `finalSize != stat.size`: `size mismatch: expected %d, got %d`.
   - `rootfs.UpdateHeaderSize()` fails with `error updating rootfs header size: %w`.
8. user Debug `Creating base sandbox template layer`.
9. `layer.NewCreateSandbox(baseSbxConfig, factory, 10min, WithRootfsCachePath(rootfsPath), [WithPreBootFn(tune2fs -r 65536 ...)])`. This is also the direct file rootfs mode. The reserved hook runs when `build-reserved-disk-space-mb` > 0 (default 256 MB, which is 65536 blocks).
   - `CreateSandbox.Sandbox` (`layer/create_sandbox.go:111-188`) uses an empty memfile of RamMB and `InitScriptPath: "/sbin/init"` (`constants.SystemdInitPath`).
   - Options: `KernelLogs: env.IsDevelopment()`, SystemdToKernelLogs false, `KvmClock: envdVersion >= "0.2.11"` (errors `error comparing envd version: %w`), IoEngine Sync, the CmdlineArgs and CPUTemplate of the config, no AccessToken (no MMDS), timeout 10 min (`baseLayerTimeout`, again only as endAt).
   - `sbx.WaitForEnvd(ctx, StartTypeCreate, 60s)` (`layer/interfaces.go:16`). Errors: `create sandbox: %w`, `wait for envd: %w`.
10. Action: `sandboxtools.SyncChangesToDisk` runs the envd process `/usr/bin/busybox sync` as `root` (`sandboxtools/command.go:247-261`). Error: `error running sync command: %w`.
11. `layerExecutor.BuildLayer({SourceTemplate: DirectSourceTemplateProvider(localTemplate), CurrentLayer: baseMetadata, Hash, UpdateEnvd: false, SandboxCreator, ActionExecutor, BuildOrigin: ObjectOriginTemplateBuildCache})` pauses, snapshots, uploads and indexes. That part is M15 territory. Error: `error building base layer: %w`.
12. Build returns `LayerResult{Metadata: baseLayer, Cached: false, Hash}`.

DirectProvider `ExportDiff` (`direct.go`): it stops the sandbox, waits for Close, msyncs and fsyncs. It then reads the whole file in 4 KiB blocks up to `header.Metadata.Size`. All-zero blocks become `empty`, and the others are written to `out` and marked `dirty`. **No base header, so it is a full image diff.**

---
## 6. Rust side today (`R` = `crates/orchestrator`)

**What exists**
- `src/builder/phase.rs`: the `Phase` trait. `prefix()`, `describe()` and `meta() -> PhaseMeta{phase, step_type, step_number}` are sync. `hash(&source)`, `layer(&source, hash)` and `build(ScopedLog, &source, current)` are `BoxFuture`. `run_phases` logs `[CACHED ][prefix] describe [hash]`, skips cached layers, validates and builds. `BuildError::user(&meta, msg)` / `Internal`. `PhaseMeta.step()` for base is `step_type` = `"base"`, which matches Go `stepString`.
- `src/builder/pipeline.rs`: `PhasePipeline::phases()` returns an **empty Vec**, and `build` then errors `template build phases are not ported yet`. M14 adds the base phase here.
- `src/builder/hash.rs`: `hash_keys`, `base_hash(provision_version, disk_size_mb: i32, source)` with only 3 keys, and `template_source`. M14 must add a provision-version computation (the files hash plus the distro fingerprint of our embedded copies) and, for parity, the optional `cmdline:` / `cpu-template:` / `envd-memory-protection:` keys.
- `src/builder/index.rs`: `LayerIndex::{save, cached}`. `src/builder/logs.rs`: `BuildLog` / `ScopedLog::push(level, msg)` / `push_with(fields)`.
- `src/sandbox.rs` + `src/sandbox/config.rs`: `Sandbox::builder()...cold_boot()`.
  - `SandboxConfig.rootfs: Arc<Overlay>` is **required**. `start()` always runs `attach_disk_and_slot()`, which uses `exposure::nbd::expose(overlay)` (NBD), so **there is no direct file rootfs mode**.
  - `init_path` defaults to `/sbin/init` and is sent as extra boot args `init=<path>`.
  - `envd_timeout` defaults to 60 s. `init_envd` does `POST /init` with retries against the deadline and races `vm.exited()`.
  - Boot always writes MMDS. Go provisioning and base boots write no MMDS (AccessToken nil).
  - Resources released are disk, slot, cgroup, vm, memory and prefetch.
- `src/firecracker/launch.rs`: the `Launch` script symlinks `rootfs` (any host path) to `/fc-vm/rootfs.ext4` in a private tmpfs, then `ip netns exec`. It already accepts a plain file path, so a direct mode only needs `rootfs(PathBuf)` = the ext4 file.
- `src/firecracker/config.rs`: `VmConfig` with `BASE_BOOT_ARGS = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw"`, `ip=169.254.0.21::169.254.0.22:255.255.255.252::eth0:off` and extra args. It is **not at parity** with Go's `buildKernelArgs`: no `quiet loglevel=1` by default, no ipv6/random.trust_cpu/rootflags/selinux/i8042/clocksource, and no KernelLogs toggle (console=ttyS0 is always on).
- `src/firecracker/client.rs::boot`: the drive is `{drive_id: rootfs, path_on_host, is_root_device, is_read_only:false}` with **no `io_engine`**, while Go sets `Sync` explicitly. It also does the MMDS config and data, then `InstanceStart`. `pause`, `create_snapshot` and the memory APIs exist.
- `src/firecracker/process.rs`: FC stdout and stderr go **to a file** `work_dir/console.log` (`File::create`). `console_tail()` reads the last 2000 bytes. There is **no streaming reader** and no FlushMetrics filter. The serial scan for `E2B_PROVISIONING_EXIT:` therefore needs either a pipe (a tokio `ChildStdout`, tee'd to the file) or a tail-follow of console.log until FC exits. EOF corresponds to FC exit.
- `src/sandbox/snapshot.rs`: `pause` / `pause_into` / `snapshot` export the memfile and the rootfs through the Overlay (`flush_disk`, `export_rootfs`). With a direct file rootfs, a new export path is needed: a full-file scan in 4 KiB blocks into `DiffMetadata` (zero blocks become empty, the rest dirty), as Go `DirectProvider.exportToDiff` does.
- `src/block/overlay.rs` (BuildFile plus BlockCache), `src/exposure/nbd*` and `src/storage/header*` (DiffMetadata, PAGE_SIZE) can be reused for the export.
- `src/envd.rs::init`: `POST /init` with retries. A "run `/usr/bin/busybox sync` via envd process.Start" helper does not exist yet. The spec puts envd command execution in M15, but the base layer needs it in M14.
- `src/server/config.rs::NodeConfig` has `E2B_FIRECRACKER`, `E2B_KERNEL`, `E2B_WORK_DIR`, `E2B_CACHE_DIR`, `ENVD_TIMEOUT_MS` (default 10 s), `ENVIRONMENT`. There is **no** envd host path, busybox path, templates dir, `PROVIDER`, `TARGET_ARCH` or `DOCKERHUB_REMOTE_REPOSITORY_*`.
- `Cargo.toml` deps: no `oci-client`, `tar`, `flate2` or `zstd`. Present: `nix 0.29` (fs, ioctl, mman, process...), `sha2`, `base64`, `reqwest` (rustls-free json), `object_store` (aws, gcp, azure), `uuid`, `tokio full`. No AWS SDK (ECR `GetAuthorizationToken` needs `aws-sdk-ecr` or a signed SigV4 call), no `libc` direct dep (fsopen and fsconfig need raw syscalls), no xattr crate (`nix` 0.29 lacks setxattr, so use `libc::lsetxattr`, `xattr` or `rustix`).

**Gaps M14 must add**
1. A direct file rootfs mode in `SandboxConfig`/`Sandbox::start`, either an enum `Rootfs::{Overlay(Arc<Overlay>), File(PathBuf)}` or similar. It skips NBD, symlinks the file, and on stop runs pause, a throwaway snapshot, kill, then fsync of the file (Go `Shutdown`). Then a full-file diff export for the base layer upload.
2. Boot options: init path `usr/bin/init` (no leading slash, as Go), kernel logs on or off, kvm-clock (amd64), IoEngine Sync, MMDS off. The boot args should become Go's sorted `KernelArgs`.
3. Console streaming with the `[external] ` filter to the user log (Debug), the marker scan (strip `\r`), a 30-line tail, and the texts from section 4.
4. Host tooling wrappers: mkfs.ext4, mount/umount loop, the overlay new mount API, the parallel layer untar with overlay whiteouts, rsync, du, e2fsck/resize2fs/tune2fs/debugfs, with the exact argv and texts from section 2.
5. OCI pull: oci-client with platform `linux/<arch>`, the arch check, auth variants (Basic for general, `_json_key` for GCP, ECR token for AWS), the Docker Hub proxy envs, and `wrapImagePullError` mapping on OCI distribution error codes plus HTTP status.
6. Assets: `assets/template/` with the 17 template outputs (or raw `.tpl` plus a tiny renderer for the 4 variable ones: envd.service and envd.openrc (MemoryLimit), provision-runner (constants), and the optional slice drop-in), plus `provision.sh` and the generated selector. The symlink layer and the preset file.
7. Node config: envd binary path (`HOST_ENVD_PATH`), busybox (`HOST_BUSYBOX_DIR`/`BUSYBOX_VERSION`/arch, or the spec's `E2B_BUSYBOX`), templates dir (`TEMPLATES_DIR`), `PROVIDER`, `TARGET_ARCH`.
8. An envd exec of `/usr/bin/busybox sync` as root.

---

## 7. Upstream unit tests relevant to M14 (test vectors)

**7.1 `filesystem/ext4_test.go` `TestParseFreeBlocks`** (`parseFreeBlocks`):
```
"Free blocks:              50000\n Group  0: block bitmap at 1, inode bitmap at 2, inode table at 3\n           28629 free blocks, 100 free inodes, 1 used directory\n Group  1: block bitmap at 4, inode bitmap at 5, inode table at 6\n           28639 free blocks, 100 free inodes, 1 used directory\n"  => 57268
" Group  0: block bitmap at 1, inode bitmap at 2, inode table at 3\n           1 free block, 1 free inode, 1 used directory\n"  => 1
" Group  0: ...\n           10 free blocks, 100 free inodes, 1 used directory\n Group  1: block bitmap at 4, inode bitmap at 5, inode table at 6\n"  => error (2 groups, 1 counter)
"Free blocks:              50000\n"  => error (no groups)
```
`TestParseReservedBlocks`:
- `"Block count:              131072\nReserved block count:     6553\nFree blocks:              120000\n"` gives 6553
- `"Reserved block count:     0\n"` gives 0
- no line gives an error

`TestMakeKeepsDirIndex`: `Make(path, 64, 4096)`, then `dumpe2fs -h` (LC_ALL=C) `Filesystem features:` must contain `dir_index`.

**7.2 `filesystem/ext4_checkintegrity_test.go` `TestCheckIntegrityExitStatus`** (fake `e2fsck` on PATH):

| script | fix | result |
|---|---|---|
| exit 0 | true | ok |
| exit 4 | true | error |
| exit 8 | true | error |
| exit 1 | false | error |
| exit 1 | true | ok |
| exit 2 | true | ok |
| exit 3 | true | ok |
| `kill -9 $$` | true | error |
| missing binary | true | error |

**7.3 `oci/oci_test.go`**
- `TestCreateExportLayersOrder`: 3 layers, each with `layerN.txt` (mode 0600, content `layer text`). Returned paths are reversed: `[0]` matches `/layer-2.*` and contains `layer2.txt`, and `[2]` matches `/layer-0.*`.
- `TestCreateExportHandlesOCIWhiteout`: lower layer `stale-file.txt` (`old-version`), upper layer `.wh.stale-file.txt` (0644, size 0). Upper dir: no `.wh.stale-file.txt`, and `stale-file.txt` **is a char device**. The lower file still exists.
- `TestCreateExportPreservesEscapingRelativeSymlink`: dirs `nix/`, `nix/store/`, `nix/store/.links/`, `etc/`, and `etc/environment` = `PATH=/usr/bin\n`. Symlink `nix/store/.links/abc` points to `../../../../../etc/environment` and must be preserved verbatim. `nix/store/link-within` points to `../../etc/environment`.
- `TestGetPublicImageWithGeneralAuth`: a local registry with Basic auth (`testuser`/`testpass`), image `test/image:latest` with config arch = TargetArch and os linux. Correct creds pull 1 layer. Wrong creds give an error. No auth on a public registry works. In all three cases the noop dockerhub repo is not used, because the host is not index.docker.io.
- `TestWrapImagePullErrorSanitizesOriginalError`: status 418 with code UNKNOWN gives `failed to pull image 'registry.example.com/test/image:latest': registry returned status code 418`.
- `TestWrapImagePullErrorUsesPredefinedRegistryCodeMessage`: 401 with UNAUTHORIZED gives `access denied to 'registry.example.com/test/image:latest': authentication required or insufficient permissions`.

**7.4 `rootfs/rootfs_test.go` / `templates_test.go`** (envd = `echo hello`, busybox = `busybox-binary`, prefix args `"provision.sh","provision.log","provision.result"`)
- `TestAdditionalOCILayers`: MemoryMB=100 gives **21 files** in the files layer.
  - inittab `::` lines contain no `|` or `$`.
  - the runner contains `#!/usr/bin/busybox ash`.
  - `envd.service` contains `ExecStartPre=/usr/local/bin/e2b-seed-certs`, `"GOMEMLIMIT=50MiB"` and `StartLimitIntervalSec=0`, and its `After=` contains `systemd-tmpfiles-setup.service`.
  - seed-certs passes `sh -n`.
  - chrony-source contains `/run/chrony-e2b/source.conf`, `refclock PHC /dev/ptp0` and `pool pool.ntp.org`.
  - `e2b-chrony-source.service` contains `ExecStart=/usr/local/bin/e2b-chrony-source`. The openrc chrony source contains `before chronyd`.
  - preset == `enable envd.service\n`.
  - envd.openrc contains `#!/sbin/openrc-run`, `supervisor=supervise-daemon` and `GOMEMLIMIT=50MiB`.
  - hostname == `e2b.local`. resolv == `nameserver 8.8.8.8`.
  - both watchdog overrides == `[Service]\nWatchdogSec=0`.
  - symlink layer `etc/systemd/system/multi-user.target.wants/envd.service` points to `/etc/systemd/system/envd.service`.
- `TestEnvdMemoryProtectionRender`:
  - off at 4096 MB: envd.service equals the golden file (no trailing newline) and there is no drop-in.
  - on (MemoryMB 128, 512, 1024, 4096, 16384, 0): the drop-in has `[Slice]`, and both files have the lines `MemoryMin=128M` and `MemoryLow=256M`. No other file contains `MemoryMin=` or `MemoryLow=`.
  - on vs off: only `MemoryMin=50M -> MemoryMin=128M` and `MemoryLow=100M -> MemoryLow=256M` change.
- `TestGenerateFile`:
  - `{{ .WriteFile "/etc/probe" 0o644 }}\nbody` renders `body` at path `etc/probe`;
  - a body in a false `if` renders nil data and no paths;
  - `body` with no path fails with `did not set path`.

**7.5 `phases/base/*_test.go`** (key composition, with index `index-v1`, provision `provision-1`, source `ubuntu:22.04`, disk 1024)
- legacy = `HashKeys("index-v1","provision-1","1024","ubuntu:22.04")`.
- protection on = `HashKeys("index-v1","provision-1","1024","ubuntu:22.04","envd-memory-protection:128:256")`.
- cmdline args `{psi:1}` change the key, and arg order does not. A FromTemplate build ignores protection.
- `TestProvisionScriptDefersChronySourceToBoot`: the raw provision.sh must not contain `[ -e /dev/ptp0 ]`, `refclock PHC` or `E2B_CHRONY_PHC`, and must contain `echo "include /run/chrony-e2b/source.conf"` and `echo "makestep 1.0 3"`.
- `TestProvisionScriptWritesSSHSettingsAsDropIn`: the SSH block uses the drop-in `sshd_config.d/00-e2b.conf` when `sshd_config` has `Include /etc/ssh/sshd_config.d/...`, and appends otherwise.
- `distro/distro_test.go`:
  - Fingerprint is 64 hex chars and equals `sha256(Version+"\x00"+ShellSelector())`;
  - the selector has `for e2b_like in $E2B_ID_LIKE; do`, `WARNING`, `unsupported base image distribution` and `e2b_profile_matched=1`, and has no `if e2b_select_profile`;
  - `rhel|ol|amzn` comes before `E2B_ID_LIKE`.

No upstream unit test covers the provisioning marker scan itself. Suggested Rust vectors from the code:
- `E2B_PROVISIONING_EXIT:0\r` gives Ok;
- `[external] a`, `[external]   `, `[external] b`, then `E2B_PROVISIONING_EXIT:1` gives `exit status: 1; provisioning output tail:\na\nb`;
- `E2B_PROVISIONING_EXIT:2` with no tail gives `exit status: 2`;
- 31 external lines keep the last 30;
- EOF gives `exit code not detected`;
- a marker that is not at the line start is ignored.

---

## Appendix A: raw rootfs templates (`B/core/rootfs/files/*.tpl`, verbatim)

Rendered outputs are under `tmp/render/out/rendered/`. Only envd.service, envd.openrc (MemoryLimit), provision-runner (prefix, result path, exit prefix) and the hostname, hosts and resolv files have variables.

### disable-watchdog.service.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "etc/systemd/system/systemd-journald.service.d/override.conf" 0o644 }}
{{ .WriteFile "etc/systemd/system/systemd-networkd.service.d/override.conf" 0o644 }}

[Service]
WatchdogSec=0
````

### envd.openrc.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/usr/local/share/e2b/envd.openrc" 0o755 }}

#!/sbin/openrc-run
# E2B env daemon — the OpenRC counterpart of envd.service (see envd.service.tpl
# for the full rationale on each step; this mirrors it for the Alpine/OpenRC
# family). Baked at a neutral path and installed to /etc/init.d/envd by the
# OpenRC e2b_init_setup only: if it lived in /etc/init.d on every image,
# Debian's `systemctl enable envd` would hand it to update-rc.d, which aborts
# on a non-LSB script and fails the whole provisioning.
#
# /tmp-wipe ordering note: OpenRC's bootmisc (boot runlevel) wipes /tmp before
# the default runlevel starts, so envd — in default — can never answer an
# update-envd upload before the wipe. The race envd.service needs an explicit
# After=systemd-tmpfiles-setup.service for cannot happen here.

description="E2B env daemon"

supervisor=supervise-daemon
command=/usr/bin/envd
# --nicelevel/--ionice/--oom-score-adj mirror envd.service's Nice=-20,
# IOSchedulingClass=realtime + IOSchedulingPriority=4 (ionice class 1, data 4)
# and OOMScoreAdjust=-1000 — without them envd runs at default priority and is
# the first OOM-kill candidate on Alpine. (The unit's cgroup weights —
# MemoryMin/CPUWeight/IOWeight — have no supervise-daemon equivalent.)
supervise_daemon_args="--nicelevel -20 --ionice 1:4 --oom-score-adj -1000 --env GOTRACEBACK=all --env GOMEMLIMIT={{ .MemoryLimit }}MiB --stdout /var/log/envd.log --stderr /var/log/envd.log"
# Retry forever (envd.service uses Restart=always + StartLimitIntervalSec=0).
respawn_delay=1
respawn_max=0

depend() {
    need localmount
    after bootmisc
    use net
}

start_pre() {
    # Shared with envd.service's ExecStartPre; warns-and-continues by design.
    /usr/local/bin/e2b-seed-certs
    # systemd-tmpfiles applies the fuse.conf tmpfiles.d rule on the systemd
    # family; OpenRC has no tmpfiles pass, so set the mode here.
    if [ -e /dev/fuse ]; then
        chmod 666 /dev/fuse
    fi
    return 0
}

````

### envd.service.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/etc/systemd/system/envd.service" 0o644 }}

[Unit]
Description=Env Daemon Service
# Start as early as possible on cold boot: envd only needs journald's socket
# and a writable rootfs; networking is configured by the kernel (ip=) before
# userspace. Default dependencies would gate it on sysinit/basic.target
# (~0.5s), and the previous After=multi-user.target on chrony-wait (~8s).
DefaultDependencies=no
# Order after /tmp is finalized so envd doesn't answer before it's safe to stage
# files there: updateEnvd uploads an update binary to /tmp during early boot.
# On our base images (Ubuntu/Debian) /tmp is a plain rootfs dir, not a tmpfs
# mount, and systemd-tmpfiles-setup.service runs `systemd-tmpfiles --remove`
# with a `D /tmp` rule that wipes /tmp's contents at boot. That service is only
# ordered After=local-fs.target, so gating envd on local-fs.target alone leaves
# them unordered and the upload races the wipe (chmod/mv then fail ENOENT).
# Ordering after systemd-tmpfiles-setup.service closes that race.
After=systemd-journald.socket systemd-remount-fs.service local-fs.target systemd-tmpfiles-setup.service
Wants=systemd-journald.socket
Conflicts=shutdown.target
Before=shutdown.target
# Disable rate limiting; retry forever
StartLimitIntervalSec=0

[Service]
Type=simple
Restart=always
User=root
Group=root
Environment=GOTRACEBACK=all
LimitCORE=infinity
# Seed the tmpfs from the tar packed as the build's last guest step — after all
# build steps, start_cmd, and ready_cmd, with update-ca-certificates run first
# (one sequential read); fall back to copying the cert dir, then to regenerating.
#
# Contract: the tar is the regenerated trust store captured at the end of the
# build, so it equals what update-ca-certificates would produce at boot —
# including CAs added in user layers or start/ready, registered or not. Seeding
# from it gives a complete ca-certificates.crt, so update-ca-certificates is
# skipped on cold boot (its scattered rootfs reads are the cost we avoid). It
# therefore does NOT re-merge a persisted egress-proxy CA
# (/usr/local/share/ca-certificates/e2b-ca.crt) into the bundle at boot. That CA
# is (re)installed by envd's POST /init for the current proxy, which runs before
# the orchestrator marks the sandbox running/routable — so the egress CA is
# guaranteed present for the sandbox's routable lifetime. The only gap is guest
# units that auto-start and egress over TLS before /init; that is accepted
# (revisit if a template needs boot-time egress).
# The seeding logic is shared with the OpenRC service (Alpine) and never fails
# the unit — a degraded trust store is recoverable, a dead envd is not.
ExecStartPre=/usr/local/bin/e2b-seed-certs
ExecStart=/usr/bin/envd
Nice=-20
IOSchedulingClass=realtime
IOSchedulingPriority=4
OOMPolicy=continue
OOMScoreAdjust=-1000
Environment="GOMEMLIMIT={{ .MemoryLimit }}MiB"

Delegate=yes
{{- if .EnvdMemoryProtection }}
MemoryMin={{ .EnvdMemoryMinMiB }}M
MemoryLow={{ .EnvdMemoryLowMiB }}M
{{- else }}
MemoryMin=50M
MemoryLow=100M
{{- end }}
CPUAccounting=yes
CPUWeight=1000
IOAccounting=yes
IOWeight=10000

[Install]
WantedBy=multi-user.target

````

### hostname.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/etc/hostname" 0o644 }}

{{ .Hostname }}
````

### hosts.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/etc/hosts" 0o644 }}

127.0.0.1	localhost
::1	        localhost ip6-localhost ip6-loopback
fe00::	    ip6-localnet
ff00::	    ip6-mcastprefix
ff02::1	    ip6-allnodes
ff02::2	    ip6-allrouters
127.0.1.1	{{ .Hostname }}
````

### chrony-source.openrc.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "usr/local/share/e2b/chrony-source.openrc" 0o755 }}

#!/sbin/openrc-run
# OpenRC counterpart of e2b-chrony-source.service (Alpine): runs the shared
# selector in the boot runlevel, before chronyd starts in default. Baked at a
# neutral path and installed into /etc/init.d by the OpenRC e2b_init_setup only
# — on a systemd image the sysv generator would turn an /etc/init.d script of
# this name into a unit that shadows the real one (and Debian's update-rc.d
# aborts on non-LSB scripts, as envd.openrc.tpl explains).

description="E2B: select the chrony time source (PHC refclock if /dev/ptp0, else the NTP pool)"

depend() {
    before chronyd
}

start() {
    ebegin "Selecting the chrony time source"
    /usr/local/bin/e2b-chrony-source
    eend $?
}

stop() {
    # Nothing to undo — the source file lives on tmpfs.
    return 0
}

````

### chrony-source.service.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "etc/systemd/system/e2b-chrony-source.service" 0o644 }}

[Unit]
Description=E2B: select the chrony time source (PHC refclock if /dev/ptp0, else the NTP pool)
# The chrony unit is chrony.service on Debian and chronyd.service elsewhere;
# ordering before a unit that doesn't exist on this image is ignored. What pulls
# this in is a Requires= drop-in written for the family's unit name by
# provisioning (distro/init.go) — a drop-in, not an enablement symlink, because
# the RHEL family's preset policy ("disable *") deletes those on first boot.
Before=chrony.service chronyd.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/bin/e2b-chrony-source

````

### chrony-source.sh.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "usr/local/bin/e2b-chrony-source" 0o755 }}

#!/bin/sh
# Writes the chrony time source the machine we are booting on can actually use.
# The baked /etc/chrony/chrony.conf includes the file this produces.
#
# The hypervisor's PTP clock (kvm-ptp) is the better source — no network, tracks
# the host directly — but it is absent wherever nested virtualization can't
# expose it, and a refclock line for a missing PHC is FATAL to chronyd. Template
# builds and sandboxes run in separate node pools (docs/ARCHITECTURE.md), so the
# device present while provisioning says nothing about the node a cold-booting
# sandbox lands on; only a boot-time decision is right on both.
#
# Shared by e2b-chrony-source.service (systemd) and the OpenRC service of the
# same name (Alpine).
set -eu

mkdir -p /run/chrony-e2b
if [ -e /dev/ptp0 ]; then
    echo "refclock PHC /dev/ptp0 poll 2 dpoll 2" >/run/chrony-e2b/source.conf
else
    echo "pool pool.ntp.org iburst maxsources 3" >/run/chrony-e2b/source.conf
fi

````

### inittab.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/etc/inittab" 0o777 }}

# Provisioning-boot inittab (busybox init). Every entry is a plain exec with
# NO shell metacharacters: busybox init hands metachar lines to /bin/sh, and
# minimal images (distroless) may have no /bin/sh — the pipeline logic lives in
# e2b-provision-runner instead.

# Run system init (mounts /proc /sys /dev /tmp /run through the baked busybox)
::sysinit:/etc/init.d/rcS

# Run the provisioning pipeline and report its exit code
::wait:/usr/bin/busybox ash /usr/local/bin/e2b-provision-runner

# Wait forever to prevent the VM from exiting until the sandbox is paused and snapshot is taken
::wait:/usr/bin/busybox sleep infinity

````

### journald.conf.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "etc/systemd/journald.conf.d/e2b.conf" 0o644 }}

[Journal]
Storage=persistent
SystemMaxUse=8M

````

### provision-runner.sh.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "usr/local/bin/e2b-provision-runner" 0o755 }}

#!/usr/bin/busybox ash
# Drives the provisioning pipeline for the busybox-init boot. This logic lives
# in a script — NOT in /etc/inittab — because busybox init hands any inittab
# line containing shell metacharacters to /bin/sh, and minimal images
# (distroless) may have no /bin/sh; a plain-exec inittab line running this
# script through the baked busybox works on every image.
BB=/usr/bin/busybox

# Run the provision script, prefix its output with the log prefix the
# orchestrator forwards to the customer's build logs.
$BB sh /usr/local/bin/provision.sh 2>&1 | $BB sed "s/^/{{ .ProvisionLogPrefix }}/"

# Flush filesystem changes to disk before the snapshot.
$BB sync
if command -v fsfreeze >/dev/null 2>&1; then
    fsfreeze --freeze /
else
    # No util-linux on this image; the double sync flushes the ext4 journal
    # and the VM is paused before the snapshot is taken. Prefixed so the
    # fallback shows up in the customer's build logs.
    echo "{{ .ProvisionLogPrefix }}fsfreeze not available on this image; using sync-only flush"
    $BB sync
fi

# Report the provisioning exit code: provision.sh writes "0" on success and
# (running under set -e) leaves no file behind on failure.
if result=$($BB cat {{ .ProvisionResultPath }} 2>/dev/null); then
    echo "{{ .ProvisionExitPrefix }}${result}"
else
    echo "{{ .ProvisionExitPrefix }}1"
fi

````

### rcS.sh.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "etc/init.d/rcS" 0o777 }}

#!/usr/bin/busybox ash
# Every command goes through the baked busybox: this runs before provisioning
# on the raw base image, and minimal images (distroless) may have no mkdir/mount
# on PATH at all.
BB=/usr/bin/busybox

echo "Mounting essential filesystems"
# Ensure necessary mount points exist
$BB mkdir -p /proc /sys /dev /tmp /run

# Mount essential filesystems
$BB mount -t proc proc /proc
$BB mount -t sysfs sysfs /sys
$BB mount -t devtmpfs devtmpfs /dev
$BB mount -t tmpfs tmpfs /tmp
$BB mount -t tmpfs tmpfs /run

echo "System Init"

````

### resolv.conf.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/etc/resolv.conf" 0o644 }}

nameserver {{ .Nameserver }}
````

### seed-certs.sh.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "usr/local/bin/e2b-seed-certs" 0o755 }}

#!/bin/sh
# Seeds the tmpfs-backed /etc/ssl/certs before envd starts — shared by
# envd.service (systemd) and /etc/init.d/envd (OpenRC). See envd.service.tpl
# for the full rationale (why a tar, why a bind mount, the egress-CA contract).
#
# Every failure path here WARNS and continues deliberately: a sandbox with a
# degraded trust store is recoverable (envd's POST /init reinstalls the egress
# CA), a sandbox whose envd never starts is not.

# The copies DEREFERENCE symlinks (-L; the build-time tar already packs with
# -h): some images ship the /etc/ssl/certs bundle as symlinks into a read-only
# store, and envd's egress-proxy CA install APPENDS to
# /etc/ssl/certs/ca-certificates.crt — that only works if the tmpfs copy is a
# real file, not a copied symlink to an immutable target.
if ! mountpoint -q /etc/ssl/certs; then
    mkdir -p /run/e2b/certs
    if [ -f /usr/local/share/e2b/ssl-certs.tar ]; then
        if ! tar -C /run/e2b/certs -xf /usr/local/share/e2b/ssl-certs.tar; then
            echo "e2b-seed-certs: ssl-certs.tar extraction failed; seeding from the live cert dir instead" >&2
            cp -aL /etc/ssl/certs/. /run/e2b/certs/
        fi
    else
        # Only expected during the base-layer boot, before finalize packs the tar.
        echo "e2b-seed-certs: ssl-certs.tar not packed yet; seeding from the live cert dir"
        cp -aL /etc/ssl/certs/. /run/e2b/certs/
    fi
    if ! mount -o bind /run/e2b/certs /etc/ssl/certs; then
        echo "e2b-seed-certs: bind mount failed; envd runs with the image's certs as-is" >&2
    fi
fi

if [ ! -s /etc/ssl/certs/ca-certificates.crt ]; then
    if command -v update-ca-certificates >/dev/null 2>&1; then
        update-ca-certificates
    elif command -v update-ca-trust >/dev/null 2>&1; then
        # RHEL family and Arch refresh with update-ca-trust. Arch's extract emits
        # the Debian-named bundle itself; RHEL keeps it under /etc/pki, and the
        # symlink provisioning made was dereferenced into this tmpfs, so copy it.
        update-ca-trust extract
        if [ ! -s /etc/ssl/certs/ca-certificates.crt ] && [ -s /etc/pki/tls/certs/ca-bundle.crt ]; then
            cp -L /etc/pki/tls/certs/ca-bundle.crt /etc/ssl/certs/ca-certificates.crt
        fi
    else
        # Provisioning guarantees the bundle on every supported family;
        # reaching this means the image diverged after the build.
        echo "e2b-seed-certs: CA bundle missing and no CA refresh tool on this image; TLS trust will be degraded" >&2
    fi
fi

exit 0

````

### serial-getty-autologin.service.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{ .WriteFile "/etc/systemd/system/serial-getty@ttyS0.service.d/autologin.conf" 0o644 }}

[Service]
ExecStart=
ExecStart=-/sbin/agetty --noissue --autologin root %I 115200,38400,9600 vt102

````

### system.slice.d-envd.conf.tpl
````
{{- /*gotype:github.com/e2b-dev/infra/packages/orchestrator/pkg/template/build/core/rootfs.templateModel*/ -}}
{{- if .EnvdMemoryProtection -}}
{{ .WriteFile "/etc/systemd/system/system.slice.d/10-e2b-envd.conf" 0o644 }}

# cgroup v2 grants a cgroup the smaller of its own memory.min request and its
# share of the parent's grant, level by level up to the root. envd.service
# requests protection but sits in system.slice, which requests none, so envd's
# grant is zero and its pages are reclaimed like any other under pressure.
# Requesting the same amount on the slice grants envd's request in full.
# The slice asks for no more than envd does: the kernel hands a slice's
# unclaimed grant to its children in proportion to their usage, so a larger
# request here would shield the other system daemons too, at the guest's
# expense. What can reach them is bounded by the request minus envd's use.
[Slice]
MemoryMin={{ .EnvdMemoryMinMiB }}M
MemoryLow={{ .EnvdMemoryLowMiB }}M
{{- end -}}

````

## Appendix B: provision.sh template (`B/phases/base/provision.sh`, verbatim)
````sh
#!/bin/sh
set -eu

BUSYBOX="{{ .BusyBox }}"
RESULT_PATH="{{ .ResultPath }}"

echo "Starting provisioning script"

{{ if eq .Provider "gcp" }}
# GCP Specific logic
{{ end }}

echo "Making configuration immutable"
$BUSYBOX chattr +i /etc/resolv.conf

# Identify the base image by its DECLARED /etc/os-release ID, never by probing
# for a package manager. The selector (generated from the distro profile
# registry) defines the E2B_* vars and pkg/CA shell functions, or exits 1 on an
# unsupported distribution. Images with no os-release (distroless, scratch) are
# rejected by name rather than guessed.
echo "Detecting base image distribution"
if [ -r /etc/os-release ]; then
    . /etc/os-release
    E2B_DISTRO_ID="${ID:-unknown}"
    # Derivative-to-parent pointer, e.g. Kali declares ID=kali ID_LIKE=debian.
    # Only consulted when the declared ID matches no profile.
    E2B_ID_LIKE="${ID_LIKE:-}"
else
    E2B_DISTRO_ID="unknown (image has no /etc/os-release)"
    E2B_ID_LIKE=""
fi

{{ .DistroSelector }}

echo "Provisioning for distro '$E2B_DISTRO_ID' (init=$E2B_INIT_BIN, timesync=$E2B_TIMESYNC_UNIT, admin-group=$E2B_ADMIN_GROUP)"

# Persist the resolved identity for later build phases (finalize sources it),
# so the profile's values are defined once, here.
mkdir -p /usr/local/share/e2b
{
    echo "E2B_DISTRO_ID='$E2B_DISTRO_ID'"
    echo "E2B_INIT_SYSTEM='$E2B_INIT_SYSTEM'"
    echo "E2B_ADMIN_GROUP='$E2B_ADMIN_GROUP'"
} > /usr/local/share/e2b/distro.env

# Helper function to check if a package is installed (distro-specific query)
is_package_installed() {
    e2b_pkg_query "$1"
}

# Install required packages if not already installed
PACKAGES="$E2B_PACKAGES"
echo "Checking presence of the following packages: $PACKAGES"

MISSING=""
for pkg in $PACKAGES; do
    if ! is_package_installed "$pkg"; then
        echo "Package $pkg is missing, will install it."
        MISSING="$MISSING $pkg"
    fi
done

if [ -n "$MISSING" ]; then
    echo "Missing packages detected, installing:$MISSING"
    # shellcheck disable=SC2086
    e2b_pkg_install $MISSING
else
    echo "All required packages are already installed."
fi

# Ensure the CA trust bundle exists where envd expects it; e2b_ca_refresh
# regenerates it per family. A refresh failure fails provisioning (set -e) — a
# silently broken trust store is worse than a legible build error.
if [ ! -s "$E2B_CA_BUNDLE" ]; then
    echo "CA trust bundle missing at $E2B_CA_BUNDLE — running the profile's refresh"
    e2b_ca_refresh
fi

# Set /dev/fuse permissions to 666 for non-root access
# Use systemd-tmpfiles to set permissions at boot
mkdir -p /etc/tmpfiles.d
echo 'z /dev/fuse 0666 root root -' > /etc/tmpfiles.d/fuse.conf

echo "Setting up shell"
# Not every base image ships /etc/profile.d or /root; create them so the
# drop-ins below always have a home.
mkdir -p /etc/profile.d /root
echo "export SHELL='/bin/bash'" >/etc/profile.d/shell.sh
echo "export PS1='\w \$ '" >/etc/profile.d/prompt.sh
echo "export PS1='\w \$ '" >>"/etc/profile"
echo "export PS1='\w \$ '" >>"/root/.bashrc"

echo "Use .bashrc and .profile"
echo "if [ -f ~/.bashrc ]; then source ~/.bashrc; fi; if [ -f ~/.profile ]; then source ~/.profile; fi" >>/etc/profile

echo "Remove root password"
# A minimal image may not ship /etc/passwd; only clear the root password when it
# exists rather than failing provisioning.
if [ -f /etc/passwd ]; then
    passwd -d root
else
    echo "No /etc/passwd on this image; skipping root password removal"
fi

echo "Setting up chrony"
mkdir -p /etc/chrony
{
    # The source line (PHC refclock vs NTP pool) is chosen at BOOT by
    # e2b-chrony-source, not here: a template provisioned on a node with
    # /dev/ptp0 can cold-boot on a node without it, and a refclock line for a
    # missing PHC is FATAL to chronyd.
    echo "include /run/chrony-e2b/source.conf"
    # Step (jump) the clock instead of slewing when the offset exceeds 1s, but
    # only for the first 3 updates after chronyd starts. chronyd restarts on
    # every cold boot/reboot, so this corrects a large boot-time offset fast
    # (TLS needs a correct clock) without risking a backward jump under a
    # running workload. Needed because chrony-wait is masked, so boot no
    # longer blocks on first sync.
    echo "makestep 1.0 3"
} >/etc/chrony/chrony.conf

# Add a proxy config, as some environments expects it there (e.g. timemaster in Node Dockerimage)
echo "include /etc/chrony/chrony.conf" >/etc/chrony.conf

echo "Setting up SSH"
mkdir -p /etc/ssh
# A drop-in leaves the package's sshd_config untouched: appending to it marks the conffile
# modified, and a later openssh upgrade in a customer build step then stops on dpkg's
# "modified configuration file" prompt until the build times out. Images whose sshd_config
# has no Include line (stock upstream config) still get the append.
if grep -qE '^Include[[:space:]]+/etc/ssh/sshd_config\.d/' /etc/ssh/sshd_config 2>/dev/null; then
  E2B_SSHD_CONF=/etc/ssh/sshd_config.d/00-e2b.conf
  mkdir -p /etc/ssh/sshd_config.d
else
  E2B_SSHD_CONF=/etc/ssh/sshd_config
fi
cat <<EOF >>"$E2B_SSHD_CONF"
PermitRootLogin yes
PermitEmptyPasswords yes
PasswordAuthentication yes
EOF

echo "Increasing inotify watch limit"
echo 'fs.inotify.max_user_watches=65536' | tee -a /etc/sysctl.conf

# Disable kcompactd background page migration. With 2 MiB host-side hugepage
# backing of guest RAM, every migration dirties a destination hugepage from
# the host UFFD's perspective and lands in the next memfile diff, with no
# corresponding workload benefit between snapshots. We trigger compaction
# explicitly pre-pause instead.
echo "Disabling proactive memory compaction"
echo 'vm.compaction_proactiveness=0' | tee -a /etc/sysctl.conf

# Init-system-specific boot arrangement (autostarts, boot-noise silencing, the
# OpenRC inittab swap), rendered per family by the distro selector (distro/init.go).
e2b_init_setup

# Clean machine-id from Docker
rm -rf /etc/machine-id

echo "Linking $E2B_INIT_BIN to init"
# Not every image ships /usr/sbin; create it before linking init.
mkdir -p /usr/sbin
ln -sf "$E2B_INIT_BIN" /usr/sbin/init
# /sbin is a real directory on non-usr-merged distros (Alpine) where the line
# above doesn't reach the /sbin/init the kernel is pointed at; link it too.
[ -L /sbin ] || ln -sf "$E2B_INIT_BIN" /sbin/init

echo "Unlocking immutable configuration"
$BUSYBOX chattr -i /etc/resolv.conf

echo "Finished provisioning script"

# Delete itself
rm -rf /etc/init.d/rcS
rm -rf /usr/local/bin/provision.sh

# Report successful provisioning
printf "0" > "$RESULT_PATH"
````

## Appendix C: rendered provision-runner (exact guest bytes, no trailing newline)
````sh
#!/usr/bin/busybox ash
# Drives the provisioning pipeline for the busybox-init boot. This logic lives
# in a script — NOT in /etc/inittab — because busybox init hands any inittab
# line containing shell metacharacters to /bin/sh, and minimal images
# (distroless) may have no /bin/sh; a plain-exec inittab line running this
# script through the baked busybox works on every image.
BB=/usr/bin/busybox

# Run the provision script, prefix its output with the log prefix the
# orchestrator forwards to the customer's build logs.
$BB sh /usr/local/bin/provision.sh 2>&1 | $BB sed "s/^/[external] /"

# Flush filesystem changes to disk before the snapshot.
$BB sync
if command -v fsfreeze >/dev/null 2>&1; then
    fsfreeze --freeze /
else
    # No util-linux on this image; the double sync flushes the ext4 journal
    # and the VM is paused before the snapshot is taken. Prefixed so the
    # fallback shows up in the customer's build logs.
    echo "[external] fsfreeze not available on this image; using sync-only flush"
    $BB sync
fi

# Report the provisioning exit code: provision.sh writes "0" on success and
# (running under set -e) leaves no file behind on failure.
if result=$($BB cat /provision.result 2>/dev/null); then
    echo "E2B_PROVISIONING_EXIT:${result}"
else
    echo "E2B_PROVISIONING_EXIT:1"
fi
````
