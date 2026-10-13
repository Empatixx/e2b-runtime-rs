use tonic::Status;

use super::api_error::ApiError;
use super::chrooted::Chrooted;
use super::chrooted::error::PathError;
use super::failure::{internal, process_error};
use super::parents::ensure_dirs;
use super::service::{DEFAULT_DIR_MODE, DEFAULT_GROUP_ID, DEFAULT_OWNER_ID, VolumeServer};
use crate::go_path;
use crate::proto::{CreateDirRequest, CreateDirResponse};

const CREATE_FAILURE: &str = "failed to create directory";

impl VolumeServer {
    pub fn make_directory(&self, request: &CreateDirRequest) -> Result<CreateDirResponse, Status> {
        let confined = self.confined(request.volume.as_ref(), &request.path)?;
        let (fs, path) = (&confined.fs, confined.path.as_str());
        let uid = request.uid.unwrap_or(DEFAULT_OWNER_ID);
        let gid = request.gid.unwrap_or(DEFAULT_GROUP_ID);
        let mode = request.mode.unwrap_or(DEFAULT_DIR_MODE);
        if request.create_parents {
            ensure_dirs(fs, &go_path::dir(path), uid, gid)
                .map_err(|e| internal(format!("failed to prepare parent directories: {e}")))?;
        }
        if create_directory(fs, path, mode, request.create_parents)? {
            fs.chown(path, Some(uid), Some(gid))
                .map_err(|e| process_error("failed to chown directory", &e))?;
            fs.chmod(path, mode)
                .map_err(|e| process_error("failed to chmod directory", &e))?;
        }
        let stat = fs
            .stat(path)
            .map_err(|e| internal(format!("failed to stat directory: {e}")))?;
        Ok(CreateDirResponse {
            entry: Some(fs.entry_info(path, &stat).into()),
        })
    }
}

fn create_directory(fs: &Chrooted, path: &str, mode: u32, create_parents: bool) -> Result<bool, Status> {
    if path == "/" {
        return match create_parents {
            true => Ok(false),
            false => Err(
                ApiError::path_already_exists(format!("{CREATE_FAILURE}: mkdir {path}: file already exists"))
                    .into_status(),
            ),
        };
    }
    match fs.mkdir(path, mode) {
        Ok(()) => Ok(true),
        Err(error) if create_parents && error.is_exist() => existing_directory(fs, path, &error).map(|()| false),
        Err(error) => Err(process_error(CREATE_FAILURE, &error)),
    }
}

fn existing_directory(fs: &Chrooted, path: &str, mkdir_error: &PathError) -> Result<(), Status> {
    match fs.stat(path) {
        Err(e) => Err(internal(format!("failed to verify existing path {path:?}: {e}"))),
        Ok(info) if info.is_dir() => Ok(()),
        Ok(_) => Err(process_error(CREATE_FAILURE, mkdir_error)),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tonic::Code;

    use super::*;
    use crate::proto::{FileType, UserErrorCode};
    use crate::volumes::test_support::{TestVolume, assert_owner_and_mode, assert_user_error};

    fn request(volume: &TestVolume, path: &str) -> CreateDirRequest {
        CreateDirRequest {
            volume: volume.info.clone(),
            path: path.to_owned(),
            ..Default::default()
        }
    }

    fn with_parents(volume: &TestVolume, path: &str) -> CreateDirRequest {
        CreateDirRequest {
            create_parents: true,
            ..request(volume, path)
        }
    }

    #[test]
    fn creates_a_directory() {
        let Some(volume) = TestVolume::new() else { return };

        volume.server.make_directory(&request(&volume, "test-dir")).unwrap();

        assert!(volume.host("test-dir").is_dir());
    }

    #[test]
    fn the_root_exists_already_unless_parents_are_asked_for() {
        let Some(volume) = TestVolume::new() else { return };

        for path in ["/", "", ".", "//"] {
            let status = volume.server.make_directory(&request(&volume, path)).unwrap_err();
            assert_user_error(&status, Code::AlreadyExists, UserErrorCode::PathAlreadyExists);
            assert_eq!(
                status.message(),
                "failed to create directory: mkdir /: file already exists"
            );

            let created = volume.server.make_directory(&with_parents(&volume, path)).unwrap();
            let entry = created.entry.unwrap();
            assert_eq!(entry.path, "/");
            assert_eq!(entry.r#type, FileType::Directory as i32);
        }
    }

    #[test]
    fn creates_nested_directories_with_parents() {
        let Some(volume) = TestVolume::new() else { return };

        volume
            .server
            .make_directory(&with_parents(&volume, "parent/child"))
            .unwrap();

        assert!(volume.host("parent/child").is_dir());
        assert_owner_and_mode(&volume.host("parent"), DEFAULT_OWNER_ID, DEFAULT_GROUP_ID, 0o777);
    }

    #[test]
    fn nested_directories_without_parents_are_not_found() {
        let Some(volume) = TestVolume::new() else { return };

        let status = volume
            .server
            .make_directory(&request(&volume, "another-parent/child"))
            .unwrap_err();

        assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
        assert_eq!(
            status.message(),
            "failed to create directory: mkdir /another-parent/child: no such file or directory"
        );
    }

    #[test]
    fn a_custom_mode_and_owner_are_applied() {
        let Some(volume) = TestVolume::new() else { return };
        let custom = CreateDirRequest {
            mode: Some(0o700),
            uid: Some(1000),
            gid: Some(1000),
            ..request(&volume, "custom-dir")
        };

        let created = volume.server.make_directory(&custom).unwrap();

        assert_owner_and_mode(&volume.host("custom-dir"), 1000, 1000, 0o700);
        let entry = created.entry.unwrap();
        assert_eq!((entry.mode, entry.uid, entry.gid), (0o700, 1000, 1000));
    }

    #[test]
    fn an_existing_directory_is_a_conflict() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::create_dir(volume.host("existing-dir")).unwrap();

        let status = volume
            .server
            .make_directory(&request(&volume, "existing-dir"))
            .unwrap_err();

        assert_user_error(&status, Code::AlreadyExists, UserErrorCode::PathAlreadyExists);
    }

    #[test]
    fn parents_do_not_turn_an_existing_file_into_success() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("existing-file", "test");

        let status = volume
            .server
            .make_directory(&with_parents(&volume, "existing-file"))
            .unwrap_err();

        assert_user_error(&status, Code::AlreadyExists, UserErrorCode::PathAlreadyExists);
    }

    #[test]
    fn parents_do_not_change_an_existing_directory() {
        let Some(volume) = TestVolume::new() else { return };
        let path = volume.host("existing-dir-to-preserve");
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::chown(&path, Some(1500), Some(1600)).unwrap();
        let request = CreateDirRequest {
            mode: Some(0o777),
            uid: Some(1100),
            gid: Some(1200),
            ..with_parents(&volume, "existing-dir-to-preserve")
        };

        volume.server.make_directory(&request).unwrap();

        assert_eq!(
            volume
                .server
                .confined(volume.info.as_ref(), &request.path)
                .unwrap()
                .path,
            "/existing-dir-to-preserve"
        );
        assert_owner_and_mode(&path, 1500, 1600, 0o700);
    }

    #[test]
    fn a_path_through_a_file_is_an_invalid_request() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("file", "x");

        let status = volume.server.make_directory(&request(&volume, "file/sub")).unwrap_err();

        assert_user_error(&status, Code::InvalidArgument, UserErrorCode::InvalidRequest);
    }
}
