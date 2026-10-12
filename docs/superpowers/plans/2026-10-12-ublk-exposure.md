# ublk rootfs exposure Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Serve the sandbox rootfs `Overlay` through ublk as well as NBD, picked per node by `ROOTFS_EXPOSURE`.

**Architecture:** `exposure.rs` becomes the entry point with `Transport`, an `Exposure` enum over `nbd::Exposure` and `ublk::Exposure`, and `expose()`. The ublk side runs one OS thread per device (libublk's control ring is thread local) with one queue served by a smol executor that hands every request to tokio, where the existing `BlockDevice` trait does the work.

**Tech Stack:** Rust 2024, tokio, libublk 0.4 (io_uring), smol 2, nix.

**Spec:** `docs/superpowers/specs/2026-10-12-ublk-exposure-design.md`

## Global Constraints

- Linux only. New modules sit behind `#[cfg(target_os = "linux")]` the way `exposure::nbd` does, and `libublk` and `smol` are Linux-only dependencies. No non-Linux stubs.
- `ROOTFS_EXPOSURE` takes `nbd` (default) or `ublk`. Other values fail with `invalid ROOTFS_EXPOSURE "<value>": want nbd or ublk`.
- With `ublk` and no `/dev/ublk-control`, start fails with `ublk is not available: /dev/ublk-control is missing, load the ublk_drv module`.
- NBD error texts do not change. The shared flush helper takes the device name, so NBD still says `sync NBD device: ...` and `flush NBD device buffers: ...`. ublk says `sync ublk device: ...` and `flush ublk device buffers: ...`. (This refines the spec, which proposed one generic text.)
- ublk device: 1 queue, depth 128, io buffer 512 KiB, logical and physical block 4096, no volatile cache, discard and write zeroes up to 32 MiB.
- No comments in source. `cargo fmt`, `cargo clippy --all-targets`, and the touched tests pass before each commit. Linux tests run through `scripts/test-linux.sh`, one run at a time.

## Review Focus

- A request that fails in the `Overlay` (storage error) answers `-EIO`, and the device keeps serving later requests. Test: `queue::tests::a_device_error_answers_eio`, plus a contract test that reads after a failed read.
- Dropping an `Exposure` without `close` must remove `/dev/ublkbN`. Otherwise every crashed sandbox leaks a device. Test: `dropping_an_exposure_removes_the_device`.
- `close` on a current-thread tokio runtime must not deadlock, because the queue hands work to tokio. Test: `close_works_on_a_current_thread_runtime`.
- Many devices at once get distinct paths and close together. Test: `concurrent_exposures_get_distinct_devices_and_close_together`.
- The data written through ublk must land in the overlay diff that a pause exports. Test: `ext4_on_ublk_survives_pause_and_resume`.

---

### Task 1: Transport selection

**Files:**
- Modify: `crates/orchestrator/src/exposure.rs`
- Modify: `crates/orchestrator/src/server/config.rs` (field `rootfs_exposure`, parse, tests)
- Modify: `crates/orchestrator/src/main.rs` (call `use_transport` after `NodeConfig::from_env`)
- Modify: `crates/orchestrator/src/resume_build.rs` (same call)

**Interfaces:**
- Produces: `exposure::Transport { Nbd, Ublk }` (`Debug, Clone, Copy, PartialEq, Eq, Default`), `Transport::parse(Option<&str>) -> Result<Transport, String>`, `exposure::use_transport(Transport) -> Result<(), String>`, `exposure::transport() -> Transport`, `NodeConfig.rootfs_exposure: Transport`.

- [ ] Step 1: tests in `exposure.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootfs_exposure_defaults_to_nbd_and_names_both_transports() {
        assert_eq!(Transport::parse(None), Ok(Transport::Nbd));
        assert_eq!(Transport::parse(Some("nbd")), Ok(Transport::Nbd));
        assert_eq!(Transport::parse(Some("ublk")), Ok(Transport::Ublk));
        assert_eq!(
            Transport::parse(Some("NBD")),
            Err("invalid ROOTFS_EXPOSURE \"NBD\": want nbd or ublk".to_owned())
        );
    }

    #[test]
    fn ublk_needs_the_control_device() {
        let missing = Path::new("/nonexistent/ublk-control");
        assert_eq!(
            available(Transport::Ublk, missing),
            Err("ublk is not available: /nonexistent/ublk-control is missing, load the ublk_drv module".to_owned())
        );
        assert_eq!(available(Transport::Nbd, missing), Ok(()));
    }
}
```

- [ ] Step 2: implement in `exposure.rs`:

```rust
const UBLK_CONTROL: &str = "/dev/ublk-control";

static TRANSPORT: OnceLock<Transport> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    #[default]
    Nbd,
    Ublk,
}

impl Transport {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("nbd") => Ok(Self::Nbd),
            Some("ublk") => Ok(Self::Ublk),
            Some(other) => Err(format!("invalid ROOTFS_EXPOSURE \"{other}\": want nbd or ublk")),
        }
    }
}

pub fn use_transport(transport: Transport) -> Result<(), String> {
    available(transport, Path::new(UBLK_CONTROL))?;
    let chosen = *TRANSPORT.get_or_init(|| transport);
    (chosen == transport)
        .then_some(())
        .ok_or_else(|| format!("rootfs exposure is already {chosen:?}"))
}

pub fn transport() -> Transport {
    TRANSPORT.get().copied().unwrap_or_default()
}

fn available(transport: Transport, control: &Path) -> Result<(), String> {
    match transport == Transport::Ublk && !control.exists() {
        true => Err(format!(
            "ublk is not available: {} is missing, load the ublk_drv module",
            control.display()
        )),
        false => Ok(()),
    }
}
```

- [ ] Step 3: `NodeConfig` gets `pub rootfs_exposure: Transport`, set with `Transport::parse(var("ROOTFS_EXPOSURE").as_deref())?`. Extend `defaults_and_local_storage` with `assert_eq!(config.rootfs_exposure, Transport::Nbd)`, and add a test that `ROOTFS_EXPOSURE=ublk` parses and `ROOTFS_EXPOSURE=x` fails with the text above.
- [ ] Step 4: in `main.rs`, after the `network_version` check: `orchestrator::exposure::use_transport(config.rootfs_exposure)?;`. Do the same in `resume_build.rs` right after its `NodeConfig::from_env()`.
- [ ] Step 5: run `cargo test -p orchestrator --lib exposure:: server::config` (host and Linux), then fmt, clippy and commit `feat(orchestrator): ROOTFS_EXPOSURE picks the rootfs transport`.

### Task 2: Shared block device pieces

**Files:**
- Create: `crates/orchestrator/src/exposure/block_device.rs`
- Modify: `crates/orchestrator/src/exposure/nbd/server.rs` (trait moves out), `nbd/device.rs` (flush uses helper), `nbd/ioctl.rs` (drop `flush_buffers`)

**Interfaces:**
- Produces: `exposure::block_device::BlockDevice` (the trait from `nbd::server`, unchanged, with its `Overlay` impl), `exposure::block_device::flush(device: &File, name: &str) -> Result<(), String>`.

- [ ] Step 1: move the `flush_failures_join_like_go_errors_join` test and `joined_failures` into `block_device.rs`, unchanged.
- [ ] Step 2: implement:

```rust
mod raw {
    nix::ioctl_none_bad!(flush_buffers, nix::request_code_none!(0x12, 97));
}

pub fn flush(device: &File, name: &str) -> Result<(), String> {
    let synced = device.sync_all().map_err(|e| format!("sync {name} device: {e}"));
    let invalidated = flush_buffers(device).map_err(|e| format!("flush {name} device buffers: {e}"));
    joined_failures([synced.err(), invalidated.err()])
}

fn flush_buffers(device: &File) -> io::Result<()> {
    Errno::result(unsafe { raw::flush_buffers(device.as_raw_fd()) })
        .map(drop)
        .map_err(io::Error::from)
}
```

- [ ] Step 3: `nbd::device::flush_device` becomes `block_device::flush(control, "NBD")`. `nbd::server` and the spike imports use `crate::exposure::block_device::BlockDevice`.
- [ ] Step 4: run `scripts/test-linux.sh -p orchestrator --lib exposure` and `--test nbd_contract`, then commit `refactor(orchestrator): BlockDevice and the device flush live in exposure::block_device`.

### Task 3: Exposure enum in front of NBD

**Files:**
- Modify: `crates/orchestrator/src/exposure.rs`, `src/sandbox.rs:33`, `src/sandbox/resume.rs:14`, `src/sandbox/resources.rs:6`, `src/builder/phases/ensure_free_disk/offline.rs:8`, `tests/nbd_contract.rs:10`, `tests/vm_contract.rs:13`

**Interfaces:**
- Consumes: `transport()` from Task 1.
- Produces: `exposure::Exposure` enum with `path() -> PathBuf`, `async flush(&self) -> Result<(), String>`, `async ended(&self)`, `async close(self) -> Result<(), String>`, and `exposure::expose(Arc<Overlay>) -> io::Result<Exposure>`.

- [ ] Step 1: add the enum with only the `Nbd` arm, and a `match` in each method. `expose` matches `transport()`. For `Transport::Ublk` in this task, it returns `Err(io::Error::other("ublk exposure is not built yet"))`. Task 4 replaces that arm.
- [ ] Step 2: switch every caller import to `crate::exposure::{Exposure, expose}` (or `orchestrator::exposure::expose`).
- [ ] Step 3: run `scripts/test-linux.sh -p orchestrator --test nbd_contract` and `cargo clippy --all-targets`, then commit `refactor(orchestrator): callers expose the rootfs through exposure::expose`.

### Task 4: ublk device and queue

**Files:**
- Modify: `crates/orchestrator/Cargo.toml` (`[target.'cfg(target_os = "linux")'.dependencies] libublk = "0.4"`, `smol = "2"`)
- Modify: `docker/test.Dockerfile` (add `libclang-dev`), `scripts/linux-container.sh` (add `-v /dev:/dev`), `.github/workflows/ci.yml` and `scripts/ci/nbd-host.sh` renamed to `scripts/ci/block-host.sh` (install `linux-modules-extra-$(uname -r)`, `modprobe ublk_drv`, `apt-get install libclang-dev`)
- Create: `crates/orchestrator/src/exposure/ublk.rs` (`Exposure`, `expose`), `src/exposure/ublk/device.rs` (thread, params, stop), `src/exposure/ublk/queue.rs` (request mapping, serve)
- Create: `crates/orchestrator/tests/ublk_contract.rs`
- Modify: `crates/orchestrator/src/exposure.rs` (`Ublk` arm)

**Interfaces:**
- Consumes: `BlockDevice` (Task 2), `Exposure` enum (Task 3).
- Produces: `ublk::expose(Arc<Overlay>) -> io::Result<ublk::Exposure>` and the same four methods as `nbd::Exposure`.

- [ ] Step 1: unit tests in `queue.rs` with a fake device that records calls and fails on offset `u64::MAX & !4095`:

```rust
#[tokio::test]
async fn reads_return_the_device_data_and_the_length() { /* op READ, 8192 bytes, Answer { result: 8192, data: 8192 bytes of 7 } */ }
#[tokio::test]
async fn writes_pass_the_payload_and_answer_the_length() { /* op WRITE */ }
#[tokio::test]
async fn discard_and_write_zeroes_write_zeroes() { /* both ops call write_zeroes_at(offset, length) */ }
#[tokio::test]
async fn flush_answers_zero() { /* op FLUSH, length 0, result 0 */ }
#[tokio::test]
async fn a_device_error_answers_eio() { /* result == -libc::EIO, data empty */ }
#[tokio::test]
async fn an_unknown_op_answers_einval() { /* op 99 */ }
#[test]
fn a_request_reads_sectors_from_the_descriptor() { /* start_sector 8, nr_sectors 16, op_flags 1 | 1 << 8 gives offset 4096, length 8192, op WRITE */ }
```

- [ ] Step 2: `queue.rs`:

```rust
const SECTOR_SHIFT: u32 = 9;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    pub op: u32,
    pub offset: u64,
    pub length: usize,
}

#[derive(Debug, PartialEq)]
pub struct Answer {
    pub result: i32,
    pub data: Vec<u8>,
}

impl Request {
    pub fn from_descriptor(descriptor: &sys::ublksrv_io_desc) -> Self {
        Self {
            op: descriptor.op_flags & 0xff,
            offset: descriptor.start_sector << SECTOR_SHIFT,
            length: (descriptor.nr_sectors as usize) << SECTOR_SHIFT,
        }
    }
}

impl Answer {
    fn failed(errno: i32) -> Self {
        Self { result: -errno, data: Vec::new() }
    }
}

pub async fn perform<D: BlockDevice>(device: Arc<D>, request: Request, payload: Vec<u8>) -> Answer {
    let done = match request.op {
        sys::UBLK_IO_OP_READ => read(&device, request).await,
        sys::UBLK_IO_OP_WRITE => blocking(move || device.write_at(request.offset, &payload)).await,
        sys::UBLK_IO_OP_DISCARD | sys::UBLK_IO_OP_WRITE_ZEROES => {
            blocking(move || device.write_zeroes_at(request.offset, request.length as u64)).await
        }
        sys::UBLK_IO_OP_FLUSH => Ok(Vec::new()),
        _ => return Answer::failed(libc::EINVAL),
    };
    done.map_or_else(
        |e| {
            tracing::warn!(offset = request.offset, length = request.length, error = %e, "ublk request failed");
            Answer::failed(libc::EIO)
        },
        |data| Answer { result: request.length as i32, data },
    )
}

async fn read<D: BlockDevice>(device: &D, request: Request) -> Result<Vec<u8>, String> {
    let mut data = vec![0; request.length];
    device.read_at(request.offset, &mut data).await.map(|()| data)
}

async fn blocking(work: impl FnOnce() -> Result<(), String> + Send + 'static) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("ublk blocking task: {e}"))?
        .map(|()| Vec::new())
}

pub fn serve<D: BlockDevice>(queue_id: u16, dev: &UblkDev, device: Arc<D>, runtime: Handle) {
    let queue = match UblkQueue::new(queue_id, dev) {
        Ok(queue) => Rc::new(queue),
        Err(e) => return tracing::error!(error = %e, "open ublk queue"),
    };
    let executor = Rc::new(smol::LocalExecutor::new());
    let tags: Vec<_> = (0..dev.dev_info.queue_depth)
        .map(|tag| {
            let (queue, device, runtime) = (queue.clone(), device.clone(), runtime.clone());
            executor.spawn(async move { serve_tag(&queue, tag, &device, &runtime).await })
        })
        .collect();
    smol::block_on(executor.run(async {
        let tick = || while executor.try_tick() {};
        let done = || tags.iter().all(|tag| tag.is_finished());
        if let Err(e) = libublk::wait_and_handle_io_events(&queue, Some(20), tick, done).await {
            tracing::debug!(error = %e, "ublk queue ended");
        }
    }));
}

async fn serve_tag<D: BlockDevice>(
    queue: &UblkQueue<'_>,
    tag: u16,
    device: &Arc<D>,
    runtime: &Handle,
) -> Result<(), UblkError> {
    let mut buffer = IoBuf::<u8>::new(queue.dev.dev_info.max_io_buf_bytes as usize);
    queue.submit_io_prep_cmd(tag, BufDesc::Slice(buffer.as_slice()), 0, Some(&buffer)).await?;
    loop {
        let request = Request::from_descriptor(queue.get_iod(tag));
        let payload = (request.op == sys::UBLK_IO_OP_WRITE)
            .then(|| buffer[..request.length].to_vec())
            .unwrap_or_default();
        let answer = runtime
            .spawn(perform(device.clone(), request, payload))
            .await
            .unwrap_or_else(|_| Answer::failed(libc::EIO));
        buffer[..answer.data.len()].copy_from_slice(&answer.data);
        queue.submit_io_commit_cmd(tag, BufDesc::Slice(buffer.as_slice()), answer.result).await?;
    }
}
```

- [ ] Step 3: `device.rs`:

```rust
const QUEUE_DEPTH: u16 = 128;
const IO_BUFFER_BYTES: u32 = 512 << 10;
const MAX_ZEROES_BYTES: u32 = 32 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const CONNECT_POLL_INTERVAL: Duration = Duration::from_micros(100);

pub fn run<D: BlockDevice>(
    device: Arc<D>,
    size: u64,
    runtime: Handle,
    started: oneshot::Sender<u32>,
) -> Result<(), String> {
    let control = UblkCtrlBuilder::default()
        .name("e2b")
        .nr_queues(1)
        .depth(QUEUE_DEPTH)
        .io_buf_bytes(IO_BUFFER_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .map_err(|e| format!("add ublk device: {e}"))?;
    let served = control
        .run_target(
            move |dev| {
                dev.tgt.dev_size = size;
                dev.tgt.params = params(size);
                Ok(())
            },
            move |queue_id, dev| queue::serve(queue_id, dev, device, runtime),
            move |control| drop(started.send(control.dev_info().dev_id)),
        )
        .map(drop)
        .map_err(|e| format!("run ublk device: {e}"));
    let deleted = control.del_dev().map(drop).map_err(|e| format!("delete ublk device: {e}"));
    served.and(deleted)
}

pub fn params(size: u64) -> sys::ublk_params {
    let block_shift = PAGE_SIZE.trailing_zeros() as u8;
    sys::ublk_params {
        types: sys::UBLK_PARAM_TYPE_BASIC | sys::UBLK_PARAM_TYPE_DISCARD,
        basic: sys::ublk_param_basic {
            logical_bs_shift: block_shift,
            physical_bs_shift: block_shift,
            io_opt_shift: block_shift,
            io_min_shift: block_shift,
            max_sectors: IO_BUFFER_BYTES >> 9,
            dev_sectors: size >> 9,
            ..Default::default()
        },
        discard: sys::ublk_param_discard {
            discard_granularity: PAGE_SIZE as u32,
            max_discard_sectors: MAX_ZEROES_BYTES >> 9,
            max_write_zeroes_sectors: MAX_ZEROES_BYTES >> 9,
            max_discard_segments: 1,
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn stop(id: u32) -> Result<(), String> {
    UblkCtrl::new_simple(id as i32)
        .and_then(|control| control.kill_dev())
        .map(drop)
        .map_err(|e| format!("stop ublk device: {e}"))
}

pub async fn wait_for_node(path: &Path) -> io::Result<()> { /* poll path.exists() every CONNECT_POLL_INTERVAL until CONNECT_TIMEOUT, then Err("ublk device <path> did not appear") */ }
```

Unit test: `params_match_the_nbd_geometry` checks the 4096 shifts, `dev_sectors == size >> 9`, no `UBLK_ATTR_VOLATILE_CACHE`, and one discard segment.

- [ ] Step 4: `ublk.rs`:

```rust
pub struct Exposure {
    id: u32,
    thread: Option<JoinHandle<Result<(), String>>>,
    serving: watch::Receiver<()>,
}

pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
    let (started, id) = oneshot::channel();
    let (alive, serving) = watch::channel(());
    let (size, runtime) = (overlay.size(), Handle::current());
    let thread = std::thread::Builder::new().name("ublk".to_owned()).spawn(move || {
        let served = device::run(overlay, size, runtime, started);
        drop(alive);
        served
    })?;
    let mut exposure = Exposure { id: u32::MAX, thread: Some(thread), serving };
    exposure.id = match id.await {
        Ok(id) => id,
        Err(_) => return Err(io::Error::other(exposure.failure().await)),
    };
    device::wait_for_node(&exposure.path()).await?;
    Ok(exposure)
}

impl Exposure {
    pub fn path(&self) -> PathBuf {
        PathBuf::from(format!("/dev/ublkb{}", self.id))
    }

    pub async fn flush(&self) -> Result<(), String> {
        let path = self.path();
        tokio::task::spawn_blocking(move || {
            File::options().read(true).write(true).open(&path)
                .map_err(|e| format!("sync ublk device: {e}"))
                .and_then(|device| block_device::flush(&device, "ublk"))
        })
        .await
        .map_err(|e| format!("flush ublk device: {e}"))?
    }

    pub async fn ended(&self) {
        let mut serving = self.serving.clone();
        while serving.changed().await.is_ok() {}
    }

    pub async fn close(mut self) -> Result<(), String> {
        let (id, thread) = (self.id, self.thread.take());
        tokio::task::spawn_blocking(move || device::stop(id).and(joined(thread)))
            .await
            .map_err(|e| format!("ublk device thread: {e}"))?
    }

    async fn failure(&mut self) -> String { /* join the thread in spawn_blocking; its Err text, or "ublk device stopped before starting" */ }
}

impl Drop for Exposure {
    fn drop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let id = self.id;
            std::thread::spawn(move || drop(device::stop(id).and(joined(Some(thread)))));
        }
    }
}

fn joined(thread: Option<JoinHandle<Result<(), String>>>) -> Result<(), String> {
    thread.map_or(Ok(()), |thread| {
        thread.join().map_err(|_| "ublk device thread panicked".to_owned()).and_then(|served| served)
    })
}
```

- [ ] Step 5: wire the `Ublk` arm in `exposure.rs` (`ublk::expose(overlay).await.map(Exposure::Ublk)` and each method).
- [ ] Step 6: `tests/ublk_contract.rs` copies the five `nbd_contract.rs` cases, with `ready()` checking root and `/dev/ublk-control`. Each test starts with `orchestrator::exposure::use_transport(Transport::Ublk).unwrap()` and uses `orchestrator::exposure::expose`. The drop test waits for `/dev/ublkbN` to disappear. Add `a_failed_read_does_not_stop_the_device`: an overlay over a missing storage object, a read of the device fails with EIO, then `close` still answers `Ok`.
- [ ] Step 7: run `scripts/test-linux.sh -p orchestrator --lib exposure` and `--test ublk_contract` and `--test nbd_contract`, then fmt, clippy (host and `scripts/clippy-linux.sh`) and commit `feat(orchestrator): ublk rootfs exposure behind ROOTFS_EXPOSURE=ublk`.

### Task 5: Measure the real implementation

- [ ] Step 1: point `spikes/ublk-bench` at `orchestrator::exposure::{expose, use_transport}` over a zero `Overlay`. Run it for NBD and ublk with `-v /dev:/dev`.
- [ ] Step 2: record attach p50 and the queue depth 1 numbers. If ublk is not faster than NBD at queue depth 1, stop and report before the docs.

### Task 6: Host setup and docs

**Files:**
- Modify: `deploy/k8s/base/scripts/host-setup.sh`, `deploy/k8s/base/orchestrator.yaml`, `deploy/k8s/overlays/example/cluster.env`, `deploy/README.md`
- Modify: `website/content/docs/orchestrator/block-devices.mdx` and `.cs.mdx`, `design/performance.mdx` and `.cs.mdx`, `design/principles.mdx` and `.cs.mdx`
- Modify: `docs/BACKLOG.md`, `.claude/skills/project-structure/SKILL.md` (exposure layout)
- Delete: `spikes/ublk-bench`

- [ ] Step 1: `host-setup.sh`: when `ROOTFS_EXPOSURE=ublk`, `modinfo ublk_drv` or `apt-get install -y linux-modules-extra-$(uname -r)`, then `modprobe ublk_drv`, add `ublk_drv` to `/etc/modules-load.d/e2b.conf`, and append `ACTION=="add|change", KERNEL=="ublkb*", OPTIONS:="nowatch"` to the udev rule. Orchestrator env gets `ROOTFS_EXPOSURE` from the cluster ConfigMap (default `nbd`). Add a hostPath `/dev` volume mounted at `/dev`.
- [ ] Step 2: web pages EN and CS: a ublk section with the setting, the host setup, the measured numbers, and the sentence "This is not in upstream E2B, which uses NBD only." Czech: "Upstream E2B tohle nemá, používá jen NBD."
- [ ] Step 3: backlog lines: zero copy on kernel 6.15+, ublk user recovery, more queues if Firecracker moves to the Async io engine.
- [ ] Step 4: run `python3 scripts/check-prose.py`, then commit `docs: ublk rootfs exposure, host setup and the difference from upstream`.
