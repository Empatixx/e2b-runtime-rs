use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use futures::future::BoxFuture;
use orchestrator::block::build_file::{Artifact, DiffStore};
use orchestrator::featureflags::FeatureFlags;
use orchestrator::peer::registry::{PeerRegistry, redis_key};
use orchestrator::peer::server::ChunkServer;
use orchestrator::peer::server::resolve::PeerCache;
use orchestrator::peer::switch::PeerSwitch;
use orchestrator::proto::chunk_service_server::ChunkServiceServer;
use orchestrator::routing::redis::{Redis, RedisSettings};
use orchestrator::storage::backend::Storage;
use orchestrator::storage::build::Build;
use orchestrator::storage::header::{Header, Metadata};
use orchestrator::storage::paths::BuildPaths;
use orchestrator::template::cache::TemplateCache;
use orchestrator::template::metadata::{CURRENT_VERSION, TemplateMetadata, TemplateVersions};
use parking_lot::Mutex;
use redissun::{Client, Object, StringCodec};
use tonic::transport::Server;
use uuid::Uuid;

type Entries = Arc<Mutex<HashMap<String, String>>>;

struct SharedRegistry {
    entries: Entries,
    address: String,
}

impl PeerRegistry for SharedRegistry {
    fn register(&self, build_id: &str, _: Duration) -> BoxFuture<'static, Result<(), String>> {
        self.entries.lock().insert(build_id.to_owned(), self.address.clone());
        async { Ok(()) }.boxed()
    }

    fn lookup(&self, build_id: &str) -> BoxFuture<'static, Result<Option<String>, String>> {
        std::future::ready(Ok(self.entries.lock().get(build_id).cloned())).boxed()
    }

    fn unregister(&self, build_id: &str) -> BoxFuture<'static, Result<(), String>> {
        self.entries.lock().remove(build_id);
        async { Ok(()) }.boxed()
    }
}

fn peer_switch(entries: &Entries, address: &str) -> Arc<PeerSwitch> {
    let flags = FeatureFlags::builder()
        .override_flag("peer-to-peer-chunk-transfer", true)
        .offline();
    let switch = PeerSwitch::new(flags);
    switch.install(
        Arc::new(SharedRegistry {
            entries: entries.clone(),
            address: address.to_owned(),
        }),
        address.to_owned(),
    );
    switch
}

fn header(build_id: Uuid, size: u64) -> Header {
    Header::new(Metadata::template(build_id, 4096, size), Vec::new()).unwrap()
}

struct Snapshot {
    build: Build,
    metadata: TemplateMetadata,
    snapfile: Vec<u8>,
    memfile: Vec<u8>,
    rootfs: Vec<u8>,
}

impl Snapshot {
    fn new() -> Self {
        let build_id = Uuid::new_v4();
        Self {
            build: Build {
                build_id,
                memfile: header(build_id, 8192),
                rootfs: header(build_id, 4096),
            },
            metadata: TemplateMetadata {
                version: CURRENT_VERSION,
                template: TemplateVersions {
                    build_id: build_id.to_string(),
                    kernel_version: "vmlinux-6.1".to_owned(),
                    firecracker_version: "v1.12".to_owned(),
                },
                ..TemplateMetadata::default()
            },
            snapfile: b"firecracker snapshot state".to_vec(),
            memfile: (0..8192u32).map(|byte| (byte % 251) as u8).collect(),
            rootfs: vec![7; 4096],
        }
    }

    fn upload_to(&self, bucket: &std::path::Path) {
        let paths = BuildPaths::new(self.build.build_id);
        let files = [
            (paths.snapfile(), self.snapfile.clone()),
            (paths.memfile(), self.memfile.clone()),
            (paths.rootfs(), self.rootfs.clone()),
            (paths.metadata(), self.metadata.to_json()),
            (paths.memfile_header(), self.build.memfile.serialize().unwrap()),
            (paths.rootfs_header(), self.build.rootfs.serialize().unwrap()),
        ];
        for (object, content) in files {
            let path = bucket.join(object);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
    }
}

struct Source {
    switch: Arc<PeerSwitch>,
    address: SocketAddr,
}

async fn serve_snapshot(
    dir: &std::path::Path,
    bucket: &std::path::Path,
    entries: &Entries,
    snapshot: &Snapshot,
) -> Source {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let storage = Storage::local(bucket).unwrap();
    let templates = Arc::new(TemplateCache::new(storage.clone(), dir.join("templates")));
    let diffs = Arc::new(DiffStore::new(storage, dir.join("diffs")));
    let build_id = snapshot.build.build_id;
    std::fs::create_dir_all(dir.join("local")).unwrap();
    let write = |name: &str, content: &[u8]| {
        let path = dir.join("local").join(name);
        std::fs::write(&path, content).unwrap();
        path
    };
    let snapfile = write("snapfile", &snapshot.snapfile);
    templates.add_snapshot(
        snapshot.build.clone(),
        snapfile,
        snapshot.metadata.clone(),
        dir.join("instance"),
    );
    diffs
        .add_local(build_id, Artifact::Memfile, &write("memfile", &snapshot.memfile))
        .unwrap();
    diffs
        .add_local(build_id, Artifact::Rootfs, &write("rootfs", &snapshot.rootfs))
        .unwrap();
    let switch = peer_switch(entries, &address.to_string());
    switch.register(build_id).await;
    let server = ChunkServer {
        cache: PeerCache { templates, diffs },
        peers: switch.clone(),
    };
    tokio::spawn(
        Server::builder()
            .add_service(ChunkServiceServer::new(server))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
    );
    Source { switch, address }
}

#[tokio::test]
async fn a_second_node_loads_a_snapshot_from_the_first_before_it_is_uploaded_then_switches_to_storage() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    std::fs::create_dir_all(&bucket).unwrap();
    let entries = Entries::default();
    let snapshot = Snapshot::new();
    let build_id = snapshot.build.build_id;
    let source = serve_snapshot(&dir.path().join("a"), &bucket, &entries, &snapshot).await;
    let switch_b = peer_switch(&entries, "10.9.9.9:5008");
    let storage_b = Storage::local(&bucket).unwrap().with_peers(switch_b.clone());
    let templates_b = TemplateCache::new(storage_b.clone(), dir.path().join("b-templates"));
    let diffs_b = DiffStore::new(storage_b.clone(), dir.path().join("b-diffs"));

    let template = templates_b.get(build_id);
    let snapfile = template.snapfile().await.unwrap();
    let build = template.build().await.unwrap();
    let metadata = template.metadata().await.unwrap();
    let memfile = diffs_b.chunker(build_id, Artifact::Memfile).await.unwrap();
    let mut page = vec![0; 4096];
    memfile.read_at(4096, &mut page).await.unwrap();

    assert_eq!(std::fs::read(snapfile).unwrap(), snapshot.snapfile);
    assert_eq!(build.memfile.mappings, snapshot.build.memfile.mappings);
    assert_eq!(build.rootfs.mappings, snapshot.build.rootfs.mappings);
    assert_eq!(build.memfile.metadata.size, snapshot.build.memfile.metadata.size);
    assert!(build.memfile.incomplete_pending_upload && build.rootfs.incomplete_pending_upload);
    assert_eq!(metadata, snapshot.metadata);
    assert_eq!(memfile.size(), 8192);
    assert_eq!(page, snapshot.memfile[4096..]);
    assert!(!bucket.join(build_id.to_string()).exists(), "nothing was uploaded yet");
    assert!(switch_b.is_active(&build_id.to_string()));

    snapshot.upload_to(&bucket);
    source.switch.finish(build_id, true).await;
    entries.lock().insert(build_id.to_string(), source.address.to_string());
    let rootfs = diffs_b.chunker(build_id, Artifact::Rootfs).await.unwrap();
    let mut block = vec![0; 4096];
    rootfs.read_at(0, &mut block).await.unwrap();

    assert_eq!(block, snapshot.rootfs);
    assert!(
        !switch_b.is_active(&build_id.to_string()),
        "the peer said to use storage"
    );
}

#[tokio::test]
async fn a_node_falls_back_to_storage_when_the_peer_no_longer_has_the_build() {
    let dir = tempfile::tempdir().unwrap();
    let bucket = dir.path().join("bucket");
    std::fs::create_dir_all(&bucket).unwrap();
    let entries = Entries::default();
    let snapshot = Snapshot::new();
    let build_id = snapshot.build.build_id;
    snapshot.upload_to(&bucket);
    let source = serve_snapshot(&dir.path().join("a"), &bucket, &entries, &snapshot).await;
    entries.lock().insert(build_id.to_string(), source.address.to_string());
    let other_build = Uuid::new_v4();
    entries
        .lock()
        .insert(other_build.to_string(), source.address.to_string());
    let switch_b = peer_switch(&entries, "10.9.9.9:5008");
    let storage_b = Storage::local(&bucket).unwrap().with_peers(switch_b).for_template();

    let missing = storage_b.get(&BuildPaths::new(other_build).snapfile()).await;
    let present = storage_b.get(&BuildPaths::new(build_id).snapfile()).await;

    assert!(missing.is_err(), "neither the peer nor the bucket has it");
    assert_eq!(present.unwrap(), snapshot.snapfile.as_slice());
}

#[tokio::test]
async fn a_registry_entry_in_redis_carries_the_node_address_and_a_ttl() {
    let Some(url) = std::env::var("E2B_TEST_REDIS_URL").ok() else {
        assert!(
            std::env::var("CI").is_err(),
            "chunks contract needs E2B_TEST_REDIS_URL in CI"
        );
        eprintln!("skipping: E2B_TEST_REDIS_URL is not set");
        return;
    };
    let settings = RedisSettings::from_vars(&HashMap::from([("REDIS_URL".to_owned(), url.clone())]))
        .unwrap()
        .unwrap();
    let registry = Redis::connect(&settings)
        .await
        .unwrap()
        .peer_registry("10.0.0.7:5008".to_owned());
    let build_id = format!("contract-{}", Uuid::new_v4());

    let before = registry.lookup(&build_id).await.unwrap();
    registry.register(&build_id, Duration::from_secs(7320)).await.unwrap();
    let looked_up = registry.lookup(&build_id).await.unwrap();
    let client = Client::builder().url(&url).codec(StringCodec).build().await.unwrap();
    let ttl = client
        .bucket::<String>(redis_key(&build_id))
        .ttl()
        .await
        .unwrap()
        .unwrap()
        .as_secs();
    registry.unregister(&build_id).await.unwrap();
    let after = registry.lookup(&build_id).await.unwrap();

    assert_eq!(before, None);
    assert_eq!(looked_up.as_deref(), Some("10.0.0.7:5008"));
    assert!(ttl > 7200 && ttl <= 7320, "ttl {ttl}");
    assert_eq!(after, None);
}
