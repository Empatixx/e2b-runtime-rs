use tonic::Status;

use super::api_error::ApiError;
use super::failure::internal;
use super::service::VolumeServer;
use crate::proto::{StatPathRequest, StatPathResponse};

impl VolumeServer {
    pub fn stat_entry(&self, request: &StatPathRequest) -> Result<StatPathResponse, Status> {
        let confined = self.confined(request.volume.as_ref(), &request.path)?;
        confined
            .fs
            .get_entry(&confined.path)
            .map(|entry| StatPathResponse {
                entry: Some(entry.into()),
            })
            .map_err(|e| match e.is_not_found() {
                true => {
                    ApiError::path_not_found(format!("failed to stat: {:?} not found.", request.path)).into_status()
                }
                false => internal(format!("failed to stat path: {e}")),
            })
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::{FileType, UserErrorCode};
    use crate::volumes::test_support::{TestVolume, assert_user_error};

    fn stat(volume: &TestVolume, path: &str) -> Result<crate::proto::EntryInfo, Status> {
        volume
            .server
            .stat_entry(&StatPathRequest {
                volume: volume.info.clone(),
                path: path.to_owned(),
                follow_symlinks: false,
            })
            .map(|stat| stat.entry.unwrap())
    }

    #[test]
    fn stats_a_file() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("test.txt", "test");

        let entry = stat(&volume, "test.txt").unwrap();

        assert_eq!(entry.r#type, FileType::File as i32);
        assert_eq!(entry.path, "/test.txt");
        assert_eq!((entry.name.as_str(), entry.size, entry.mode), ("test.txt", 4, 0o644));
        assert!(entry.modified_time.is_some() && entry.created_time.is_some() && entry.accessed_time.is_some());
    }

    #[test]
    fn stats_a_directory() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::create_dir(volume.host("test-dir")).unwrap();

        assert_eq!(stat(&volume, "test-dir").unwrap().r#type, FileType::Directory as i32);
    }

    #[test]
    fn stats_the_root() {
        let Some(volume) = TestVolume::new() else { return };

        let entry = stat(&volume, "").unwrap();

        assert_eq!((entry.name.as_str(), entry.path.as_str()), ("/", "/"));
        assert_eq!(entry.r#type, FileType::Directory as i32);
    }

    #[test]
    fn a_missing_path_is_not_found() {
        let Some(volume) = TestVolume::new() else { return };

        let status = stat(&volume, "non-existent").unwrap_err();

        assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
        assert_eq!(status.message(), "failed to stat: \"non-existent\" not found.");
    }

    #[test]
    fn a_link_reports_the_type_of_its_target_and_where_it_leads() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("target.txt", "test");
        std::os::unix::fs::symlink("target.txt", volume.host("link.txt")).unwrap();

        let entry = stat(&volume, "link.txt").unwrap();

        assert_eq!(entry.r#type, FileType::File as i32);
        assert_eq!(entry.symlink_target.as_deref(), Some("/target.txt"));
        assert_eq!(entry.name, "link.txt");
    }
}
