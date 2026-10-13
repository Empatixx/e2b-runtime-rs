use tonic::Status;

use super::api_error::ApiError;
use super::chrooted::Chrooted;
use super::chrooted::error::PathError;
use super::failure::internal;
use super::service::VolumeServer;
use crate::go_path;
use crate::proto::{ListDirRequest, ListDirResponse, VolumeDirectoryItem};

const MIN_DEPTH: u32 = 1;
const MAX_DEPTH: u32 = 10;

struct ListError {
    cause: PathError,
    text: String,
}

impl VolumeServer {
    pub fn list_directory(&self, request: &ListDirRequest) -> Result<ListDirResponse, Status> {
        let confined = self.confined(request.volume.as_ref(), &request.path)?;
        let depth = request.depth.max(MIN_DEPTH);
        if depth > MAX_DEPTH {
            return Err(
                ApiError::depth_out_of_range(format!("depth must be between {MIN_DEPTH} and {MAX_DEPTH}"))
                    .into_status(),
            );
        }
        list_recursive(&confined.fs, &confined.path, depth)
            .map(|files| ListDirResponse { files })
            .map_err(|error| match error.cause.is_not_found() {
                true => {
                    ApiError::path_not_found(format!("failed to read: {:?} not found.", request.path)).into_status()
                }
                false => internal(format!("failed to read directory {:?}: {}", confined.path, error.text)),
            })
    }
}

fn list_recursive(fs: &Chrooted, path: &str, depth: u32) -> Result<Vec<VolumeDirectoryItem>, ListError> {
    if depth == 0 {
        return Ok(Vec::new());
    }
    fs.read_dir(path)
        .map_err(|cause| ListError {
            text: format!("failed to read directory {path:?}: {cause}"),
            cause,
        })?
        .iter()
        .try_fold(Vec::new(), |mut items, info| {
            let item_path = go_path::join(&[path, &info.name]);
            items.push(VolumeDirectoryItem {
                entry: Some(fs.entry_info(&item_path, info).into()),
            });
            if info.is_dir() && depth > 1 {
                match list_recursive(fs, &item_path, depth - 1) {
                    Ok(children) => items.extend(children),
                    Err(error) if error.cause.is_not_found() => {
                        tracing::warn!(path = %item_path, "directory deleted during traversal");
                    }
                    Err(error) => return Err(error),
                }
            }
            Ok(items)
        })
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::{EntryInfo, FileType, StatPathRequest, UserErrorCode};
    use crate::volumes::test_support::{TestVolume, assert_user_error};

    fn list(volume: &TestVolume, path: &str, depth: u32) -> Result<Vec<String>, Status> {
        volume
            .server
            .list_directory(&ListDirRequest {
                volume: volume.info.clone(),
                path: path.to_owned(),
                depth,
            })
            .map(|listed| listed.files.into_iter().map(|item| item.entry.unwrap().path).collect())
    }

    fn sorted(mut paths: Vec<String>) -> Vec<String> {
        paths.sort();
        paths
    }

    fn nested_volume() -> Option<TestVolume> {
        let volume = TestVolume::new()?;
        volume.write("dir/test.txt", "test");
        volume.write("dir/deep/test.txt", "deep test");
        volume.write("dir/deep/deeper/test.txt", "deeper test");
        Some(volume)
    }

    #[test]
    fn symlinks_resolve_inside_the_volume() {
        let Some(volume) = TestVolume::new() else { return };
        volume.write("target.txt", "target");
        std::os::unix::fs::symlink("/target.txt", volume.host("etc")).unwrap();
        std::os::unix::fs::symlink("/nowhere", volume.host("dangling")).unwrap();
        assert!(
            std::path::Path::new("/etc").is_dir(),
            "the test needs a host directory sharing the link's name"
        );

        let listed = volume
            .server
            .list_directory(&ListDirRequest {
                volume: volume.info.clone(),
                path: "/".to_owned(),
                depth: 0,
            })
            .unwrap();

        let by_path: std::collections::HashMap<String, EntryInfo> = listed
            .files
            .into_iter()
            .map(|item| item.entry.unwrap())
            .map(|entry| (entry.path.clone(), entry))
            .collect();
        assert_eq!(by_path.len(), 3);
        assert_eq!(by_path["/etc"].r#type, FileType::File as i32);
        assert_eq!(by_path["/etc"].symlink_target.as_deref(), Some("/target.txt"));
        assert_eq!(by_path["/dangling"].r#type, FileType::Unspecified as i32);
        assert_eq!(by_path["/dangling"].symlink_target.as_deref(), Some("/dangling"));
        let stat = volume
            .server
            .stat_entry(&StatPathRequest {
                volume: volume.info.clone(),
                path: "/etc".to_owned(),
                follow_symlinks: false,
            })
            .unwrap()
            .entry
            .unwrap();
        assert_eq!(stat.r#type, by_path["/etc"].r#type);
        assert_eq!(stat.symlink_target, by_path["/etc"].symlink_target);
    }

    #[test]
    fn depth_zero_and_one_list_the_children_only() {
        let Some(volume) = nested_volume() else { return };

        for depth in [0, 1] {
            assert_eq!(
                sorted(list(&volume, "dir", depth).unwrap()),
                ["/dir/deep", "/dir/test.txt"]
            );
        }
    }

    #[test]
    fn depth_two_lists_one_level_of_grandchildren() {
        let Some(volume) = nested_volume() else { return };

        assert_eq!(
            sorted(list(&volume, "dir", 2).unwrap()),
            ["/dir/deep", "/dir/deep/deeper", "/dir/deep/test.txt", "/dir/test.txt"]
        );
    }

    #[test]
    fn a_directory_is_followed_by_its_children() {
        let Some(volume) = nested_volume() else { return };

        assert_eq!(
            list(&volume, "dir", 3).unwrap(),
            [
                "/dir/deep",
                "/dir/deep/deeper",
                "/dir/deep/deeper/test.txt",
                "/dir/deep/test.txt",
                "/dir/test.txt"
            ]
        );
    }

    #[test]
    fn a_missing_directory_is_not_found() {
        let Some(volume) = nested_volume() else { return };

        let status = list(&volume, "non-existent-dir", 0).unwrap_err();

        assert_user_error(&status, Code::NotFound, UserErrorCode::PathNotFound);
        assert_eq!(status.message(), "failed to read: \"non-existent-dir\" not found.");
    }

    #[test]
    fn a_depth_above_the_limit_is_out_of_range() {
        let Some(volume) = nested_volume() else { return };

        let status = list(&volume, "dir", 11).unwrap_err();

        assert_user_error(&status, Code::InvalidArgument, UserErrorCode::DepthOutOfRange);
        assert_eq!(status.message(), "depth must be between 1 and 10");
        assert!(list(&volume, "dir", 10).is_ok());
    }

    #[test]
    fn listing_a_file_repeats_the_prefix_like_upstream() {
        let Some(volume) = nested_volume() else { return };

        let status = list(&volume, "dir/test.txt", 0).unwrap_err();

        assert_eq!(status.code(), Code::Unknown);
        assert_eq!(
            status.message(),
            "failed to read directory \"/dir/test.txt\": failed to read directory \"/dir/test.txt\": readdir /dir/test.txt: not a directory"
        );
    }
}
