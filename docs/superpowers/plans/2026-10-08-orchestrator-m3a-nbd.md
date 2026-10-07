# Orchestrator M3a: Expose the Overlay as a Kernel Block Device over NBD

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve an `Overlay` (M2) to the Linux kernel as `/dev/nbdN`, so Firecracker can use it as the VM root disk. An ext4 filesystem created on the device must survive export → diff upload → resume onto a new device.

**Architecture:** Three pieces in `crates/orchestrator/src/exposure/nbd/`:

| File | Job |
|---|---|
| `protocol.rs` | NBD wire format: decode the 28-byte request, encode the 16-byte reply (big-endian) |
| `server.rs` | async loop over a unix socket: READ / WRITE / TRIM / WRITE_ZEROES / FLUSH / DISC dispatched to the `Overlay` |
| `ioctl.rs` (Linux) | the only `unsafe`: thin safe wrappers over the NBD ioctls |
| `device.rs` (Linux) | `NbdDevice`: pick a free `/dev/nbdN`, configure it, run `NBD_DO_IT` on a worker thread, disconnect |

`nbd.rs` adds `expose(overlay) -> Exposure`. It creates a socket pair, gives one end to the kernel, and serves the other.

This uses the classic ioctl interface instead of upstream's generic-netlink connect. That is fewer moving parts and no netlink dependency. Multi-connection and the netlink path can come later behind the same `expose`. ublk follows in M3b with the same `expose` shape.

**Tech Stack:** Rust 1.98, tokio (`UnixStream`), nix (`ioctl`), the M2 block layer, GitHub Actions ubuntu-24.04 (`nbd` module, `e2fsprogs`).

**Spec:** `docs/superpowers/specs/2026-10-07-orchestrator-rs-design.md`, milestone 3 (exposure) and milestone 9 (NBD fallback, pulled forward because CI has no ublk).

## Global Constraints

- Code style follows `CLAUDE.md`:
  - readability first, small methods, chains, named fields;
  - no comments and no `.0` access;
  - `unsafe` lives only in `exposure/nbd/ioctl.rs`, behind one safe function per ioctl.
- The NBD wire format is big-endian.
  - Request (28 bytes): `u32 magic 0x25609513`, `u16 flags`, `u16 type`, `[u8; 8] handle`, `u64 offset`, `u32 length`.
  - Reply (16 bytes): `u32 magic 0x67446698`, `u32 error`, `[u8; 8] handle`. A successful READ reply is followed by `length` bytes of data.
- Command codes: READ 0, WRITE 1, DISC 2, FLUSH 3, TRIM 4, WRITE_ZEROES 6.
- Device error replies use `EIO` (5). A broken socket or a protocol violation ends the server with an error.
- Device flags: `NBD_FLAG_HAS_FLAGS (1) | NBD_FLAG_SEND_FLUSH (4) | NBD_FLAG_SEND_TRIM (32) | NBD_FLAG_SEND_WRITE_ZEROES (64)`.
- Block size is `PAGE_SIZE`, the timeout is 60 s, and ioctl numbers are `_IO(0xab, n)`. The `n` values are SET_SOCK 0, SET_BLKSIZE 1, DO_IT 3, CLEAR_SOCK 4, SET_SIZE_BLOCKS 7, DISCONNECT 8, SET_TIMEOUT 9, SET_FLAGS 10.
- The server handles one request at a time per connection.
- Protocol and server tests run on macOS and Linux without root. The kernel test needs Linux, root and the `nbd` module, and skips itself otherwise.
- Before every commit run `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test -p orchestrator`.

## Review Focus

1. **A WRITE whose payload arrives in several socket reads** must be read completely before it is applied. Covered in Task 2 (the client writes the header and payload separately).
2. **A device error** (a misaligned read reaching the overlay) must return an `EIO` reply and keep serving, not drop the connection. Covered in Task 2.
3. **A bad magic or an unknown command** must end the server with an error instead of guessing. Covered in Tasks 1 and 2.
4. **A clean disconnect (DISC, or EOF on the socket)** must end `serve` with `Ok`, so `Exposure::close` can succeed. Covered in Tasks 2 and 3.
5. **mkfs, mount, write, umount, export, upload, resume, mount** must show the same file content. Covered in Task 3.

---

### Task 1: NBD protocol

**Files:**
- Create: `crates/orchestrator/src/exposure.rs`, `crates/orchestrator/src/exposure/nbd.rs`, `crates/orchestrator/src/exposure/nbd/protocol.rs`
- Modify: `crates/orchestrator/src/lib.rs` (`pub mod exposure;`)

**Interfaces:**
- Produces:
  - `exposure::nbd::protocol::{REQUEST_SIZE: usize = 28, REPLY_SIZE: usize = 16, EIO: u32 = 5}`
  - `Command { Read, Write, Disconnect, Flush, Trim, WriteZeroes }` with `code(&self) -> u16`
  - `Request { command: Command, handle: [u8; 8], offset: u64, length: u32 }` with:
    - `Request::decode(&[u8; REQUEST_SIZE]) -> Result<Request, String>`
    - `request.encode() -> [u8; REQUEST_SIZE]`
  - `encode_reply(handle: [u8; 8], error: u32) -> [u8; REPLY_SIZE]`

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/exposure.rs`:
```rust
pub mod nbd;
```
`crates/orchestrator/src/exposure/nbd.rs`:
```rust
pub mod protocol;
```
Add `pub mod exposure;` to `crates/orchestrator/src/lib.rs` (after `pub mod block;`).

`crates/orchestrator/src/exposure/nbd/protocol.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(parts: &[&[u8]]) -> [u8; REQUEST_SIZE] {
        parts.concat().try_into().unwrap()
    }

    #[test]
    fn decodes_a_big_endian_request() {
        let raw = bytes(&[
            &0x2560_9513u32.to_be_bytes(),
            &0u16.to_be_bytes(),
            &1u16.to_be_bytes(),
            &[7; 8],
            &8192u64.to_be_bytes(),
            &4096u32.to_be_bytes(),
        ]);
        let request = Request::decode(&raw).unwrap();
        assert_eq!(
            request,
            Request {
                command: Command::Write,
                handle: [7; 8],
                offset: 8192,
                length: 4096,
            }
        );
        assert_eq!(request.encode(), raw);
    }

    #[test]
    fn rejects_bad_magic_and_unknown_commands() {
        let bad_magic = bytes(&[&[0; 4], &[0; 2], &[0; 2], &[0; 8], &[0; 8], &[0; 4]]);
        assert_eq!(Request::decode(&bad_magic).unwrap_err(), "bad nbd request magic 0x0");

        let unknown = bytes(&[
            &0x2560_9513u32.to_be_bytes(),
            &[0; 2],
            &5u16.to_be_bytes(),
            &[0; 8],
            &[0; 8],
            &[0; 4],
        ]);
        assert_eq!(Request::decode(&unknown).unwrap_err(), "unsupported nbd command 5");
    }

    #[test]
    fn encodes_a_reply() {
        let reply = encode_reply([3; 8], EIO);
        assert_eq!(reply[..4], 0x6744_6698u32.to_be_bytes());
        assert_eq!(reply[4..8], 5u32.to_be_bytes());
        assert_eq!(reply[8..], [3; 8]);
    }
}
```

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator --lib exposure::`
Expected: FAIL to compile, `cannot find type Request`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/exposure/nbd/protocol.rs`:
```rust
pub const REQUEST_SIZE: usize = 28;
pub const REPLY_SIZE: usize = 16;
pub const EIO: u32 = 5;

const REQUEST_MAGIC: u32 = 0x2560_9513;
const REPLY_MAGIC: u32 = 0x6744_6698;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Read,
    Write,
    Disconnect,
    Flush,
    Trim,
    WriteZeroes,
}

impl Command {
    pub fn code(&self) -> u16 {
        match self {
            Command::Read => 0,
            Command::Write => 1,
            Command::Disconnect => 2,
            Command::Flush => 3,
            Command::Trim => 4,
            Command::WriteZeroes => 6,
        }
    }

    fn from_code(code: u16) -> Result<Self, String> {
        match code {
            0 => Ok(Command::Read),
            1 => Ok(Command::Write),
            2 => Ok(Command::Disconnect),
            3 => Ok(Command::Flush),
            4 => Ok(Command::Trim),
            6 => Ok(Command::WriteZeroes),
            other => Err(format!("unsupported nbd command {other}")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub command: Command,
    pub handle: [u8; 8],
    pub offset: u64,
    pub length: u32,
}

impl Request {
    pub fn decode(bytes: &[u8; REQUEST_SIZE]) -> Result<Self, String> {
        let magic = u32::from_be_bytes(field(bytes, 0));
        if magic != REQUEST_MAGIC {
            return Err(format!("bad nbd request magic {magic:#x}"));
        }
        Ok(Self {
            command: Command::from_code(u16::from_be_bytes(field(bytes, 6)))?,
            handle: field(bytes, 8),
            offset: u64::from_be_bytes(field(bytes, 16)),
            length: u32::from_be_bytes(field(bytes, 24)),
        })
    }

    pub fn encode(&self) -> [u8; REQUEST_SIZE] {
        let parts: [&[u8]; 6] = [
            &REQUEST_MAGIC.to_be_bytes(),
            &0u16.to_be_bytes(),
            &self.command.code().to_be_bytes(),
            &self.handle,
            &self.offset.to_be_bytes(),
            &self.length.to_be_bytes(),
        ];
        parts.concat().try_into().unwrap_or([0; REQUEST_SIZE])
    }
}

pub fn encode_reply(handle: [u8; 8], error: u32) -> [u8; REPLY_SIZE] {
    let parts: [&[u8]; 3] = [&REPLY_MAGIC.to_be_bytes(), &error.to_be_bytes(), &handle];
    parts.concat().try_into().unwrap_or([0; REPLY_SIZE])
}

fn field<const N: usize>(bytes: &[u8], start: usize) -> [u8; N] {
    bytes[start..start + N].try_into().unwrap_or([0; N])
}
```

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator --lib exposure::`
Expected: PASS, 3 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator
git commit -m "feat(orchestrator): NBD wire protocol"
```

---

### Task 2: NBD server over the Overlay

**Files:**
- Create: `crates/orchestrator/src/exposure/nbd/server.rs`
- Modify: `crates/orchestrator/src/exposure/nbd.rs` (`pub mod server;`)

**Interfaces:**
- Consumes: `protocol::*` (Task 1); `block::overlay::Overlay` (M2: `read_at`, `write_at`, `write_zeroes_at`, `size`).
- Produces: `exposure::nbd::server::serve(device: Arc<Overlay>, socket: tokio::net::UnixStream) -> Result<(), String>` (async). It returns `Ok(())` on DISC or EOF and `Err` on a protocol or socket error.

- [ ] **Step 1: Write the failing tests**

`crates/orchestrator/src/exposure/nbd/server.rs`:
```rust
#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::{Artifact, BuildFile, DiffStore};
    use crate::storage::backend::Storage;
    use crate::storage::header::{Header, Metadata};

    struct Client {
        socket: UnixStream,
        server: JoinHandle<Result<(), String>>,
        _dir: tempfile::TempDir,
    }

    async fn start() -> Client {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let build_id = Uuid::from_bytes([1; 16]);
        storage
            .put(&Artifact::Rootfs.object(build_id), Bytes::from(vec![1; 4 * 4096]))
            .await
            .unwrap();
        let header = Header::new(Metadata::template(build_id, 4096, 4 * 4096), Vec::new()).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
        let overlay = Arc::new(Overlay::new(base, &dir.path().join("overlay")).unwrap());

        let (socket, server_side) = UnixStream::pair().unwrap();
        let server = tokio::spawn(serve(overlay, server_side));
        Client {
            socket,
            server,
            _dir: dir,
        }
    }

    impl Client {
        async fn send(&mut self, command: Command, offset: u64, length: u32) {
            let request = Request {
                command,
                handle: [9; 8],
                offset,
                length,
            };
            self.socket.write_all(&request.encode()).await.unwrap();
        }

        async fn reply(&mut self) -> u32 {
            let mut reply = [0; REPLY_SIZE];
            self.socket.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[8..], [9; 8]);
            u32::from_be_bytes(reply[4..8].try_into().unwrap())
        }

        async fn read(&mut self, offset: u64, length: u32) -> Vec<u8> {
            self.send(Command::Read, offset, length).await;
            assert_eq!(self.reply().await, 0);
            let mut data = vec![0; length as usize];
            self.socket.read_exact(&mut data).await.unwrap();
            data
        }
    }

    #[tokio::test]
    async fn serves_reads_writes_and_zeroes() {
        let mut client = start().await;
        assert_eq!(client.read(0, 4096).await, vec![1; 4096]);

        client.send(Command::Write, 4096, 4096).await;
        client.socket.write_all(&[7; 1000]).await.unwrap();
        client.socket.write_all(&[7; 3096]).await.unwrap();
        assert_eq!(client.reply().await, 0);
        assert_eq!(client.read(4096, 4096).await, vec![7; 4096]);

        client.send(Command::WriteZeroes, 4096, 4096).await;
        assert_eq!(client.reply().await, 0);
        client.send(Command::Trim, 8192, 4096).await;
        assert_eq!(client.reply().await, 0);
        client.send(Command::Flush, 0, 0).await;
        assert_eq!(client.reply().await, 0);
        assert_eq!(client.read(4096, 8192).await, vec![0; 8192]);

        client.send(Command::Disconnect, 0, 0).await;
        assert_eq!(client.server.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn device_errors_reply_eio_and_keep_serving() {
        let mut client = start().await;
        client.send(Command::Read, 100, 4096).await;
        assert_eq!(client.reply().await, EIO);
        assert_eq!(client.read(0, 4096).await, vec![1; 4096]);

        drop(client.socket);
        assert_eq!(client.server.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn protocol_violations_end_the_server() {
        let mut client = start().await;
        client.socket.write_all(&[0; REQUEST_SIZE]).await.unwrap();
        assert_eq!(
            client.server.await.unwrap(),
            Err("bad nbd request magic 0x0".to_owned())
        );
    }
}
```
Add `pub mod server;` to `crates/orchestrator/src/exposure/nbd.rs`.

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p orchestrator --lib exposure::nbd::server`
Expected: FAIL to compile, `cannot find function serve`.

- [ ] **Step 3: Implement**

Put at the top of `crates/orchestrator/src/exposure/nbd/server.rs`:
```rust
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::protocol::{Command, EIO, REPLY_SIZE, REQUEST_SIZE, Request, encode_reply};
use crate::block::overlay::Overlay;

struct Response {
    error: u32,
    data: Vec<u8>,
}

impl Response {
    fn ok(data: Vec<u8>) -> Self {
        Self { error: 0, data }
    }

    fn from_result(result: Result<(), String>) -> Self {
        Self {
            error: if result.is_ok() { 0 } else { EIO },
            data: Vec::new(),
        }
    }
}

pub async fn serve(device: Arc<Overlay>, mut socket: UnixStream) -> Result<(), String> {
    while let Some(request) = next_request(&mut socket).await? {
        if request.command == Command::Disconnect {
            return Ok(());
        }
        let response = handle(&device, &mut socket, request).await?;
        send(&mut socket, request.handle, response).await?;
    }
    Ok(())
}

async fn next_request(socket: &mut UnixStream) -> Result<Option<Request>, String> {
    let mut header = [0; REQUEST_SIZE];
    match socket.read_exact(&mut header).await {
        Ok(_) => Request::decode(&header).map(Some),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(format!("read nbd request: {e}")),
    }
}

async fn handle(device: &Overlay, socket: &mut UnixStream, request: Request) -> Result<Response, String> {
    let length = request.length as usize;
    match request.command {
        Command::Read => {
            let mut data = vec![0; length];
            let read = device.read_at(request.offset, &mut data).await;
            Ok(match read {
                Ok(()) => Response::ok(data),
                Err(e) => Response::from_result(Err(log_device_error(&request, e))),
            })
        }
        Command::Write => {
            let mut payload = vec![0; length];
            socket
                .read_exact(&mut payload)
                .await
                .map_err(|e| format!("read nbd write payload: {e}"))?;
            Ok(Response::from_result(device.write_at(request.offset, &payload)))
        }
        Command::Trim | Command::WriteZeroes => Ok(Response::from_result(
            device.write_zeroes_at(request.offset, u64::from(request.length)),
        )),
        Command::Flush | Command::Disconnect => Ok(Response::ok(Vec::new())),
    }
}

async fn send(socket: &mut UnixStream, handle: [u8; 8], response: Response) -> Result<(), String> {
    let reply: [u8; REPLY_SIZE] = encode_reply(handle, response.error);
    socket
        .write_all(&reply)
        .await
        .map_err(|e| format!("write nbd reply: {e}"))?;
    socket
        .write_all(&response.data)
        .await
        .map_err(|e| format!("write nbd read data: {e}"))
}

fn log_device_error(request: &Request, error: String) -> String {
    tracing::warn!(offset = request.offset, length = request.length, %error, "nbd request failed");
    error
}
```

Add `tracing = "0.1"` to `crates/orchestrator/Cargo.toml` (`cargo add -p orchestrator tracing`).

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p orchestrator --lib exposure::`
Expected: PASS, 6 tests.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator Cargo.lock
git commit -m "feat(orchestrator): NBD server dispatching to the overlay"
```

---

### Task 3: Kernel device and the ext4 round trip

**Files:**
- Create: `crates/orchestrator/src/exposure/nbd/ioctl.rs`, `crates/orchestrator/src/exposure/nbd/device.rs`, `crates/orchestrator/tests/nbd_contract.rs`
- Modify: `crates/orchestrator/src/exposure/nbd.rs` (Linux modules, `expose`, `Exposure`), `crates/orchestrator/Cargo.toml` (nix feature `ioctl`; dev-dependency `nix` features `user`), `.github/workflows/ci.yml` (load `nbd` before tests)

**Interfaces:**
- Consumes: `server::serve` (Task 2), `Overlay::size` (M2), `PAGE_SIZE`.
- Produces (Linux only):
  - `exposure::nbd::device::NbdDevice` with:
    - `attach(kernel_socket: std::os::unix::net::UnixStream, size: u64, block_size: u64) -> io::Result<NbdDevice>`
    - `index(&self) -> u32`
    - `path(&self) -> PathBuf`
    - `disconnect(self) -> io::Result<()>`
  - `exposure::nbd::Exposure` with `path(&self) -> PathBuf` and async `close(self) -> Result<(), String>`
  - `exposure::nbd::expose(overlay: Arc<Overlay>) -> io::Result<Exposure>`

- [ ] **Step 1: Write the failing test**

`crates/orchestrator/tests/nbd_contract.rs`:
```rust
#![cfg(target_os = "linux")]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
use orchestrator::block::overlay::Overlay;
use orchestrator::exposure::nbd::expose;
use orchestrator::storage::backend::Storage;
use orchestrator::storage::header::{Header, Metadata};
use uuid::Uuid;

const SIZE: u64 = 64 << 20;

fn ready() -> bool {
    nix::unistd::geteuid().is_root() && Path::new("/dev/nbd0").exists()
}

fn run(program: &str, args: &[&str]) {
    let status = Command::new(program).args(args).status().unwrap();
    assert!(status.success(), "{program} {args:?} failed: {status}");
}

async fn overlay_over(header: Header, store: Arc<DiffStore>, path: &Path) -> Arc<Overlay> {
    let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
    Arc::new(Overlay::new(base, path).unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ext4_on_nbd_survives_pause_and_resume() {
    if !ready() {
        eprintln!("skipping: needs root and the nbd kernel module");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mount = dir.path().join("mnt");
    std::fs::create_dir(&mount).unwrap();
    let mount = mount.to_str().unwrap();

    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template_id = Uuid::new_v4();
    storage
        .put(&Artifact::Rootfs.object(template_id), Bytes::from(vec![0; SIZE as usize]))
        .await
        .unwrap();
    let template = Header::new(Metadata::template(template_id, 4096, SIZE), Vec::new()).unwrap();
    let store = Arc::new(DiffStore::new(storage.clone(), dir.path().join("cache")));

    let overlay = overlay_over(template.clone(), store.clone(), &dir.path().join("overlay-1")).await;
    let exposure = expose(overlay.clone()).unwrap();
    let device = exposure.path();
    let device = device.to_str().unwrap();
    run("mkfs.ext4", &["-q", "-F", device]);
    run("mount", &[device, mount]);
    std::fs::write(Path::new(mount).join("hello.txt"), "from the first sandbox").unwrap();
    run("umount", &[mount]);
    exposure.close().await.unwrap();

    let mut diff_file = tempfile::tempfile().unwrap();
    let diff = overlay.export_diff(&mut diff_file).unwrap();
    let diff_bytes = {
        use std::io::{Read, Seek, SeekFrom};
        let mut bytes = Vec::new();
        diff_file.seek(SeekFrom::Start(0)).unwrap();
        diff_file.read_to_end(&mut bytes).unwrap();
        bytes
    };
    let snapshot_id = Uuid::new_v4();
    storage
        .put(&Artifact::Rootfs.object(snapshot_id), Bytes::from(diff_bytes))
        .await
        .unwrap();
    let snapshot = diff.to_diff_header(&template, snapshot_id).unwrap();

    let resumed = overlay_over(snapshot, store, &dir.path().join("overlay-2")).await;
    let exposure = expose(resumed).unwrap();
    let device = exposure.path();
    run("mount", &[device.to_str().unwrap(), mount]);
    let content = std::fs::read_to_string(Path::new(mount).join("hello.txt")).unwrap();
    run("umount", &[mount]);
    exposure.close().await.unwrap();

    assert_eq!(content, "from the first sandbox");
}
```

Run `cargo add -p orchestrator --dev nix@0.29 --features user`.

- [ ] **Step 2: Run the test to see it fail**

Run: `cargo test -p orchestrator --test nbd_contract`
Expected: on macOS the file compiles to nothing (cfg linux), so 0 tests. On Linux it fails to compile with `unresolved import orchestrator::exposure::nbd::expose`.

- [ ] **Step 3: Implement the ioctl wrappers and the device**

`crates/orchestrator/src/exposure/nbd/ioctl.rs`:
```rust
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, RawFd};

mod raw {
    use nix::{ioctl_none_bad, ioctl_write_int_bad, request_code_none};

    ioctl_write_int_bad!(set_sock, request_code_none!(0xab, 0));
    ioctl_write_int_bad!(set_block_size, request_code_none!(0xab, 1));
    ioctl_none_bad!(do_it, request_code_none!(0xab, 3));
    ioctl_none_bad!(clear_sock, request_code_none!(0xab, 4));
    ioctl_write_int_bad!(set_size_blocks, request_code_none!(0xab, 7));
    ioctl_none_bad!(disconnect, request_code_none!(0xab, 8));
    ioctl_write_int_bad!(set_timeout, request_code_none!(0xab, 9));
    ioctl_write_int_bad!(set_flags, request_code_none!(0xab, 10));
}

pub fn set_sock(device: &File, socket: RawFd) -> io::Result<()> {
    check(unsafe { raw::set_sock(device.as_raw_fd(), socket) })
}

pub fn set_block_size(device: &File, block_size: u64) -> io::Result<()> {
    check(unsafe { raw::set_block_size(device.as_raw_fd(), block_size as i32) })
}

pub fn set_size_blocks(device: &File, blocks: u64) -> io::Result<()> {
    check(unsafe { raw::set_size_blocks(device.as_raw_fd(), blocks as i32) })
}

pub fn set_timeout(device: &File, seconds: u64) -> io::Result<()> {
    check(unsafe { raw::set_timeout(device.as_raw_fd(), seconds as i32) })
}

pub fn set_flags(device: &File, flags: i32) -> io::Result<()> {
    check(unsafe { raw::set_flags(device.as_raw_fd(), flags) })
}

pub fn do_it(device: &File) -> io::Result<()> {
    check(unsafe { raw::do_it(device.as_raw_fd()) })
}

pub fn clear_sock(device: &File) -> io::Result<()> {
    check(unsafe { raw::clear_sock(device.as_raw_fd()) })
}

pub fn disconnect(device: &File) -> io::Result<()> {
    check(unsafe { raw::disconnect(device.as_raw_fd()) })
}

fn check(result: nix::Result<i32>) -> io::Result<()> {
    result.map(|_| ()).map_err(io::Error::from)
}
```

`crates/orchestrator/src/exposure/nbd/device.rs`:
```rust
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::thread::JoinHandle;
use std::time::Duration;

use super::ioctl;

const FLAGS: i32 = 1 | 4 | 32 | 64;
const TIMEOUT_SECONDS: u64 = 60;
const CONNECT_POLLS: u32 = 200;

pub struct NbdDevice {
    index: u32,
    control: File,
    _kernel_socket: UnixStream,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl NbdDevice {
    pub fn attach(kernel_socket: UnixStream, size: u64, block_size: u64) -> io::Result<Self> {
        let index = free_index()?;
        let control = OpenOptions::new().read(true).write(true).open(device_path(index))?;
        configure(&control, &kernel_socket, size, block_size)?;
        let worker = spawn_worker(control.try_clone()?);
        wait_until_connected(index)?;
        Ok(Self {
            index,
            control,
            _kernel_socket: kernel_socket,
            worker: Some(worker),
        })
    }

    pub fn index(&self) -> u32 {
        self.index
    }

    pub fn path(&self) -> PathBuf {
        device_path(self.index)
    }

    pub fn disconnect(mut self) -> io::Result<()> {
        ioctl::disconnect(&self.control)?;
        let served = self
            .worker
            .take()
            .map(|worker| worker.join().map_err(|_| io::Error::other("nbd worker panicked")))
            .transpose()?;
        if let Some(Err(e)) = served {
            tracing::debug!(index = self.index, error = %e, "nbd worker ended with an error");
        }
        Ok(())
    }
}

fn configure(control: &File, kernel_socket: &UnixStream, size: u64, block_size: u64) -> io::Result<()> {
    ioctl::set_block_size(control, block_size)?;
    ioctl::set_size_blocks(control, size / block_size)?;
    ioctl::set_timeout(control, TIMEOUT_SECONDS)?;
    ioctl::set_flags(control, FLAGS)?;
    ioctl::set_sock(control, kernel_socket.as_raw_fd())
}

fn spawn_worker(control: File) -> JoinHandle<io::Result<()>> {
    std::thread::spawn(move || {
        let served = ioctl::do_it(&control);
        let _ = ioctl::clear_sock(&control);
        served
    })
}

fn wait_until_connected(index: u32) -> io::Result<()> {
    (0..CONNECT_POLLS)
        .find(|_| {
            let connected = pid_path(index).exists();
            if !connected {
                std::thread::sleep(Duration::from_millis(10));
            }
            connected
        })
        .map(|_| ())
        .ok_or_else(|| io::Error::other(format!("nbd{index} did not connect")))
}

fn free_index() -> io::Result<u32> {
    (0..)
        .take_while(|index| device_path(*index).exists())
        .find(|index| !pid_path(*index).exists())
        .ok_or_else(|| io::Error::other("no free nbd device (load the nbd module with a larger nbds_max)"))
}

fn device_path(index: u32) -> PathBuf {
    PathBuf::from(format!("/dev/nbd{index}"))
}

fn pid_path(index: u32) -> PathBuf {
    Path::new("/sys/block").join(format!("nbd{index}")).join("pid")
}
```

Run `cargo add -p orchestrator nix@0.29 --features fs,ioctl`.

- [ ] **Step 4: Implement `expose`**

Replace `crates/orchestrator/src/exposure/nbd.rs` with:
```rust
pub mod protocol;
pub mod server;

#[cfg(target_os = "linux")]
pub mod device;
#[cfg(target_os = "linux")]
mod ioctl;

#[cfg(target_os = "linux")]
pub use linux::{Exposure, expose};

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::task::JoinHandle;

    use super::device::NbdDevice;
    use super::server::serve;
    use crate::block::overlay::Overlay;
    use crate::storage::header::PAGE_SIZE;

    pub struct Exposure {
        device: NbdDevice,
        server: JoinHandle<Result<(), String>>,
    }

    pub fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
        let (kernel_socket, server_socket) = std::os::unix::net::UnixStream::pair()?;
        server_socket.set_nonblocking(true)?;
        let size = overlay.size();
        let server = tokio::spawn(serve(overlay, tokio::net::UnixStream::from_std(server_socket)?));
        let device = NbdDevice::attach(kernel_socket, size, PAGE_SIZE)?;
        Ok(Exposure { device, server })
    }

    impl Exposure {
        pub fn path(&self) -> PathBuf {
            self.device.path()
        }

        pub async fn close(self) -> Result<(), String> {
            self.device
                .disconnect()
                .map_err(|e| format!("disconnect nbd device: {e}"))?;
            self.server
                .await
                .map_err(|e| format!("nbd server task: {e}"))?
        }
    }
}
```

In `.github/workflows/ci.yml`, add this step to the `test` job before the clippy step:
```yaml
      - run: sudo modprobe nbd nbds_max=16
```

- [ ] **Step 5: Run the tests**

Run on macOS: `cargo test -p orchestrator`
Expected: PASS. The NBD contract test is compiled out; all other tests pass.

Then commit and push, and read the CI log for the contract test:
```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings
git add crates/orchestrator Cargo.lock .github/workflows/ci.yml
git commit -m "feat(orchestrator): expose the overlay as /dev/nbdN with an ext4 pause/resume test"
git push origin main
gh run watch --repo Empatixx/e2b-runtime-rs --exit-status
gh run view --repo Empatixx/e2b-runtime-rs --log | grep ext4_on_nbd_survives_pause_and_resume
```
Expected: CI `test` is green and the log shows `test ext4_on_nbd_survives_pause_and_resume ... ok` (not the skip message).
