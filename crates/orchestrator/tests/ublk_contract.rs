#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
use orchestrator::block::overlay::Overlay;
use orchestrator::exposure::block_device::BlockDevice;
use orchestrator::exposure::ublk::device;
use orchestrator::exposure::{Transport, expose, use_transport};
use orchestrator::storage::backend::Storage;
use orchestrator::storage::header::{Header, Metadata};
use uuid::Uuid;

const SIZE: u64 = 64 << 20;
const ZERO_SIZE: u64 = 8 << 20;
const SILENT_FROM: u64 = 16 << 20;
const SILENT_TO: u64 = 17 << 20;
const SERVER_READY: &str = "ublk-id ";

fn ready() -> bool {
    let ready = nix::unistd::geteuid().is_root() && Path::new("/dev/ublk-control").exists();
    if ready {
        use_transport(Transport::Ublk).unwrap();
    } else {
        eprintln!("skipping: needs root and the ublk_drv kernel module");
    }
    ready
}

fn run(program: &str, args: &[&str]) {
    assert!(succeeds(program, args), "{program} {args:?} failed");
}

fn succeeds(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

fn dd(args: &[String]) -> bool {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    succeeds("dd", &args)
}

fn exited_within(child: &mut Child, limit: Duration) -> Option<bool> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status.success());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

struct Mount {
    target: String,
    mounted: bool,
}

impl Mount {
    fn new(device: &str, target: &str) -> Self {
        run("mount", &[device, target]);
        Self {
            target: target.to_owned(),
            mounted: true,
        }
    }

    fn unmount(mut self) {
        run("umount", &[&self.target]);
        self.mounted = false;
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if self.mounted {
            let _ = Command::new("umount").args(["-l", &self.target]).status();
        }
    }
}

async fn overlay_over(header: Header, store: Arc<DiffStore>, path: &Path) -> Arc<Overlay> {
    let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
    Arc::new(Overlay::new(base, path).unwrap())
}

async fn template(storage: &Storage, size: u64, stored: bool) -> Header {
    let template_id = Uuid::new_v4();
    if stored {
        storage
            .put(
                &Artifact::Rootfs.object(template_id),
                Bytes::from(vec![0; size as usize]),
            )
            .await
            .unwrap();
    }
    Header::new(Metadata::template(template_id, 4096, size), Vec::new()).unwrap()
}

async fn zero_overlay(dir: &Path, name: &str) -> Arc<Overlay> {
    let storage = Storage::local(&dir.join("store")).unwrap();
    let header = template(&storage, ZERO_SIZE, true).await;
    let store = Arc::new(DiffStore::new(storage, dir.join(format!("cache-{name}"))));
    overlay_over(header, store, &dir.join(format!("overlay-{name}"))).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ext4_on_ublk_survives_pause_and_resume() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mount = dir.path().join("mnt");
    std::fs::create_dir(&mount).unwrap();
    let mount = mount.to_str().unwrap();

    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template = template(&storage, SIZE, true).await;
    let store = Arc::new(DiffStore::new(storage.clone(), dir.path().join("cache")));

    let overlay = overlay_over(template.clone(), store.clone(), &dir.path().join("overlay-1")).await;
    let exposure = expose(overlay.clone()).await.unwrap();
    let device = exposure.path();
    let device = device.to_str().unwrap();
    run("mkfs.ext4", &["-q", "-F", device]);
    let mounted = Mount::new(device, mount);
    std::fs::write(Path::new(mount).join("hello.txt"), "from the first sandbox").unwrap();
    mounted.unmount();
    exposure.flush().await.unwrap();
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
    let exposure = expose(resumed).await.unwrap();
    let device = exposure.path();
    let mounted = Mount::new(device.to_str().unwrap(), mount);
    let content = std::fs::read_to_string(Path::new(mount).join("hello.txt")).unwrap();
    mounted.unmount();
    exposure.close().await.unwrap();

    assert_eq!(content, "from the first sandbox");
}

#[tokio::test]
async fn close_works_on_a_current_thread_runtime() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let exposure = expose(zero_overlay(dir.path(), "single").await).await.unwrap();
    let closed = tokio::time::timeout(Duration::from_secs(20), exposure.close()).await;
    assert_eq!(closed, Ok(Ok(())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_exposures_get_distinct_devices_and_close_together() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let mut overlays = Vec::new();
    for name in ["a", "b", "c", "d"] {
        overlays.push(zero_overlay(dir.path(), name).await);
    }
    let exposures: Vec<_> = futures::future::join_all(overlays.into_iter().map(expose))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();

    let mut paths: Vec<_> = exposures.iter().map(|exposure| exposure.path()).collect();
    paths.sort();
    paths.dedup();
    assert_eq!(paths.len(), 4, "{paths:?}");

    let closing = futures::future::join_all(exposures.into_iter().map(|exposure| exposure.close()));
    let closed = tokio::time::timeout(Duration::from_secs(30), closing).await;
    assert!(closed.is_ok(), "closing four devices on two worker threads timed out");
    assert!(closed.unwrap().into_iter().all(|result| result.is_ok()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_an_exposure_removes_the_device() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let exposure = expose(zero_overlay(dir.path(), "dropped").await).await.unwrap();
    let device = exposure.path();
    assert!(device.exists());

    drop(exposure);

    let deadline = Instant::now() + Duration::from_secs(5);
    while device.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!device.exists(), "a dropped exposure left {} behind", device.display());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parallel_writers_and_readers_see_their_own_data() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let exposure = expose(zero_overlay(dir.path(), "parallel").await).await.unwrap();
    let device = exposure.path().display().to_string();
    let part = ZERO_SIZE / 8;

    let workers: Vec<_> = (0..8u64)
        .map(|worker| {
            let (device, dir) = (device.clone(), dir.path().to_path_buf());
            std::thread::spawn(move || {
                let (written, back) = (dir.join(format!("in-{worker}")), dir.join(format!("out-{worker}")));
                let data = vec![worker as u8 + 1; part as usize];
                std::fs::write(&written, &data).unwrap();
                let blocks = part / 4096;
                let wrote = dd(&[
                    format!("if={}", written.display()),
                    format!("of={device}"),
                    "bs=4096".to_owned(),
                    format!("seek={}", worker * blocks),
                    "oflag=direct".to_owned(),
                    "conv=fsync,notrunc".to_owned(),
                ]);
                let read = dd(&[
                    format!("if={device}"),
                    format!("of={}", back.display()),
                    "bs=4096".to_owned(),
                    format!("skip={}", worker * blocks),
                    format!("count={blocks}"),
                    "iflag=direct".to_owned(),
                ]);
                wrote && read && std::fs::read(&back).unwrap() == data
            })
        })
        .collect();
    let matched: Vec<bool> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    exposure.close().await.unwrap();

    assert_eq!(matched, vec![true; 8]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_read_does_not_stop_the_device() {
    if !ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let header = template(&storage, ZERO_SIZE, false).await;
    let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
    let overlay = overlay_over(header, store, &dir.path().join("overlay")).await;
    let exposure = expose(overlay).await.unwrap();
    let device = exposure.path().display().to_string();
    let block = dir.path().join("block");
    std::fs::write(&block, [9; 4096]).unwrap();

    let (read, written) = tokio::task::spawn_blocking(move || {
        let read = dd(&[
            format!("if={device}"),
            "of=/dev/null".to_owned(),
            "bs=4096".to_owned(),
            "skip=256".to_owned(),
            "count=1".to_owned(),
            "iflag=direct".to_owned(),
        ]);
        let written = dd(&[
            format!("if={}", block.display()),
            format!("of={device}"),
            "bs=4096".to_owned(),
            "oflag=direct".to_owned(),
            "conv=fsync,notrunc".to_owned(),
        ]);
        (read, written)
    })
    .await
    .unwrap();
    let closed = exposure.close().await;

    assert_eq!((read, written, closed), (false, true, Ok(())));
}

struct SilentInTheMiddle;

impl BlockDevice for SilentInTheMiddle {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        if (SILENT_FROM..SILENT_TO).contains(&offset) {
            std::future::pending::<()>().await;
        }
        buf.fill(0);
        Ok(())
    }

    fn write_at(&self, _: u64, _: &[u8]) -> Result<(), String> {
        Ok(())
    }

    fn write_zeroes_at(&self, _: u64, _: u64) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "the server process for a_killed_server_fails_its_reads_and_leaves_an_orphan_to_remove"]
async fn serve_a_device_that_never_answers_the_middle() {
    let (started, id) = tokio::sync::oneshot::channel();
    let runtime = tokio::runtime::Handle::current();
    std::thread::spawn(move || device::run(Arc::new(SilentInTheMiddle), SIZE, runtime, started));
    println!("{SERVER_READY}{}", id.await.unwrap());
    std::future::pending::<()>().await;
}

fn start_server() -> (Child, u32) {
    let mut server = Command::new(std::env::current_exe().unwrap())
        .args([
            "serve_a_device_that_never_answers_the_middle",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let id = BufReader::new(server.stdout.take().unwrap())
        .lines()
        .map_while(Result::ok)
        .find_map(|line| line.strip_prefix(SERVER_READY).map(|id| id.parse().unwrap()))
        .expect("the server process printed no device id");
    (server, id)
}

#[test]
fn a_killed_server_fails_its_reads_and_leaves_an_orphan_to_remove() {
    if !ready() {
        return;
    }
    let (mut server, id) = start_server();
    let device = format!("/dev/ublkb{id}");
    let mut reader = Command::new("dd")
        .args([
            format!("if={device}"),
            "of=/dev/null".to_owned(),
            "bs=4096".to_owned(),
            format!("skip={}", SILENT_FROM / 4096),
            "count=1".to_owned(),
            "iflag=direct".to_owned(),
        ])
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert_eq!(
        exited_within(&mut reader, Duration::from_millis(500)),
        None,
        "the read was answered"
    );

    server.kill().unwrap();
    server.wait().unwrap();

    assert_eq!(exited_within(&mut reader, Duration::from_secs(15)), Some(false));
    assert!(device::remove_orphans().contains(&id));
    assert!(!Path::new(&format!("/dev/ublkc{id}")).exists());
}
