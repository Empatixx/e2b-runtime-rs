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
use orchestrator::sandbox::config::sandbox_builder::{
    SetFirecracker, SetId, SetKernel, SetSlot, SetTemplateId, SetWorkDir,
};
use orchestrator::sandbox::snapshot::in_place::memory::MemoryCheckpointRequest;
use orchestrator::storage::backend::{ObjectMetadata, Storage};
use orchestrator::storage::build::Build;
use orchestrator::storage::header::{Header, Metadata};
use orchestrator::template::cache::TemplateCache;
use orchestrator::template::optimize::collect_prefetch;
use uuid::Uuid;

const ACCESS_TOKEN: &str = "resume-token";
const NOTE: &str = "/home/user/note.txt";
const NOTE_TEXT: &str = "hello from before the snapshot";
const HUGE_PAGE: u64 = 2 << 20;
const GUEST_MEMORY_MIB: u64 = 1024;

struct Host {
    firecracker: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

struct Templates {
    _dir: tempfile::TempDir,
    storage: Storage,
    store: Arc<DiffStore>,
    cache: TemplateCache,
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
    let cache = TemplateCache::new(storage.clone(), dir.path().join("templates"));
    Templates {
        _dir: dir,
        storage,
        store,
        cache,
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

    fn sandbox(&self, host: &Host, id: &str, slot: Slot) -> SandboxBuilder<TestSandbox> {
        std::fs::create_dir_all(&self.work).unwrap();
        Sandbox::builder()
            .id(id)
            .template_id(self.template.metadata.build_id.to_string())
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

type TestSandbox = SetWorkDir<SetKernel<SetFirecracker<SetSlot<SetTemplateId<SetId>>>>>;

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

async fn run_in_guest(sandbox: &Sandbox, command: &str) {
    let request = serde_json::json!({ "process": { "cmd": "/bin/sh", "args": ["-c", command] } }).to_string();
    let envelope = [
        vec![0],
        (request.len() as u32).to_be_bytes().to_vec(),
        request.into_bytes(),
    ]
    .concat();
    let response = reqwest::Client::new()
        .post(format!("http://{}/process.Process/Start", sandbox.envd_address()))
        .header("content-type", "application/connect+json")
        .header("connect-protocol-version", "1")
        .header("authorization", "Basic cm9vdDo=")
        .header("X-Access-Token", ACCESS_TOKEN)
        .body(envelope)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "start {command}");
    response.bytes().await.unwrap();
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
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    write_file(&cold, NOTE, NOTE_TEXT).await;
    run_in_guest(&cold, "sync").await;
    let build_id = Uuid::new_v4();
    let build = cold.pause(&templates.storage, build_id).await.unwrap().build;
    let leftovers: Vec<PathBuf> = ["snapfile", "memfile.diff", "rootfs.diff"]
        .iter()
        .map(|name| templates.work.join("sbx-cold").join(name))
        .filter(|path| path.exists())
        .collect();
    assert!(
        leftovers.is_empty(),
        "snapshot files left in the work dir: {leftovers:?}"
    );

    let template = templates.cache.get(build_id);
    let loaded = template.build().await.unwrap();
    let uploaded = Build {
        memfile: build.memfile.finalized(),
        rootfs: build.rootfs.finalized(),
        ..build.clone()
    };
    assert_eq!(loaded, uploaded);
    let snapfile = template.snapfile().await.unwrap();
    let started = Instant::now();
    let resumed = templates
        .sandbox(&host, "sbx-resumed", resumed_slot)
        .rootfs(templates.overlay(loaded.rootfs.clone(), "resumed"))
        .memfile(templates.memfile(loaded.memfile.clone()))
        .resume(&snapfile)
        .await
        .unwrap();
    let took = started.elapsed();
    println!("resume took {} ms", took.as_millis());
    println!("resume phases: {}", resumed.phases());

    let note = read_file(&resumed, NOTE).await;
    let health = health(&resumed).await;
    let faults = resumed.faults_served();
    resumed.stop().await.unwrap();

    let disk_only = templates
        .sandbox(&host, "sbx-disk-only", fresh_slot(4036).await)
        .rootfs(templates.overlay(loaded.rootfs.clone(), "disk-only"))
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    let note_on_disk = read_file(&disk_only, NOTE).await;
    disk_only.stop().await.unwrap();

    assert_eq!(note, (200, NOTE_TEXT.to_owned()));
    assert_eq!(
        note_on_disk,
        (200, NOTE_TEXT.to_owned()),
        "the rootfs diff lost a synced write"
    );
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
    let memfile = Header::new(Metadata::template(Uuid::nil(), 4096, 1 << 20), Vec::new()).unwrap();

    let error = templates
        .sandbox(&host, "sbx-no-snapfile", slot)
        .rootfs(templates.overlay(templates.template.clone(), "no-snapfile"))
        .memfile(templates.memfile(memfile))
        .resume(&templates.work.join("missing.snapfile"))
        .await
        .err()
        .unwrap();

    assert!(error.contains("snapshot"), "{error}");
    assert!(!namespace_exists(&slot));
}

impl Templates {
    async fn resume(&self, host: &Host, id: &str, slot: u32, build: &Build) -> Sandbox {
        self.sandbox(host, id, fresh_slot(slot).await)
            .rootfs(self.overlay(build.rootfs.clone(), id))
            .memfile(self.memfile(build.memfile.clone()))
            .resume(&self.cache.get(build.build_id).snapfile().await.unwrap())
            .await
            .unwrap()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pause_and_resume_twice_keeps_files_memory_and_processes() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let templates = templates(&host).await;

    let first = templates
        .sandbox(&host, "sbx-gen0", fresh_slot(4033).await)
        .rootfs(templates.overlay(templates.template.clone(), "gen0"))
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    write_file(&first, "/home/user/a.txt", "disk a").await;
    write_file(&first, "/dev/shm/e2b/a", "memory a").await;
    run_in_guest(
        &first,
        "nohup sleep 600 >/dev/null 2>&1 & echo $! > /dev/shm/sleep.pid; sync",
    )
    .await;
    let paused = first.pause(&templates.storage, Uuid::new_v4()).await.unwrap();

    let second = templates.resume(&host, "sbx-gen1", 4034, &paused.build).await;
    assert_eq!(read_file(&second, "/dev/shm/e2b/a").await, (200, "memory a".to_owned()));
    write_file(&second, "/dev/shm/e2b/a", "memory a2").await;
    write_file(&second, "/home/user/b.txt", "disk b").await;
    write_file(&second, "/dev/shm/e2b/b", "memory b").await;
    run_in_guest(&second, "sync").await;
    let pause_started = Instant::now();
    let paused_again = second.pause(&templates.storage, Uuid::new_v4()).await.unwrap();
    println!("pause took {} ms", pause_started.elapsed().as_millis());
    println!(
        "pause 1: memfile {} of {} bytes, rootfs {} bytes; pause 2: memfile {} bytes (tracked writes: {}), rootfs {} bytes",
        paused.memfile_bytes,
        paused.memory_bytes,
        paused.rootfs_bytes,
        paused_again.memfile_bytes,
        paused_again.tracked_writes,
        paused_again.rootfs_bytes
    );

    let resume_started = Instant::now();
    let third = templates.resume(&host, "sbx-gen2", 4035, &paused_again.build).await;
    println!("second resume took {} ms", resume_started.elapsed().as_millis());
    println!("second resume phases: {}", third.phases());
    run_in_guest(
        &third,
        "kill -0 $(cat /dev/shm/sleep.pid) && echo alive > /dev/shm/alive",
    )
    .await;
    let files = [
        read_file(&third, "/home/user/a.txt").await,
        read_file(&third, "/home/user/b.txt").await,
        read_file(&third, "/dev/shm/e2b/a").await,
        read_file(&third, "/dev/shm/e2b/b").await,
        read_file(&third, "/dev/shm/alive").await,
    ];
    third.stop().await.unwrap();

    let disk_only = templates
        .sandbox(&host, "sbx-gen2-disk", fresh_slot(4037).await)
        .rootfs(templates.overlay(paused_again.build.rootfs.clone(), "sbx-gen2-disk"))
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    let disk_files = [
        read_file(&disk_only, "/home/user/a.txt").await,
        read_file(&disk_only, "/home/user/b.txt").await,
    ];
    disk_only.stop().await.unwrap();

    let expected = ["disk a", "disk b", "memory a2", "memory b", "alive\n"].map(|text| (200, text.to_owned()));
    assert_eq!(files, expected);
    assert_eq!(disk_files, ["disk a", "disk b"].map(|text| (200, text.to_owned())));
    assert!(!paused.tracked_writes, "a cold-booted sandbox has no write tracking");
    assert!(
        paused_again.tracked_writes,
        "the resumed sandbox did not use write-protect tracking"
    );
    assert!(
        paused_again.memfile_bytes < paused_again.memory_bytes / 2,
        "second pause exported {} of {} memory bytes",
        paused_again.memfile_bytes,
        paused_again.memory_bytes
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_optimized_trace_prefetches_memory_on_resume() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let templates = templates(&host).await;
    let cold = templates
        .sandbox(&host, "sbx-opt-cold", fresh_slot(4040).await)
        .rootfs(templates.overlay(templates.template.clone(), "opt-cold"))
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    let build = cold.pause(&templates.storage, Uuid::new_v4()).await.unwrap().build;

    let templates_ref = &templates;
    let host_ref = &host;
    let build_ref = &build;
    let mapping = collect_prefetch(|run| async move {
        let sandbox = templates_ref
            .resume(host_ref, &format!("sbx-opt-{run}"), 4041 + run as u32, build_ref)
            .await;
        let trace = sandbox.memory_trace().ok_or("no memory server".to_owned());
        sandbox.stop().await?;
        trace
    })
    .await
    .unwrap()
    .expect("the two runs share no pages");

    let plain_started = Instant::now();
    let plain = templates.resume(&host, "sbx-opt-plain", 4043, &build).await;
    let plain_took = plain_started.elapsed();
    println!(
        "resume without prefetch took {} ms: {}",
        plain_took.as_millis(),
        plain.phases()
    );
    plain.stop().await.unwrap();

    let snapfile = templates.cache.get(build.build_id).snapfile().await.unwrap();
    let prefetched_started = Instant::now();
    let prefetched = templates
        .sandbox(&host, "sbx-opt-prefetched", fresh_slot(4044).await)
        .rootfs(templates.overlay(build.rootfs.clone(), "opt-prefetched"))
        .memfile(templates.memfile(build.memfile.clone()))
        .prefetch(mapping.clone())
        .resume(&snapfile)
        .await
        .unwrap();
    let prefetched_took = prefetched_started.elapsed();
    println!(
        "resume with a {}-page prefetch took {} ms: {}",
        mapping.indices.len(),
        prefetched_took.as_millis(),
        prefetched.phases()
    );
    let installed = prefetched.prefetched();
    let health = health(&prefetched).await;
    prefetched.stop().await.unwrap();

    assert!(!mapping.indices.is_empty());
    assert!(installed > 0, "no page was prefetched");
    assert_eq!(health, 204);
}

fn free_huge_pages() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .unwrap_or_default()
        .lines()
        .find_map(|line| line.strip_prefix("HugePages_Free:"))
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

fn huge_pages_ready() -> bool {
    let in_ci = std::env::var("CI").is_ok_and(|value| value == "true");
    let ready = free_huge_pages() * HUGE_PAGE >= GUEST_MEMORY_MIB << 20;
    assert!(
        ready || !in_ci,
        "huge page resume needs {GUEST_MEMORY_MIB} MiB of free 2 MiB pages in CI"
    );
    ready
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_huge_page_snapshot_resumes_with_2_mib_faults() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    if !huge_pages_ready() {
        eprintln!("skipping: needs {GUEST_MEMORY_MIB} MiB of free 2 MiB huge pages");
        return;
    }
    let templates = templates(&host).await;
    let cold = templates
        .sandbox(&host, "sbx-huge-cold", fresh_slot(4050).await)
        .rootfs(templates.overlay(templates.template.clone(), "huge-cold"))
        .huge_pages(true)
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    write_file(&cold, "/dev/shm/e2b/huge", "memory in huge pages").await;
    let build = cold.pause(&templates.storage, Uuid::new_v4()).await.unwrap().build;

    let started = Instant::now();
    let resumed = templates.resume(&host, "sbx-huge-resumed", 4051, &build).await;
    let took = started.elapsed();
    println!("huge page resume took {} ms: {}", took.as_millis(), resumed.phases());
    let memory = read_file(&resumed, "/dev/shm/e2b/huge").await;
    let health = health(&resumed).await;
    let faults = resumed.faults_served();
    println!("huge page resume served {faults} faults");
    resumed.stop().await.unwrap();

    assert_eq!(build.memfile.metadata.block_size, HUGE_PAGE);
    assert_eq!(memory, (200, "memory in huge pages".to_owned()));
    assert_eq!(health, 204);
    assert!(faults > 0, "no page was served over userfaultfd");
    assert!(
        faults <= GUEST_MEMORY_MIB / 2,
        "{faults} faults for {GUEST_MEMORY_MIB} MiB of 2 MiB pages"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_memory_checkpoint_in_place_keeps_the_sandbox_running() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let templates = templates(&host).await;
    let cold = templates
        .sandbox(&host, "sbx-wp-cold", fresh_slot(4052).await)
        .rootfs(templates.overlay(templates.template.clone(), "wp-cold"))
        .kernel_logs(true)
        .cold_boot()
        .await
        .unwrap();
    let paused = cold.pause(&templates.storage, Uuid::new_v4()).await.unwrap().build;
    let snapfile = templates.cache.get(paused.build_id).snapfile().await.unwrap();
    let running = templates
        .sandbox(&host, "sbx-wp-running", fresh_slot(4053).await)
        .rootfs(templates.overlay(paused.rootfs.clone(), "wp-running"))
        .memfile(templates.memfile(paused.memfile.clone()))
        .use_sync_wp(true)
        .resume(&snapfile)
        .await
        .unwrap();
    write_file(&running, "/dev/shm/e2b/state", "before checkpoint").await;
    write_file(&running, "/home/user/disk.txt", "disk before checkpoint").await;
    run_in_guest(
        &running,
        "nohup sleep 600 >/dev/null 2>&1 & echo $! > /dev/shm/sleep.pid; sync",
    )
    .await;

    let checkpoint_build = Uuid::new_v4();
    let started = Instant::now();
    let checkpoint = running
        .checkpoint_memory_in_place(&MemoryCheckpointRequest {
            dir: templates.work.join("wp-checkpoint"),
            build_id: checkpoint_build,
            team_id: "test-team".to_owned(),
            balloon_mode: String::new(),
            defer_export: false,
            tracker_dirty: false,
            defer_rootfs: true,
        })
        .await
        .map_err(|failure| failure.message)
        .unwrap();
    println!("in-place memory checkpoint took {} ms", started.elapsed().as_millis());
    checkpoint
        .snapshot
        .upload(&templates.storage, &ObjectMetadata::default())
        .await
        .unwrap();
    write_file(&running, "/dev/shm/e2b/state", "after checkpoint").await;
    write_file(&running, "/home/user/after.txt", "disk after checkpoint").await;
    run_in_guest(&running, "sync").await;
    running.wait_for_rootfs_seal().await.unwrap();
    let still_running = (health(&running).await, read_file(&running, "/dev/shm/e2b/state").await);
    let disk_after_fold = [
        read_file(&running, "/home/user/disk.txt").await,
        read_file(&running, "/home/user/after.txt").await,
    ];
    running.stop().await.unwrap();

    let restored = templates
        .resume(&host, "sbx-wp-restored", 4054, &checkpoint.snapshot.snapshot.build)
        .await;
    run_in_guest(
        &restored,
        "kill -0 $(cat /dev/shm/sleep.pid) && echo alive > /dev/shm/alive",
    )
    .await;
    let restored_files = [
        read_file(&restored, "/dev/shm/e2b/state").await,
        read_file(&restored, "/home/user/disk.txt").await,
        read_file(&restored, "/dev/shm/alive").await,
    ];
    restored.stop().await.unwrap();

    let exported = &checkpoint.snapshot.snapshot;
    println!(
        "checkpoint: memfile {} of {} bytes (tracked writes: {}), rootfs {} bytes",
        exported.memfile_bytes, exported.memory_bytes, exported.tracked_writes, exported.rootfs_bytes
    );
    assert_eq!(still_running, (204, (200, "after checkpoint".to_owned())));
    assert_eq!(
        disk_after_fold,
        ["disk before checkpoint", "disk after checkpoint"].map(|text| (200, text.to_owned()))
    );
    assert_eq!(
        restored_files,
        ["before checkpoint", "disk before checkpoint", "alive\n"].map(|text| (200, text.to_owned()))
    );
    assert!(!checkpoint.deferred);
    assert!(
        exported.tracked_writes,
        "the checkpoint did not use write-protect tracking"
    );
    assert!(
        exported.memfile_bytes < exported.memory_bytes / 2,
        "checkpoint exported {} of {} memory bytes",
        exported.memfile_bytes,
        exported.memory_bytes
    );
}
