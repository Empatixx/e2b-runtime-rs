#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use orchestrator::block::build_file::{Artifact, BuildFile, DiffStore};
use orchestrator::block::overlay::Overlay;
use orchestrator::envd::{ENVD_PORT, EnvdInit};
use orchestrator::network::slot::Slot;
use orchestrator::proto::sandbox_service_client::SandboxServiceClient;
use orchestrator::proto::sandbox_service_server::SandboxServiceServer;
use orchestrator::proto::{
    SandboxConfig, SandboxCreateRequest, SandboxDeleteRequest, SandboxPauseRequest, SandboxUpdateRequest,
};
use orchestrator::sandbox::Sandbox;
use orchestrator::server::config::{NodeConfig, StorageConfig};
use orchestrator::server::node::Node;
use orchestrator::server::service::SandboxServer;
use orchestrator::storage::backend::Storage;
use orchestrator::storage::header::{Header, Metadata};
use tonic::Code;
use tonic::transport::Channel;
use uuid::Uuid;

const TOKEN: &str = "grpc-token";
const LIFECYCLE_POOL: Pool = Pool {
    template_slot: 4099,
    first: 4100,
    last: 4103,
};
const EXPIRY_POOL: Pool = Pool {
    template_slot: 4098,
    first: 4104,
    last: 4105,
};

#[derive(Clone, Copy)]
struct Pool {
    template_slot: u32,
    first: u32,
    last: u32,
}

impl Pool {
    fn indices(&self) -> std::ops::RangeInclusive<u32> {
        self.first..=self.last
    }
}

struct Host {
    firecracker: PathBuf,
    kernel: PathBuf,
    rootfs: PathBuf,
}

struct Cluster {
    dir: tempfile::TempDir,
    client: SandboxServiceClient<Channel>,
    template_build: Uuid,
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
        "grpc contract needs root, KVM, nbd and E2B_* assets in CI"
    );
    host
}

async fn template_build(host: &Host, dir: &Path, storage: &Storage, slot: u32) -> Uuid {
    let template_id = Uuid::new_v4();
    storage
        .put_file(&Artifact::Rootfs.object(template_id), &host.rootfs)
        .await
        .unwrap();
    let size = std::fs::metadata(&host.rootfs).unwrap().len();
    let header = Header::new(Metadata::template(template_id, 4096, size), Vec::new()).unwrap();
    let store = Arc::new(DiffStore::new(storage.clone(), dir.join("template-cache")));
    let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
    let overlay = Arc::new(Overlay::new(base, &dir.join("template.overlay")).unwrap());
    let slot = Slot::new(slot).unwrap();
    let _ = slot.destroy().await;
    let sandbox = Sandbox::builder()
        .id("grpc-template")
        .template_id(&template_id.to_string())
        .rootfs(overlay)
        .slot(slot)
        .firecracker(&host.firecracker)
        .kernel(&host.kernel)
        .work_dir(dir.join("template"))
        .envd(EnvdInit {
            access_token: Some(TOKEN.to_owned()),
            default_user: Some("user".to_owned()),
            default_workdir: Some("/home/user".to_owned()),
            ..EnvdInit::default()
        })
        .cold_boot()
        .await
        .unwrap();
    let build_id = Uuid::new_v4();
    sandbox.pause(storage, build_id).await.unwrap();
    build_id
}

async fn cluster(host: &Host, pool: Pool, local_expiry: bool) -> Cluster {
    let dir = tempfile::tempdir().unwrap();
    let store_root = dir.path().join("store");
    let storage = Storage::local(&store_root).unwrap();
    let template_build = template_build(host, dir.path(), &storage, pool.template_slot).await;
    pool.indices().for_each(|index| {
        drop(
            Command::new("ip")
                .args(["netns", "delete", &format!("ns-{index}")])
                .status(),
        )
    });
    let config = NodeConfig {
        grpc_port: 0,
        node_id: "grpc-node".to_owned(),
        firecracker: host.firecracker.clone(),
        kernel: host.kernel.clone(),
        storage: StorageConfig::Local { root: store_root },
        work_dir: dir.path().join("sandboxes"),
        cache_dir: dir.path().join("cache"),
        slots: pool.indices(),
        local_expiry,
    };
    let node = Arc::new(Node::new(&config).unwrap());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(SandboxServiceServer::new(SandboxServer::new(node)))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    let client = SandboxServiceClient::connect(format!("http://{address}"))
        .await
        .unwrap();
    Cluster {
        dir,
        client,
        template_build,
    }
}

fn create_request(id: &str, build: Uuid, lifetime: Duration) -> SandboxCreateRequest {
    let now = SystemTime::now();
    SandboxCreateRequest {
        sandbox: Some(SandboxConfig {
            sandbox_id: id.to_owned(),
            template_id: "grpc-template".to_owned(),
            build_id: build.to_string(),
            envd_access_token: Some(TOKEN.to_owned()),
            team_id: "team".to_owned(),
            vcpu: 2,
            ram_mb: 1024,
            ..SandboxConfig::default()
        }),
        start_time: Some(now.into()),
        end_time: Some((now + lifetime).into()),
        filesystem_boot: None,
    }
}

fn envd(slot: u32) -> String {
    format!(
        "http://{}",
        SocketAddr::from((Slot::new(slot).unwrap().host_ip(), ENVD_PORT))
    )
}

async fn put_note(base: &str, text: &str) {
    let form =
        reqwest::multipart::Form::new().part("file", reqwest::multipart::Part::text(text.to_owned()).file_name("n"));
    let status = reqwest::Client::new()
        .post(format!("{base}/files?path=/home/user/grpc.txt&username=user"))
        .header("X-Access-Token", TOKEN)
        .multipart(form)
        .send()
        .await
        .unwrap()
        .status();
    assert_eq!(status, 200);
}

async fn note(base: &str) -> String {
    reqwest::Client::new()
        .get(format!("{base}/files?path=/home/user/grpc.txt&username=user"))
        .header("X-Access-Token", TOKEN)
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap()
}

fn template_size(host: &Host) -> u64 {
    std::fs::metadata(&host.rootfs).unwrap().len()
}

fn nbd_connected_with_size(bytes: u64) -> bool {
    std::fs::read_dir("/sys/block")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("nbd"))
        .filter(|entry| entry.path().join("pid").exists())
        .any(|entry| {
            std::fs::read_to_string(entry.path().join("size")).is_ok_and(|sectors| {
                sectors
                    .trim()
                    .parse::<u64>()
                    .is_ok_and(|sectors| sectors * 512 == bytes)
            })
        })
}

fn pool_namespaces_left(pool: Pool) -> Vec<String> {
    let listed = Command::new("ip").args(["netns", "list"]).output().unwrap();
    String::from_utf8_lossy(&listed.stdout)
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| {
            pool.indices()
                .map(|index| format!("ns-{index}"))
                .any(|pooled| pooled == *name)
        })
        .map(str::to_owned)
        .collect()
}

async fn eventually(within: Duration, check: impl Fn() -> bool) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    while tokio::time::Instant::now() < deadline {
        if check() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    check()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sandbox_lifecycle_over_grpc() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let pool = LIFECYCLE_POOL;
    let mut cluster = cluster(&host, pool, false).await;
    let build = cluster.template_build;

    let created = cluster
        .client
        .create(create_request("grpc-sbx", build, Duration::from_secs(600)))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(created.client_id, "grpc-node");
    let base = envd(pool.first);
    put_note(&base, "before pause").await;

    let listed = cluster.client.list(()).await.unwrap().into_inner().sandboxes;
    assert_eq!(
        listed
            .iter()
            .map(|sandbox| sandbox.sandbox_id.as_str())
            .collect::<Vec<_>>(),
        vec!["grpc-sbx"]
    );
    assert_eq!(
        (listed[0].vcpu, listed[0].ram_mb, listed[0].team_id.as_str()),
        (2, 1024, "team")
    );

    let duplicate = cluster
        .client
        .create(create_request("grpc-sbx", build, Duration::from_secs(600)))
        .await
        .unwrap_err();
    assert_eq!(duplicate.code(), Code::AlreadyExists);

    let later = SystemTime::now() + Duration::from_secs(3600);
    cluster
        .client
        .update(SandboxUpdateRequest {
            sandbox_id: "grpc-sbx".to_owned(),
            end_time: Some(later.into()),
            egress: None,
        })
        .await
        .unwrap();
    let listed = cluster.client.list(()).await.unwrap().into_inner().sandboxes;
    assert_eq!(listed[0].end_time, Some(later.into()));

    let paused_build = Uuid::new_v4();
    cluster
        .client
        .pause(SandboxPauseRequest {
            sandbox_id: "grpc-sbx".to_owned(),
            template_id: "grpc-template".to_owned(),
            build_id: paused_build.to_string(),
            filesystem_only: false,
        })
        .await
        .unwrap();
    assert!(cluster.client.list(()).await.unwrap().into_inner().sandboxes.is_empty());

    cluster
        .client
        .create(create_request("grpc-sbx", paused_build, Duration::from_secs(600)))
        .await
        .unwrap();
    let resumed_slot = pool
        .indices()
        .find(|index| Path::new(&format!("/run/netns/ns-{index}")).exists())
        .unwrap();
    assert_eq!(note(&envd(resumed_slot)).await, "before pause");

    cluster
        .client
        .delete(SandboxDeleteRequest {
            sandbox_id: "grpc-sbx".to_owned(),
            kill_reason: None,
        })
        .await
        .unwrap();
    let again = cluster
        .client
        .delete(SandboxDeleteRequest {
            sandbox_id: "grpc-sbx".to_owned(),
            kill_reason: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        (again.code(), again.message()),
        (Code::NotFound, "sandbox 'grpc-sbx' not found")
    );

    let mut racer = cluster.client.clone();
    let mut other = cluster.client.clone();
    let (first, second) = tokio::join!(
        racer.create(create_request("grpc-race", build, Duration::from_secs(600))),
        other.create(create_request("grpc-race", build, Duration::from_secs(600)))
    );
    let codes = [first.err().map(|e| e.code()), second.err().map(|e| e.code())];
    assert!(
        codes == [None, Some(Code::AlreadyExists)] || codes == [Some(Code::AlreadyExists), None],
        "{codes:?}"
    );
    cluster
        .client
        .delete(SandboxDeleteRequest {
            sandbox_id: "grpc-race".to_owned(),
            kill_reason: None,
        })
        .await
        .unwrap();

    let missing = cluster
        .client
        .create(create_request("grpc-missing", Uuid::new_v4(), Duration::from_secs(60)))
        .await
        .unwrap_err();
    assert_eq!(
        (missing.code(), missing.message()),
        (Code::FailedPrecondition, "sandbox files for 'grpc-missing' not found")
    );

    assert!(
        eventually(Duration::from_secs(20), || pool_namespaces_left(pool).is_empty()
            && !Path::new("/sys/fs/cgroup/e2b/grpc-sbx-2").exists())
        .await,
        "left behind: {:?}",
        pool_namespaces_left(pool)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_expired_sandbox_is_removed() {
    let Some(host) = host() else {
        eprintln!("skipping: needs root, KVM, nbd and E2B_FIRECRACKER / E2B_KERNEL / E2B_ROOTFS");
        return;
    };
    let cluster = cluster(&host, EXPIRY_POOL, true).await;
    let mut client = cluster.client.clone();

    client
        .create(create_request(
            "grpc-expiring",
            cluster.template_build,
            Duration::from_secs(3),
        ))
        .await
        .unwrap();

    let mut emptied = false;
    for _ in 0..100 {
        if client.list(()).await.unwrap().into_inner().sandboxes.is_empty() {
            emptied = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(emptied, "the expired sandbox is still listed");
    let released =
        || !Path::new("/sys/fs/cgroup/e2b/grpc-expiring").exists() && pool_namespaces_left(EXPIRY_POOL).is_empty();
    assert!(
        eventually(Duration::from_secs(20), released).await,
        "the expired sandbox left its cgroup or slot"
    );
}
