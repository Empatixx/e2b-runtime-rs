# Orchestrator M5: Cold Boot with envd and the SDK against a real VM

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cold-boot a Firecracker sandbox whose rootfs is a Debian image with envd-rs started by systemd. Set the MMDS metadata, call envd `POST /init` the way upstream does, and run the official E2B JS SDK tests (commands, files, pty) against envd inside the microVM.

**Architecture:**
- `firecracker/client.rs` gains:
  - MMDS: `PUT /mmds/config` during boot and `PUT /mmds` with `MmdsMetadata`;
  - a per-request timeout.
- `envd.rs` is the orchestrator's envd client, a port of upstream `doRequestWithInfiniteRetries` plus `initEnvd`:
  - `POST http://<host_ip>:49983/init` with the upstream JSON body;
  - a retry every 5 ms until a deadline;
  - `204 No Content` required, anything else fails with `unexpected status code: N`.
- `sandbox.rs` adds `Sandbox` with a builder and `cold_boot()`, which runs:
  1. NBD rootfs exposure and slot creation, in parallel;
  2. cgroup;
  3. Firecracker inside the netns;
  4. boot with MMDS config;
  5. `PUT /mmds`;
  6. envd `/init`.

  Any failure tears down what was already created, in reverse order. `stop()` does the same teardown for a running sandbox.
- `storage/backend.rs` gains `put_file`, a streaming multipart upload, so a 2 GiB rootfs is never held in memory.
- The guest rootfs is built from `docker/guest.Dockerfile`:
  - Debian trixie, with the same packages and user as `docker/Dockerfile`;
  - plus `systemd-sysv`, `curl` and an `envd.service` modelled on upstream;
  - `docker export` → `mkfs.ext4 -d` produces `/opt/e2b/rootfs.ext4`.
- `crates/orchestrator/examples/cold_boot.rs` is a dev runner. It boots one sandbox and forwards `127.0.0.1:49983` to the guest, so the unchanged SDK in `E2B_DEBUG` mode talks to envd in the VM.

**Tech Stack:** Rust 1.98, tokio, hyper 1 client (http1), sha2 (SHA-512 token hash), chrono (RFC 3339 timestamp), object_store `BufWriter`, Docker (rootfs build only), systemd in the guest, Firecracker E2B fork v1.14, guest kernel 6.1.158, pnpm + vitest (E2B JS SDK tests), GitHub Actions ubuntu-24.04 with `/dev/kvm`.

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`, milestone 5 ("Cold boot: VM from a hand-made ext4 with envd-rs, `/init`" — proof: "SDK `commands.run` against a real VM").

## Global Constraints

- Code style follows `CLAUDE.md` and `.claude/skills/project-structure`:
  - readability first, small well-named methods, method chains;
  - builders for values created in several places or with many optional parts;
  - named struct fields, no tuple structs and no `.0` access;
  - no comments of any kind;
  - no `#[allow]`.
- Upstream parity where it is visible to envd:
  - MMDS JSON keys are `instanceID`, `envID`, `address` (always present) and `accessTokenHash` (omitted when there is no token).
  - The MMDS config is `{"version":"V2","network_interfaces":["eth0"]}`.
  - `/init` JSON keys are `envVars`, `accessToken`, `timestamp` (RFC 3339 with nanoseconds, UTC), `defaultUser` and `defaultWorkdir`.
  - `X-Access-Token` is set on the request when a token is configured.
  - Retry delay 5 ms. A non-204 answer fails with `unexpected status code: <code>`.
- The access token hash is lowercase hex SHA-512 of the token, matching envd `host::hash_access_token`.
- envd answers on `http://<slot host_ip>:49983`.
- The guest init is `/sbin/init` (systemd), passed as `init=/sbin/init` boot arg.
- The guest gets 2 vCPUs and 1024 MiB in the SDK run.
- Firecracker API requests time out after 10 s with `<call>: timed out after 10s`.
- Never `cargo clean`, never touch `target/` by hand, never apply anything to a kube context.
- Before each commit run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace`. Linux-only tests run in CI: push to `main` and watch the run.

## Review Focus

1. **Failure mid-boot leaks nothing.** If boot fails after the slot, disk and cgroup exist (bad kernel path, envd never answers), all of them are gone afterwards. `sandbox_contract::failed_cold_boot_leaves_no_slot_cgroup_or_device` pins this.
2. **envd not up yet.** Connection refused, or a SYN into a guest that is still booting, must retry rather than fail. Each attempt is bounded, so one hung connect cannot eat the whole deadline. `envd::tests::init_retries_until_envd_listens` and `init_gives_up_at_the_deadline` pin this.
3. **Token and MMDS agree.** envd accepts `/init` from the orchestrator through the MMDS hash, then enforces the token. `sandbox_contract::cold_boot_initialises_envd_with_token_and_mmds` checks 401 without the token, 200 with it, and `E2B_SANDBOX_ID` taken from MMDS.
4. **The guest firewall behaves, not just exists.** The guest reaches the internet but not a private address on the host network. `examples/sdk-playground/egress.mjs` pins this in CI.
5. **Large rootfs does not load into memory.** `put_file` streams. `backend::tests::put_file_streams_a_file_into_storage` checks the content round-trips. The 2 GiB rootfs path in CI proves it at size.

---

### Task 1: Firecracker client — MMDS and request timeout

**Files:**
- Modify: `crates/orchestrator/src/firecracker/client.rs`
- Create: `crates/orchestrator/src/firecracker/mmds.rs`
- Modify: `crates/orchestrator/src/firecracker.rs`

**Interfaces:**
- Produces:
  - `firecracker::mmds::MmdsMetadata { sandbox_id: String, template_id: String, logs_collector_address: String, access_token_hash: Option<String> }` with `fn to_json(&self) -> serde_json::Value`.
  - `FirecrackerClient::new(socket: PathBuf) -> Self`, which keeps the default 10 s timeout.
  - `FirecrackerClient::timeout(self, timeout: Duration) -> Self`.
  - `FirecrackerClient::boot(&self, &VmConfig)`: now also sends `PUT /mmds/config` after the network interface and before `/actions`.
  - `FirecrackerClient::set_mmds(&self, &MmdsMetadata) -> Result<(), String>`.

- [ ] **Step 1: Write the failing tests.**
  - In `mmds.rs`, a unit test `metadata_uses_upstream_keys_and_omits_an_empty_hash`:
    - with a hash, `to_json()` is `{"instanceID":"sbx","envID":"tpl","address":"","accessTokenHash":"ab"}`;
    - without a hash, the `accessTokenHash` key is absent.
  - In `client.rs` tests:
    - Extend `boot_sends_the_api_calls_in_order` so the expected paths are `/machine-config`, `/boot-source`, `/drives/rootfs`, `/network-interfaces/eth0`, `/mmds/config`, `/actions`, with `calls[4].body == json!({"version":"V2","network_interfaces":["eth0"]})` and the `/actions` assertion moved to `calls[5]`.
    - New `set_mmds_puts_the_metadata` asserts one `PUT /mmds` with the metadata JSON.
    - New `a_silent_api_times_out`: `fake_firecracker` gains a `hang_path: Option<&'static str>` that accepts the connection, reads the request and never answers. `FirecrackerClient::new(socket).timeout(Duration::from_millis(100)).put("/boot-source", json!({}))` returns `Err("PUT /boot-source: timed out after 100ms")`.

```rust
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmdsMetadata {
    pub sandbox_id: String,
    pub template_id: String,
    pub logs_collector_address: String,
    pub access_token_hash: Option<String>,
}

impl MmdsMetadata {
    pub fn to_json(&self) -> Value {
        let mut metadata = json!({
            "instanceID": self.sandbox_id,
            "envID": self.template_id,
            "address": self.logs_collector_address,
        });
        if let Some(hash) = &self.access_token_hash {
            metadata["accessTokenHash"] = json!(hash);
        }
        metadata
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail.**
  - Run: `cargo test -p orchestrator firecracker`
  - Expected: compile errors for `mmds`, `set_mmds` and `timeout`, plus a path-list mismatch.

- [ ] **Step 3: Implement.**
  - `FirecrackerClient` gets a `timeout: Duration` field, defaulting to `API_TIMEOUT = Duration::from_secs(10)`.
  - `put` wraps the connect, handshake, send and body read in `tokio::time::timeout(self.timeout, …)`. On elapse it returns `format!("{call}: timed out after {:?}", self.timeout)`.
  - `boot` inserts `self.put("/mmds/config", json!({"version":"V2","network_interfaces":["eth0"]}))` before `/actions`.
  - `set_mmds` is `self.put("/mmds", metadata.to_json())`.
  - `firecracker.rs` gains `pub mod mmds;`.

- [ ] **Step 4: Run the tests and watch them pass.**
  - Run: `cargo test -p orchestrator firecracker`
  - Expected: all firecracker tests pass.

- [ ] **Step 5: Commit.** fmt, clippy and test first, then `feat(orchestrator): MMDS config and metadata, Firecracker API timeout`.

### Task 2: envd client — `/init` with retries

**Files:**
- Create: `crates/orchestrator/src/envd.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`pub mod envd;`)
- Modify: `crates/orchestrator/Cargo.toml` (add `sha2 = "0.10"` and `chrono = { version = "0.4", default-features = false, features = ["clock"] }`)

**Interfaces:**
- Produces:
  - `envd::ENVD_PORT: u16 = 49983`.
  - `envd::EnvdInit { access_token: Option<String>, default_user: Option<String>, default_workdir: Option<String>, env_vars: BTreeMap<String, String> }`, which derives `Default` and `Clone`.
  - `EnvdInit::body(&self, timestamp: DateTime<Utc>) -> serde_json::Value`.
  - `envd::hash_access_token(token: &str) -> String`: lowercase hex SHA-512.
  - `envd::init(address: SocketAddr, init: &EnvdInit, deadline: Instant) -> Result<(), String>`.

- [ ] **Step 1: Write the failing tests** in `envd.rs`. `fake_envd(status)` is a small hyper server on `127.0.0.1:0`. It records each request's headers and body and answers with the given status.

  - `body_uses_upstream_keys`: `EnvdInit { access_token: Some("t"), default_user: Some("user"), default_workdir: Some("/home/user"), env_vars: {"A":"1"} }.body(ts)` equals:

    ```json
    {"envVars":{"A":"1"},"accessToken":"t","timestamp":"2026-10-09T10:00:00.000000001Z","defaultUser":"user","defaultWorkdir":"/home/user"}
    ```

    The timestamp is fixed with `Utc.timestamp_opt(…)`. Missing options are serialised as `""`, as Go dereferences them with `DerefOrDefault`.
  - `hash_matches_envd`: `hash_access_token("abc")` equals the known SHA-512 hex of `"abc"` (`ddaf35a1…a54ca49f`, written out in full).
  - `init_sends_token_header_and_accepts_204`: the server answers 204. `init` returns `Ok(())`, and the recorded request has `x-access-token: t`, method POST and path `/init`.
  - `init_retries_until_envd_listens`:
    1. Bind a std listener to get a free port, then drop it.
    2. Start `init` against that port with a 5 s deadline.
    3. After 200 ms, start `fake_envd(204)` on the same port.
    4. `init` returns `Ok`.
  - `init_rejects_non_204`: the server answers 401. The result is `Err("unexpected status code: 401")`.
  - `init_gives_up_at_the_deadline`: nothing listens and the deadline is 300 ms. The result is an `Err` that starts with `failed to init envd:`, and the call returns within 1 s.

- [ ] **Step 2: Run the tests and watch them fail.**
  - Run: `cargo test -p orchestrator envd`
  - Expected: does not compile, because the `envd` module does not exist yet.

- [ ] **Step 3: Implement.**

```rust
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, SecondsFormat, Utc};
use http_body_util::Full;
use hyper::Request;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use tokio::net::TcpStream;
use tokio::time::Instant;

pub const ENVD_PORT: u16 = 49983;
const RETRY_DELAY: Duration = Duration::from_millis(5);
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Default)]
pub struct EnvdInit {
    pub access_token: Option<String>,
    pub default_user: Option<String>,
    pub default_workdir: Option<String>,
    pub env_vars: BTreeMap<String, String>,
}

impl EnvdInit {
    pub fn body(&self, timestamp: DateTime<Utc>) -> Value {
        json!({
            "envVars": self.env_vars,
            "accessToken": self.access_token.clone().unwrap_or_default(),
            "timestamp": timestamp.to_rfc3339_opts(SecondsFormat::Nanos, true),
            "defaultUser": self.default_user.clone().unwrap_or_default(),
            "defaultWorkdir": self.default_workdir.clone().unwrap_or_default(),
        })
    }
}

pub fn hash_access_token(token: &str) -> String {
    format!("{:x}", Sha512::digest(token.as_bytes()))
}

pub async fn init(address: SocketAddr, init: &EnvdInit, deadline: Instant) -> Result<(), String> {
    loop {
        match attempt(address, init).await {
            Ok(204) => return Ok(()),
            Ok(status) => return Err(format!("unexpected status code: {status}")),
            Err(error) if Instant::now() + RETRY_DELAY >= deadline => {
                return Err(format!("failed to init envd: {error}"));
            }
            Err(_) => tokio::time::sleep(RETRY_DELAY).await,
        }
    }
}

async fn attempt(address: SocketAddr, init: &EnvdInit) -> Result<u16, String> {
    tokio::time::timeout(ATTEMPT_TIMEOUT, post_init(address, init))
        .await
        .unwrap_or_else(|_| Err(format!("attempt timed out after {ATTEMPT_TIMEOUT:?}")))
}
```

`post_init` does the following:
1. Connect a `TcpStream` and run the `http1::handshake`, then spawn the connection.
2. Build `POST /init` with `host: <address>`, `content-type: application/json`, the optional `x-access-token`, and the body `init.body(Utc::now())`.
3. Send it and return `response.status().as_u16()`.

Every error is mapped to a `String`.

- [ ] **Step 4: Run the tests and watch them pass.**
  - Run: `cargo test -p orchestrator envd`
  - Expected: 6 passed.

- [ ] **Step 5: Commit.** `feat(orchestrator): envd /init client with upstream retry semantics`.

### Task 3: Storage — streaming `put_file`

**Files:**
- Modify: `crates/orchestrator/src/storage/backend.rs`

**Interfaces:**
- Produces: `Storage::put_file(&self, path: &str, file: &Path) -> Result<(), String>`.

- [ ] **Step 1: Write the failing test** `put_file_streams_a_file_into_storage`:
  1. Write 9 MiB of patterned bytes (`(0..).map(|i| (i % 251) as u8)`) to a temp file.
  2. `put_file("t/rootfs.ext4", path)`.
  3. `get` returns identical bytes.

  This uses local storage. The S3 contract test stays as it is.

- [ ] **Step 2: Run it and watch it fail.**
  - Run: `cargo test -p orchestrator put_file`
  - Expected: `no method named put_file`.

- [ ] **Step 3: Implement** with `object_store::buffered::BufWriter`:

```rust
pub async fn put_file(&self, path: &str, file: &Path) -> Result<(), String> {
    let mut source = tokio::fs::File::open(file)
        .await
        .map_err(|e| format!("open {}: {e}", file.display()))?;
    let mut writer = BufWriter::new(self.store.clone(), ObjectPath::from(path));
    tokio::io::copy(&mut source, &mut writer)
        .await
        .map_err(|e| format!("put {path}: {e}"))?;
    writer.shutdown().await.map_err(|e| format!("put {path}: {e}"))
}
```

`shutdown` comes from `tokio::io::AsyncWriteExt`.

- [ ] **Step 4: Run it and watch it pass.**
  - Run: `cargo test -p orchestrator put_file`
  - Expected: 1 passed.

- [ ] **Step 5: Commit.** `feat(orchestrator): stream a file into storage`.

### Task 4: Sandbox cold boot and teardown

**Files:**
- Create: `crates/orchestrator/src/sandbox.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`#[cfg(target_os = "linux")] pub mod sandbox;`)
- Test: `crates/orchestrator/tests/sandbox_contract.rs`
- Modify: `crates/orchestrator/Cargo.toml` (add the dev-dependency `reqwest = { version = "0.12", default-features = false, features = ["json"] }`)

**Interfaces:**
- Consumes:
  - `expose(Arc<Overlay>) -> io::Result<Exposure>`, `Exposure::{path, close}`;
  - `Slot::{create, destroy, namespace, host_ip}`;
  - `Cgroup::{create, add, remove}`;
  - `FirecrackerProcess::{spawn, pid, client, kill}`;
  - `FirecrackerClient::{boot, set_mmds}`;
  - `VmConfig::builder()`;
  - `envd::{init, EnvdInit, hash_access_token, ENVD_PORT}`;
  - `MmdsMetadata`.
- Produces:
  - `sandbox::Sandbox::builder() -> SandboxBuilder`, with setters `id(&str)`, `template_id(&str)`, `rootfs(Arc<Overlay>)`, `slot(Slot)`, `firecracker(impl Into<PathBuf>)`, `kernel(impl Into<PathBuf>)`, `init_path(&str)` (default `/sbin/init`), `vcpus(u32)` (default 2), `memory_mib(u32)` (default 1024), `work_dir(impl Into<PathBuf>)`, `envd(EnvdInit)` and `envd_timeout(Duration)` (default 60 s).
  - `SandboxBuilder::cold_boot(self) -> Result<Sandbox, String>` (async).
  - `Sandbox::id(&self) -> &str`.
  - `Sandbox::envd_address(&self) -> SocketAddr`, which is `host_ip:49983`.
  - `Sandbox::stop(self) -> Result<(), String>` (async).

- [ ] **Step 1: Write the failing contract tests.** `sandbox_contract.rs` is `#![cfg(target_os = "linux")]` and skips unless the test is root and has `/dev/kvm`, `/dev/nbd0`, `E2B_FIRECRACKER` and `E2B_KERNEL`.

  - `failed_cold_boot_leaves_no_slot_cgroup_or_device`:
    1. Make a 16 MiB zero template in local storage with `put`, then build the overlay.
    2. Use slot 4011 and the kernel path `/nonexistent/vmlinux`.
    3. `cold_boot()` returns `Err` containing `PUT /boot-source`.
    4. Afterwards `ip netns list` has no `ns-4011`, `/sys/fs/cgroup/e2b/sbx-fail` does not exist, and every `/sys/block/nbd*/pid` is absent.
  - `cold_boot_initialises_envd_with_token_and_mmds`: also skips without `E2B_ROOTFS`.
    1. Upload `E2B_ROOTFS` with `put_file` as `Artifact::Rootfs.object(template_id)`. The header is `Metadata::template(template_id, 4096, file size)`.
    2. Use slot 4012 and `EnvdInit { access_token: Some("contract-token"), default_user: Some("user"), default_workdir: Some("/home/user"), env_vars: {} }`.
    3. `cold_boot()` returns `Ok`.
    4. Against `http://{envd_address}`:
       - `GET /health` → 204;
       - `GET /envs` with no header → 401;
       - `GET /envs` with `X-Access-Token: contract-token`, polled for up to 10 s, → 200, and the JSON has `E2B_SANDBOX_ID == "sbx-contract"`.
    5. `stop()` returns `Ok`, after which `ns-4012` is gone and the cgroup dir is gone.

- [ ] **Step 2: Run on Linux and watch it fail.**
  - Run: push to `main` and watch CI.
  - Expected: does not compile (`orchestrator::sandbox` missing).
  - Locally on macOS, `cargo test -p orchestrator` still passes, because the file is cfg-gated.

- [ ] **Step 3: Implement `sandbox.rs`.** `Sandbox` holds the resources as `Option`s so one teardown serves both a failed boot and `stop`:

```rust
pub struct Sandbox {
    config: SandboxConfig,
    disk: Option<Exposure>,
    slot_created: bool,
    cgroup: Option<Cgroup>,
    vm: Option<FirecrackerProcess>,
}

impl SandboxBuilder {
    pub async fn cold_boot(self) -> Result<Sandbox, String> {
        let mut sandbox = Sandbox::new(self.build()?);
        match sandbox.start().await {
            Ok(()) => Ok(sandbox),
            Err(e) => {
                let _ = sandbox.teardown().await;
                Err(e)
            }
        }
    }
}

impl Sandbox {
    async fn start(&mut self) -> Result<(), String> {
        self.attach_disk_and_slot().await?;
        self.cgroup = Some(Cgroup::create(&self.config.id).map_err(|e| format!("create cgroup: {e}"))?);
        self.spawn_firecracker().await?;
        self.boot().await?;
        envd::init(self.envd_address(), &self.config.envd, Instant::now() + self.config.envd_timeout).await
    }

    pub async fn stop(mut self) -> Result<(), String> {
        self.teardown().await
    }

    async fn teardown(&mut self) -> Result<(), String> {
        let results = [
            self.kill_firecracker().await,
            self.close_disk().await,
            self.remove_cgroup(),
            self.destroy_slot().await,
        ];
        let errors: Vec<String> = results.into_iter().filter_map(Result::err).collect();
        errors.is_empty().then_some(()).ok_or_else(|| errors.join("; "))
    }
}
```

Each step is a method of a few lines:

- **`attach_disk_and_slot`** runs `tokio::join!(expose(rootfs), slot.create())`. It stores whichever side succeeded, then returns the first error.
- **`spawn_firecracker`** spawns into `work_dir/firecracker.sock` with `work_dir/console.log`, then adds the pid to the cgroup.
- **`boot`** does two things:
  - builds `VmConfig` with `extra_boot_args(&format!("init={}", init_path))` and calls `client.boot`;
  - calls `client.set_mmds(&MmdsMetadata { sandbox_id, template_id, logs_collector_address: String::new(), access_token_hash: envd.access_token.as_deref().map(hash_access_token) })`.
- **Teardown helpers** use `Option::take()`, so running them twice is a no-op.

- [ ] **Step 4: Run in CI and watch it pass.** `cold_boot_initialises_envd_with_token_and_mmds` stays skipped until Task 5 provides `E2B_ROOTFS`.
  - Run: commit, push `main`, `gh run watch`.
  - Expected: `failed_cold_boot_leaves_no_slot_cgroup_or_device` passes, the rootfs test prints its skip line, and everything else is green.

- [ ] **Step 5: Commit.** This is the commit pushed in Step 4: `feat(orchestrator): sandbox cold boot with MMDS and envd init, full teardown on failure`.

### Task 5: Guest rootfs with envd under systemd

**Files:**
- Create: `docker/guest.Dockerfile`
- Create: `docker/envd.service`
- Create: `scripts/ci/guest-rootfs.sh`
- Modify: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: `/opt/e2b/rootfs.ext4` (2 GiB sparse ext4) and the `E2B_ROOTFS` environment variable in `$GITHUB_ENV`.

- [ ] **Step 1: The test already exists.** `cold_boot_initialises_envd_with_token_and_mmds` from Task 4 is the failing check, because it currently skips. Make it fail loudly in CI: when `CI=true` and `E2B_ROOTFS` is unset, panic instead of skipping. A green run then really means the VM ran.

- [ ] **Step 2: Push and watch it fail.**
  - Expected: `cold_boot_initialises_envd_with_token_and_mmds` panics with `E2B_ROOTFS is required in CI`.

- [ ] **Step 3: Implement.**

`docker/envd.service`:

```ini
[Unit]
Description=Env Daemon Service
DefaultDependencies=no
After=systemd-journald.socket systemd-remount-fs.service local-fs.target systemd-tmpfiles-setup.service
Wants=systemd-journald.socket
Conflicts=shutdown.target
Before=shutdown.target
StartLimitIntervalSec=0

[Service]
Type=simple
Restart=always
User=root
Group=root
ExecStart=/usr/bin/envd
OOMPolicy=continue
OOMScoreAdjust=-1000
Delegate=yes

[Install]
WantedBy=multi-user.target
```

`docker/guest.Dockerfile`:

```dockerfile
FROM debian:trixie
ARG DEBIAN_FRONTEND=noninteractive
RUN apt-get update -qq \
    && apt-get install -y -qq systemd-sysv ca-certificates curl sudo python3 python-is-python3 nodejs npm procps util-linux iproute2 \
    && rm -rf /var/lib/apt/lists/*
RUN passwd -d root \
    && useradd -ms /bin/bash user \
    && usermod -aG sudo user \
    && passwd -d user \
    && echo "user ALL=(ALL:ALL) NOPASSWD: ALL" >> /etc/sudoers \
    && chmod 777 -R /home/user /usr/local
COPY envd /usr/bin/envd
COPY envd.service /etc/systemd/system/envd.service
RUN systemctl enable envd.service \
    && systemctl mask serial-getty@ttyS0.service getty@tty1.service systemd-networkd-wait-online.service
```

`scripts/ci/guest-rootfs.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

DIR=${1:-/opt/e2b}
SIZE=2G
WORK=$(mktemp -d)
trap 'sudo rm -rf "$WORK"; docker rm -f e2b-guest-export >/dev/null 2>&1 || true' EXIT

cargo build -q -p envd
cp target/debug/envd docker/envd.service "$WORK/"
docker build -q -f docker/guest.Dockerfile -t e2b-guest:dev "$WORK" >/dev/null
docker create --name e2b-guest-export e2b-guest:dev >/dev/null
mkdir "$WORK/root"
docker export e2b-guest-export | sudo tar -x -C "$WORK/root"
echo "nameserver 8.8.8.8" | sudo tee "$WORK/root/etc/resolv.conf" >/dev/null
echo "e2b" | sudo tee "$WORK/root/etc/hostname" >/dev/null
sudo mkdir -p "$DIR"
sudo rm -f "$DIR/rootfs.ext4"
sudo mkfs.ext4 -q -F -d "$WORK/root" "$DIR/rootfs.ext4" "$SIZE"
echo "E2B_ROOTFS=$DIR/rootfs.ext4" >> "${GITHUB_ENV:-/dev/null}"
```

The `cp` into `$WORK` happens before the `docker build`, so the build context is `$WORK` alone. In `ci.yml`, add `- run: scripts/ci/guest-rootfs.sh` after `scripts/ci/vm-assets.sh`.

- [ ] **Step 4: Push and watch it pass.**
  - Expected: `cold_boot_initialises_envd_with_token_and_mmds` passes.
  - If it fails, the console log is in the assertion message. Diagnose with superpowers:systematic-debugging; do not loosen the test.

- [ ] **Step 5: Commit.** `feat(ci): Debian guest rootfs with envd under systemd`.

### Task 6: The official SDK tests against the microVM, plus guest egress

**Files:**
- Create: `crates/orchestrator/examples/cold_boot.rs`
- Create: `scripts/sdk-run.sh`. It is the clone-and-vitest half of today's `scripts/sdk-tests.sh`.
- Modify: `scripts/sdk-tests.sh`, which now starts the Docker envd and then calls `scripts/sdk-run.sh "$@"`.
- Create: `scripts/ci/vm-sdk-tests.sh`
- Create: `examples/sdk-playground/egress.mjs`
- Modify: `.github/workflows/ci.yml` (setup-node 22, pnpm 9, then a `scripts/ci/vm-sdk-tests.sh` step after `cargo test`)

**Interfaces:**
- Consumes: `Sandbox::builder()…cold_boot()`, `Sandbox::envd_address()`, `Sandbox::stop()`, `Storage::put_file` and `EnvdInit`.

- [ ] **Step 1: Write the failing check.**
  - `examples/sdk-playground/egress.mjs` connects like `demo.mjs` (`Sandbox.create()` under `E2B_DEBUG=true`).
  - It runs `curl -s -m 5 -o /dev/null -w '%{http_code}' http://1.1.1.1` and expects a status other than `000`.
  - It runs `curl -s -m 3 -o /dev/null http://$BLOCKED_URL`, which must fail. The SDK throws `CommandExitError`; catch it and check `exitCode !== 0`.
  - It exits 1 with a message when either expectation is wrong.
  - In `ci.yml`, add the step `scripts/ci/vm-sdk-tests.sh`, which does not exist yet.

- [ ] **Step 2: Push and watch it fail.**
  - Expected: the step fails with `No such file or directory`.

- [ ] **Step 3: Implement.** `examples/cold_boot.rs` does the following:
  1. Read `E2B_FIRECRACKER`, `E2B_KERNEL` and `E2B_ROOTFS`.
  2. Create a temp dir and local storage, then `put_file` the rootfs.
  3. Build the overlay.
  4. Cold boot slot 4020 with `EnvdInit { default_user: Some("user"), default_workdir: Some("/home/user"), ..Default::default() }`. No token, because the SDK in debug mode sends none.
  5. Print `ready <envd_address>`.
  6. Forward `127.0.0.1:49983` to `envd_address` with `TcpListener` and `copy_bidirectional`, one task per connection.
  7. On `ctrl_c` or SIGTERM, `stop()` and exit 0.

  `scripts/ci/vm-sdk-tests.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/../.."

cargo build -q -p orchestrator --example cold_boot
sudo -E target/debug/examples/cold_boot > "$RUNNER_TEMP/cold_boot.log" 2>&1 &
BOOT=$!
trap 'sudo kill -TERM $BOOT; wait $BOOT || true; cat "$RUNNER_TEMP/cold_boot.log"' EXIT

for _ in $(seq 1 120); do curl -sf http://127.0.0.1:49983/health && break; sleep 1; done
curl -sf http://127.0.0.1:49983/health

HOST_IP=$(ip -4 route get 1.1.1.1 | sed -n 's/.* src \([0-9.]*\).*/\1/p')
python3 -m http.server 8099 --bind "$HOST_IP" >/dev/null 2>&1 &
SERVER=$!
curl -sf -o /dev/null "http://$HOST_IP:8099"
(cd examples/sdk-playground && npm install --silent && BLOCKED_URL="$HOST_IP:8099" E2B_DEBUG=true node egress.mjs)
kill $SERVER

scripts/sdk-run.sh
```

- `sudo kill -TERM $BOOT` signals the `sudo` process, and sudo relays the signal to the example.
- `sdk-run.sh` is the bottom half of the old `sdk-tests.sh`, starting at `SDK_COMMIT=…`, unchanged.

- [ ] **Step 4: Push and watch it pass.**
  - Expected: egress prints `egress ok`, then vitest reports all of commands, files and pty passed.
  - A failing SDK test is a real envd-in-VM difference. Fix the cause in envd or the guest image and re-run; never skip tests.
  - The backlog's PTY items may surface here. Fix them only if a test fails on them.

- [ ] **Step 5: Commit.** `test(ci): official E2B SDK tests and guest egress against a Firecracker sandbox`.

### Task 7: Docs, backlog, skill

**Files:**
- Modify: `docs/BACKLOG.md`
  - Delete the `Firecracker API calls have no request timeout` and `Guest egress firewall is only checked by rule presence` lines.
  - Add any final-review minors.
- Modify: `.claude/skills/project-structure/SKILL.md`. Add orchestrator lines:
  - `envd.rs` (`/init` client);
  - `sandbox.rs` (lifecycle);
  - `firecracker/mmds.rs`;
  - `examples/cold_boot.rs` (dev runner);
  - `docker/guest.Dockerfile` + `envd.service` (test guest image).
- Modify: `README.md`. Add a short "Run the SDK against a microVM (Linux, KVM)" section that names `scripts/ci/vm-assets.sh`, `scripts/ci/guest-rootfs.sh` and `scripts/ci/vm-sdk-tests.sh`.

- [ ] **Step 1: Make the edits.**
- [ ] **Step 2: Run `cargo test --workspace` and expect green.**
- [ ] **Step 3: Commit.** `docs: M5 layout, backlog and microVM SDK run`.
