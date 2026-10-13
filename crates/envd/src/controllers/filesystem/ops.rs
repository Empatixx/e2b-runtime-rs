use std::io;
use std::path::{Path, PathBuf};

use connectrpc::{ConnectError, ErrorCode};

use super::entry::{entry_info, entry_info_with_owners};
use crate::proto::filesystem::EntryInfo;
use crate::user::{OwnerNames, User};
use crate::util::fs::{dir, eval_symlinks, go_rename, os_error, reason};
use crate::util::paths::ensure_dirs;

pub fn make_dir(path: &Path, user: &User) -> Result<EntryInfo, ConnectError> {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => Err(ConnectError::already_exists(format!(
            "directory already exists: {}",
            path.display()
        ))),
        Ok(_) => Err(ConnectError::invalid_argument(format!(
            "path already exists but it is not a directory: {}",
            path.display()
        ))),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            ensure_dirs(path, user.uid, user.gid).map_err(ConnectError::internal)?;
            entry_info(path)
        }
        Err(e) => Err(ConnectError::internal(format!(
            "error getting file info: {}",
            os_error("stat", path, &e)
        ))),
    }
}

pub fn rename(source: &Path, destination: &Path, user: &User) -> Result<EntryInfo, ConnectError> {
    let parent = dir(destination);
    ensure_dirs(&parent, user.uid, user.gid).map_err(ConnectError::internal)?;

    if let Err(e) = go_rename(source, destination) {
        let text = format!("rename {} {}: {}", source.display(), destination.display(), reason(&e));
        return Err(if e.kind() == io::ErrorKind::NotFound {
            ConnectError::not_found(format!("source file not found: {text}"))
        } else {
            ConnectError::internal(format!("error renaming: {text}"))
        });
    }

    entry_info(destination)
}

pub fn remove_all(path: &Path) -> Result<(), ConnectError> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(ConnectError::internal(format!(
            "error removing file or directory: {}",
            os_error("unlinkat", path, &e)
        ))),
        _ => Ok(()),
    }
}

pub fn list_dir(path: &Path, depth: u32) -> Result<Vec<EntryInfo>, ConnectError> {
    let resolved = match eval_symlinks(path) {
        Ok(resolved) => resolved,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ConnectError::not_found(format!("path not found: {e}")));
        }
        Err(e) if e.to_string().contains("too many links") => {
            return Err(ConnectError::failed_precondition(format!(
                "cyclic symlink or chain >255 links at {:?}",
                path.display().to_string()
            )));
        }
        Err(e) => return Err(ConnectError::internal(format!("error resolving symlink: {e}"))),
    };

    match std::fs::metadata(&resolved) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(ConnectError::invalid_argument(format!(
                "path is not a directory: {}",
                resolved.display()
            )));
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Err(ConnectError::not_found(format!(
                "directory not found: {}",
                os_error("stat", &resolved, &e)
            )));
        }
        Err(e) => {
            return Err(ConnectError::internal(format!(
                "error getting file info: {}",
                os_error("stat", &resolved, &e)
            )));
        }
    }

    let mut listing = Listing {
        shown_root: path.to_path_buf(),
        real_root: resolved.clone(),
        max_depth: depth.max(1) as usize,
        entries: Vec::new(),
        owners: OwnerNames::default(),
    };
    walk(&mut listing, &resolved, 1)
        .map_err(|cause| ConnectError::internal(format!("error reading directory {}: {cause}", resolved.display())))?;
    Ok(listing.entries)
}

struct Listing {
    shown_root: PathBuf,
    real_root: PathBuf,
    max_depth: usize,
    entries: Vec<EntryInfo>,
    owners: OwnerNames,
}

struct DirChild {
    path: PathBuf,
    is_dir: bool,
}

fn walk(listing: &mut Listing, dir: &Path, depth: usize) -> Result<(), String> {
    let mut children = std::fs::read_dir(dir)
        .and_then(|entries| {
            entries
                .map(|entry| {
                    entry.and_then(|entry| {
                        Ok(DirChild {
                            is_dir: entry.file_type()?.is_dir(),
                            path: entry.path(),
                        })
                    })
                })
                .collect::<io::Result<Vec<_>>>()
        })
        .map_err(|e| os_error("open", dir, &e))?;
    if depth > listing.max_depth {
        return Ok(());
    }
    children.sort_by(|a, b| a.path.cmp(&b.path));

    for child in children {
        let mut entry = match entry_info_with_owners(&child.path, &mut listing.owners) {
            Ok(entry) => entry,
            Err(e) if e.code == ErrorCode::NotFound => continue,
            Err(e) => return Err(format!("{}: {}", e.code.as_str(), e.message.unwrap_or_default())),
        };
        let relative = child.path.strip_prefix(&listing.real_root).unwrap_or(&child.path);
        entry.path = listing.shown_root.join(relative).display().to_string();
        listing.entries.push(entry);

        if child.is_dir {
            walk(listing, &child.path, depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::path::PathBuf;

    use super::*;

    fn me() -> User {
        let name = nix::unistd::User::from_uid(nix::unistd::getuid())
            .unwrap()
            .unwrap()
            .name;
        User::lookup(&name).unwrap()
    }

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path()).unwrap();
        (dir, path)
    }

    fn paths(entries: &[EntryInfo]) -> Vec<&str> {
        entries.iter().map(|e| e.path.as_str()).collect()
    }

    fn s(path: PathBuf) -> String {
        path.display().to_string()
    }

    #[test]
    fn list_dir_depths() {
        let (_tmp, root) = tmp();
        std::fs::create_dir_all(root.join("test-dir/sub-dir-1")).unwrap();
        std::fs::create_dir_all(root.join("test-dir/sub-dir-2")).unwrap();
        std::fs::write(root.join("test-dir/sub-dir-1/file.txt"), b"x").unwrap();

        let level1 = [s(root.join("test-dir"))];
        assert_eq!(paths(&list_dir(&root, 0).unwrap()), level1);
        assert_eq!(paths(&list_dir(&root, 1).unwrap()), level1);
        assert_eq!(
            paths(&list_dir(&root, 2).unwrap()),
            [
                s(root.join("test-dir")),
                s(root.join("test-dir/sub-dir-1")),
                s(root.join("test-dir/sub-dir-2"))
            ]
        );
        assert_eq!(
            paths(&list_dir(&root, 3).unwrap()),
            [
                s(root.join("test-dir")),
                s(root.join("test-dir/sub-dir-1")),
                s(root.join("test-dir/sub-dir-1/file.txt")),
                s(root.join("test-dir/sub-dir-2")),
            ]
        );
    }

    #[test]
    fn list_dir_missing() {
        let err = list_dir(Path::new("/definitely/not/here"), 1).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert_eq!(
            err.message.as_deref(),
            Some("path not found: lstat /definitely: no such file or directory")
        );
    }

    #[test]
    fn list_dir_symlinks() {
        let (_tmp, root) = tmp();
        let real_dir = root.join("real-dir");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(real_dir.join("file.txt"), b"x").unwrap();
        std::fs::write(root.join("real-file.txt"), b"x").unwrap();
        symlink(&real_dir, root.join("link-dir")).unwrap();
        symlink(root.join("real-file.txt"), root.join("link-file")).unwrap();
        symlink(root.join("cyclic"), root.join("cyclic")).unwrap();

        let link_dir = root.join("link-dir");
        assert_eq!(paths(&list_dir(&link_dir, 1).unwrap()), [s(link_dir.join("file.txt"))]);

        let err = list_dir(&root.join("link-file"), 1).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert!(err.message.unwrap().contains("not a directory"));

        let cyclic = root.join("cyclic");
        let err = list_dir(&cyclic, 1).unwrap_err();
        assert_eq!(err.code, ErrorCode::FailedPrecondition);
        assert_eq!(
            err.message.unwrap(),
            format!("cyclic symlink or chain >255 links at {:?}", s(cyclic))
        );

        assert_eq!(
            paths(&list_dir(&root, 3).unwrap()),
            [
                s(root.join("cyclic")),
                s(root.join("link-dir")),
                s(root.join("link-file")),
                s(root.join("real-dir")),
                s(root.join("real-dir/file.txt")),
                s(root.join("real-file.txt")),
            ]
        );
    }

    #[test]
    fn unreadable_dir_at_max_depth_fails_the_listing() {
        if nix::unistd::geteuid().is_root() {
            return;
        }
        let (_tmp, root) = tmp();
        let locked = root.join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o000)).unwrap();
        let err = list_dir(&root, 1).unwrap_err();
        std::fs::set_permissions(&locked, PermissionsExt::from_mode(0o755)).unwrap();

        assert_eq!(err.code, ErrorCode::Internal);
        assert_eq!(
            err.message.unwrap(),
            format!(
                "error reading directory {}: open {}: permission denied",
                root.display(),
                locked.display()
            )
        );
    }

    #[test]
    fn make_dir_creates_parents_and_rejects_existing() {
        let (_tmp, root) = tmp();
        let dir = root.join("a/b/c");
        assert_eq!(make_dir(&dir, &me()).unwrap().path, s(dir.clone()));
        assert!(dir.is_dir());

        let err = make_dir(&dir, &me()).unwrap_err();
        assert_eq!(err.code, ErrorCode::AlreadyExists);
        assert_eq!(
            err.message.unwrap(),
            format!("directory already exists: {}", dir.display())
        );

        let file = root.join("f");
        std::fs::write(&file, b"").unwrap();
        let err = make_dir(&file, &me()).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument);
        assert_eq!(
            err.message.unwrap(),
            format!("path already exists but it is not a directory: {}", file.display())
        );
    }

    #[test]
    fn rename_into_new_dirs() {
        let (_tmp, root) = tmp();
        let source = root.join("a.txt");
        std::fs::write(&source, b"hi").unwrap();
        let destination = root.join("x/y/b.txt");

        let entry = rename(&source, &destination, &me()).unwrap();
        assert_eq!(entry.path, s(destination.clone()));
        assert!(!source.exists());
        assert_eq!(std::fs::read(&destination).unwrap(), b"hi");
    }

    #[test]
    fn rename_onto_a_directory_fails_with_file_exists_like_go() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("file"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join("dir/inside")).unwrap();
        std::fs::create_dir(tmp.path().join("empty")).unwrap();

        let error = go_rename(&tmp.path().join("file"), &tmp.path().join("dir")).unwrap_err();
        assert_eq!(reason(&error), "file exists");
        let error = go_rename(&tmp.path().join("empty"), &tmp.path().join("dir/inside")).unwrap_err();
        assert_eq!(reason(&error), "file exists");
        let error = go_rename(&tmp.path().join("missing"), &tmp.path().join("dir")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        go_rename(&tmp.path().join("file"), &tmp.path().join("other")).unwrap();
    }

    #[test]
    fn rename_missing_source() {
        let (_tmp, root) = tmp();
        let (source, destination) = (root.join("nope"), root.join("dst"));
        let err = rename(&source, &destination, &me()).unwrap_err();
        assert_eq!(err.code, ErrorCode::NotFound);
        assert_eq!(
            err.message.unwrap(),
            format!(
                "source file not found: rename {} {}: no such file or directory",
                source.display(),
                destination.display()
            )
        );
    }

    #[test]
    fn remove_all_cases() {
        let (_tmp, root) = tmp();
        let file = root.join("f.txt");
        std::fs::write(&file, b"x").unwrap();
        let dir = root.join("d");
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        let target = root.join("target");
        std::fs::create_dir(&target).unwrap();
        let link = root.join("link");
        symlink(&target, &link).unwrap();

        remove_all(&file).unwrap();
        remove_all(&dir).unwrap();
        remove_all(&link).unwrap();
        remove_all(Path::new("/definitely/not/here")).unwrap();

        assert!(!file.exists());
        assert!(!dir.exists());
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(target.is_dir());
    }
}
