use std::io::ErrorKind;

use tonic::Status;

use super::failure::internal;
use super::service::VolumeServer;
use crate::go_os::{mkdir_all, path_error};
use crate::proto::{CreateVolumeRequest, CreateVolumeResponse, DeleteVolumeRequest, DeleteVolumeResponse};

const VOLUME_ROOT_MODE: u32 = 0o700;
const BUILD_PATH_FAILURE: &str = "failed to build volume path: ";

impl VolumeServer {
    pub fn create_volume_root(&self, request: &CreateVolumeRequest) -> Result<CreateVolumeResponse, Status> {
        let path = self
            .volume_root_path(request.volume.as_ref())
            .map_err(|e| e.wrapped(BUILD_PATH_FAILURE))?;
        mkdir_all(&path, VOLUME_ROOT_MODE).map_err(|e| internal(format!("failed to create volume: {e}")))?;
        Ok(CreateVolumeResponse {
            volume_type: request
                .volume
                .as_ref()
                .map(|volume| volume.volume_type.clone())
                .filter(|volume_type| !volume_type.is_empty()),
        })
    }

    pub fn delete_volume_root(&self, request: &DeleteVolumeRequest) -> Result<DeleteVolumeResponse, Status> {
        let path = self
            .volume_root_path(request.volume.as_ref())
            .map_err(|e| e.wrapped(BUILD_PATH_FAILURE))?;
        std::fs::remove_dir_all(&path)
            .or_else(|e| match e.kind() {
                ErrorKind::NotFound => Ok(()),
                _ => Err(e),
            })
            .map(|()| DeleteVolumeResponse {})
            .map_err(|e| {
                internal(format!(
                    "failed to delete volume: {}",
                    path_error("unlinkat", &path, &e)
                ))
            })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tonic::Code;

    use super::*;
    use crate::proto::{UserErrorCode, VolumeInfo};
    use crate::volumes::test_support::{TestVolume, VOLUME_TYPE, assert_user_error};

    fn create_request(info: &Option<VolumeInfo>) -> CreateVolumeRequest {
        CreateVolumeRequest { volume: info.clone() }
    }

    #[test]
    fn a_volume_is_created_and_deleted() {
        let Some(volume) = TestVolume::new() else { return };

        let created = volume.server.create_volume_root(&create_request(&volume.info)).unwrap();
        assert!(volume.root.is_dir());
        assert_eq!(created.volume_type.as_deref(), Some(VOLUME_TYPE));

        volume
            .server
            .delete_volume_root(&DeleteVolumeRequest {
                volume: volume.info.clone(),
            })
            .unwrap();
        assert!(!volume.root.exists());
    }

    #[test]
    fn a_volume_root_is_private_to_the_node() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::remove_dir(&volume.root).unwrap();

        volume.server.create_volume_root(&create_request(&volume.info)).unwrap();

        assert_eq!(
            std::fs::metadata(&volume.root).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn deleting_a_volume_that_does_not_exist_succeeds() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::remove_dir(&volume.root).unwrap();

        let deleted = volume.server.delete_volume_root(&DeleteVolumeRequest {
            volume: volume.info.clone(),
        });

        assert!(deleted.is_ok());
    }

    #[test]
    fn a_volume_without_a_type_is_not_supported() {
        let Some(volume) = TestVolume::new() else { return };
        let mut info = volume.info.clone().unwrap();
        info.volume_type = String::new();

        let status = volume
            .server
            .create_volume_root(&create_request(&Some(info)))
            .unwrap_err();

        assert_user_error(&status, Code::Internal, UserErrorCode::NotSupported);
        assert_eq!(status.message(), "failed to build volume path: unknown volume type");
    }

    #[test]
    fn a_bad_team_id_keeps_the_detail_and_prefixes_the_message() {
        let Some(volume) = TestVolume::new() else { return };
        let mut info = volume.info.clone().unwrap();
        info.team_id = "not-a-uuid".to_owned();

        let status = volume
            .server
            .delete_volume_root(&DeleteVolumeRequest { volume: Some(info) })
            .unwrap_err();

        assert_user_error(&status, Code::InvalidArgument, UserErrorCode::InvalidRequest);
        assert_eq!(
            status.message(),
            "failed to build volume path: invalid team ID \"not-a-uuid\""
        );
    }
}
