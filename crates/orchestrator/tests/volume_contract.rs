#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::os::unix::fs::{MetadataExt, symlink};
use std::path::PathBuf;
use std::sync::Arc;

use orchestrator::proto::volume_service_client::VolumeServiceClient;
use orchestrator::proto::{
    CreateDirRequest, CreateFileRequest, CreateVolumeRequest, DeletePathRequest, DeleteVolumeRequest, FileType,
    GetFileRequest, ListDirRequest, StatPathRequest, UpdatePathRequest, UserErrorCode, VolumeFileCreateContent,
    VolumeFileCreateFinish, VolumeFileCreateStart, VolumeInfo, create_file_request, get_file_response,
};
use orchestrator::server::app::App;
use orchestrator::server::config::NodeConfig;
use orchestrator::server::node::Node;
use orchestrator::server::user_error::user_error_of;
use tempfile::TempDir;
use tonic::transport::Channel;
use tonic::{Code, Status};
use uuid::Uuid;

const VOLUME_TYPE: &str = "contract-vt";
const CHUNK: usize = 1024 * 1024;

struct Volumes {
    client: VolumeServiceClient<Channel>,
    info: VolumeInfo,
    root: PathBuf,
    _dir: TempDir,
}

async fn serve() -> Volumes {
    let dir = tempfile::tempdir().unwrap();
    let mounts = dir.path().join("mounts");
    std::fs::create_dir(&mounts).unwrap();
    let path = |name: &str| dir.path().join(name).display().to_string();
    let vars: HashMap<String, String> = [
        ("STORAGE_PROVIDER", "Local".to_owned()),
        ("LOCAL_TEMPLATE_STORAGE_BASE_PATH", path("store")),
        ("DEFAULT_CACHE_DIR", path("cache")),
        ("TEMPLATE_CACHE_DIR", path("cache/template")),
        ("SANDBOX_CACHE_DIR", path("sandboxes")),
        ("E2B_FIRECRACKER", "/unused".to_owned()),
        ("E2B_KERNEL", "/unused".to_owned()),
        (
            "PERSISTENT_VOLUME_MOUNTS",
            format!("{VOLUME_TYPE}:{}", mounts.display()),
        ),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_owned(), value))
    .collect();
    let config = NodeConfig::from_vars(&vars).unwrap();
    let app = App::new(Arc::new(Node::new(&config).unwrap()), None).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(axum::serve(listener, app.router()).into_future());
    let info = VolumeInfo {
        volume_type: VOLUME_TYPE.to_owned(),
        team_id: Uuid::new_v4().to_string(),
        volume_id: Uuid::new_v4().to_string(),
    };
    let root = mounts
        .join(format!("team-{}", info.team_id))
        .join(format!("vol-{}", info.volume_id));
    Volumes {
        client: VolumeServiceClient::connect(format!("http://{address}")).await.unwrap(),
        info,
        root,
        _dir: dir,
    }
}

impl Volumes {
    async fn created(mut self) -> Self {
        self.client
            .create_volume(CreateVolumeRequest {
                volume: Some(self.info.clone()),
            })
            .await
            .unwrap();
        self
    }

    async fn upload(&mut self, start: VolumeFileCreateStart, chunks: Vec<Vec<u8>>) -> Result<(), Status> {
        let messages = std::iter::once(create_file_request::Message::Start(start))
            .chain(
                chunks
                    .into_iter()
                    .map(|content| create_file_request::Message::Content(VolumeFileCreateContent { content })),
            )
            .chain(std::iter::once(create_file_request::Message::Finish(
                VolumeFileCreateFinish {},
            )))
            .map(|message| CreateFileRequest { message: Some(message) });
        self.client
            .create_file(tokio_stream::iter(messages))
            .await
            .map(|response| response.into_inner())
            .map(drop)
    }

    fn start(&self, path: &str) -> VolumeFileCreateStart {
        VolumeFileCreateStart {
            volume: Some(self.info.clone()),
            path: path.to_owned(),
            ..Default::default()
        }
    }

    async fn download(&mut self, path: &str) -> Result<Vec<get_file_response::Message>, Status> {
        let mut stream = self
            .client
            .get_file(GetFileRequest {
                volume: Some(self.info.clone()),
                path: path.to_owned(),
            })
            .await?
            .into_inner();
        let mut messages = Vec::new();
        while let Some(response) = stream.message().await? {
            messages.push(response.message.unwrap());
        }
        Ok(messages)
    }
}

fn start_message(volumes: &Volumes, path: &str) -> CreateFileRequest {
    CreateFileRequest {
        message: Some(create_file_request::Message::Start(volumes.start(path))),
    }
}

fn content_of(messages: &[get_file_response::Message]) -> Vec<u8> {
    messages
        .iter()
        .filter_map(|message| match message {
            get_file_response::Message::Content(content) => Some(content.content.clone()),
            _ => None,
        })
        .flatten()
        .collect()
}

fn assert_user_error(status: &Status, code: Code, user_code: UserErrorCode, http_status: u32) {
    assert_eq!(status.code(), code, "{status:?}");
    let detail = user_error_of(status).expect("the status carries a UserError");
    assert_eq!(
        (detail.code, detail.http_status),
        (user_code as i32, http_status),
        "{status:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_volume_is_created_filled_listed_and_deleted_over_grpc() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let mut volumes = serve().await.created().await;
    assert!(volumes.root.is_dir());

    let made = volumes
        .client
        .create_dir(CreateDirRequest {
            volume: Some(volumes.info.clone()),
            path: "docs/deep".to_owned(),
            create_parents: true,
            mode: Some(0o750),
            uid: Some(1001),
            gid: Some(1002),
        })
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!((made.path.as_str(), made.name.as_str()), ("/docs/deep", "deep"));
    assert_eq!((made.mode, made.uid, made.gid), (0o750, 1001, 1002));
    assert_eq!(made.r#type, FileType::Directory as i32);

    let start = VolumeFileCreateStart {
        mode: Some(0o640),
        uid: Some(1003),
        gid: Some(1004),
        ..volumes.start("docs/deep/a.txt")
    };
    volumes
        .upload(start, vec![b"hello ".to_vec(), b"world".to_vec()])
        .await
        .unwrap();
    let metadata = std::fs::metadata(volumes.root.join("docs/deep/a.txt")).unwrap();
    assert_eq!(
        (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
        (1003, 1004, 0o640)
    );

    let stat = volumes
        .client
        .stat_path(StatPathRequest {
            volume: Some(volumes.info.clone()),
            path: "/docs/deep/a.txt".to_owned(),
            follow_symlinks: true,
        })
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!((stat.size, stat.r#type), (11, FileType::File as i32));
    assert!(stat.modified_time.is_some() && stat.created_time.is_some() && stat.accessed_time.is_some());

    let listed = volumes
        .client
        .list_dir(ListDirRequest {
            volume: Some(volumes.info.clone()),
            path: "/".to_owned(),
            depth: 3,
        })
        .await
        .unwrap()
        .into_inner();
    let paths: Vec<&str> = listed
        .files
        .iter()
        .map(|item| item.entry.as_ref().unwrap().path.as_str())
        .collect();
    assert_eq!(paths, ["/docs", "/docs/deep", "/docs/deep/a.txt"]);

    let updated = volumes
        .client
        .update_path(UpdatePathRequest {
            volume: Some(volumes.info.clone()),
            path: "docs/deep/a.txt".to_owned(),
            mode: Some(0o600),
            uid: None,
            gid: Some(77),
        })
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!((updated.mode, updated.uid, updated.gid), (0o600, 1003, 77));

    let downloaded = volumes.download("docs/deep/a.txt").await.unwrap();
    assert_eq!(content_of(&downloaded), b"hello world");
    assert!(matches!(downloaded.first(), Some(get_file_response::Message::Start(start)) if start.size == 11));
    assert!(matches!(downloaded.last(), Some(get_file_response::Message::Finish(_))));

    volumes
        .client
        .delete_path(DeletePathRequest {
            volume: Some(volumes.info.clone()),
            path: "docs".to_owned(),
        })
        .await
        .unwrap();
    assert!(!volumes.root.join("docs").exists());

    volumes
        .client
        .delete_volume(DeleteVolumeRequest {
            volume: Some(volumes.info.clone()),
        })
        .await
        .unwrap();
    assert!(!volumes.root.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_bigger_than_a_chunk_streams_both_ways_in_order() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let mut volumes = serve().await.created().await;
    let content: Vec<u8> = (0..CHUNK * 2 + 1234).map(|index| (index % 251) as u8).collect();
    let chunks = content.chunks(CHUNK / 2).map(<[u8]>::to_vec).collect();

    volumes.upload(volumes.start("big.bin"), chunks).await.unwrap();
    let downloaded = volumes.download("big.bin").await.unwrap();

    assert_eq!(content_of(&downloaded), content);
    let sizes: Vec<usize> = downloaded
        .iter()
        .filter_map(|message| match message {
            get_file_response::Message::Content(content) => Some(content.content.len()),
            _ => None,
        })
        .collect();
    assert!(sizes.len() >= 3 && sizes.iter().all(|size| *size <= CHUNK), "{sizes:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_volume_type_is_not_supported_with_the_user_error_of_upstream() {
    let mut volumes = serve().await;
    volumes.info.volume_type = "no-such-type".to_owned();

    let created = volumes
        .client
        .create_volume(CreateVolumeRequest {
            volume: Some(volumes.info.clone()),
        })
        .await
        .unwrap_err();
    let stat = volumes
        .client
        .stat_path(StatPathRequest {
            volume: Some(volumes.info.clone()),
            path: "/".to_owned(),
            follow_symlinks: false,
        })
        .await
        .unwrap_err();

    assert_user_error(&created, Code::Internal, UserErrorCode::NotSupported, 500);
    assert_eq!(created.message(), "failed to build volume path: unknown volume type");
    assert_user_error(&stat, Code::Internal, UserErrorCode::NotSupported, 500);
    assert_eq!(stat.message(), "volume type not found");
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_ids_are_invalid_requests() {
    let mut volumes = serve().await;
    volumes.info.team_id = "nope".to_owned();

    let status = volumes
        .client
        .delete_path(DeletePathRequest {
            volume: Some(volumes.info.clone()),
            path: "x".to_owned(),
        })
        .await
        .unwrap_err();

    assert_user_error(&status, Code::InvalidArgument, UserErrorCode::InvalidRequest, 400);
    assert_eq!(status.message(), "invalid team ID \"nope\"");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_conflicting_upload_is_already_exists_and_force_replaces() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let mut volumes = serve().await.created().await;
    volumes
        .upload(volumes.start("f.txt"), vec![b"first".to_vec()])
        .await
        .unwrap();

    let conflict = volumes
        .upload(volumes.start("f.txt"), vec![b"second".to_vec()])
        .await
        .unwrap_err();
    let forced = VolumeFileCreateStart {
        force: true,
        ..volumes.start("f.txt")
    };
    volumes.upload(forced, vec![b"third".to_vec()]).await.unwrap();

    assert_user_error(&conflict, Code::AlreadyExists, UserErrorCode::PathAlreadyExists, 409);
    assert_eq!(
        conflict.message(),
        "failed to open file for create: open /f.txt: file exists"
    );
    assert_eq!(std::fs::read_to_string(volumes.root.join("f.txt")).unwrap(), "third");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_that_does_not_start_with_a_start_message_is_refused() {
    let mut volumes = serve().await.created().await;
    let content = CreateFileRequest {
        message: Some(create_file_request::Message::Content(VolumeFileCreateContent {
            content: b"x".to_vec(),
        })),
    };

    let first_content = volumes
        .client
        .create_file(tokio_stream::iter([content]))
        .await
        .unwrap_err();
    let empty = volumes
        .client
        .create_file(tokio_stream::iter(Vec::<CreateFileRequest>::new()))
        .await
        .unwrap_err();

    assert_eq!(
        (first_content.code(), first_content.message()),
        (Code::Unknown, "expected start message")
    );
    assert_eq!(
        (empty.code(), empty.message()),
        (Code::Unknown, "failed to receive start message: EOF")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_start_message_and_a_stream_without_finish_are_refused() {
    let mut volumes = serve().await.created().await;
    let (one, two, three) = (
        start_message(&volumes, "one"),
        start_message(&volumes, "two"),
        start_message(&volumes, "three"),
    );

    let second_start = volumes
        .client
        .create_file(tokio_stream::iter([one, two]))
        .await
        .unwrap_err();
    let no_finish = volumes
        .client
        .create_file(tokio_stream::iter([three]))
        .await
        .unwrap_err();

    assert_eq!(
        (second_start.code(), second_start.message()),
        (Code::Unknown, "unexpected start message")
    );
    assert_eq!(
        (no_finish.code(), no_finish.message()),
        (Code::Unknown, "failed to receive chunk: EOF")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn paths_and_links_cannot_leave_the_volume() {
    if !nix::unistd::geteuid().is_root() {
        return;
    }
    let mut volumes = serve().await.created().await;
    std::fs::create_dir_all(volumes.root.join("etc")).unwrap();
    std::fs::write(volumes.root.join("etc/passwd"), "inside").unwrap();
    symlink("/etc/passwd", volumes.root.join("absolute")).unwrap();
    symlink("../../../../etc/passwd", volumes.root.join("climbing")).unwrap();

    for path in [
        "etc/passwd",
        "../../etc/passwd",
        "/etc/../etc/passwd",
        "absolute",
        "climbing",
    ] {
        let downloaded = volumes.download(path).await.unwrap();

        assert_eq!(content_of(&downloaded), b"inside", "path {path:?}");
    }
    let stat = volumes
        .client
        .stat_path(StatPathRequest {
            volume: Some(volumes.info.clone()),
            path: "absolute".to_owned(),
            follow_symlinks: false,
        })
        .await
        .unwrap()
        .into_inner()
        .entry
        .unwrap();
    assert_eq!(stat.symlink_target.as_deref(), Some("/etc/passwd"));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_paths_and_the_root_have_their_own_user_errors() {
    let mut volumes = serve().await.created().await;
    let info = Some(volumes.info.clone());

    let missing = volumes.download("missing").await.unwrap_err();
    let root = volumes
        .client
        .delete_path(DeletePathRequest {
            volume: info.clone(),
            path: "/".to_owned(),
        })
        .await
        .unwrap_err();
    let deep = volumes
        .client
        .list_dir(ListDirRequest {
            volume: info,
            path: "/".to_owned(),
            depth: 11,
        })
        .await
        .unwrap_err();

    assert_user_error(&missing, Code::NotFound, UserErrorCode::PathNotFound, 404);
    assert_user_error(&root, Code::InvalidArgument, UserErrorCode::CannotDeleteRoot, 400);
    assert_user_error(&deep, Code::InvalidArgument, UserErrorCode::DepthOutOfRange, 400);
}
