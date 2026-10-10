use tonic::Status;

use super::api_error::ApiError;
use super::failure::internal;
use super::service::VolumeServer;
use crate::go_strconv::quote;
use crate::proto::{DeletePathRequest, DeletePathResponse};

impl VolumeServer {
    pub fn remove_path(&self, request: &DeletePathRequest) -> Result<DeletePathResponse, Status> {
        let confined = self.confined(request.volume.as_ref(), &request.path)?;
        let (fs, path) = (&confined.fs, confined.path.as_str());
        if path == "/" {
            return Err(ApiError::cannot_delete_root("cannot delete root").into_status());
        }
        fs.lstat(path).map_err(|e| match e.is_not_found() {
            true => {
                ApiError::path_not_found(format!("failed to delete: {} not found.", quote(&request.path))).into_status()
            }
            false => internal(format!("failed to stat path: {e}")),
        })?;
        fs.remove_all(path)
            .map(|()| DeletePathResponse {})
            .map_err(|e| internal(format!("failed to delete path: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::UserErrorCode;
    use crate::volumes::test_support::{TestVolume, assert_user_error};

    fn delete(volume: &TestVolume, path: &str) -> Result<DeletePathResponse, Status> {
        volume.server.remove_path(&DeletePathRequest {
            volume: volume.info.clone(),
            path: path.to_owned(),
        })
    }

    #[test]
    fn deletes_a_file() {
        let Some(volume) = TestVolume::new() else { return };
        let path = volume.write("file-to-delete.txt", "test content");

        delete(&volume, "file-to-delete.txt").unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn deletes_an_empty_directory() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::create_dir(volume.host("empty-dir-to-delete")).unwrap();

        delete(&volume, "empty-dir-to-delete").unwrap();

        assert!(!volume.host("empty-dir-to-delete").exists());
    }

    #[test]
    fn deletes_a_non_empty_directory_recursively() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("non-empty-dir/child.txt", "child content");
        volume.write("non-empty-dir/nested/nested-child.txt", "nested content");

        delete(&volume, "non-empty-dir").unwrap();

        assert!(!volume.host("non-empty-dir").exists());
    }

    #[test]
    fn a_missing_path_is_not_found() {
        let Some(volume) = TestVolume::new() else { return };

        let status = delete(&volume, "non-existent-path").unwrap_err();

        assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
        assert_eq!(status.message(), "failed to delete: \"non-existent-path\" not found.");
    }

    #[test]
    fn the_root_cannot_be_deleted() {
        let Some(volume) = TestVolume::new() else { return };

        for path in ["/", "", ".", "../.."] {
            let status = delete(&volume, path).unwrap_err();

            assert_user_error(&status, Code::InvalidArgument, UserErrorCode::CannotDeleteRoot);
            assert_eq!(status.message(), "cannot delete root");
        }
    }

    #[test]
    fn deleting_a_link_keeps_its_target() {
        let Some(volume) = TestVolume::new() else { return };
        let target = volume.write("symlink-target.txt", "target content");
        std::os::unix::fs::symlink("symlink-target.txt", volume.host("symlink-to-delete")).unwrap();

        delete(&volume, "symlink-to-delete").unwrap();

        assert!(std::fs::symlink_metadata(volume.host("symlink-to-delete")).is_err());
        assert!(target.exists());
    }

    #[test]
    fn deleting_a_broken_link_succeeds() {
        let Some(volume) = TestVolume::new() else { return };
        std::os::unix::fs::symlink("broken-symlink-target.txt", volume.host("broken-symlink-to-delete")).unwrap();

        delete(&volume, "broken-symlink-to-delete").unwrap();

        assert!(std::fs::symlink_metadata(volume.host("broken-symlink-to-delete")).is_err());
    }

    #[test]
    fn deleting_a_link_to_a_directory_outside_the_volume_keeps_the_host_untouched() {
        let Some(volume) = TestVolume::new() else { return };
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("keep"), "host").unwrap();
        std::os::unix::fs::symlink(outside.path(), volume.host("escape")).unwrap();

        delete(&volume, "escape/keep").unwrap_err();
        delete(&volume, "escape").unwrap();

        assert!(outside.path().join("keep").exists());
    }
}
