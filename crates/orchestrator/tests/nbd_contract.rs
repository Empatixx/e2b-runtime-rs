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
    let exposure = expose(overlay.clone()).await.unwrap();
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
    let exposure = expose(resumed).await.unwrap();
    let device = exposure.path();
    run("mount", &[device.to_str().unwrap(), mount]);
    let content = std::fs::read_to_string(Path::new(mount).join("hello.txt")).unwrap();
    run("umount", &[mount]);
    exposure.close().await.unwrap();

    assert_eq!(content, "from the first sandbox");
}

async fn zero_overlay(dir: &Path, name: &str) -> Arc<Overlay> {
    let storage = Storage::local(&dir.join("store")).unwrap();
    let template_id = Uuid::new_v4();
    storage
        .put(&Artifact::Rootfs.object(template_id), Bytes::from(vec![0; 8 << 20]))
        .await
        .unwrap();
    let template = Header::new(Metadata::template(template_id, 4096, 8 << 20), Vec::new()).unwrap();
    let store = Arc::new(DiffStore::new(storage, dir.join(format!("cache-{name}"))));
    overlay_over(template, store, &dir.join(format!("overlay-{name}"))).await
}

#[tokio::test]
async fn close_works_on_a_current_thread_runtime() {
    if !ready() {
        eprintln!("skipping: needs root and the nbd kernel module");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let exposure = expose(zero_overlay(dir.path(), "single").await).await.unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(20), exposure.close()).await;
    assert_eq!(closed, Ok(Ok(())));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_exposures_get_distinct_devices_and_close_together() {
    if !ready() {
        eprintln!("skipping: needs root and the nbd kernel module");
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
    let closed = tokio::time::timeout(std::time::Duration::from_secs(30), closing).await;
    assert!(closed.is_ok(), "closing four devices on two worker threads timed out");
    assert!(closed.unwrap().into_iter().all(|result| result.is_ok()));
}
