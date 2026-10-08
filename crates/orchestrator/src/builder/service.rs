use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio_util::task::TaskTracker;
use tonic::{Request, Response, Status};

use super::builds::{BuildCache, BuildInfo, BuildResult};
use super::logs::LogQuery;
use super::phase::BuildError;
use crate::proto::template_service_server::TemplateService;
use crate::proto::{
    InitLayerFileUploadRequest, InitLayerFileUploadResponse, LogLevel, LogsDirection, TemplateBuildDeleteRequest,
    TemplateBuildMetadata, TemplateBuildState, TemplateBuildStatusResponse, TemplateCreateRequest,
    TemplateStatusRequest, template_config,
};
use crate::storage::backend::Storage;

pub const DEFAULT_VERSION: &str = "v2.0.0";
const SIGNED_URL_EXPIRATION: Duration = Duration::from_secs(30 * 60);

pub trait TemplateBuilder: Send + Sync + 'static {
    fn build(
        &self,
        request: TemplateCreateRequest,
        info: Arc<BuildInfo>,
    ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>>;
}

pub struct TemplateServer<B> {
    builds: Arc<BuildCache>,
    storage: Storage,
    builder: Arc<B>,
    tasks: TaskTracker,
}

impl<B: TemplateBuilder> TemplateServer<B> {
    pub fn new(builds: Arc<BuildCache>, storage: Storage, builder: Arc<B>) -> Self {
        Self {
            builds,
            storage,
            builder,
            tasks: TaskTracker::new(),
        }
    }

    pub async fn wait_for_builds(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    fn start(&self, mut request: TemplateCreateRequest, info: Arc<BuildInfo>, build_id: String) {
        request.version.get_or_insert_with(|| DEFAULT_VERSION.to_owned());
        let build = self.builder.build(request, info.clone());
        let storage = self.storage.clone();
        self.tasks.spawn(async move {
            let built = tokio::select! {
                built = build => built,
                () = info.cancel.cancelled() => Err(BuildError::cancelled()),
            };
            if let Err(e) = &built {
                tracing::error!(build = %build_id, error = ?e, "template build failed");
                if let Err(e) = storage.delete_prefix(&build_id).await {
                    tracing::warn!(build = %build_id, error = %e, "could not remove the failed build's files");
                }
            }
            info.finish(result_of(built));
        });
    }
}

fn result_of(built: Result<TemplateBuildMetadata, BuildError>) -> BuildResult {
    match built {
        Ok(metadata) => BuildResult {
            status: TemplateBuildState::Completed,
            reason: None,
            metadata: Some(metadata),
        },
        Err(e) => BuildResult {
            status: TemplateBuildState::Failed,
            reason: Some(e.reason()),
            metadata: None,
        },
    }
}

#[tonic::async_trait]
impl<B: TemplateBuilder> TemplateService for TemplateServer<B> {
    async fn template_create(&self, request: Request<TemplateCreateRequest>) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        let config = request.template.clone().unwrap_or_default();
        if !matches!(
            config.source,
            Some(template_config::Source::FromImage(_) | template_config::Source::FromTemplate(_))
        ) {
            return Err(Status::invalid_argument(
                "template build requires either fromImage or fromTemplate",
            ));
        }
        let info = self
            .builds
            .register(&config.build_id)
            .map_err(|e| Status::unknown(format!("error while creating build cache: {e}")))?;
        self.start(request, info, config.build_id);
        Ok(Response::new(()))
    }

    async fn template_build_status(
        &self,
        request: Request<TemplateStatusRequest>,
    ) -> Result<Response<TemplateBuildStatusResponse>, Status> {
        let request = request.into_inner();
        let info = self
            .builds
            .get(&request.build_id)
            .map_err(|e| Status::unknown(format!("error while getting build info, maybe already expired: {e}")))?;
        let query = LogQuery {
            level: request
                .level
                .and_then(|level| LogLevel::try_from(level).ok())
                .unwrap_or_default(),
            start: request.start.and_then(|start| start.try_into().ok()),
            end: request.end.and_then(|end| end.try_into().ok()),
            direction: request
                .direction
                .and_then(|direction| LogsDirection::try_from(direction).ok())
                .unwrap_or_default(),
            offset: request.offset.unwrap_or_default().max(0) as u32,
            limit: request.limit,
        };
        let log_entries = info.log.query(&query);
        let response = match info.result() {
            Some(result) => TemplateBuildStatusResponse {
                status: result.status.into(),
                metadata: result.metadata,
                log_entries,
                reason: result.reason,
            },
            None => TemplateBuildStatusResponse {
                status: TemplateBuildState::Building.into(),
                metadata: None,
                log_entries,
                reason: None,
            },
        };
        Ok(Response::new(response))
    }

    async fn template_build_delete(
        &self,
        request: Request<TemplateBuildDeleteRequest>,
    ) -> Result<Response<()>, Status> {
        let request = request.into_inner();
        if request.build_id.is_empty() || request.template_id.is_empty() {
            return Err(Status::unknown("template id and build id are required fields"));
        }
        if let Ok(info) = self.builds.get(&request.build_id)
            && info.result().is_none()
        {
            info.finish(result_of(Err(BuildError::cancelled())));
            info.cancel.cancel();
        }
        self.storage
            .delete_prefix(&request.build_id)
            .await
            .map_err(|e| Status::unknown(format!("error deleting template files from storage: {e}")))?;
        Ok(Response::new(()))
    }

    async fn init_layer_file_upload(
        &self,
        request: Request<InitLayerFileUploadRequest>,
    ) -> Result<Response<InitLayerFileUploadResponse>, Status> {
        let request = request.into_inner();
        let scope = request.cache_scope.clone().unwrap_or(request.template_id.clone());
        let path = format!("{scope}/files/{}.tar", request.hash);
        let present = self
            .storage
            .exists(&path)
            .await
            .map_err(|e| Status::unknown(format!("failed to check if layer files exists: {e}")))?;
        let response = match self.storage.upload_signed_url(&path, SIGNED_URL_EXPIRATION).await {
            Ok(url) => InitLayerFileUploadResponse {
                present,
                url: Some(url),
                headers: Default::default(),
            },
            Err(e) if present => {
                tracing::warn!(error = %e, "serving layer-file cache hit without an upload URL: signing the upload URL failed");
                InitLayerFileUploadResponse {
                    present: true,
                    url: None,
                    headers: Default::default(),
                }
            }
            Err(e) => return Err(Status::unknown(format!("failed to get signed url: {e}"))),
        };
        Ok(Response::new(response))
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures::FutureExt;

    use super::*;
    use crate::proto::TemplateConfig;

    struct Stub;

    impl TemplateBuilder for Stub {
        fn build(
            &self,
            request: TemplateCreateRequest,
            info: Arc<BuildInfo>,
        ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>> {
            async move {
                info.log.push(LogLevel::Info, "building");
                match request.template.unwrap_or_default().start_command.as_str() {
                    "fail" => Err(BuildError::Internal("disk".to_owned())),
                    "hang" => std::future::pending().await,
                    _ => Ok(TemplateBuildMetadata {
                        rootfs_size_key: 1024,
                        ..TemplateBuildMetadata::default()
                    }),
                }
            }
            .boxed()
        }
    }

    struct Fixture {
        server: TemplateServer<Stub>,
        storage: Storage,
        _dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        Fixture {
            server: TemplateServer::new(Arc::default(), storage.clone(), Arc::new(Stub)),
            storage,
            _dir: dir,
        }
    }

    fn create(build_id: &str, start_command: &str) -> Request<TemplateCreateRequest> {
        Request::new(TemplateCreateRequest {
            template: Some(TemplateConfig {
                template_id: "t1".to_owned(),
                build_id: build_id.to_owned(),
                start_command: start_command.to_owned(),
                source: Some(template_config::Source::FromImage("debian:12".to_owned())),
                ..TemplateConfig::default()
            }),
            ..TemplateCreateRequest::default()
        })
    }

    async fn status(server: &TemplateServer<Stub>, build_id: &str) -> Result<TemplateBuildStatusResponse, Status> {
        server
            .template_build_status(Request::new(TemplateStatusRequest {
                build_id: build_id.to_owned(),
                ..TemplateStatusRequest::default()
            }))
            .await
            .map(Response::into_inner)
    }

    async fn finished(server: &TemplateServer<Stub>, build_id: &str) -> TemplateBuildStatusResponse {
        let info = server.builds.get(build_id).unwrap();
        tokio::time::timeout(Duration::from_secs(5), info.finished())
            .await
            .unwrap();
        status(server, build_id).await.unwrap()
    }

    #[tokio::test]
    async fn a_build_without_a_source_is_rejected() {
        let fixture = fixture();
        let mut request = create("b0", "");
        request.get_mut().template.as_mut().unwrap().source = None;

        let error = fixture.server.template_create(request).await.unwrap_err();

        assert_eq!(
            (error.code(), error.message()),
            (
                tonic::Code::InvalidArgument,
                "template build requires either fromImage or fromTemplate"
            )
        );
    }

    #[tokio::test]
    async fn a_build_completes_with_logs_and_a_build_id_registers_once() {
        let fixture = fixture();
        fixture.server.template_create(create("b1", "")).await.unwrap();

        let done = finished(&fixture.server, "b1").await;
        let duplicate = fixture.server.template_create(create("b1", "")).await.unwrap_err();

        assert_eq!(done.status, TemplateBuildState::Completed as i32);
        assert_eq!(done.metadata.unwrap().rootfs_size_key, 1024);
        assert_eq!(done.log_entries[0].message, "building");
        assert_eq!(
            duplicate.message(),
            "error while creating build cache: build b1 already exists in cache"
        );
    }

    #[tokio::test]
    async fn a_failed_build_reports_the_internal_reason_and_removes_its_files() {
        let fixture = fixture();
        fixture
            .storage
            .put("b2/rootfs.ext4", Bytes::from_static(b"x"))
            .await
            .unwrap();
        fixture.server.template_create(create("b2", "fail")).await.unwrap();

        let done = finished(&fixture.server, "b2").await;

        assert_eq!(done.status, TemplateBuildState::Failed as i32);
        assert_eq!(
            done.reason.unwrap().message,
            crate::builder::phase::INTERNAL_ERROR_MESSAGE
        );
        assert!(!fixture.storage.exists("b2/rootfs.ext4").await.unwrap());
    }

    #[tokio::test]
    async fn delete_cancels_a_running_build() {
        let fixture = fixture();
        fixture.server.template_create(create("b3", "hang")).await.unwrap();

        fixture
            .server
            .template_build_delete(Request::new(TemplateBuildDeleteRequest {
                build_id: "b3".to_owned(),
                template_id: "t1".to_owned(),
            }))
            .await
            .unwrap();
        let done = finished(&fixture.server, "b3").await;

        assert_eq!(done.status, TemplateBuildState::Failed as i32);
        assert_eq!(done.reason.unwrap().message, "build was cancelled");
        let missing = fixture
            .server
            .template_build_delete(Request::new(TemplateBuildDeleteRequest::default()))
            .await
            .unwrap_err();
        assert_eq!(missing.message(), "template id and build id are required fields");
    }

    #[tokio::test]
    async fn status_of_an_unknown_build_says_it_may_have_expired() {
        let error = status(&fixture().server, "nope").await.unwrap_err();

        assert_eq!(
            error.message(),
            "error while getting build info, maybe already expired: build nope not found in cache"
        );
    }

    #[tokio::test]
    async fn layer_file_upload_without_signing_serves_only_cache_hits() {
        let fixture = fixture();
        let upload = |hash: &str| {
            fixture
                .server
                .init_layer_file_upload(Request::new(InitLayerFileUploadRequest {
                    template_id: "t1".to_owned(),
                    hash: hash.to_owned(),
                    cache_scope: None,
                }))
        };
        fixture
            .storage
            .put("t1/files/h1.tar", Bytes::from_static(b"tar"))
            .await
            .unwrap();

        let hit = upload("h1").await.unwrap().into_inner();
        let miss = upload("h2").await.unwrap_err();

        assert_eq!((hit.present, hit.url), (true, None));
        assert_eq!(
            miss.message(),
            "failed to get signed url: failed to presign PUT URL: local storage cannot sign URLs"
        );
    }
}
