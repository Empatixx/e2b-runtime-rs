use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_stream::Stream;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status, Streaming};
use uuid::Uuid;

use super::api_error::ApiError;
use super::chrooted::Chrooted;
use super::chrooted::metrics::ChrootMetrics;
use super::failure::{internal, received};
use super::spans::traced;
use crate::go_path;
use crate::go_strconv::quote;
use crate::proto::volume_service_server::VolumeService;
use crate::proto::{
    CreateDirRequest, CreateDirResponse, CreateFileRequest, CreateFileResponse, CreateVolumeRequest,
    CreateVolumeResponse, DeletePathRequest, DeletePathResponse, DeleteVolumeRequest, DeleteVolumeResponse,
    GetFileRequest, GetFileResponse, ListDirRequest, ListDirResponse, StatPathRequest, StatPathResponse,
    UpdatePathRequest, UpdatePathResponse, VolumeInfo, create_file_request,
};
use crate::server::config::volume_mounts::VolumeMounts;

const DOWNLOAD_BUFFERED_MESSAGES: usize = 2;

pub const DEFAULT_DIR_MODE: u32 = 0o777;
pub const DEFAULT_FILE_MODE: u32 = 0o666;
pub const DEFAULT_OWNER_ID: u32 = 9090;
pub const DEFAULT_GROUP_ID: u32 = 9090;

#[derive(Clone)]
pub struct VolumeServer {
    pub(super) mounts: Arc<VolumeMounts>,
    _metrics: Arc<ChrootMetrics>,
}

pub struct Confined {
    pub fs: Chrooted,
    pub path: String,
}

impl VolumeServer {
    pub fn new(mounts: VolumeMounts) -> Self {
        Self {
            mounts: Arc::new(mounts),
            _metrics: Arc::new(ChrootMetrics::register()),
        }
    }

    pub(super) fn volume_root_path(&self, volume: Option<&VolumeInfo>) -> Result<PathBuf, ApiError> {
        let volume = volume.cloned().unwrap_or_default();
        let (team_id, volume_id) = parse_ids(&volume)?;
        self.mounts
            .volume_path(&volume.volume_type, team_id, volume_id)
            .map_err(|_| ApiError::not_supported("unknown volume type"))
    }

    pub(super) fn confined(&self, volume: Option<&VolumeInfo>, requested_path: &str) -> Result<Confined, Status> {
        let volume = volume.cloned().unwrap_or_default();
        let (team_id, volume_id) = parse_ids(&volume)?;
        let root = self
            .mounts
            .volume_path(&volume.volume_type, team_id, volume_id)
            .map_err(|_| ApiError::not_supported("volume type not found"))?;
        Chrooted::open(&root)
            .map(|fs| Confined {
                fs,
                path: volume_path(requested_path),
            })
            .map_err(|e| ApiError::unknown(format!("failed to get chroot: {e}")).into_status())
    }
}

pub fn volume_path(requested: &str) -> String {
    match requested.starts_with('/') {
        true => go_path::clean(requested),
        false => go_path::clean(&format!("/{requested}")),
    }
}

fn parse_ids(volume: &VolumeInfo) -> Result<(Uuid, Uuid), ApiError> {
    let team_id = Uuid::parse_str(&volume.team_id)
        .map_err(|_| ApiError::invalid_request(format!("invalid team ID {}", quote(&volume.team_id))))?;
    let volume_id = Uuid::parse_str(&volume.volume_id)
        .map_err(|_| ApiError::invalid_request(format!("invalid volume ID {}", quote(&volume.volume_id))))?;
    Ok((team_id, volume_id))
}

pub async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Status> + Send + 'static,
) -> Result<T, Status> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Status::internal(e.to_string()))?
}

type DownloadStream = Pin<Box<dyn Stream<Item = Result<GetFileResponse, Status>> + Send>>;

#[tonic::async_trait]
impl VolumeService for VolumeServer {
    async fn create_volume(
        &self,
        request: Request<CreateVolumeRequest>,
    ) -> Result<Response<CreateVolumeResponse>, Status> {
        let server = self.clone();
        traced(
            "create volume",
            blocking(move || server.create_volume_root(request.get_ref())),
        )
        .await
        .map(Response::new)
    }

    async fn delete_volume(
        &self,
        request: Request<DeleteVolumeRequest>,
    ) -> Result<Response<DeleteVolumeResponse>, Status> {
        let server = self.clone();
        traced(
            "delete volume",
            blocking(move || server.delete_volume_root(request.get_ref())),
        )
        .await
        .map(Response::new)
    }

    async fn create_dir(&self, request: Request<CreateDirRequest>) -> Result<Response<CreateDirResponse>, Status> {
        let server = self.clone();
        traced(
            "create directory in volume",
            blocking(move || server.make_directory(request.get_ref())),
        )
        .await
        .map(Response::new)
    }

    async fn list_dir(&self, request: Request<ListDirRequest>) -> Result<Response<ListDirResponse>, Status> {
        let server = self.clone();
        traced(
            "list directory in volume",
            blocking(move || server.list_directory(request.get_ref())),
        )
        .await
        .map(Response::new)
    }

    async fn create_file(
        &self,
        request: Request<Streaming<CreateFileRequest>>,
    ) -> Result<Response<CreateFileResponse>, Status> {
        traced("create file in volume", self.receive_file(request.into_inner()))
            .await
            .map(Response::new)
    }

    type GetFileStream = DownloadStream;

    async fn get_file(&self, request: Request<GetFileRequest>) -> Result<Response<Self::GetFileStream>, Status> {
        let server = self.clone();
        let download = traced(
            "get file from volume",
            blocking(move || server.open_download(request.get_ref())),
        )
        .await?;
        let (sender, receiver) = mpsc::channel(DOWNLOAD_BUFFERED_MESSAGES);
        tokio::task::spawn_blocking(move || download.send_all(&sender));
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    async fn delete_path(&self, request: Request<DeletePathRequest>) -> Result<Response<DeletePathResponse>, Status> {
        let server = self.clone();
        traced(
            "delete file in volume",
            blocking(move || server.remove_path(request.get_ref())),
        )
        .await
        .map(Response::new)
    }

    async fn stat_path(&self, request: Request<StatPathRequest>) -> Result<Response<StatPathResponse>, Status> {
        let server = self.clone();
        traced(
            "stat path in volume",
            blocking(move || server.stat_entry(request.get_ref())),
        )
        .await
        .map(Response::new)
    }

    async fn update_path(&self, request: Request<UpdatePathRequest>) -> Result<Response<UpdatePathResponse>, Status> {
        let server = self.clone();
        traced(
            "update file metadata in volume",
            blocking(move || server.update_entry(request.get_ref())),
        )
        .await
        .map(Response::new)
    }
}

impl VolumeServer {
    async fn receive_file(&self, mut stream: Streaming<CreateFileRequest>) -> Result<CreateFileResponse, Status> {
        let start = match next_message(&mut stream, "failed to receive start message").await? {
            Some(create_file_request::Message::Start(start)) => start,
            _ => return Err(internal("expected start message")),
        };
        let server = self.clone();
        let mut upload = blocking(move || server.begin_upload(&start)).await?;
        loop {
            match next_message(&mut stream, "failed to receive chunk").await? {
                Some(create_file_request::Message::Content(content)) => {
                    upload = blocking(move || upload.written(&content.content)).await?;
                }
                Some(create_file_request::Message::Finish(_)) => return blocking(move || upload.finish()).await,
                _ => return Err(internal("unexpected start message")),
            }
        }
    }
}

async fn next_message(
    stream: &mut Streaming<CreateFileRequest>,
    prefix: &str,
) -> Result<Option<create_file_request::Message>, Status> {
    match stream.message().await {
        Ok(Some(request)) => Ok(request.message),
        Ok(None) => Err(received(prefix, None)),
        Err(status) => Err(received(prefix, Some(status))),
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::UserErrorCode;
    use crate::volumes::test_support::assert_user_error;

    #[test]
    fn requested_paths_are_anchored_at_the_volume_root() {
        let cases = [
            ("/", "/"),
            (".", "/"),
            ("", "/"),
            ("../", "/"),
            ("./a/.././", "/"),
            ("dir/file.txt", "/dir/file.txt"),
            ("../escape1", "/escape1"),
            ("../../../../escape2", "/escape2"),
            ("./a/.././../escape3", "/escape3"),
            ("/etc/passwd", "/etc/passwd"),
            ("//", "/"),
        ];

        for (requested, expected) in cases {
            assert_eq!(volume_path(requested), expected, "path {requested:?}");
        }
    }

    fn server_mounting(directory: &std::path::Path) -> VolumeServer {
        let vars = std::collections::HashMap::from([(
            "PERSISTENT_VOLUME_MOUNTS".to_owned(),
            format!("good-vol:{}", directory.display()),
        )]);
        VolumeServer::new(VolumeMounts::from_vars(&vars).unwrap())
    }

    fn volume(volume_type: &str, team_id: &str, volume_id: &str) -> VolumeInfo {
        VolumeInfo {
            volume_type: volume_type.to_owned(),
            team_id: team_id.to_owned(),
            volume_id: volume_id.to_owned(),
        }
    }

    #[test]
    fn the_volume_root_is_the_team_and_volume_under_the_mount_of_the_type() {
        let mount = tempfile::tempdir().unwrap();
        let (team_id, volume_id) = (Uuid::new_v4(), Uuid::new_v4());

        let path = server_mounting(mount.path())
            .volume_root_path(Some(&volume("good-vol", &team_id.to_string(), &volume_id.to_string())))
            .unwrap();

        assert_eq!(
            path,
            mount
                .path()
                .join(format!("team-{team_id}"))
                .join(format!("vol-{volume_id}"))
        );
    }

    #[test]
    fn a_bad_volume_is_refused_with_the_user_error_of_upstream() {
        let mount = tempfile::tempdir().unwrap();
        let server = server_mounting(mount.path());
        let good = Uuid::new_v4().to_string();
        let cases = [
            (
                "invalid team ID",
                volume("good-vol", "invalid", &good),
                Code::InvalidArgument,
                UserErrorCode::InvalidRequest,
                "invalid team ID \"invalid\"",
            ),
            (
                "invalid volume ID",
                volume("good-vol", &good, "invalid"),
                Code::InvalidArgument,
                UserErrorCode::InvalidRequest,
                "invalid volume ID \"invalid\"",
            ),
            (
                "missing team ID",
                volume("good-vol", "", &good),
                Code::InvalidArgument,
                UserErrorCode::InvalidRequest,
                "invalid team ID \"\"",
            ),
            (
                "volume type not found",
                volume("non-existent", &good, &good),
                Code::Internal,
                UserErrorCode::NotSupported,
                "unknown volume type",
            ),
        ];

        for (name, info, code, user_code, message) in cases {
            let status = server.volume_root_path(Some(&info)).unwrap_err().into_status();

            assert_user_error(&status, code, user_code);
            assert_eq!(status.message(), message, "{name}");
        }
    }

    #[test]
    fn a_missing_volume_is_refused_like_an_empty_one() {
        let mount = tempfile::tempdir().unwrap();

        let status = server_mounting(mount.path())
            .volume_root_path(None)
            .unwrap_err()
            .into_status();

        assert_eq!(status.message(), "invalid team ID \"\"");
    }

    #[test]
    fn confining_a_volume_of_an_unknown_type_says_so() {
        let mount = tempfile::tempdir().unwrap();
        let good = Uuid::new_v4().to_string();

        let status = server_mounting(mount.path())
            .confined(Some(&volume("non-existent", &good, &good)), "/")
            .err()
            .unwrap();

        assert_user_error(&status, Code::Internal, UserErrorCode::NotSupported);
        assert_eq!(status.message(), "volume type not found");
    }

    #[test]
    fn confining_a_volume_that_was_never_created_fails_to_get_the_chroot() {
        let mount = tempfile::tempdir().unwrap();
        let good = Uuid::new_v4();

        let status = server_mounting(mount.path())
            .confined(Some(&volume("good-vol", &good.to_string(), &good.to_string())), "/")
            .err()
            .unwrap();

        assert_user_error(&status, Code::Internal, UserErrorCode::UnknownUserErrorCode);
        assert!(
            status.message().starts_with("failed to get chroot: open "),
            "{}",
            status.message()
        );
    }
}
