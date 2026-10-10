pub mod blob;
pub mod chunk;
pub mod resolve;
pub mod seekable;
pub mod source;

use std::sync::Arc;

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tonic::{Request, Response, Status};

use self::resolve::PeerCache;
use self::source::{Chunks, SourceError};
use super::availability::{not_available, use_storage};
use super::switch::PeerSwitch;
use crate::proto::chunk_service_server::ChunkService;
use crate::proto::{
    GetBuildBlobRequest, GetBuildBlobResponse, GetBuildFileExistsRequest, GetBuildFileExistsResponse,
    GetBuildFileSizeRequest, GetBuildFileSizeResponse, PeerAvailability, ReadAtBuildSeekableRequest,
    ReadAtBuildSeekableResponse,
};

type Responses<R> = BoxStream<'static, Result<R, Status>>;

pub struct ChunkServer {
    pub cache: PeerCache,
    pub peers: Arc<PeerSwitch>,
}

impl ChunkServer {
    fn served_from_storage(&self, build_id: &str) -> bool {
        let uploaded = self.peers.uploaded(build_id);
        if uploaded {
            tracing::Span::current().record("uploaded", true);
        }
        uploaded
    }
}

#[tonic::async_trait]
impl ChunkService for ChunkServer {
    #[tracing::instrument(skip_all, fields(build_id = %request.get_ref().build_id, file_name = %request.get_ref().name, uploaded = tracing::field::Empty))]
    async fn get_build_file_size(
        &self,
        request: Request<GetBuildFileSizeRequest>,
    ) -> Result<Response<GetBuildFileSizeResponse>, Status> {
        let request = request.into_inner();
        if self.served_from_storage(&request.build_id) {
            return Ok(Response::new(size_reply(0, Some(use_storage()))));
        }
        match self.cache.resolve_seekable(&request.build_id, &request.name).await {
            Ok(source) => Ok(Response::new(size_reply(source.size() as i64, None))),
            Err(SourceError::NotAvailable) => Ok(Response::new(size_reply(0, Some(not_available())))),
            Err(e) => Err(e.into()),
        }
    }

    #[tracing::instrument(skip_all, fields(build_id = %request.get_ref().build_id, file_name = %request.get_ref().name, uploaded = tracing::field::Empty))]
    async fn get_build_file_exists(
        &self,
        request: Request<GetBuildFileExistsRequest>,
    ) -> Result<Response<GetBuildFileExistsResponse>, Status> {
        let request = request.into_inner();
        if self.served_from_storage(&request.build_id) {
            return Ok(Response::new(exists_reply(Some(use_storage()))));
        }
        let source = match self.cache.resolve_blob(&request.build_id, &request.name) {
            Ok(source) => source,
            Err(SourceError::NotAvailable) => return Ok(Response::new(exists_reply(Some(not_available())))),
            Err(e) => return Err(e.into()),
        };
        match source.exists().await? {
            true => Ok(Response::new(exists_reply(None))),
            false => Ok(Response::new(exists_reply(Some(not_available())))),
        }
    }

    type ReadAtBuildSeekableStream = Responses<ReadAtBuildSeekableResponse>;

    #[tracing::instrument(skip_all, fields(build_id = %request.get_ref().build_id, file_name = %request.get_ref().name, offset = request.get_ref().offset, length = request.get_ref().length, uploaded = tracing::field::Empty))]
    async fn read_at_build_seekable(
        &self,
        request: Request<ReadAtBuildSeekableRequest>,
    ) -> Result<Response<Self::ReadAtBuildSeekableStream>, Status> {
        let request = request.into_inner();
        if request.offset < 0 || request.length < 0 {
            return Err(Status::invalid_argument("offset and length must be non-negative"));
        }
        if self.served_from_storage(&request.build_id) {
            return Ok(Response::new(single(seekable_reply(Vec::new(), Some(use_storage())))));
        }
        let opened = match self.cache.resolve_seekable(&request.build_id, &request.name).await {
            Ok(source) => source.stream(request.offset as u64, request.length as u64).await,
            Err(e) => Err(e),
        };
        respond(
            opened,
            |data| seekable_reply(data, None),
            seekable_reply(Vec::new(), Some(not_available())),
        )
        .map(Response::new)
    }

    type GetBuildBlobStream = Responses<GetBuildBlobResponse>;

    #[tracing::instrument(skip_all, fields(build_id = %request.get_ref().build_id, file_name = %request.get_ref().name, uploaded = tracing::field::Empty))]
    async fn get_build_blob(
        &self,
        request: Request<GetBuildBlobRequest>,
    ) -> Result<Response<Self::GetBuildBlobStream>, Status> {
        let request = request.into_inner();
        if self.served_from_storage(&request.build_id) {
            return Ok(Response::new(single(blob_reply(Vec::new(), Some(use_storage())))));
        }
        let opened = match self.cache.resolve_blob(&request.build_id, &request.name) {
            Ok(source) => source.stream().await,
            Err(e) => Err(e),
        };
        respond(
            opened,
            |data| blob_reply(data, None),
            blob_reply(Vec::new(), Some(not_available())),
        )
        .map(Response::new)
    }
}

fn respond<R: Send + 'static>(
    opened: Result<Chunks, SourceError>,
    data: impl Fn(Vec<u8>) -> R + Send + 'static,
    unavailable: R,
) -> Result<Responses<R>, Status> {
    match opened {
        Ok(chunks) => Ok(chunks
            .map(move |chunk| chunk.map(|chunk| data(chunk.to_vec())).map_err(Status::from))
            .boxed()),
        Err(SourceError::NotAvailable) => Ok(single(unavailable)),
        Err(e) => Err(e.into()),
    }
}

fn single<R: Send + 'static>(reply: R) -> Responses<R> {
    stream::once(std::future::ready(Ok(reply))).boxed()
}

fn size_reply(total_size: i64, availability: Option<PeerAvailability>) -> GetBuildFileSizeResponse {
    GetBuildFileSizeResponse {
        total_size,
        availability,
    }
}

fn exists_reply(availability: Option<PeerAvailability>) -> GetBuildFileExistsResponse {
    GetBuildFileExistsResponse { availability }
}

fn seekable_reply(data: Vec<u8>, availability: Option<PeerAvailability>) -> ReadAtBuildSeekableResponse {
    ReadAtBuildSeekableResponse { data, availability }
}

fn blob_reply(data: Vec<u8>, availability: Option<PeerAvailability>) -> GetBuildBlobResponse {
    GetBuildBlobResponse { data, availability }
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use tonic::Code;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::Artifact;
    use crate::featureflags::FeatureFlags;
    use crate::peer::testing::{CacheFixture, FixedRegistry, cache_fixture, header};

    fn server(fixture: &CacheFixture) -> ChunkServer {
        ChunkServer {
            cache: fixture.cache.clone(),
            peers: PeerSwitch::new(FeatureFlags::offline()),
        }
    }

    async fn uploaded_server(fixture: &CacheFixture, build_id: Uuid) -> ChunkServer {
        let flags = FeatureFlags::builder()
            .override_flag("peer-to-peer-chunk-transfer", true)
            .offline();
        let peers = PeerSwitch::new(flags);
        peers.install(Arc::new(FixedRegistry { lookup: Ok(None) }), "10.0.0.1:5008".to_owned());
        peers.register(build_id).await;
        peers.finish(build_id, true).await;
        ChunkServer {
            cache: fixture.cache.clone(),
            peers,
        }
    }

    fn only<R>(mut replies: Vec<R>) -> R {
        assert_eq!(replies.len(), 1);
        replies.remove(0)
    }

    async fn seekable_replies(
        server: &ChunkServer,
        build_id: &str,
        offset: i64,
        length: i64,
    ) -> Vec<ReadAtBuildSeekableResponse> {
        let request = ReadAtBuildSeekableRequest {
            build_id: build_id.to_owned(),
            name: "memfile".to_owned(),
            offset,
            length,
        };
        let stream = server
            .read_at_build_seekable(Request::new(request))
            .await
            .unwrap()
            .into_inner();
        stream.try_collect().await.unwrap()
    }

    async fn blob_replies(
        server: &ChunkServer,
        build_id: &str,
        name: &str,
    ) -> Result<Vec<GetBuildBlobResponse>, Status> {
        let request = GetBuildBlobRequest {
            build_id: build_id.to_owned(),
            name: name.to_owned(),
        };
        let stream = server.get_build_blob(Request::new(request)).await?.into_inner();
        stream.try_collect().await
    }

    #[tokio::test]
    async fn read_at_build_seekable_rejects_a_negative_range() {
        let fixture = cache_fixture();
        let request = ReadAtBuildSeekableRequest {
            build_id: "build-1".to_owned(),
            name: "memfile".to_owned(),
            offset: -1,
            length: 1,
        };

        let error = server(&fixture)
            .read_at_build_seekable(Request::new(request))
            .await
            .err()
            .unwrap();

        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(error.message(), "offset and length must be non-negative");
    }

    #[tokio::test]
    async fn read_at_build_seekable_streams_the_range_of_a_cached_diff() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Memfile, &[9; 8192]);

        let reply = only(seekable_replies(&server(&fixture), &build_id.to_string(), 4096, 4096).await);

        assert_eq!(reply.data, vec![9; 4096]);
        assert_eq!(reply.availability, None);
    }

    #[tokio::test]
    async fn read_at_build_seekable_of_an_uncached_build_sends_not_available_once() {
        let fixture = cache_fixture();

        let reply = only(seekable_replies(&server(&fixture), &Uuid::new_v4().to_string(), 0, 4096).await);

        assert_eq!(reply.availability, Some(not_available()));
        assert!(reply.data.is_empty());
    }

    #[tokio::test]
    async fn read_at_build_seekable_of_an_uploaded_build_sends_use_storage_once() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Memfile, &[9; 4096]);
        let server = uploaded_server(&fixture, build_id).await;

        let reply = only(seekable_replies(&server, &build_id.to_string(), 0, 4096).await);

        assert_eq!(reply.availability, Some(use_storage()));
    }

    #[tokio::test]
    async fn read_at_build_seekable_past_the_end_is_internal() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Memfile, &[9; 4096]);
        let request = ReadAtBuildSeekableRequest {
            build_id: build_id.to_string(),
            name: "memfile".to_owned(),
            offset: 4096,
            length: 4096,
        };

        let error = server(&fixture)
            .read_at_build_seekable(Request::new(request))
            .await
            .err()
            .unwrap();

        assert_eq!(error.code(), Code::Internal);
    }

    #[tokio::test]
    async fn get_build_file_size_reports_the_size_without_availability() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_diff(build_id, Artifact::Rootfs, &[1; 8192]);
        let request = GetBuildFileSizeRequest {
            build_id: build_id.to_string(),
            name: "rootfs.ext4".to_owned(),
        };

        let reply = server(&fixture)
            .get_build_file_size(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply, size_reply(8192, None));
    }

    #[tokio::test]
    async fn get_build_file_size_of_an_uncached_build_is_not_available() {
        let fixture = cache_fixture();
        let request = GetBuildFileSizeRequest {
            build_id: Uuid::new_v4().to_string(),
            name: "memfile".to_owned(),
        };

        let reply = server(&fixture)
            .get_build_file_size(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply.availability, Some(not_available()));
    }

    #[tokio::test]
    async fn get_build_file_size_of_an_unknown_file_is_an_invalid_argument() {
        let fixture = cache_fixture();
        let request = GetBuildFileSizeRequest {
            build_id: "build-1".to_owned(),
            name: "snapfile".to_owned(),
        };

        let error = server(&fixture)
            .get_build_file_size(Request::new(request))
            .await
            .err()
            .unwrap();

        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(error.message(), "unknown file: \"snapfile\"");
    }

    #[tokio::test]
    async fn get_build_file_size_of_an_uploaded_build_says_use_storage() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let server = uploaded_server(&fixture, build_id).await;
        let request = GetBuildFileSizeRequest {
            build_id: build_id.to_string(),
            name: "memfile".to_owned(),
        };

        let reply = server
            .get_build_file_size(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply.availability, Some(use_storage()));
    }

    #[tokio::test]
    async fn get_build_file_exists_is_empty_when_the_snapfile_is_on_disk() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let snapfile = fixture.write_file("snapfile", b"snap");
        fixture.add_template(build_id, header(build_id, 4096), &snapfile);
        let request = GetBuildFileExistsRequest {
            build_id: build_id.to_string(),
            name: "snapfile".to_owned(),
        };

        let reply = server(&fixture)
            .get_build_file_exists(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply, exists_reply(None));
    }

    #[tokio::test]
    async fn get_build_file_exists_is_not_available_when_the_snapfile_is_gone() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("missing"));
        let request = GetBuildFileExistsRequest {
            build_id: build_id.to_string(),
            name: "snapfile".to_owned(),
        };

        let reply = server(&fixture)
            .get_build_file_exists(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply.availability, Some(not_available()));
    }

    #[tokio::test]
    async fn get_build_file_exists_of_an_uncached_build_is_not_available() {
        let fixture = cache_fixture();
        let request = GetBuildFileExistsRequest {
            build_id: Uuid::new_v4().to_string(),
            name: "snapfile".to_owned(),
        };

        let reply = server(&fixture)
            .get_build_file_exists(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply.availability, Some(not_available()));
    }

    #[tokio::test]
    async fn get_build_file_exists_of_a_header_is_an_invalid_argument() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("snapfile"));
        let request = GetBuildFileExistsRequest {
            build_id: build_id.to_string(),
            name: "memfile.header".to_owned(),
        };

        let error = server(&fixture)
            .get_build_file_exists(Request::new(request))
            .await
            .err()
            .unwrap();

        assert_eq!(error.code(), Code::InvalidArgument);
        assert_eq!(error.message(), "operation not supported");
    }

    #[tokio::test]
    async fn get_build_file_exists_of_an_uploaded_build_says_use_storage() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let server = uploaded_server(&fixture, build_id).await;
        let request = GetBuildFileExistsRequest {
            build_id: build_id.to_string(),
            name: "snapfile".to_owned(),
        };

        let reply = server
            .get_build_file_exists(Request::new(request))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(reply.availability, Some(use_storage()));
    }

    #[tokio::test]
    async fn get_build_blob_streams_the_snapfile() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let snapfile = fixture.write_file("snapfile", b"snapfile content");
        fixture.add_template(build_id, header(build_id, 4096), &snapfile);

        let reply = only(
            blob_replies(&server(&fixture), &build_id.to_string(), "snapfile")
                .await
                .unwrap(),
        );

        assert_eq!(reply.data, b"snapfile content");
        assert_eq!(reply.availability, None);
    }

    #[tokio::test]
    async fn get_build_blob_of_a_snapfile_that_is_gone_sends_not_available() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("missing"));

        let reply = only(
            blob_replies(&server(&fixture), &build_id.to_string(), "snapfile")
                .await
                .unwrap(),
        );

        assert_eq!(reply.availability, Some(not_available()));
    }

    #[tokio::test]
    async fn get_build_blob_of_an_unknown_file_is_an_invalid_argument() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("snapfile"));

        let error = blob_replies(&server(&fixture), &build_id.to_string(), "unknown.file")
            .await
            .unwrap_err();

        assert_eq!(error.code(), Code::InvalidArgument);
    }

    #[tokio::test]
    async fn get_build_blob_of_an_uploaded_build_sends_use_storage_once() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let server = uploaded_server(&fixture, build_id).await;

        let reply = only(blob_replies(&server, &build_id.to_string(), "snapfile").await.unwrap());

        assert_eq!(reply.availability, Some(use_storage()));
    }
}
