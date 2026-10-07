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
    run(
        "mkfs.ext4",
        &["-q", "-F", "-d", root.to_str().unwrap(), image.to_str().unwrap(), &size],
    );
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
