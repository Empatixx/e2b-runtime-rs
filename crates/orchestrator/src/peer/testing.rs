use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use std::time::Duration;

use futures::future::BoxFuture;
use futures::stream::{self, BoxStream};
use futures::{FutureExt, StreamExt};
use tonic::transport::{Channel, Endpoint, Server};
use tonic::{Request, Response, Status};

use super::availability::{not_available, use_storage};
use super::handle::PeerHandle;
use super::registry::PeerRegistry;
use crate::block::build_file::{Artifact, DiffStore};
use crate::peer::server::resolve::PeerCache;
use crate::proto::chunk_service_client::ChunkServiceClient;
use crate::proto::chunk_service_server::{ChunkService, ChunkServiceServer};
use crate::proto::{
    GetBuildBlobRequest, GetBuildBlobResponse, GetBuildFileExistsRequest, GetBuildFileExistsResponse,
    GetBuildFileSizeRequest, GetBuildFileSizeResponse, PeerAvailability, ReadAtBuildSeekableRequest,
    ReadAtBuildSeekableResponse,
};
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::storage::header::{Header, Metadata};
use crate::template::Template;
use crate::template::cache::TemplateCache;
use crate::template::metadata::{TemplateMetadata, TemplateVersions};
use uuid::Uuid;

type Handler<Request, Response> = Arc<dyn Fn(Request) -> Result<Response, Status> + Send + Sync>;

#[derive(Clone)]
pub struct FakeChunks {
    pub size: Handler<GetBuildFileSizeRequest, GetBuildFileSizeResponse>,
    pub exists: Handler<GetBuildFileExistsRequest, GetBuildFileExistsResponse>,
    pub seekable: Handler<ReadAtBuildSeekableRequest, Vec<ReadAtBuildSeekableResponse>>,
    pub blob: Handler<GetBuildBlobRequest, Vec<GetBuildBlobResponse>>,
}

impl Default for FakeChunks {
    fn default() -> Self {
        Self {
            size: Arc::new(|_| Err(Status::unimplemented("size"))),
            exists: Arc::new(|_| Err(Status::unimplemented("exists"))),
            seekable: Arc::new(|_| Err(Status::unimplemented("seekable"))),
            blob: Arc::new(|_| Err(Status::unimplemented("blob"))),
        }
    }
}

impl FakeChunks {
    pub async fn serve(self) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(
            Server::builder()
                .add_service(ChunkServiceServer::new(self))
                .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener)),
        );
        address
    }
}

#[tonic::async_trait]
impl ChunkService for FakeChunks {
    async fn get_build_file_size(
        &self,
        request: Request<GetBuildFileSizeRequest>,
    ) -> Result<Response<GetBuildFileSizeResponse>, Status> {
        (self.size)(request.into_inner()).map(Response::new)
    }

    async fn get_build_file_exists(
        &self,
        request: Request<GetBuildFileExistsRequest>,
    ) -> Result<Response<GetBuildFileExistsResponse>, Status> {
        (self.exists)(request.into_inner()).map(Response::new)
    }

    type ReadAtBuildSeekableStream = BoxStream<'static, Result<ReadAtBuildSeekableResponse, Status>>;

    async fn read_at_build_seekable(
        &self,
        request: Request<ReadAtBuildSeekableRequest>,
    ) -> Result<Response<Self::ReadAtBuildSeekableStream>, Status> {
        let messages = (self.seekable)(request.into_inner())?;
        Ok(Response::new(stream::iter(messages.into_iter().map(Ok)).boxed()))
    }

    type GetBuildBlobStream = BoxStream<'static, Result<GetBuildBlobResponse, Status>>;

    async fn get_build_blob(
        &self,
        request: Request<GetBuildBlobRequest>,
    ) -> Result<Response<Self::GetBuildBlobStream>, Status> {
        let messages = (self.blob)(request.into_inner())?;
        Ok(Response::new(stream::iter(messages.into_iter().map(Ok)).boxed()))
    }
}

pub fn channel(address: SocketAddr) -> Channel {
    Endpoint::from_shared(format!("http://{address}"))
        .unwrap()
        .connect_lazy()
}

pub fn handle(address: SocketAddr, build_id: &str, name: &str, uploaded: Arc<AtomicBool>) -> PeerHandle {
    PeerHandle::builder()
        .client(ChunkServiceClient::new(channel(address)))
        .build_id(build_id)
        .name(name)
        .uploaded(uploaded)
        .build()
}

pub fn flag() -> Arc<AtomicBool> {
    Arc::default()
}

pub fn blob_data(data: &str) -> GetBuildBlobResponse {
    GetBuildBlobResponse {
        data: data.as_bytes().to_vec(),
        availability: None,
    }
}

pub fn blob_availability(availability: PeerAvailability) -> GetBuildBlobResponse {
    GetBuildBlobResponse {
        data: Vec::new(),
        availability: Some(availability),
    }
}

pub fn blob_data_then_use_storage(data: &str) -> GetBuildBlobResponse {
    GetBuildBlobResponse {
        data: data.as_bytes().to_vec(),
        availability: Some(use_storage()),
    }
}

pub fn blob_not_available() -> GetBuildBlobResponse {
    blob_availability(not_available())
}

pub fn seekable_data(data: &[u8]) -> ReadAtBuildSeekableResponse {
    ReadAtBuildSeekableResponse {
        data: data.to_vec(),
        availability: None,
    }
}

pub fn seekable_use_storage() -> ReadAtBuildSeekableResponse {
    ReadAtBuildSeekableResponse {
        data: Vec::new(),
        availability: Some(use_storage()),
    }
}

pub fn local_storage() -> (tempfile::TempDir, Storage) {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    (dir, storage)
}

pub async fn put(storage: &Storage, path: &str, content: &[u8]) {
    storage.put(path, bytes::Bytes::copy_from_slice(content)).await.unwrap();
}

pub struct FixedRegistry {
    pub lookup: Result<Option<String>, String>,
}

impl PeerRegistry for FixedRegistry {
    fn register(&self, _: &str, _: Duration) -> BoxFuture<'static, Result<(), String>> {
        async { Ok(()) }.boxed()
    }

    fn lookup(&self, _: &str) -> BoxFuture<'static, Result<Option<String>, String>> {
        std::future::ready(self.lookup.clone()).boxed()
    }

    fn unregister(&self, _: &str) -> BoxFuture<'static, Result<(), String>> {
        async { Ok(()) }.boxed()
    }
}

pub struct CacheFixture {
    pub dir: tempfile::TempDir,
    pub cache: PeerCache,
}

pub fn cache_fixture() -> CacheFixture {
    let dir = tempfile::tempdir().unwrap();
    let storage = Storage::local(&dir.path().join("store")).unwrap();
    let cache = PeerCache {
        templates: Arc::new(TemplateCache::new(storage.clone(), dir.path().join("templates"))),
        diffs: Arc::new(DiffStore::new(storage, dir.path().join("diffs"))),
    };
    CacheFixture { dir, cache }
}

impl CacheFixture {
    pub fn add_template(&self, build_id: Uuid, memfile: Header, snapfile: &std::path::Path) -> Arc<Template> {
        let build = Build {
            build_id,
            memfile,
            rootfs: header(build_id, 4096),
        };
        let metadata = TemplateMetadata {
            version: crate::template::metadata::CURRENT_VERSION,
            template: TemplateVersions {
                build_id: build_id.to_string(),
                ..TemplateVersions::default()
            },
            ..TemplateMetadata::default()
        };
        self.cache.templates.add_snapshot(
            build,
            snapfile.to_path_buf(),
            metadata,
            self.dir.path().join(format!("dir-{build_id}")),
        )
    }

    pub fn write_file(&self, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = self.dir.path().join(name);
        std::fs::write(&path, content).unwrap();
        path
    }

    pub fn add_diff(&self, build_id: Uuid, artifact: Artifact, content: &[u8]) {
        let path = self.write_file(&format!("{build_id}-{}", artifact.name()), content);
        self.cache.diffs.add_local(build_id, artifact, &path).unwrap();
    }
}

pub fn header(build_id: Uuid, size: u64) -> Header {
    Header::new(Metadata::template(build_id, 4096, size), Vec::new()).unwrap()
}

#[derive(Default)]
pub struct RecordingRegistry {
    pub calls: parking_lot::Mutex<Vec<String>>,
}

impl PeerRegistry for RecordingRegistry {
    fn register(&self, build_id: &str, ttl: Duration) -> BoxFuture<'static, Result<(), String>> {
        self.calls
            .lock()
            .push(format!("register {build_id} {}s", ttl.as_secs()));
        async { Ok(()) }.boxed()
    }

    fn lookup(&self, _: &str) -> BoxFuture<'static, Result<Option<String>, String>> {
        async { Ok(None) }.boxed()
    }

    fn unregister(&self, build_id: &str) -> BoxFuture<'static, Result<(), String>> {
        self.calls.lock().push(format!("unregister {build_id}"));
        async { Ok(()) }.boxed()
    }
}
