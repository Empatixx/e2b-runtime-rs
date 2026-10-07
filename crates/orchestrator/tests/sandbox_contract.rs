#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
use orchestrator::block::overlay::Overlay;
use orchestrator::envd::EnvdInit;
use orchestrator::network::slot::Slot;
use orchestrator::sandbox::Sandbox;
use orchestrator::storage::backend::Storage;
use orchestrator::storage::header::{Header, Metadata};
use uuid::Uuid;

const ACCESS_TOKEN: &str = "contract-token";

struct Host {
    firecracker: PathBuf,
    kernel: PathBuf,
}

fn host() -> Option<Host> {
    let env = |name: &str| std::env::var(name).ok().map(PathBuf::from);
    let ready = nix::unistd::geteuid().is_root() && Path::new("/dev/kvm").exists() && Path::new("/dev/nbd0").exists();
    ready.then_some(())?;
    Some(Host {
        firecracker: env("E2B_FIRECRACKER")?,
        kernel: env("E2B_KERNEL")?,
    })
}

fn guest_rootfs() -> Option<PathBuf> {
    let rootfs = std::env::var("E2B_ROOTFS").ok().map(PathBuf::from);
    let in_ci = std::env::var("CI").is_ok_and(|value| value == "true");
    assert!(rootfs.is_some() || !in_ci, "E2B_ROOTFS is required in CI");
    rootfs
}

async fn overlay(storage: Storage, dir: &Path, template_id: Uuid, size: u64) -> Arc<Overlay> {
    let header = Header::new(Metadata::template(template_id, 4096, size), Vec::new()).unwrap();
    let store = Arc::new(DiffStore::new(storage, dir.join("cache")));
    let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
    Arc::new(Overlay::new(base, &dir.join("overlay")).unwrap())
}

fn namespace_exists(slot: &Slot) -> bool {
    let listed = Command::new("ip").args(["netns", "list"]).output().unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|line| line.split_whitespace().next() == Some(slot.namespace().as_str()))
}

fn any_nbd_connected() -> bool {
    std::fs::read_dir("/sys/block")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("nbd"))
        .any(|entry| entry.path().join("pid").exists())
}

fn cgroup_exists(id: &str) -> bool {
    Path::new("/sys/fs/cgroup/e2b").join(id).exists()
}

async fn envs(client: &reqwest::Client, base: &str, token: Option<&str>) -> reqwest::Response {
    token
        .into_iter()
        .fold(client.get(format!("{base}/envs")), |request, token| {
            request.header("X-Access-Token", token)
        })
        .send()
        .await
        .unwrap()
}

async fn sandbox_id_from_mmds(client: &reqwest::Client, base: &str) -> Option<String> {
    for _ in 0..100 {
        let vars: serde_json::Value = envs(client, base, Some(ACCESS_TOKEN)).await.json().await.unwrap();
        if let Some(id) = vars["E2B_SANDBOX_ID"].as_str() {
            return Some(id.to_owned());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_cold_boot_leaves_no_slot_cgroup_or_device() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, /dev/kvm, nbd and E2B_FIRECRACKER / E2B_KERNEL");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template_id = Uuid::new_v4();
    let size = 16 << 20;
    storage
        .put(
            &Artifact::Rootfs.object(template_id),
            Bytes::from(vec![0; size as usize]),
        )
        .await
        .unwrap();
    let slot = Slot::new(4011).unwrap();
    let _ = slot.destroy().await;

    let error = Sandbox::builder()
        .id("sbx-fail")
        .template_id(&template_id.to_string())
        .rootfs(overlay(storage, dir.path(), template_id, size).await)
        .slot(slot)
        .firecracker(&host.firecracker)
        .kernel("/nonexistent/vmlinux")
        .work_dir(dir.path().join("sbx-fail"))
        .cold_boot()
        .await
        .err()
        .unwrap();

    assert!(error.contains("PUT /boot-source"), "{error}");
    assert!(!namespace_exists(&slot), "slot namespace left behind");
    assert!(!cgroup_exists("sbx-fail"), "cgroup left behind");
    assert!(!any_nbd_connected(), "nbd device left connected");
}

async fn cleaned_up_within(within: Duration, slot: &Slot, id: &str) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if !namespace_exists(slot) && !cgroup_exists(id) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_cold_boot_tears_everything_down() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, /dev/kvm, nbd and E2B_FIRECRACKER / E2B_KERNEL");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template_id = Uuid::new_v4();
    let size = 16 << 20;
    storage
        .put(
            &Artifact::Rootfs.object(template_id),
            Bytes::from(vec![0; size as usize]),
        )
        .await
        .unwrap();
    let slot = Slot::new(4013).unwrap();
    let _ = slot.destroy().await;

    let boot = Sandbox::builder()
        .id("sbx-cancel")
        .template_id(&template_id.to_string())
        .rootfs(overlay(storage, dir.path(), template_id, size).await)
        .slot(slot)
        .firecracker(&host.firecracker)
        .kernel(&host.kernel)
        .work_dir(dir.path().join("sbx-cancel"))
        .envd_timeout(Duration::from_secs(120))
        .cold_boot();
    let cancelled = tokio::time::timeout(Duration::from_secs(3), boot).await;

    assert!(
        cancelled.is_err(),
        "cold boot of an empty disk must still be waiting for envd"
    );
    assert!(
        cleaned_up_within(Duration::from_secs(15), &slot, "sbx-cancel").await,
        "a cancelled cold boot left its slot or cgroup behind"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_boot_initialises_envd_with_token_and_mmds() {
    let rootfs = guest_rootfs();
    let (Some(host), Some(rootfs)) = (host(), rootfs) else {
        eprintln!("skipping: needs root, /dev/kvm, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template_id = Uuid::new_v4();
    storage
        .put_file(&Artifact::Rootfs.object(template_id), &rootfs)
        .await
        .unwrap();
    let size = std::fs::metadata(&rootfs).unwrap().len();
    let slot = Slot::new(4012).unwrap();
    let _ = slot.destroy().await;

    let sandbox = Sandbox::builder()
        .id("sbx-contract")
        .template_id(&template_id.to_string())
        .rootfs(overlay(storage, dir.path(), template_id, size).await)
        .slot(slot)
        .firecracker(&host.firecracker)
        .kernel(&host.kernel)
        .work_dir(dir.path().join("sbx-contract"))
        .envd(EnvdInit {
            access_token: Some(ACCESS_TOKEN.to_owned()),
            default_user: Some("user".to_owned()),
            default_workdir: Some("/home/user".to_owned()),
            ..EnvdInit::default()
        })
        .cold_boot()
        .await
        .unwrap();

    let base = format!("http://{}", sandbox.envd_address());
    let client = reqwest::Client::new();
    let health = client.get(format!("{base}/health")).send().await.unwrap().status();
    let without_token = envs(&client, &base, None).await.status();
    let with_token = envs(&client, &base, Some(ACCESS_TOKEN)).await.status();
    let sandbox_id = sandbox_id_from_mmds(&client, &base).await;

    sandbox.stop().await.unwrap();

    assert_eq!(health, 204);
    assert_eq!(without_token, 401);
    assert_eq!(with_token, 200);
    assert_eq!(sandbox_id.as_deref(), Some("sbx-contract"));
    assert!(!namespace_exists(&slot), "slot namespace left behind");
    assert!(!cgroup_exists("sbx-contract"), "cgroup left behind");
}
