use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use nix::unistd::{Gid, Uid};

use crate::user::User;
use crate::util::fs::{clean, os_error};

pub fn expand_and_resolve(path: &str, user: &User, default_workdir: Option<&str>) -> Result<PathBuf, String> {
    let path = if path.is_empty() {
        default_workdir.unwrap_or_default()
    } else {
        path
    };

    let expanded = match path.strip_prefix('~') {
        None => PathBuf::from(path),
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            clean(&user.home.join(rest.trim_start_matches(['/', '\\'])))
        }
        Some(_) => {
            return Err(format!(
                "failed to expand path '{path}' for user '{}': cannot expand user-specific home dir",
                user.name
            ));
        }
    };

    if expanded.is_absolute() {
        return Ok(expanded);
    }
    Ok(clean(&user.home.join(expanded)))
}

pub fn ensure_dirs(path: &Path, uid: u32, gid: u32) -> Result<(), String> {
    let mut dirs: Vec<&Path> = path.ancestors().take_while(|dir| *dir != Path::new("/")).collect();
    dirs.reverse();

    for dir in dirs {
        match std::fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => return Err(format!("path is a file: {}", dir.display())),
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(format!("failed to stat directory: {}", os_error("stat", dir, &e)));
            }
            Err(_) => {}
        }

        if let Err(e) = std::fs::DirBuilder::new().mode(0o755).create(dir)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(format!("failed to create directory: {}", os_error("mkdir", dir, &e)));
        }

        if let Err(errno) = nix::unistd::chown(dir, Some(Uid::from_raw(uid)), Some(Gid::from_raw(gid))) {
            let e = io::Error::from(errno);
            return Err(format!("failed to chown directory: {}", os_error("chown", dir, &e)));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> User {
        User {
            name: "user".into(),
            uid: 1000,
            gid: 1000,
            home: "/home/user".into(),
            groups: vec![],
        }
    }

    #[test]
    fn expand_and_resolve_cases() {
        let u = user();
        let resolve = |p: &str, default: Option<&str>| expand_and_resolve(p, &u, default).unwrap();
        assert_eq!(resolve("", None), Path::new("/home/user"));
        assert_eq!(resolve("", Some("/work")), Path::new("/work"));
        assert_eq!(resolve("/abs/../x", None).as_os_str(), "/abs/../x");
        assert_eq!(resolve("/home/user/d/", None).as_os_str(), "/home/user/d/");
        assert_eq!(resolve("~/a/../b", None).as_os_str(), "/home/user/b");
        assert_eq!(resolve("rel/a", None), Path::new("/home/user/rel/a"));
        assert_eq!(resolve("../other", None), Path::new("/home/other"));
        assert_eq!(resolve("~", None), Path::new("/home/user"));
        assert_eq!(resolve("~/x", None), Path::new("/home/user/x"));
        assert_eq!(
            expand_and_resolve("~other/x", &u, None).unwrap_err(),
            "failed to expand path '~other/x' for user 'user': cannot expand user-specific home dir"
        );
    }

    #[test]
    fn ensure_dirs_creates_missing_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a/b/c");
        let (uid, gid) = (nix::unistd::getuid().as_raw(), nix::unistd::getgid().as_raw());
        ensure_dirs(&target, uid, gid).unwrap();
        assert!(target.is_dir());
        ensure_dirs(&target, uid, gid).unwrap();
    }

    #[test]
    fn ensure_dirs_rejects_file_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), b"x").unwrap();
        let err = ensure_dirs(&tmp.path().join("f/sub"), 0, 0).unwrap_err();
        assert_eq!(err, format!("path is a file: {}", tmp.path().join("f").display()));
    }
}
