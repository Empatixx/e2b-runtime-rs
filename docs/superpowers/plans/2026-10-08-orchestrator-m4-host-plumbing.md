# Orchestrator M4: Network Slot, Cgroup and Firecracker

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Boot a real Firecracker microVM (the E2B fork) whose root disk is our Overlay exposed over NBD. The VM sits inside a per-sandbox network slot and a cgroup, and the host reaches it by pinging the slot's unique host IP.

**Architecture:**
- `system.rs`: one helper that runs a command and turns a non-zero exit into an error that carries stderr.
- `network/`:
  - `slot.rs` holds the pure addressing of slot `N` (names, IPs) plus `create` / `destroy`, which shell out to `ip`, `iptables` and `sysctl`.
  - The netns is `ns-N`. The veth pair is `veth-N` (host) ↔ `eth0` (inside the netns) on a /31 from 10.12.0.0/16. The tap is `tap0` at 169.254.0.22/30, and the guest is always 169.254.0.21.
  - Inside the netns, NAT maps guest ↔ host IP `10.11.0.N`.
- `cgroup.rs`: a cgroup v2 directory per sandbox under `/sys/fs/cgroup/e2b/`.
- `firecracker/`:
  - `config.rs` holds `VmConfig` and its builder.
  - `client.rs` holds `FirecrackerClient`, which speaks HTTP/1.1 over the API unix socket with hyper.
  - `process.rs` holds `FirecrackerProcess`, which runs `ip netns exec ns-N firecracker --api-sock …`, writes the console to a file and kills on drop.

Shelling out to `ip` / `iptables` keeps the code short and readable. Upstream does the same for iptables. Netlink can replace the `ip` calls later behind `Slot` when slot creation latency matters, and pre-created slot pools come with the gRPC server in M8.

**Tech Stack:** Rust 1.98, tokio (process, net), hyper 1 (client, http1), hyper-util (tokio), http-body-util, serde_json, iproute2/iptables, the E2B Firecracker fork v1.14, the E2B guest kernel 6.1.158, Alpine minirootfs (test rootfs), GitHub Actions ubuntu-24.04 with `/dev/kvm`.

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`, milestone 4 (and the network section).

## Global Constraints

- Code style follows `CLAUDE.md`:
  - readability first, small well-named methods, method chains;
  - builders for values with many optional parts (`VmConfig::builder()`), named fields;
  - no comments and no `.0` access;
  - low-level details hidden behind small types.
- Slot addressing for slot `N` (1 ≤ N ≤ 16383):
  - netns `ns-N`, host veth `veth-N`, netns-side veth `eth0`;
  - host veth IP `10.12.0.0 + 2N`, netns veth IP `10.12.0.0 + 2N + 1` (both /31);
  - host IP `10.11.0.0 + N`;
  - tap `tap0` at `169.254.0.22/30`, guest `169.254.0.21`;
  - guest MAC `02:FC:00:00:00:05`.
- Guest boot args: `console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw ip=169.254.0.21::169.254.0.22:255.255.255.252::eth0:off`, plus whatever the caller appends (the test appends `init=/bin/sleep -- 3600`).
- Firecracker API calls, in order: `PUT /machine-config`, `PUT /boot-source`, `PUT /drives/rootfs`, `PUT /network-interfaces/eth0`, `PUT /actions {"action_type":"InstanceStart"}`. A non-2xx response is an error carrying the status and body.
- The cgroup root is `/sys/fs/cgroup/e2b`.
- Linux-only modules (`network::slot` commands, `cgroup`, `firecracker::process`) are cfg'd for Linux. Pure addressing, `VmConfig` and the API client compile and are tested on macOS too.
- The VM test needs root, `/dev/kvm`, the `nbd` module and the env vars `E2B_FIRECRACKER`, `E2B_KERNEL` and `E2B_ALPINE_TAR`. Otherwise it skips itself.
- Before every commit run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test -p orchestrator`.

## Review Focus

1. **Slot addressing at the edges** (slot 1, the highest slot, and an address that crosses an octet) must give unique, valid IPs and interface names of at most 15 characters. Covered in Task 1.
2. **A failing command** (for example creating a slot that already exists) must return an error that includes stderr, and must not leave a half-created slot when `create` fails. Covered in Task 2.
3. **A Firecracker API error** (400 with a fault message) must come back as an error naming the method, path, status and body. Covered in Task 4.
4. **The API socket appearing late** must not race the first request: the process waits for the socket file. Covered in Task 5.
5. **Ping from the host to the slot's host IP** must reach the guest through DNAT/SNAT, and destroying the slot afterwards must remove the netns and host route. Covered in Task 5.

---

### Task 1: Command runner and slot addressing

**Files:**
- Create: `crates/orchestrator/src/system.rs`, `crates/orchestrator/src/network.rs`, `crates/orchestrator/src/network/slot.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`pub mod network;`, `pub mod system;`)

**Interfaces:**
- Produces:
  - `system::run(program: &str, args: &[&str]) -> Result<(), String>`. The error is `"{program} {args joined}: exit {code}: {stderr trimmed}"`.
  - `network::slot::Slot { index: u32 }` with:
    - `Slot::new(index: u32) -> Result<Slot, String>` (index 1..=16383)
    - `namespace() -> String`, `host_veth() -> String`
    - `host_veth_ip() -> Ipv4Addr`, `peer_veth_ip() -> Ipv4Addr`, `host_ip() -> Ipv4Addr`
    - constants `TAP_NAME = "tap0"`, `TAP_IP = 169.254.0.22`, `GUEST_IP = 169.254.0.21`, `GUEST_MAC = "02:FC:00:00:00:05"`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/system.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_and_failure_with_stderr() {
        assert_eq!(run("true", &[]), Ok(()));
        let error = run("sh", &["-c", "echo boom >&2; exit 3"]).unwrap_err();
        assert_eq!(error, "sh -c echo boom >&2; exit 3: exit 3: boom");
        assert!(run("definitely-not-a-program", &[]).unwrap_err().starts_with("definitely-not-a-program: "));
    }
}
```

`crates/orchestrator/src/network.rs`:
```rust
pub mod slot;
```

`crates/orchestrator/src/network/slot.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_for_the_first_slot() {
        let slot = Slot::new(1).unwrap();
        assert_eq!(slot.namespace(), "ns-1");
        assert_eq!(slot.host_veth(), "veth-1");
        assert_eq!(slot.host_veth_ip(), Ipv4Addr::new(10, 12, 0, 2));
        assert_eq!(slot.peer_veth_ip(), Ipv4Addr::new(10, 12, 0, 3));
        assert_eq!(slot.host_ip(), Ipv4Addr::new(10, 11, 0, 1));
    }

    #[test]
    fn addresses_cross_octets_and_stay_short() {
        let slot = Slot::new(300).unwrap();
        assert_eq!(slot.host_veth_ip(), Ipv4Addr::new(10, 12, 2, 88));
        assert_eq!(slot.peer_veth_ip(), Ipv4Addr::new(10, 12, 2, 89));
        assert_eq!(slot.host_ip(), Ipv4Addr::new(10, 11, 1, 44));

        let last = Slot::new(MAX_SLOT).unwrap();
        assert_eq!(last.peer_veth_ip(), Ipv4Addr::new(10, 12, 127, 255));
        assert!(last.host_veth().len() <= 15);
    }

    #[test]
    fn rejects_out_of_range_indices() {
        assert_eq!(Slot::new(0).unwrap_err(), "slot index 0 is outside 1..=16383");
        assert_eq!(Slot::new(16384).unwrap_err(), "slot index 16384 is outside 1..=16383");
    }
}
```
Add `pub mod network;` and `pub mod system;` to `crates/orchestrator/src/lib.rs`, keeping the module list sorted.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator --lib system:: network::`
Expected: FAIL to compile, `cannot find function run` and `cannot find type Slot`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/system.rs`:
```rust
use std::process::Command;

pub fn run(program: &str, args: &[&str]) -> Result<(), String> {
    let command_line = std::iter::once(program)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ");
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{command_line}: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let code = output.status.code().map_or("signal".to_owned(), |code| code.to_string());
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!("{command_line}: exit {code}: {}", stderr.trim()))
}
```

Put at the top of `crates/orchestrator/src/network/slot.rs`:
```rust
use std::net::Ipv4Addr;

pub const MAX_SLOT: u32 = 16383;
pub const TAP_NAME: &str = "tap0";
pub const TAP_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 22);
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 21);
pub const GUEST_MAC: &str = "02:FC:00:00:00:05";

const VETH_BASE: Ipv4Addr = Ipv4Addr::new(10, 12, 0, 0);
const HOST_IP_BASE: Ipv4Addr = Ipv4Addr::new(10, 11, 0, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub index: u32,
}

impl Slot {
    pub fn new(index: u32) -> Result<Self, String> {
        if !(1..=MAX_SLOT).contains(&index) {
            return Err(format!("slot index {index} is outside 1..={MAX_SLOT}"));
        }
        Ok(Self { index })
    }

    pub fn namespace(&self) -> String {
        format!("ns-{}", self.index)
    }

    pub fn host_veth(&self) -> String {
        format!("veth-{}", self.index)
    }

    pub fn host_veth_ip(&self) -> Ipv4Addr {
        offset(VETH_BASE, 2 * self.index)
    }

    pub fn peer_veth_ip(&self) -> Ipv4Addr {
        offset(VETH_BASE, 2 * self.index + 1)
    }

    pub fn host_ip(&self) -> Ipv4Addr {
        offset(HOST_IP_BASE, self.index)
    }
}

fn offset(base: Ipv4Addr, by: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(base) + by)
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator --lib system:: network::`
Expected: PASS, 4 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator
git commit -m "feat(orchestrator): command runner and network slot addressing"
```

---

### Task 2: Creating and destroying a network slot

**Files:**
- Modify: `crates/orchestrator/src/network/slot.rs` (Linux `create` / `destroy`)
- Create: `crates/orchestrator/tests/network_contract.rs`

**Interfaces:**
- Consumes: `system::run`, `Slot` (Task 1).
- Produces: (Linux) `Slot::create(&self) -> Result<(), String>` and `Slot::destroy(&self) -> Result<(), String>`. A failed `create` destroys whatever it built before returning the error.

- [ ] **Step 1: Write the failing test**

`crates/orchestrator/tests/network_contract.rs`:
```rust
#![cfg(target_os = "linux")]

use std::process::Command;

use orchestrator::network::slot::Slot;

fn root() -> bool {
    nix::unistd::geteuid().is_root()
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().unwrap().status.success()
}

#[test]
fn slot_is_created_once_and_destroyed_cleanly() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let slot = Slot::new(4001).unwrap();
    let _ = slot.destroy();

    slot.create().unwrap();
    assert!(succeeds("ip", &["netns", "exec", "ns-4001", "ip", "link", "show", "tap0"]));
    assert!(succeeds("ip", &["link", "show", "veth-4001"]));
    assert!(succeeds("ip", &["route", "show", "10.11.15.161"]));

    let again = slot.create().unwrap_err();
    assert!(again.contains("ip netns add ns-4001"), "{again}");
    assert!(succeeds("ip", &["link", "show", "veth-4001"]), "a failed create must not tear down the live slot");

    slot.destroy().unwrap();
    assert!(!succeeds("ip", &["netns", "exec", "ns-4001", "true"]));
    assert!(!succeeds("ip", &["link", "show", "veth-4001"]));
}
```

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator --test network_contract`
Expected: on macOS, 0 tests (cfg linux). On Linux CI it fails to compile, `no method named create`.

- [ ] **Step 3: Implement**

Append to `crates/orchestrator/src/network/slot.rs`, above `#[cfg(test)]`:
```rust
#[cfg(target_os = "linux")]
impl Slot {
    pub fn create(&self) -> Result<(), String> {
        crate::system::run("ip", &["netns", "add", &self.namespace()])?;
        self.configure().inspect_err(|_| {
            let _ = self.destroy();
        })
    }

    pub fn destroy(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let _ = crate::system::run("iptables", &["-D", "FORWARD", "-i", &veth, "-j", "ACCEPT"]);
        let _ = crate::system::run("iptables", &["-D", "FORWARD", "-o", &veth, "-j", "ACCEPT"]);
        crate::system::run("ip", &["netns", "delete", &self.namespace()])
    }

    fn configure(&self) -> Result<(), String> {
        self.create_veth()?;
        self.create_tap()?;
        self.route_and_nat()
    }

    fn create_veth(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let namespace = self.namespace();
        let host_cidr = format!("{}/31", self.host_veth_ip());
        let peer_cidr = format!("{}/31", self.peer_veth_ip());
        crate::system::run(
            "ip",
            &["link", "add", &veth, "type", "veth", "peer", "name", "eth0", "netns", &namespace],
        )?;
        crate::system::run("ip", &["addr", "add", &host_cidr, "dev", &veth])?;
        crate::system::run("ip", &["link", "set", &veth, "up"])?;
        self.inside(&["ip", "addr", "add", &peer_cidr, "dev", "eth0"])?;
        self.inside(&["ip", "link", "set", "eth0", "up"])?;
        self.inside(&["ip", "link", "set", "lo", "up"])
    }

    fn create_tap(&self) -> Result<(), String> {
        let tap_cidr = format!("{TAP_IP}/30");
        self.inside(&["ip", "tuntap", "add", TAP_NAME, "mode", "tap"])?;
        self.inside(&["ip", "addr", "add", &tap_cidr, "dev", TAP_NAME])?;
        self.inside(&["ip", "link", "set", TAP_NAME, "up"])
    }

    fn route_and_nat(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let host_ip = self.host_ip().to_string();
        let guest_ip = GUEST_IP.to_string();
        let host_veth_ip = self.host_veth_ip().to_string();
        let peer_veth_ip = self.peer_veth_ip().to_string();
        self.inside(&["sysctl", "-qw", "net.ipv4.ip_forward=1"])?;
        self.inside(&["ip", "route", "add", "default", "via", &host_veth_ip])?;
        self.inside(&[
            "iptables", "-t", "nat", "-A", "POSTROUTING", "-o", "eth0", "-s", &guest_ip, "-j", "SNAT", "--to", &host_ip,
        ])?;
        self.inside(&[
            "iptables", "-t", "nat", "-A", "PREROUTING", "-i", "eth0", "-d", &host_ip, "-j", "DNAT", "--to", &guest_ip,
        ])?;
        crate::system::run("sysctl", &["-qw", "net.ipv4.ip_forward=1"])?;
        crate::system::run("ip", &["route", "add", &host_ip, "via", &peer_veth_ip, "dev", &veth])?;
        crate::system::run("iptables", &["-I", "FORWARD", "-i", &veth, "-j", "ACCEPT"])?;
        crate::system::run("iptables", &["-I", "FORWARD", "-o", &veth, "-j", "ACCEPT"])
    }

    fn inside(&self, command: &[&str]) -> Result<(), String> {
        let namespace = self.namespace();
        let args: Vec<&str> = ["netns", "exec", namespace.as_str()]
            .into_iter()
            .chain(command.iter().copied())
            .collect();
        crate::system::run("ip", &args)
    }
}
```

The `create` call after the first succeeds fails at `ip netns add`, before `configure`, so the live slot is untouched. That is exactly what the test checks.

- [ ] **Step 4: Run, commit and push to see it pass in CI**

Run on macOS: `cargo test -p orchestrator`. Expected: PASS (Linux-only code cfg'd out).
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator
git commit -m "feat(orchestrator): create and destroy per-sandbox network slots"
git push origin main
gh run watch --repo Empatixx/e2b-runtime-rs --exit-status
```
Expected: CI log shows `test slot_is_created_once_and_destroyed_cleanly ... ok`.

---

### Task 3: Cgroup per sandbox

**Files:**
- Create: `crates/orchestrator/src/cgroup.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`#[cfg(target_os = "linux")] pub mod cgroup;`), `crates/orchestrator/tests/network_contract.rs` (append the cgroup test, since both are root-only host plumbing)

**Interfaces:**
- Produces (Linux): `cgroup::Cgroup` with:
  - `create(name: &str) -> io::Result<Cgroup>` (under `/sys/fs/cgroup/e2b/{name}`; creates the parent directory if missing)
  - `add(&self, pid: u32) -> io::Result<()>`
  - `pids(&self) -> io::Result<Vec<u32>>`
  - `path(&self) -> &Path`
  - `remove(self) -> io::Result<()>`

- [ ] **Step 1: Write the failing test**

Append to `crates/orchestrator/tests/network_contract.rs`:
```rust
#[test]
fn cgroup_holds_a_process_and_is_removed() {
    if !root() {
        eprintln!("skipping: needs root");
        return;
    }
    let cgroup = orchestrator::cgroup::Cgroup::create("contract-test").unwrap();
    let mut child = Command::new("sleep").arg("30").spawn().unwrap();
    cgroup.add(child.id()).unwrap();
    assert_eq!(cgroup.pids().unwrap(), vec![child.id()]);

    child.kill().unwrap();
    child.wait().unwrap();
    let path = cgroup.path().to_path_buf();
    cgroup.remove().unwrap();
    assert!(!path.exists());
}
```

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator --test network_contract` (Linux CI).
Expected: FAIL to compile, `could not find cgroup in orchestrator`.

- [ ] **Step 3: Implement**

`crates/orchestrator/src/cgroup.rs`:
```rust
use std::io;
use std::path::{Path, PathBuf};

const ROOT: &str = "/sys/fs/cgroup/e2b";

pub struct Cgroup {
    path: PathBuf,
}

impl Cgroup {
    pub fn create(name: &str) -> io::Result<Self> {
        let path = Path::new(ROOT).join(name);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn add(&self, pid: u32) -> io::Result<()> {
        std::fs::write(self.path.join("cgroup.procs"), pid.to_string())
    }

    pub fn pids(&self) -> io::Result<Vec<u32>> {
        let procs = std::fs::read_to_string(self.path.join("cgroup.procs"))?;
        Ok(procs.lines().filter_map(|line| line.trim().parse().ok()).collect())
    }

    pub fn remove(self) -> io::Result<()> {
        std::fs::remove_dir(&self.path)
    }
}
```
Add `#[cfg(target_os = "linux")] pub mod cgroup;` to `crates/orchestrator/src/lib.rs`.

- [ ] **Step 4: Commit and push to see it pass in CI**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p orchestrator
git add crates/orchestrator
git commit -m "feat(orchestrator): cgroup v2 directory per sandbox"
git push origin main
gh run watch --repo Empatixx/e2b-runtime-rs --exit-status
```
Expected: CI log shows `test cgroup_holds_a_process_and_is_removed ... ok`.

---

### Task 4: VmConfig builder and the Firecracker API client

**Files:**
- Create: `crates/orchestrator/src/firecracker.rs`, `crates/orchestrator/src/firecracker/config.rs`, `crates/orchestrator/src/firecracker/client.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`pub mod firecracker;`), `crates/orchestrator/Cargo.toml` (`hyper` with `client` and `http1`, `hyper-util` with `tokio`, `http-body-util`, `serde_json`)

**Interfaces:**
- Consumes: `network::slot::{TAP_NAME, GUEST_MAC, GUEST_IP, TAP_IP}`.
- Produces:
  - `firecracker::config::VmConfig { vcpus: u32, memory_mib: u32, kernel: PathBuf, rootfs: PathBuf, extra_boot_args: String }`, with:
    - `VmConfig::builder() -> VmConfigBuilder`
    - builder methods `vcpus(u32)`, `memory_mib(u32)`, `kernel(impl Into<PathBuf>)`, `rootfs(impl Into<PathBuf>)`, `extra_boot_args(&str)`, `build() -> Result<VmConfig, String>` (kernel and rootfs are required; vcpus defaults to 1 and memory to 256)
    - `config.boot_args() -> String`
  - `firecracker::client::FirecrackerClient` with `new(socket: PathBuf)`, async `boot(&self, config: &VmConfig) -> Result<(), String>`, and async `put(&self, path: &str, body: serde_json::Value) -> Result<(), String>`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/firecracker.rs`:
```rust
pub mod client;
pub mod config;
```

`crates/orchestrator/src/firecracker/config.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_requires_kernel_and_rootfs_and_has_defaults() {
        assert_eq!(VmConfig::builder().rootfs("/dev/nbd0").build().unwrap_err(), "kernel is required");
        assert_eq!(VmConfig::builder().kernel("/k").build().unwrap_err(), "rootfs is required");

        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/dev/nbd3")
            .extra_boot_args("init=/bin/sleep -- 3600")
            .build()
            .unwrap();
        assert_eq!(config.vcpus, 1);
        assert_eq!(config.memory_mib, 256);
        assert_eq!(
            config.boot_args(),
            "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw ip=169.254.0.21::169.254.0.22:255.255.255.252::eth0:off init=/bin/sleep -- 3600"
        );
    }
}
```

`crates/orchestrator/src/firecracker/client.rs`:
```rust
#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;
    use crate::firecracker::config::VmConfig;

    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        method: String,
        path: String,
        body: Value,
    }

    async fn fake_firecracker(socket: PathBuf, fail_path: Option<&'static str>) -> Arc<Mutex<Vec<Call>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = UnixListener::bind(&socket).unwrap();
        let recorded = calls.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).await.unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let mut parts = request_line.split_whitespace();
                let call = Call {
                    method: parts.next().unwrap().to_owned(),
                    path: parts.next().unwrap().to_owned(),
                    body: serde_json::from_slice(&body).unwrap(),
                };
                let failing = fail_path == Some(call.path.as_str());
                recorded.lock().push(call);
                let response = if failing {
                    "HTTP/1.1 400 Bad Request\r\ncontent-length: 27\r\n\r\n{\"fault_message\":\"bad cfg\"}"
                } else {
                    "HTTP/1.1 204 No Content\r\n\r\n"
                };
                reader.get_mut().write_all(response.as_bytes()).await.unwrap();
            }
        });
        calls
    }

    fn config() -> VmConfig {
        VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/dev/nbd3")
            .memory_mib(512)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn boot_sends_the_api_calls_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), None).await;

        FirecrackerClient::new(socket).boot(&config()).await.unwrap();

        let calls = calls.lock().clone();
        let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
        assert_eq!(
            paths,
            vec!["/machine-config", "/boot-source", "/drives/rootfs", "/network-interfaces/eth0", "/actions"]
        );
        assert!(calls.iter().all(|call| call.method == "PUT"));
        assert_eq!(calls[0].body, json!({ "vcpu_count": 1, "mem_size_mib": 512 }));
        assert_eq!(
            calls[2].body,
            json!({ "drive_id": "rootfs", "path_on_host": "/dev/nbd3", "is_root_device": true, "is_read_only": false })
        );
        assert_eq!(
            calls[3].body,
            json!({ "iface_id": "eth0", "host_dev_name": "tap0", "guest_mac": "02:FC:00:00:00:05" })
        );
        assert_eq!(calls[4].body, json!({ "action_type": "InstanceStart" }));
    }

    #[tokio::test]
    async fn api_errors_name_the_call_status_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let _calls = fake_firecracker(socket.clone(), Some("/boot-source")).await;

        let error = FirecrackerClient::new(socket).boot(&config()).await.unwrap_err();
        assert_eq!(
            error,
            "PUT /boot-source: 400 Bad Request: {\"fault_message\":\"bad cfg\"}"
        );
    }
}
```
Add `pub mod firecracker;` to `crates/orchestrator/src/lib.rs`. Run:
```bash
cargo add -p orchestrator hyper --features client,http1
cargo add -p orchestrator hyper-util --features tokio
cargo add -p orchestrator http-body-util serde_json
```
Normalize the new versions in `crates/orchestrator/Cargo.toml` to major (`"1"`, `"0.1"`, `"0.1"`, `"1"`).

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator --lib firecracker::`
Expected: FAIL to compile, `cannot find type VmConfig` and `cannot find type FirecrackerClient`.

- [ ] **Step 3: Implement the config**

Put at the top of `crates/orchestrator/src/firecracker/config.rs`:
```rust
use std::path::PathBuf;

use crate::network::slot::{GUEST_IP, TAP_IP};

const BASE_BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off root=/dev/vda rw";
const GUEST_NETMASK: &str = "255.255.255.252";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmConfig {
    pub vcpus: u32,
    pub memory_mib: u32,
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub extra_boot_args: String,
}

#[derive(Debug, Default)]
pub struct VmConfigBuilder {
    vcpus: Option<u32>,
    memory_mib: Option<u32>,
    kernel: Option<PathBuf>,
    rootfs: Option<PathBuf>,
    extra_boot_args: String,
}

impl VmConfig {
    pub fn builder() -> VmConfigBuilder {
        VmConfigBuilder::default()
    }

    pub fn boot_args(&self) -> String {
        let network = format!("ip={GUEST_IP}::{TAP_IP}:{GUEST_NETMASK}::eth0:off");
        [BASE_BOOT_ARGS, network.as_str(), self.extra_boot_args.as_str()]
            .into_iter()
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl VmConfigBuilder {
    pub fn vcpus(mut self, vcpus: u32) -> Self {
        self.vcpus = Some(vcpus);
        self
    }

    pub fn memory_mib(mut self, memory_mib: u32) -> Self {
        self.memory_mib = Some(memory_mib);
        self
    }

    pub fn kernel(mut self, kernel: impl Into<PathBuf>) -> Self {
        self.kernel = Some(kernel.into());
        self
    }

    pub fn rootfs(mut self, rootfs: impl Into<PathBuf>) -> Self {
        self.rootfs = Some(rootfs.into());
        self
    }

    pub fn extra_boot_args(mut self, args: &str) -> Self {
        self.extra_boot_args = args.to_owned();
        self
    }

    pub fn build(self) -> Result<VmConfig, String> {
        Ok(VmConfig {
            vcpus: self.vcpus.unwrap_or(1),
            memory_mib: self.memory_mib.unwrap_or(256),
            kernel: self.kernel.ok_or("kernel is required")?,
            rootfs: self.rootfs.ok_or("rootfs is required")?,
            extra_boot_args: self.extra_boot_args,
        })
    }
}
```

- [ ] **Step 4: Implement the client**

Put at the top of `crates/orchestrator/src/firecracker/client.rs`:
```rust
use std::path::PathBuf;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

use super::config::VmConfig;
use crate::network::slot::{GUEST_MAC, TAP_NAME};

pub struct FirecrackerClient {
    socket: PathBuf,
}

impl FirecrackerClient {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub async fn boot(&self, config: &VmConfig) -> Result<(), String> {
        self.put("/machine-config", json!({ "vcpu_count": config.vcpus, "mem_size_mib": config.memory_mib }))
            .await?;
        self.put(
            "/boot-source",
            json!({ "kernel_image_path": config.kernel, "boot_args": config.boot_args() }),
        )
        .await?;
        self.put(
            "/drives/rootfs",
            json!({ "drive_id": "rootfs", "path_on_host": config.rootfs, "is_root_device": true, "is_read_only": false }),
        )
        .await?;
        self.put(
            "/network-interfaces/eth0",
            json!({ "iface_id": "eth0", "host_dev_name": TAP_NAME, "guest_mac": GUEST_MAC }),
        )
        .await?;
        self.put("/actions", json!({ "action_type": "InstanceStart" })).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<(), String> {
        let call = format!("PUT {path}");
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("{call}: connect {}: {e}", self.socket.display()))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("{call}: {e}"))?;
        tokio::spawn(connection);

        let request = Request::put(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| format!("{call}: {e}"))?;
        let response = sender.send_request(request).await.map_err(|e| format!("{call}: {e}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response
            .into_body()
            .collect()
            .await
            .map(|collected| String::from_utf8_lossy(&collected.to_bytes()).into_owned())
            .unwrap_or_default();
        Err(format!("{call}: {status}: {body}"))
    }
}
```

- [ ] **Step 5: Run the tests to see them pass**

Run: `cargo test -p orchestrator --lib firecracker::`
Expected: PASS, 3 tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator Cargo.lock
git commit -m "feat(orchestrator): VmConfig builder and Firecracker API client"
```

---

### Task 5: Firecracker process and the first VM

**Files:**
- Create: `crates/orchestrator/src/firecracker/process.rs`, `crates/orchestrator/tests/vm_contract.rs`, `scripts/ci/vm-assets.sh`
- Modify: `crates/orchestrator/src/firecracker.rs` (`#[cfg(target_os = "linux")] pub mod process;`), `.github/workflows/ci.yml` (download VM assets, export env vars)

**Interfaces:**
- Consumes: `Slot` (Tasks 1–2), `Cgroup` (Task 3), `VmConfig` and `FirecrackerClient` (Task 4), `exposure::nbd::expose` (M3a), `DiffStore` / `BuildFile` / `Overlay` (M2).
- Produces (Linux): `firecracker::process::FirecrackerProcess` with:
  - async `spawn(binary: &Path, socket: &Path, namespace: &str, console: &Path) -> Result<FirecrackerProcess, String>` (it waits up to 5 s for the socket file)
  - `pid(&self) -> u32`
  - `client(&self) -> FirecrackerClient`
  - async `kill(self) -> Result<(), String>`
  - kill on drop

- [ ] **Step 1: Write the asset script and the failing VM test**

`scripts/ci/vm-assets.sh`:
```bash
#!/usr/bin/env bash
set -euo pipefail

FC_RELEASE=v1.14.1_621718e
KERNEL_RELEASE=2026.07.28
KERNEL_ASSET=vmlinux-6.1.158-amd64.bin
ALPINE=https://dl-cdn.alpinelinux.org/alpine/v3.20/releases/x86_64/alpine-minirootfs-3.20.3-x86_64.tar.gz
DIR=${1:-/opt/e2b}

sudo mkdir -p "$DIR"
sudo curl -fsSL -o "$DIR/firecracker" "https://github.com/e2b-dev/fc-versions/releases/download/$FC_RELEASE/firecracker-amd64"
sudo curl -fsSL -o "$DIR/vmlinux" "https://github.com/e2b-dev/fc-kernels/releases/download/$KERNEL_RELEASE/$KERNEL_ASSET"
sudo curl -fsSL -o "$DIR/alpine.tar.gz" "$ALPINE"
sudo chmod +x "$DIR/firecracker"
{
  echo "E2B_FIRECRACKER=$DIR/firecracker"
  echo "E2B_KERNEL=$DIR/vmlinux"
  echo "E2B_ALPINE_TAR=$DIR/alpine.tar.gz"
} >> "${GITHUB_ENV:-/dev/null}"
```

In `.github/workflows/ci.yml`, add these two steps to the `test` job right after `sudo modprobe nbd nbds_max=16`:
```yaml
      - run: scripts/ci/vm-assets.sh
      - run: sudo chmod 666 /dev/kvm
```

`crates/orchestrator/tests/vm_contract.rs`:
```rust
#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
use orchestrator::block::overlay::Overlay;
use orchestrator::cgroup::Cgroup;
use orchestrator::exposure::nbd::expose;
use orchestrator::firecracker::config::VmConfig;
use orchestrator::firecracker::process::FirecrackerProcess;
use orchestrator::network::slot::Slot;
use orchestrator::storage::backend::Storage;
use orchestrator::storage::header::{Header, Metadata};
use uuid::Uuid;

const ROOTFS_SIZE: u64 = 128 << 20;

struct Assets {
    firecracker: PathBuf,
    kernel: PathBuf,
    alpine: PathBuf,
}

fn assets() -> Option<Assets> {
    let env = |name: &str| std::env::var(name).ok().map(PathBuf::from);
    let ready = nix::unistd::geteuid().is_root() && Path::new("/dev/kvm").exists() && Path::new("/dev/nbd0").exists();
    ready.then_some(())?;
    Some(Assets {
        firecracker: env("E2B_FIRECRACKER")?,
        kernel: env("E2B_KERNEL")?,
        alpine: env("E2B_ALPINE_TAR")?,
    })
}

fn run(program: &str, args: &[&str]) {
    let status = Command::new(program).args(args).status().unwrap();
    assert!(status.success(), "{program} {args:?} failed: {status}");
}

fn alpine_ext4(alpine: &Path, dir: &Path) -> Vec<u8> {
    let root = dir.join("alpine");
    let image = dir.join("rootfs.ext4");
    std::fs::create_dir(&root).unwrap();
    run("tar", &["-xzf", alpine.to_str().unwrap(), "-C", root.to_str().unwrap()]);
    let size = format!("{}M", ROOTFS_SIZE >> 20);
    run("mkfs.ext4", &["-q", "-F", "-d", root.to_str().unwrap(), image.to_str().unwrap(), &size]);
    std::fs::read(image).unwrap()
}

async fn pings(address: &str, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        let replied = Command::new("ping")
            .args(["-c", "1", "-W", "1", address])
            .status()
            .is_ok_and(|status| status.success());
        if replied {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn firecracker_boots_on_an_nbd_rootfs_inside_a_network_slot() {
    let Some(assets) = assets() else {
        eprintln!("skipping: needs root, /dev/kvm, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ALPINE_TAR");
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template_id = Uuid::new_v4();
    let rootfs = alpine_ext4(&assets.alpine, dir.path());
    storage
        .put(&Artifact::Rootfs.object(template_id), Bytes::from(rootfs))
        .await
        .unwrap();
    let header = Header::new(Metadata::template(template_id, 4096, ROOTFS_SIZE), Vec::new()).unwrap();
    let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
    let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
    let overlay = Arc::new(Overlay::new(base, &dir.path().join("overlay")).unwrap());
    let disk = expose(overlay).await.unwrap();

    let slot = Slot::new(4002).unwrap();
    let _ = slot.destroy();
    slot.create().unwrap();
    let cgroup = Cgroup::create("vm-contract").unwrap();

    let socket = dir.path().join("fc.sock");
    let console = dir.path().join("console.log");
    let vm = FirecrackerProcess::spawn(&assets.firecracker, &socket, &slot.namespace(), &console)
        .await
        .unwrap();
    cgroup.add(vm.pid()).unwrap();
    let config = VmConfig::builder()
        .kernel(&assets.kernel)
        .rootfs(disk.path())
        .extra_boot_args("init=/bin/sleep -- 3600")
        .build()
        .unwrap();
    vm.client().boot(&config).await.unwrap();

    let reachable = pings(&slot.host_ip().to_string(), Duration::from_secs(30)).await;
    let console_log = std::fs::read_to_string(&console).unwrap_or_default();

    vm.kill().await.unwrap();
    disk.close().await.unwrap();
    cgroup.remove().unwrap();
    slot.destroy().unwrap();

    assert!(reachable, "guest did not answer ping; console:\n{console_log}");
    assert!(console_log.contains("Linux version"), "{console_log}");
}
```

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator --test vm_contract` (Linux CI).
Expected: FAIL to compile, `could not find process in firecracker`.

- [ ] **Step 3: Implement the process**

`crates/orchestrator/src/firecracker/process.rs`:
```rust
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::{Child, Command};

use super::client::FirecrackerClient;

const SOCKET_POLLS: u32 = 500;
const SOCKET_POLL_INTERVAL: Duration = Duration::from_millis(10);

pub struct FirecrackerProcess {
    child: Child,
    socket: PathBuf,
}

impl FirecrackerProcess {
    pub async fn spawn(binary: &Path, socket: &Path, namespace: &str, console: &Path) -> Result<Self, String> {
        let console_file = File::create(console).map_err(|e| format!("create console {}: {e}", console.display()))?;
        let console_err = console_file
            .try_clone()
            .map_err(|e| format!("clone console {}: {e}", console.display()))?;
        let child = Command::new("ip")
            .args(["netns", "exec", namespace])
            .arg(binary)
            .arg("--api-sock")
            .arg(socket)
            .stdin(Stdio::null())
            .stdout(console_file)
            .stderr(console_err)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("spawn firecracker {}: {e}", binary.display()))?;
        let process = Self {
            child,
            socket: socket.to_path_buf(),
        };
        process.wait_for_socket().await?;
        Ok(process)
    }

    pub fn pid(&self) -> u32 {
        self.child.id().unwrap_or_default()
    }

    pub fn client(&self) -> FirecrackerClient {
        FirecrackerClient::new(self.socket.clone())
    }

    pub async fn kill(mut self) -> Result<(), String> {
        self.child.kill().await.map_err(|e| format!("kill firecracker: {e}"))
    }

    async fn wait_for_socket(&self) -> Result<(), String> {
        for _ in 0..SOCKET_POLLS {
            if self.socket.exists() {
                return Ok(());
            }
            tokio::time::sleep(SOCKET_POLL_INTERVAL).await;
        }
        Err(format!("firecracker api socket {} did not appear", self.socket.display()))
    }
}
```
Add `#[cfg(target_os = "linux")] pub mod process;` to `crates/orchestrator/src/firecracker.rs`.

- [ ] **Step 4: Commit, push and read the CI result**

```bash
chmod +x scripts/ci/vm-assets.sh
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p orchestrator
git add crates/orchestrator scripts/ci/vm-assets.sh .github/workflows/ci.yml
git commit -m "feat(orchestrator): boot a Firecracker VM on an NBD rootfs inside a network slot"
git push origin main
gh run watch --repo Empatixx/e2b-runtime-rs --exit-status
gh run view --repo Empatixx/e2b-runtime-rs --log | grep -E "firecracker_boots_on_an_nbd_rootfs|slot_is_created|cgroup_holds"
```
Expected: CI `test` is green, and the log shows all three Linux contract tests `... ok`.
