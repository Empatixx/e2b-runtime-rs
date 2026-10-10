use tonic::Status;

use super::chrooted::Chrooted;
use super::chrooted::file::ConfinedFile;
use super::failure::{internal, process_error};
use super::parents::ensure_dirs;
use super::service::{DEFAULT_FILE_MODE, DEFAULT_GROUP_ID, DEFAULT_OWNER_ID, VolumeServer};
use crate::go_path;
use crate::proto::{CreateFileResponse, VolumeFileCreateStart};

pub struct FileUpload {
    fs: Chrooted,
    file: ConfinedFile,
    path: String,
    uid: u32,
    gid: u32,
    mode: u32,
}

impl VolumeServer {
    pub fn begin_upload(&self, start: &VolumeFileCreateStart) -> Result<FileUpload, Status> {
        let confined = self.confined(start.volume.as_ref(), &start.path)?;
        let uid = start.uid.unwrap_or(DEFAULT_OWNER_ID);
        let gid = start.gid.unwrap_or(DEFAULT_GROUP_ID);
        let mode = start.mode.unwrap_or(DEFAULT_FILE_MODE);
        if start.force {
            ensure_dirs(&confined.fs, &go_path::dir(&confined.path), uid, gid)
                .map_err(|e| internal(format!("failed to prepare parent directories: {e}")))?;
        }
        let opened = match start.force {
            true => confined.fs.create_truncating(&confined.path, mode),
            false => confined.fs.create_new(&confined.path, mode),
        };
        opened
            .map(|file| FileUpload {
                fs: confined.fs,
                file,
                path: confined.path,
                uid,
                gid,
                mode,
            })
            .map_err(|e| process_error("failed to open file for create", &e))
    }
}

impl FileUpload {
    pub fn written(mut self, content: &[u8]) -> Result<Self, Status> {
        self.file
            .write_all(content)
            .map(|()| self)
            .map_err(|e| internal(format!("failed to write file content: {e}")))
    }

    pub fn finish(self) -> Result<CreateFileResponse, Status> {
        self.file
            .sync()
            .map_err(|e| internal(format!("failed to sync file to disk: {e}")))?;
        self.fs
            .chown(&self.path, Some(self.uid), Some(self.gid))
            .map_err(|e| internal(format!("failed to set file ownership: {e}")))?;
        self.fs
            .chmod(&self.path, self.mode)
            .map_err(|e| internal(format!("failed to set file mode: {e}")))?;
        self.fs
            .stat(&self.path)
            .map(|info| CreateFileResponse {
                entry: Some(self.fs.entry_info(&self.path, &info).into()),
            })
            .map_err(|e| internal(format!("failed to stat created file: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::UserErrorCode;
    use crate::volumes::test_support::{TestVolume, assert_owner_and_mode, assert_user_error};

    fn start(volume: &TestVolume, path: &str, force: bool) -> VolumeFileCreateStart {
        VolumeFileCreateStart {
            volume: volume.info.clone(),
            path: path.to_owned(),
            force,
            ..Default::default()
        }
    }

    #[test]
    fn creates_a_file_from_streamed_content() {
        let Some(volume) = TestVolume::new() else { return };

        let upload = volume
            .server
            .begin_upload(&start(&volume, "test-create.txt", false))
            .unwrap();
        let created = upload
            .written(b"hello ")
            .unwrap()
            .written(b"world")
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(volume.host("test-create.txt")).unwrap(),
            "hello world"
        );
        let entry = created.entry.unwrap();
        assert_eq!(
            (entry.path.as_str(), entry.size, entry.mode),
            ("/test-create.txt", 11, 0o666)
        );
        assert_owner_and_mode(
            &volume.host("test-create.txt"),
            DEFAULT_OWNER_ID,
            DEFAULT_GROUP_ID,
            0o666,
        );
    }

    #[test]
    fn force_creates_the_missing_parents_owned_like_the_file() {
        let Some(volume) = TestVolume::new() else { return };
        let forced = VolumeFileCreateStart {
            uid: Some(1000),
            gid: Some(2000),
            ..start(&volume, "nested/dir/test-create.txt", true)
        };

        volume.server.begin_upload(&forced).unwrap().finish().unwrap();

        assert!(volume.host("nested/dir/test-create.txt").is_file());
        assert_owner_and_mode(&volume.host("nested/dir"), 1000, 2000, 0o777);
        assert_owner_and_mode(&volume.host("nested/dir/test-create.txt"), 1000, 2000, 0o666);
    }

    #[test]
    fn force_truncates_an_existing_file() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("existing.txt", "a long old content");

        volume
            .server
            .begin_upload(&start(&volume, "existing.txt", true))
            .unwrap()
            .written(b"new")
            .unwrap()
            .finish()
            .unwrap();

        assert_eq!(std::fs::read_to_string(volume.host("existing.txt")).unwrap(), "new");
    }

    #[test]
    fn an_existing_file_without_force_is_a_conflict() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("already-exists.txt", "existing");

        let status = volume
            .server
            .begin_upload(&start(&volume, "already-exists.txt", false))
            .err()
            .unwrap();

        assert_user_error(&status, Code::AlreadyExists, UserErrorCode::PathAlreadyExists);
        assert_eq!(
            status.message(),
            "failed to open file for create: open /already-exists.txt: file exists"
        );
        assert_eq!(
            std::fs::read_to_string(volume.host("already-exists.txt")).unwrap(),
            "existing"
        );
    }

    #[test]
    fn a_mode_is_applied_past_the_umask() {
        let Some(volume) = TestVolume::new() else { return };
        let moded = VolumeFileCreateStart {
            mode: Some(0o640),
            ..start(&volume, "moded.txt", false)
        };

        volume.server.begin_upload(&moded).unwrap().finish().unwrap();

        assert_owner_and_mode(&volume.host("moded.txt"), DEFAULT_OWNER_ID, DEFAULT_GROUP_ID, 0o640);
    }

    #[test]
    fn a_missing_parent_without_force_is_not_found() {
        let Some(volume) = TestVolume::new() else { return };

        let status = volume
            .server
            .begin_upload(&start(&volume, "missing/file.txt", false))
            .err()
            .unwrap();

        assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
    }
}
