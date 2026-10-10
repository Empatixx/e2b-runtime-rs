use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use tempfile::TempDir;
use tonic::{Code, Status};

use super::VolumeServer;
use super::chrooted::Chrooted;
use crate::proto::{UserErrorCode, VolumeInfo};
use crate::server::config::volume_mounts::VolumeMounts;
use crate::server::user_error::user_error_of;

pub const VOLUME_TYPE: &str = "test-vt";

pub struct TestVolume {
    pub server: VolumeServer,
    pub root: PathBuf,
    pub info: Option<VolumeInfo>,
    _mounts: TempDir,
}

impl TestVolume {
    pub fn new() -> Option<Self> {
        nix::unistd::geteuid().is_root().then(Self::create)
    }

    fn create() -> Self {
        let mounts_dir = tempfile::tempdir().unwrap();
        let vars = HashMap::from([(
            "PERSISTENT_VOLUME_MOUNTS".to_owned(),
            format!("{VOLUME_TYPE}:{}", mounts_dir.path().display()),
        )]);
        let server = VolumeServer::new(VolumeMounts::from_vars(&vars).unwrap());
        let info = VolumeInfo {
            volume_type: VOLUME_TYPE.to_owned(),
            team_id: uuid::Uuid::new_v4().to_string(),
            volume_id: uuid::Uuid::new_v4().to_string(),
        };
        let root = server.volume_root_path(Some(&info)).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        Self {
            server,
            root,
            info: Some(info),
            _mounts: mounts_dir,
        }
    }

    pub fn host(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    pub fn write(&self, relative: &str, content: &str) -> PathBuf {
        let path = self.host(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    pub fn fs(&self) -> Chrooted {
        Chrooted::open(&self.root).unwrap()
    }
}

pub fn assert_user_error(status: &Status, code: Code, user_code: UserErrorCode) {
    assert_eq!(status.code(), code, "{status:?}");
    let detail = user_error_of(status).expect("expected UserError detail not found");
    assert_eq!(detail.code, user_code as i32, "{status:?}");
}

pub fn assert_owner_and_mode(path: &Path, uid: u32, gid: u32, mode: u32) {
    let metadata = std::fs::metadata(path).unwrap();
    assert_eq!(
        (metadata.uid(), metadata.gid(), metadata.mode() & 0o777),
        (uid, gid, mode)
    );
}
