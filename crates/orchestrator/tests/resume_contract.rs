#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
use orchestrator::block::overlay::Overlay;
use orchestrator::envd::EnvdInit;
use orchestrator::network::slot::Slot;
use orchestrator::sandbox::Sandbox;
use orchestrator::sandbox::config::SandboxBuilder;
use orchestrator::storage::backend::Storage;
use orchestrator::storage::build::Build;
use orchestrator::storage::header::{Header, Metadata};
use uuid::Uuid;

const ACCESS_TOKEN: &str = "resume-token";
const NOTE: &str = "/home/user/note.txt";
const NOTE_TEXT: &str = "hello from before the snapshot";

struct Host {
    firecracker: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

struct Templates {
    _dir: tempfile::TempDir,
    storage: Storage,
    store: Arc<DiffStore>,
    template: Header,
    work: PathBuf,
}

fn host() -> Option<Host> {
    let env = |name: &str| std::env::var(name).ok().map(PathBuf::from);
    let in_ci = std::env::var("CI").is_ok_and(|value| value == "true");
    let ready = nix::unistd::geteuid().is_root() && Path::new("/dev/kvm").exists() && Path::new("/dev/nbd0").exists();
    let host = ready.then_some(()).and_then(|()| {
        Some(Host {
            firecracker: env("E2B_FIRECRACKER")?,
            kernel: env("E2B_KERNEL")?,
            rootfs: env("E2B_ROOTFS")?,
        })
    });
    assert!(
        host.is_some() || !in_ci,
        "resume contract needs root, KVM, nbd and E2B_* assets in CI"
    );
    host
}

async fn templates(host: &Host) -> Templates {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let template_id = Uuid::new_v4();
    storage
        .put_file(&Artifact::Rootfs.object(template_id), &host.rootfs)
        .await
        .unwrap();
    let size = std::fs::metadata(&host.rootfs).unwrap().len();
    let template = Header::new(Metadata::template(template_id, 4096, size), Vec::new()).unwrap();
    let store = Arc::new(DiffStore::new(storage.clone(), dir.path().join("cache")));
    let work = dir.path().join("work");
    Templates {
        _dir: dir,
        storage,
        store,
        template,
        work,
    }
}

impl Templates {
    fn overlay(&self, header: Header, name: &str) -> Arc<Overlay> {
        let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, self.store.clone()));
        Arc::new(Overlay::new(base, &self.work.join(format!("{name}.overlay"))).unwrap())
    }

    fn memfile(&self, header: Header) -> Arc<BuildFile> {
        Arc::new(BuildFile::new(header, Artifact::Memfile, self.store.clone()))
    }

    fn sandbox(&self, host: &Host, id: &str, slot: Slot) -> SandboxBuilder {
        std::fs::create_dir_all(&self.work).unwrap();
        Sandbox::builder()
            .id(id)
            .template_id(&self.template.metadata.build_id.to_string())
            .slot(slot)
            .firecracker(&host.firecracker)
            .kernel(&host.kernel)
            .work_dir(self.work.join(id))
            .envd(EnvdInit {
                access_token: Some(ACCESS_TOKEN.to_owned()),
                default_user: Some("user".to_owned()),
                default_workdir: Some("/home/user".to_owned()),
                ..EnvdInit::default()
            })
    }
}

async fn fresh_slot(index: u32) -> Slot {
    let slot = Slot::new(index).unwrap();
    let _ = slot.destroy().await;
    slot
}

fn namespace_exists(slot: &Slot) -> bool {
    let listed = Command::new("ip").args(["netns", "list"]).output().unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|line| line.split_whitespace().next() == Some(slot.namespace().as_str()))
}

async fn write_file(sandbox: &Sandbox, path: &str, text: &str) {
    let form =
        reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::text(text.to_owned()).file_name("f"));
    let status = reqwest::Client::new()
        .post(format!(
            "http://{}/files?path={path}&username=user",
            sandbox.envd_address()
        ))
        .header("X-Access-Token", ACCESS_TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200, "upload {path}");
}

async fn read_file(sandbox: &Sandbox, path: &str) -> (u16, String) {
    let response = reqwest::Client::new()
        .get(format!(
            "http://{}/files?path={path}&username=user",
            sandbox.envd_address()
        ))
        .header("X-Access-Token", ACCESS_TOKEN)
        .send()
        .await
        .unwrap();
    (response.status().as_u16(), response.text().await.unwrap())
}

async fn health(sandbox: &Sandbox) -> u16 {
    reqwest::get(format!("http://{}/health", sandbox.envd_address()))
        .await
        .unwrap()
        .status()
        .as_u16()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resumed_sandbox_keeps_files_and_answers_quickly() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let templates = templates(&host).await;
    let cold_slot = fresh_slot(4030).await;
    let resumed_slot = fresh_slot(4031).await;

    let cold = templates
        .sandbox(&host, "sbx-cold", cold_slot)
        .rootfs(templates.overlay(templates.template.clone(), "cold"))
        .cold_boot()
        .await
        .unwrap();
    write_file(&cold, NOTE, NOTE_TEXT).await;
    let build_id = Uuid::new_v4();
    let build = cold.snapshot(&templates.storage, build_id).await.unwrap();
    cold.stop().await.unwrap();

    let loaded = Build::load(&templates.storage, build_id).await.unwrap();
    assert_eq!(loaded, build);
    let started = Instant::now();
    let resumed = templates
        .sandbox(&host, "sbx-resumed", resumed_slot)
        .rootfs(templates.overlay(loaded.rootfs.clone(), "resumed"))
        .memfile(templates.memfile(loaded.memfile.clone()))
        .resume(&loaded, &templates.storage)
        .await
        .unwrap();
    let took = started.elapsed();
    println!("resume took {} ms", took.as_millis());

    let note = read_file(&resumed, NOTE).await;
    let health = health(&resumed).await;
    let faults = resumed.faults_served();
    resumed.stop().await.unwrap();

    assert_eq!(note, (200, NOTE_TEXT.to_owned()));
    assert_eq!(health, 204);
    assert!(faults > 0, "no page was served over userfaultfd");
    assert!(took < Duration::from_secs(10), "resume took {took:?}");
    assert!(!namespace_exists(&cold_slot) && !namespace_exists(&resumed_slot));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_without_a_snapfile_fails_cleanly() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let templates = templates(&host).await;
    let slot = fresh_slot(4032).await;
    let build_id = Uuid::new_v4();
    let memfile = Header::new(Metadata::template(Uuid::nil(), 4096, 1 << 20), Vec::new()).unwrap();
    let build = Build {
        build_id,
        memfile: memfile.clone(),
        rootfs: templates.template.clone(),
    };

    let error = templates
        .sandbox(&host, "sbx-no-snapfile", slot)
        .rootfs(templates.overlay(templates.template.clone(), "no-snapfile"))
        .memfile(templates.memfile(memfile))
        .resume(&build, &templates.storage)
        .await
        .err()
        .unwrap();

    assert!(error.contains("snapfile"), "{error}");
    assert!(!namespace_exists(&slot));
}
