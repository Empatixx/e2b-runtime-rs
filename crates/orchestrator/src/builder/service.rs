use std::sync::Arc;
use std::time::Duration;

use std::panic::AssertUnwindSafe;

use futures::FutureExt;
use futures::future::BoxFuture;
use tokio::time::Instant;
use tokio_util::task::TaskTracker;
use tonic::{Request, Response, Status};

use super::builds::{BuildCache, BuildInfo, BuildResult};
use super::index::layer_files_path;
use super::logs::LogQuery;
use super::phase::{BuildError, CANCELLED};
use super::uploads::LayerUploads;
use crate::proto::template_service_server::TemplateService;
use crate::proto::{
    InitLayerFileUploadRequest, InitLayerFileUploadResponse, LogLevel, LogsDirection, TemplateBuildDeleteRequest,
    TemplateBuildMetadata, TemplateBuildState, TemplateBuildStatusReason, TemplateBuildStatusResponse,
    TemplateCreateRequest, TemplateStatusRequest, template_config,
};
use crate::storage::backend::Storage;

pub const DEFAULT_VERSION: &str = "v2.0.0";
const SIGNED_URL_EXPIRATION: Duration = Duration::from_secs(30 * 60);
const PANIC_MESSAGE: &str = "fatal error occurred during template build, please contact us";
const CANCEL_GRACE: Duration = Duration::from_secs(10);

pub trait TemplateBuilder: Send + Sync + 'static {
    fn build(
        &self,
        request: TemplateCreateRequest,
        info: Arc<BuildInfo>,
        uploads: Arc<LayerUploads>,
    ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>>;
}

struct Settled {
    built: Result<TemplateBuildMetadata, BuildError>,
    remove_files: bool,
}

#[derive(Clone)]
pub struct TemplateStorages {
    pub template: Storage,
    pub build: Storage,
}

pub struct TemplateServer<B> {
    builds: Arc<BuildCache>,
    storage: Storage,
    build_storage: Storage,
    builder: Arc<B>,
    tasks: TaskTracker,
}

impl<B: TemplateBuilder> TemplateServer<B> {
    pub fn new(builds: Arc<BuildCache>, storages: TemplateStorages, builder: Arc<B>) -> Self {
        Self {
            builds,
            storage: storages.template,
            build_storage: storages.build,
            builder,
            tasks: TaskTracker::new(),
        }
    }

    pub async fn wait_for_builds(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    fn start(&self, mut request: TemplateCreateRequest, info: Arc<BuildInfo>, build_id: String) {
        if request.version.as_deref().is_none_or(str::is_empty) {
            request.version = Some(DEFAULT_VERSION.to_owned());
        }
        let template_id = request
            .template
            .as_ref()
            .map(|template| template.template_id.clone())
            .unwrap_or_default();
        let storage = self.storage.clone();
        let builder = self.builder.clone();
        self.tasks.spawn(async move {
            let started = Instant::now();
            info.log
                .push(LogLevel::Info, format!("Building template {template_id}/{build_id}"));
            let uploads = Arc::new(LayerUploads::new(info.clone()));
            let build = builder.build(request, info.clone(), uploads.clone());
            let finished = AssertUnwindSafe(until_cancelled(build, &info))
                .catch_unwind()
                .await
                .ok();
            let landed = uploads.wait().await;
            if let Err(e) = &landed {
                tracing::error!(build = %build_id, error = %e, "template layer upload wait failed");
            }
            let Settled { built, remove_files } = settle(finished, landed);
            if remove_files && let Err(e) = storage.delete_prefix(&build_id).await {
                tracing::warn!(build = %build_id, error = %e, "could not remove the failed build's files");
            }
            let built = cancelled_as_user_error(built, info.cancel.is_cancelled());
            if let Err(e) = &built {
                tracing::error!(build = %build_id, error = ?e, "template build failed");
            }
            info.log.push(
                match built {
                    Ok(_) => LogLevel::Info,
                    Err(_) => LogLevel::Error,
                },
                outcome_line(&built, started.elapsed()),
            );
            info.finish(result_of(built));
        });
    }
}

async fn until_cancelled(
    build: BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>>,
    info: &BuildInfo,
) -> Result<TemplateBuildMetadata, BuildError> {
    let abandoned = async {
        info.cancel.cancelled().await;
        tokio::time::sleep(CANCEL_GRACE).await;
    };
    tokio::select! {
        built = build => built,
        () = abandoned => Err(BuildError::cancelled()),
    }
}

fn settle(finished: Option<Result<TemplateBuildMetadata, BuildError>>, landed: Result<(), String>) -> Settled {
    match (finished, landed) {
        (None, landed) => Settled {
            built: Err(BuildError::Internal(PANIC_MESSAGE.to_owned())),
            remove_files: landed.is_err(),
        },
        (Some(Ok(_)), Err(upload)) => Settled {
            built: Err(BuildError::Internal(format!(
                "error uploading template layers: {upload}"
            ))),
            remove_files: true,
        },
        (Some(built), _) => Settled {
            remove_files: built.is_err(),
            built,
        },
    }
}

fn cancelled_as_user_error(
    built: Result<TemplateBuildMetadata, BuildError>,
    cancelled: bool,
) -> Result<TemplateBuildMetadata, BuildError> {
    built.map_err(|e| match (e, cancelled) {
        (BuildError::Internal(message), true) if message != PANIC_MESSAGE => BuildError::cancelled(),
        (e, _) => e,
    })
}

fn outcome_line(built: &Result<TemplateBuildMetadata, BuildError>, took: Duration) -> String {
    match built {
        Ok(_) => format!(
            "Build finished, took {}",
            crate::go_time::duration(Duration::from_secs(took.as_secs()))
        ),
        Err(e) => format!("Build failed: {}", e.reason().message),
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
        let has_source = match &config.source {
            Some(template_config::Source::FromImage(image)) => !image.is_empty(),
            Some(template_config::Source::FromTemplate(_)) => true,
            None => false,
        };
        if !has_source {
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
            level: request.level.unwrap_or_default(),
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
        let _tracked = self.tasks.token();
        if let Ok(info) = self.builds.get(&request.build_id)
            && info.result().is_none()
        {
            tracing::info!(template = %request.template_id, build = %request.build_id, "Canceling running template build");
            info.finish(BuildResult {
                status: TemplateBuildState::Failed,
                reason: Some(TemplateBuildStatusReason {
                    message: CANCELLED.to_owned(),
                    step: None,
                }),
                metadata: None,
            });
            info.cancel.cancel();
        }
        self.storage
            .delete_prefix(&request.build_id)
            .await
            .map_err(|e| Status::unknown(format!("error when deleting template objects: {e}")))?;
        Ok(Response::new(()))
    }

    async fn init_layer_file_upload(
        &self,
        request: Request<InitLayerFileUploadRequest>,
    ) -> Result<Response<InitLayerFileUploadResponse>, Status> {
        let request = request.into_inner();
        let scope = request.cache_scope.clone().unwrap_or(request.template_id.clone());
        let path = layer_files_path(&scope, &request.hash);
        self.build_storage
            .open_blob(&path)
            .map_err(|e| Status::unknown(format!("failed to open layer files cache: {e}")))?;
        let present = self
            .build_storage
            .exists(&path)
            .await
            .map_err(|e| Status::unknown(format!("failed to check if layer files exists: {e}")))?;
        let response = match self.build_storage.upload_signed_url(&path, SIGNED_URL_EXPIRATION).await {
            Ok(upload) => InitLayerFileUploadResponse {
                present,
                url: Some(upload.url),
                headers: upload.headers,
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
    use crate::builder::phase::INTERNAL_ERROR_MESSAGE;
    use crate::go_url::GoUrl;
    use crate::proto::TemplateConfig;
    use crate::storage::local_upload::UploadSigner;
    use crate::storage::spec::StorageSpec;

    struct Stub;

    impl TemplateBuilder for Stub {
        fn build(
            &self,
            request: TemplateCreateRequest,
            info: Arc<BuildInfo>,
            _uploads: Arc<LayerUploads>,
        ) -> BoxFuture<'static, Result<TemplateBuildMetadata, BuildError>> {
            async move {
                info.log.push(LogLevel::Info, "building");
                match request.template.unwrap_or_default().start_command.as_str() {
                    "fail" => Err(BuildError::Internal("disk".to_owned())),
                    "hang" => std::future::pending().await,
                    "panic" => panic!("builder bug"),
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
        let storages = TemplateStorages {
            template: storage.clone(),
            build: storage.clone(),
        };
        Fixture {
            server: TemplateServer::new(Arc::default(), storages, Arc::new(Stub)),
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
    async fn an_empty_image_is_no_source_and_an_empty_version_gets_the_default() {
        let fixture = fixture();
        let mut request = create("b0", "");
        request.get_mut().template.as_mut().unwrap().source = Some(template_config::Source::FromImage(String::new()));

        let error = fixture.server.template_create(request).await.unwrap_err();

        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn a_panicking_build_fails_with_the_internal_reason_and_keeps_its_files() {
        let fixture = fixture();
        fixture
            .storage
            .put("bp/rootfs.ext4", Bytes::from_static(b"x"))
            .await
            .unwrap();
        fixture.server.template_create(create("bp", "panic")).await.unwrap();
        fixture.server.wait_for_builds().await;

        let done = status(&fixture.server, "bp").await.unwrap();

        assert_eq!(done.status, TemplateBuildState::Failed as i32);
        assert_eq!(done.reason.unwrap().message, INTERNAL_ERROR_MESSAGE);
        assert_eq!(
            done.log_entries.last().unwrap().message,
            format!("Build failed: {INTERNAL_ERROR_MESSAGE}")
        );
        assert!(fixture.storage.exists("bp/rootfs.ext4").await.unwrap());
    }

    #[test]
    fn the_outcome_settles_like_the_go_defers() {
        let failed = || Err(BuildError::Internal("disk".to_owned()));
        let built = || Ok(TemplateBuildMetadata::default());
        let lost = || Err("storage down".to_owned());

        let panicked = settle(None, Ok(()));
        assert!(!panicked.remove_files);
        assert_eq!(panicked.built, Err(BuildError::Internal(PANIC_MESSAGE.to_owned())));
        assert!(settle(None, lost()).remove_files);
        let upload_failed = settle(Some(built()), lost());
        assert!(upload_failed.remove_files);
        assert_eq!(
            upload_failed.built,
            Err(BuildError::Internal(
                "error uploading template layers: storage down".to_owned()
            ))
        );
        assert!(settle(Some(failed()), Ok(())).remove_files);
        assert!(!settle(Some(built()), Ok(())).remove_files);
    }

    #[test]
    fn only_internal_errors_of_a_cancelled_build_read_as_cancelled() {
        let user = BuildError::User {
            phase: "steps".to_owned(),
            step: "2".to_owned(),
            message: "failed to run command 'sleep 60': context: context canceled".to_owned(),
        };

        assert_eq!(
            cancelled_as_user_error(Err(BuildError::Internal("create sandbox: x".to_owned())), true),
            Err(BuildError::cancelled())
        );
        assert_eq!(cancelled_as_user_error(Err(user.clone()), true), Err(user));
        assert_eq!(
            cancelled_as_user_error(Err(BuildError::Internal("x".to_owned())), false),
            Err(BuildError::Internal("x".to_owned()))
        );
    }

    #[tokio::test]
    async fn deleting_a_running_build_cancels_it_without_a_step() {
        let fixture = fixture();
        fixture.server.template_create(create("bc", "hang")).await.unwrap();
        fixture
            .server
            .template_build_delete(Request::new(TemplateBuildDeleteRequest {
                build_id: "bc".to_owned(),
                template_id: "t1".to_owned(),
            }))
            .await
            .unwrap();
        fixture.server.wait_for_builds().await;

        let done = status(&fixture.server, "bc").await.unwrap();

        assert_eq!(
            done.reason,
            Some(TemplateBuildStatusReason {
                message: "build was cancelled".to_owned(),
                step: None,
            })
        );
        assert_eq!(
            done.log_entries.last().unwrap().message,
            "Build failed: build was cancelled"
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
        let lines: Vec<&str> = done.log_entries.iter().map(|entry| entry.message.as_str()).collect();
        assert_eq!(lines[..2], ["Building template t1/b1", "building"]);
        assert_eq!(lines[2], "Build finished, took 0s");
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
            "failed to get signed url: file system storage does not support signed URLs (no local upload endpoint configured)"
        );
    }

    #[tokio::test]
    async fn layer_file_upload_signs_local_uploads() {
        let dir = tempfile::tempdir().unwrap();
        let signer = UploadSigner::with_key("http://node:5008", vec![1; 32]);
        let build = Storage::open(
            &StorageSpec::local(&dir.path().display().to_string()),
            Some(signer.clone()),
        )
        .unwrap();
        let storages = TemplateStorages {
            template: Storage::local(&dir.path().join("templates")).unwrap(),
            build,
        };
        let server = TemplateServer::new(Arc::default(), storages, Arc::new(Stub));

        let response = server
            .init_layer_file_upload(Request::new(InitLayerFileUploadRequest {
                template_id: "t1".to_owned(),
                hash: "h1".to_owned(),
                cache_scope: Some("scope".to_owned()),
            }))
            .await
            .unwrap()
            .into_inner();

        assert!(!response.present);
        assert!(response.headers.is_empty());
        let url = GoUrl::parse(&response.url.unwrap()).unwrap();
        assert_eq!((url.host.as_str(), url.path.as_str()), ("node:5008", "/upload"));
        assert_eq!(url.query().values["path"], vec!["scope/files/h1.tar".to_owned()]);
        assert!(dir.path().join("scope/files").is_dir());
    }
}
