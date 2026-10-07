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
        .put(
            &Artifact::Rootfs.object(template_id),
            Bytes::from(vec![0; SIZE as usize]),
        )
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
