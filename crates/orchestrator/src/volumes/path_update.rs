use tonic::Status;

use super::api_error::ApiError;
use super::chrooted::error::PathError;
use super::failure::internal;
use super::service::VolumeServer;
use crate::go_strconv::quote;
use crate::proto::{UpdatePathRequest, UpdatePathResponse};

impl VolumeServer {
    pub fn update_entry(&self, request: &UpdatePathRequest) -> Result<UpdatePathResponse, Status> {
        let confined = self.confined(request.volume.as_ref(), &request.path)?;
        let (fs, path) = (&confined.fs, confined.path.as_str());
        if let Some(mode) = request.mode {
            fs.chmod(path, mode)
                .map_err(|e| update_failure("chmod", "failed to update file mode", &request.path, &e))?;
        }
        if request.uid.is_some() || request.gid.is_some() {
            fs.chown(path, request.uid, request.gid)
                .map_err(|e| update_failure("chown", "failed to update file ownership", &request.path, &e))?;
        }
        fs.stat(path)
            .map(|info| UpdatePathResponse {
                entry: Some(fs.entry_info(path, &info).into()),
            })
            .map_err(|e| update_failure("stat", "failed to stat file", &request.path, &e))
    }
}

fn update_failure(operation: &str, failure: &str, requested_path: &str, error: &PathError) -> Status {
    match error.is_not_found() {
        true => ApiError::path_not_found(format!("failed to {operation}: {} not found.", quote(requested_path)))
            .into_status(),
        false => internal(format!("{failure}: {error}")),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use tonic::Code;

    use super::*;
    use crate::proto::{FileType, UserErrorCode};
    use crate::volumes::test_support::{TestVolume, assert_owner_and_mode, assert_user_error};

    fn update(
        volume: &TestVolume,
        path: &str,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<crate::proto::EntryInfo, Status> {
        volume
            .server
            .update_entry(&UpdatePathRequest {
                volume: volume.info.clone(),
                path: path.to_owned(),
                mode,
                uid,
                gid,
            })
            .map(|updated| updated.entry.unwrap())
    }

    #[test]
    fn updates_the_mode_only() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("update-mode.txt", "test");

        let entry = update(&volume, "update-mode.txt", Some(0o600), None, None).unwrap();

        assert_eq!(entry.mode, 0o600);
        assert_eq!(
            std::fs::metadata(volume.host("update-mode.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn updates_uid_and_gid() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("update-owner.txt", "test");

        let entry = update(&volume, "update-owner.txt", None, Some(1234), Some(5678)).unwrap();

        assert_eq!((entry.uid, entry.gid), (1234, 5678));
        assert_owner_and_mode(&volume.host("update-owner.txt"), 1234, 5678, 0o644);
    }

    #[test]
    fn a_uid_alone_leaves_the_gid_unchanged() {
        let Some(volume) = TestVolume::new() else { return };
        let path = volume.write("update-uid-only.txt", "test");
        std::os::unix::fs::chown(&path, Some(1000), Some(2000)).unwrap();

        let entry = update(&volume, "update-uid-only.txt", None, Some(4321), None).unwrap();

        assert_eq!((entry.uid, entry.gid), (4321, 2000));
    }

    #[test]
    fn a_gid_alone_leaves_the_uid_unchanged() {
        let Some(volume) = TestVolume::new() else { return };
        let path = volume.write("update-gid-only.txt", "test");
        std::os::unix::fs::chown(&path, Some(1000), Some(2000)).unwrap();

        let entry = update(&volume, "update-gid-only.txt", None, None, Some(8765)).unwrap();

        assert_eq!((entry.uid, entry.gid), (1000, 8765));
    }

    #[test]
    fn updates_mode_uid_and_gid_together() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("update-all.txt", "test");

        let entry = update(&volume, "update-all.txt", Some(0o640), Some(111), Some(222)).unwrap();

        assert_eq!((entry.mode, entry.uid, entry.gid), (0o640, 111, 222));
        assert_owner_and_mode(&volume.host("update-all.txt"), 111, 222, 0o640);
    }

    #[test]
    fn updates_a_directory() {
        let Some(volume) = TestVolume::new() else { return };
        std::fs::create_dir(volume.host("update-dir")).unwrap();

        let entry = update(&volume, "update-dir", Some(0o700), None, None).unwrap();

        assert_eq!((entry.r#type, entry.mode), (FileType::Directory as i32, 0o700));
    }

    #[test]
    fn with_no_fields_it_only_stats() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("update-noop.txt", "test");

        let entry = update(&volume, "update-noop.txt", None, None, None).unwrap();

        assert_eq!((entry.path.as_str(), entry.mode), ("/update-noop.txt", 0o644));
    }

    #[test]
    fn a_missing_path_is_not_found_whichever_step_meets_it() {
        let Some(volume) = TestVolume::new() else { return };
        let cases = [
            (
                update(&volume, "missing-chmod", Some(0o600), None, None),
                "failed to chmod: \"missing-chmod\" not found.",
            ),
            (
                update(&volume, "missing-chown", None, Some(111), None),
                "failed to chown: \"missing-chown\" not found.",
            ),
            (
                update(&volume, "missing-stat", None, None, None),
                "failed to stat: \"missing-stat\" not found.",
            ),
        ];

        for (result, message) in cases {
            let status = result.unwrap_err();

            assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
            assert_eq!(status.message(), message);
        }
    }

    #[test]
    fn an_invalid_team_id_is_an_invalid_request() {
        let Some(volume) = TestVolume::new() else { return };
        let mut info = volume.info.clone().unwrap();
        info.team_id = "not-a-uuid".to_owned();

        let status = volume
            .server
            .update_entry(&UpdatePathRequest {
                volume: Some(info),
                path: "whatever.txt".to_owned(),
                mode: Some(0o600),
                ..Default::default()
            })
            .unwrap_err();

        assert_user_error(&status, Code::InvalidArgument, UserErrorCode::InvalidRequest);
    }
}
