//! Building `EntryInfo` for a path (upstream `services/filesystem/utils.go`
//! and `shared/pkg/filesystem/entry.go`).

use std::io;
use std::path::Path;
use std::os::unix::fs::MetadataExt;

use buffa::{Inline, MessageField};
use buffa_types::google::protobuf::Timestamp;
use connectrpc::ConnectError;

use crate::proto::filesystem::{EntryInfo, FileType};

/// What kind of filesystem entry we are describing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// Same text as Go's `os.FileMode.String()`, e.g. `0o755` dir -> `"drwxr-xr-x"`.
///
/// `mode` = permission bits incl. setuid/setgid/sticky (`st_mode & 0o7777`).
pub fn permissions_string(kind: Kind, mode: u32) -> String {
    let mut s = String::new();
    match kind {
        Kind::Dir => {
            s.push_str("d")
        }
        Kind::Symlink => {
            s.push_str("L")
        }
        Kind::File => {
        }
    }

    let special_permissions = [(0o4000,'u'), (0o2000,'g'), (0o1000,'t')];
    let permissions = [(0o400, 'r'), (0o200, 'w'), (0o100, 'x'),
        (0o040, 'r'), (0o020, 'w'), (0o010, 'x'),
        (0o004, 'r'), (0o002, 'w'), (0o001, 'x')];
    for (bit, c) in special_permissions.iter() {
        if mode & bit != 0 {
            s.push(*c);
        }
    }
    if s.is_empty() {
        s.push('-');
    }
    for (bit, c) in permissions.iter() {
        if mode & bit != 0 {
            s.push(*c);
        } else {
            s.push('-')
        }
    }
    s
}

/// Go-style error text, e.g. `lstat /x: no such file or directory` (Go's `*PathError`).
pub fn go_path_error(op: &str, path: &Path, err: &io::Error) -> String {
    let reason = match err.raw_os_error() {
        Some(code) => {
            let desc = nix::errno::Errno::from_raw(code).desc();
            let mut chars = desc.chars();
            match chars.next() {
                Some(first) => first.to_lowercase().chain(chars).collect(),
                None => String::new(),
            }
        }
        None => err.to_string(),
    };
    format!("{op} {}: {reason}", path.display())
}

/// Seconds + nanoseconds -> protobuf `Timestamp`; `0, 0` means "no time" (like Go's zero `time.Time`).
pub fn to_timestamp(seconds: i64, nanos: i64) -> MessageField<Timestamp, Inline<Timestamp>> {
    if seconds == 0 && nanos == 0 {
        return MessageField::none();
    }
    MessageField::some(Timestamp { seconds, nanos: nanos as i32, ..Default::default() })
}

/// Upstream `entryInfo` + `GetEntryFromPath` + `GetEntryInfo`.
pub fn entry_info(path: &Path) -> Result<EntryInfo, ConnectError> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ConnectError::not_found(format!(
                "file not found: {}",
                go_path_error("lstat", path, &e)
            )));
        }
        Err(e) => {
            return Err(ConnectError::internal(format!(
                "error getting file info: {}",
                go_path_error("lstat", path, &e)
            )));
        }
    };
    let is_symlink = meta.file_type().is_symlink();
    let (symlink_target, file_type, mode) = if is_symlink {
        let target = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let (file_type, mode) = match std::fs::metadata(&target) {
            Ok(target_meta) => (file_type_of(&target_meta), target_meta.mode() & 0o777),
            Err(_) => (FileType::FILE_TYPE_UNSPECIFIED, 0),
        };
        (Some(target.to_string_lossy().into_owned()), file_type, mode)
    } else {
        (None, file_type_of(&meta), meta.mode() & 0o777)
    };

    let kind = if is_symlink {
        Kind::Symlink
    } else if meta.is_dir() {
        Kind::Dir
    } else {
        Kind::File
    };

    Ok(EntryInfo {
        name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string()),
        r#type: file_type.into(),
        path: path.display().to_string(),
        size: meta.size() as i64,
        mode,
        permissions: permissions_string(kind, meta.mode() & 0o7777),
        owner: owner_name(meta.uid()),
        group: group_name(meta.gid()),
        modified_time: to_timestamp(meta.mtime(), meta.mtime_nsec()),
        is_symlink: symlink_target.is_some(),
        symlink_target,
        ..Default::default()
    })
}

/// Upstream `getEntryType`: only regular files and directories have a type.
fn file_type_of(meta: &std::fs::Metadata) -> FileType {
    if meta.is_file() {
        FileType::FILE_TYPE_FILE
    } else if meta.is_dir() {
        FileType::FILE_TYPE_DIRECTORY
    } else {
        FileType::FILE_TYPE_UNSPECIFIED
    }
}

/// User name for `uid`, or the number as text when there is no such user (upstream `getFileOwnership`).
fn owner_name(uid: u32) -> String {
    nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
        .ok()
        .flatten()
        .map(|u| u.name)
        .unwrap_or_else(|| uid.to_string())
}

/// Group name for `gid`, or the number as text.
fn group_name(gid: u32) -> String {
    nix::unistd::Group::from_gid(nix::unistd::Gid::from_raw(gid))
        .ok()
        .flatten()
        .map(|g| g.name)
        .unwrap_or_else(|| gid.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn plain_file_and_dir() {
        assert_eq!(permissions_string(Kind::File, 0o644), "-rw-r--r--");
        assert_eq!(permissions_string(Kind::File, 0o755), "-rwxr-xr-x");
        assert_eq!(permissions_string(Kind::Dir, 0o755), "drwxr-xr-x");
        assert_eq!(permissions_string(Kind::File, 0o000), "----------");
    }

    #[test]
    fn symlink_uses_capital_l() {
        assert_eq!(permissions_string(Kind::Symlink, 0o777), "Lrwxrwxrwx");
    }

    #[test]
    fn special_bits_are_extra_letters_like_go() {
        // /tmp: directory + sticky bit
        assert_eq!(permissions_string(Kind::Dir, 0o1777), "dtrwxrwxrwx");
        // setuid binary like /usr/bin/sudo
        assert_eq!(permissions_string(Kind::File, 0o4755), "urwxr-xr-x");
        // setgid
        assert_eq!(permissions_string(Kind::File, 0o2755), "grwxr-xr-x");
    }

    #[test]
    fn entry_info_file() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();

        let e = entry_info(&f).unwrap();
        assert_eq!(e.name, "a.txt");
        assert_eq!(e.path, f.to_str().unwrap());
        assert_eq!(e.r#type, buffa::EnumValue::from(FileType::FILE_TYPE_FILE));
        assert_eq!(e.size, 5);
        assert_eq!(e.mode & 0o777, 0o644 & !0o022 & 0o777); // default umask 022
        assert_eq!(e.permissions, "-rw-r--r--");
        assert!(!e.owner.is_empty());
        assert!(!e.group.is_empty());
        assert!(e.modified_time.is_set());
        assert_eq!(e.symlink_target, None);
        assert!(!e.is_symlink);
    }

    #[test]
    fn entry_info_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let e = entry_info(tmp.path()).unwrap();
        assert_eq!(e.r#type, buffa::EnumValue::from(FileType::FILE_TYPE_DIRECTORY));
        assert!(e.permissions.starts_with('d'));
    }

    #[test]
    fn entry_info_symlink_takes_type_and_mode_from_target() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target.sh");
        std::fs::write(&target, b"#!/bin/sh").unwrap();
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let link = tmp.path().join("link");
        symlink(&target, &link).unwrap();

        let e = entry_info(&link).unwrap();
        assert_eq!(e.name, "link");
        assert_eq!(e.r#type, buffa::EnumValue::from(FileType::FILE_TYPE_FILE)); // type of the TARGET
        assert_eq!(e.mode, 0o755); // mode of the TARGET
        assert!(e.permissions.starts_with('L')); // but permissions of the link itself
        let resolved = std::fs::canonicalize(&target).unwrap();
        assert_eq!(e.symlink_target.as_deref(), Some(resolved.to_str().unwrap()));
        assert!(e.is_symlink);
    }

    #[test]
    fn entry_info_broken_symlink_is_unspecified() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("broken");
        symlink(tmp.path().join("missing"), &link).unwrap();

        let e = entry_info(&link).unwrap();
        assert_eq!(e.r#type, buffa::EnumValue::from(FileType::FILE_TYPE_UNSPECIFIED));
        assert_eq!(e.mode, 0);
        // Go: EvalSymlinks fails -> target falls back to the link path itself
        assert_eq!(e.symlink_target.as_deref(), Some(link.to_str().unwrap()));
        assert!(e.is_symlink);
    }

    #[test]
    fn entry_info_not_found() {
        let err = entry_info(Path::new("/definitely/not/here")).unwrap_err();
        assert_eq!(err.code, connectrpc::ErrorCode::NotFound);
        assert_eq!(
            err.message.as_deref(),
            Some("file not found: lstat /definitely/not/here: no such file or directory")
        );
    }
}
