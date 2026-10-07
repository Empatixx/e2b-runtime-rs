use std::fs::Metadata;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::Path;

use buffa::EnumValue;
use buffa_types::google::protobuf::Timestamp;
use connectrpc::ConnectError;

use crate::fsutil::{eval_symlinks, os_error};
use crate::proto::filesystem::{EntryInfo, FileType};
use crate::user::{group_name, user_name};

const METADATA_XATTR_PREFIX: &str = "user.e2b.";

pub fn entry_info(path: &Path) -> Result<EntryInfo, ConnectError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ConnectError::not_found(format!("file not found: {}", os_error("lstat", path, &e))));
        }
        Err(e) => {
            return Err(ConnectError::internal(format!("error getting file info: {}", os_error("lstat", path, &e))));
        }
    };

    let (file_type, mode, symlink_target) = if meta.is_symlink() {
        let target = eval_symlinks(path).unwrap_or_else(|_| path.to_path_buf());
        let (file_type, mode) = std::fs::metadata(&target)
            .map(|target_meta| (file_type(&target_meta), target_meta.mode() & 0o777))
            .unwrap_or((FileType::FILE_TYPE_UNSPECIFIED, 0));
        (file_type, mode, Some(target.display().to_string()))
    } else {
        (file_type(&meta), meta.mode() & 0o777, None)
    };

    let mut entry = EntryInfo {
        name: path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()),
        r#type: EnumValue::from(file_type),
        path: path.display().to_string(),
        size: meta.size() as i64,
        mode,
        permissions: permissions(&meta),
        owner: user_name(meta.uid()),
        group: group_name(meta.gid()),
        modified_time: modified_time(&meta).into(),
        is_symlink: symlink_target.is_some(),
        symlink_target,
        ..Default::default()
    };
    entry.metadata.extend(xattr_metadata(path));
    Ok(entry)
}

fn file_type(meta: &Metadata) -> FileType {
    if meta.is_file() {
        FileType::FILE_TYPE_FILE
    } else if meta.is_dir() {
        FileType::FILE_TYPE_DIRECTORY
    } else {
        FileType::FILE_TYPE_UNSPECIFIED
    }
}

fn permissions(meta: &Metadata) -> String {
    let file_type = meta.file_type();
    let mode = meta.mode();
    let flags = [
        (file_type.is_dir(), 'd'),
        (file_type.is_symlink(), 'L'),
        (file_type.is_block_device() || file_type.is_char_device(), 'D'),
        (file_type.is_fifo(), 'p'),
        (file_type.is_socket(), 'S'),
        (mode & 0o4000 != 0, 'u'),
        (mode & 0o2000 != 0, 'g'),
        (file_type.is_char_device(), 'c'),
        (mode & 0o1000 != 0, 't'),
    ];

    let mut text: String = flags.iter().filter(|(set, _)| *set).map(|(_, c)| c).collect();
    if text.is_empty() {
        text.push('-');
    }
    text.extend(
        "rwxrwxrwx"
            .chars()
            .enumerate()
            .map(|(i, c)| if mode & (0o400 >> i) != 0 { c } else { '-' }),
    );
    text
}

fn modified_time(meta: &Metadata) -> Option<Timestamp> {
    let (seconds, nanos) = (meta.mtime(), meta.mtime_nsec());
    (seconds != 0 || nanos != 0).then(|| Timestamp { seconds, nanos: nanos as i32, ..Default::default() })
}

fn xattr_metadata(path: &Path) -> Vec<(String, String)> {
    let Ok(names) = xattr::list_deref(path) else {
        return Vec::new();
    };
    names
        .filter_map(|name| {
            let key = name.to_str()?.strip_prefix(METADATA_XATTR_PREFIX)?.to_owned();
            let value = xattr::get_deref(path, &name).ok().flatten()?;
            Some((key, String::from_utf8(value).ok()?))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use connectrpc::ErrorCode;

    use super::*;

    fn permissions_of(path: &Path) -> String {
        permissions(&std::fs::symlink_metadata(path).unwrap())
    }

    #[test]
    fn permission_strings() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"").unwrap();

        for (mode, expected) in [(0o644, "-rw-r--r--"), (0o755, "-rwxr-xr-x"), (0o000, "----------"), (0o4755, "urwxr-xr-x"), (0o2755, "grwxr-xr-x")] {
            std::fs::set_permissions(&file, PermissionsExt::from_mode(mode)).unwrap();
            assert_eq!(permissions_of(&file), expected);
        }

        let dir = tmp.path().join("d");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, PermissionsExt::from_mode(0o1777)).unwrap();
        assert_eq!(permissions_of(&dir), "dtrwxrwxrwx");

        let link = tmp.path().join("l");
        symlink(&file, &link).unwrap();
        assert!(permissions_of(&link).starts_with('L'));

        assert_eq!(permissions_of(Path::new("/dev/null")), "Dcrw-rw-rw-");
    }

    #[test]
    fn file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, b"hello").unwrap();
        std::fs::set_permissions(&file, PermissionsExt::from_mode(0o644)).unwrap();

        let entry = entry_info(&file).unwrap();
        assert_eq!(entry.name, "a.txt");
        assert_eq!(entry.path, file.to_str().unwrap());
        assert_eq!(entry.r#type, EnumValue::from(FileType::FILE_TYPE_FILE));
        assert_eq!(entry.size, 5);
        assert_eq!(entry.mode, 0o644);
        assert_eq!(entry.permissions, "-rw-r--r--");
        assert_eq!(entry.owner, user_name(nix::unistd::getuid().as_raw()));
        assert!(entry.modified_time.is_set());
        assert_eq!(entry.symlink_target, None);
        assert!(!entry.is_symlink);
    }

    #[test]
    fn symlink_takes_type_and_mode_from_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.sh");
        std::fs::write(&target, b"#!/bin/sh").unwrap();
        std::fs::set_permissions(&target, PermissionsExt::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        symlink(&target, &link).unwrap();

        let entry = entry_info(&link).unwrap();
        assert_eq!(entry.name, "link");
        assert_eq!(entry.r#type, EnumValue::from(FileType::FILE_TYPE_FILE));
        assert_eq!(entry.mode, 0o755);
        assert!(entry.permissions.starts_with('L'));
        let resolved = std::fs::canonicalize(&target).unwrap();
        assert_eq!(entry.symlink_target.as_deref(), resolved.to_str());
        assert!(entry.is_symlink);
    }

    #[test]
    fn broken_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("broken");
        symlink(tmp.path().join("missing"), &link).unwrap();

        let entry = entry_info(&link).unwrap();
        assert_eq!(entry.r#type, EnumValue::from(FileType::FILE_TYPE_UNSPECIFIED));
        assert_eq!(entry.mode, 0);
        assert_eq!(entry.symlink_target.as_deref(), link.to_str());
        assert!(entry.is_symlink);
    }

    #[test]
    fn not_found() {
        let err = entry_info(Path::new("/definitely/not/here")).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert_eq!(
            err.message.as_deref(),
            Some("file not found: lstat /definitely/not/here: no such file or directory")
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn user_metadata_from_xattrs() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"").unwrap();
        if xattr::set(&file, "user.e2b.owner", b"team-a").is_err() {
            return;
        }
        xattr::set(&file, "user.other", b"x").unwrap();
        let entry = entry_info(&file).unwrap();
        assert_eq!(entry.metadata.len(), 1);
        assert_eq!(entry.metadata.get("owner").map(String::as_str), Some("team-a"));
    }
}
