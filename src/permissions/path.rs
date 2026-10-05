//! Path expansion and resolution, 1:1 with upstream `permissions/path.go`.

use std::io;
use std::path::{Component, Path, PathBuf};
use std::os::unix::fs::DirBuilderExt;

use super::user::UserInfo;
use crate::execcontext::resolve_default_workdir;

/// `~` / `~/x` -> home dir based; `~other/x` is an error; anything else unchanged.
fn expand(path: &str, home: &Path) -> Result<PathBuf, String> {
    if path.is_empty() {
        return Ok(PathBuf::new());
    }
    let components = path.strip_prefix('~');
    match components {
        Some(rest) if rest.is_empty() || rest.starts_with('/') || rest.starts_with('\\') => {
            Ok(home.join(rest.trim_start_matches(['/', '\\'])))
        }
        Some(_) => Err("cannot expand user-specific home dir".to_owned()),
        None => Ok(PathBuf::from(path)),
    }
}

/// Lexical clean like Go's `filepath.Clean` (removes `.`, resolves `..`, no symlinks).
fn clean(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() { PathBuf::from("/") } else { out }
}

pub fn expand_and_resolve(path: &str, user: &UserInfo, default: Option<&str>) -> Result<PathBuf, String> {
    let path = resolve_default_workdir(path, default);
    let expanded = expand(&path, &user.home)
        .map_err(|e| format!("failed to expand path '{path}' for user '{}': {e}", user.name))?;
    if expanded.is_absolute() {
        return Ok(clean(&expanded));
    }
    Ok(clean(&user.home.join(expanded)))
}

/// Walks from `/` down to `path`: missing dirs are created (0o755, subject to umask) and chowned;
/// an existing non-directory is an error. Mirrors upstream `EnsureDirs`.
pub fn ensure_dirs(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let mut subpaths: Vec<&Path> = path.ancestors().filter(|p| *p != Path::new("/")).collect();
    subpaths.reverse();
    for dir in subpaths {
        match std::fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => continue,
            Ok(_) => return Err(io::Error::other(format!("path is a file: {}", dir.display()))),
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(io::Error::new(e.kind(), format!("failed to stat directory: {e}")));
            }
            Err(_) => {}
        }
        if let Err(e) = std::fs::DirBuilder::new().mode(0o755).create(dir)
            && e.kind() != io::ErrorKind::AlreadyExists
        {
            return Err(io::Error::new(e.kind(), format!("failed to create directory: {e}")));
        }
        // chown even if another process created it meanwhile (it may not have chowned yet)
        nix::unistd::chown(dir, Some(nix::unistd::Uid::from_raw(uid)), Some(nix::unistd::Gid::from_raw(gid)))
            .map_err(|e| io::Error::other(format!("failed to chown directory: {e}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u() -> UserInfo {
        UserInfo { name: "user".into(), uid: 1000, gid: 1000, home: "/home/user".into(), groups: vec![] }
    }

    #[test]
    fn expand_cases() {
        let home = Path::new("/home/user");
        assert_eq!(expand("", home).unwrap(), PathBuf::from(""));
        assert_eq!(expand("/abs", home).unwrap(), PathBuf::from("/abs"));
        assert_eq!(expand("rel", home).unwrap(), PathBuf::from("rel"));
        assert_eq!(expand("~", home).unwrap(), PathBuf::from("/home/user"));
        assert_eq!(expand("~/x", home).unwrap(), PathBuf::from("/home/user/x"));
        assert_eq!(expand("~other/x", home).unwrap_err(), "cannot expand user-specific home dir");
    }

    #[test]
    fn expand_and_resolve_matches_go() {
        assert_eq!(expand_and_resolve("/abs", &u(), Some("/w")).unwrap(), PathBuf::from("/abs"));
        assert_eq!(expand_and_resolve("", &u(), Some("/w")).unwrap(), PathBuf::from("/w"));
        assert_eq!(expand_and_resolve("", &u(), None).unwrap(), PathBuf::from("/home/user"));
        assert_eq!(expand_and_resolve("~", &u(), None).unwrap(), PathBuf::from("/home/user"));
        assert_eq!(expand_and_resolve("~/x", &u(), None).unwrap(), PathBuf::from("/home/user/x"));
        assert_eq!(expand_and_resolve("rel/../dir", &u(), None).unwrap(), PathBuf::from("/home/user/dir"));
        assert_eq!(
            expand_and_resolve("~other/x", &u(), None).unwrap_err(),
            "failed to expand path '~other/x' for user 'user': cannot expand user-specific home dir"
        );
    }

    #[test]
    fn ensure_dirs_creates_missing_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("a/b/c");
        let uid = nix::unistd::getuid().as_raw();
        let gid = nix::unistd::getgid().as_raw();
        ensure_dirs(&target, uid, gid).unwrap();
        assert!(target.is_dir());
        ensure_dirs(&target, uid, gid).unwrap(); // idempotent
    }

    #[test]
    fn ensure_dirs_rejects_file_in_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("f"), b"x").unwrap();
        let err = ensure_dirs(&tmp.path().join("f/sub"), 0, 0).unwrap_err();
        assert_eq!(err.to_string(), format!("path is a file: {}", tmp.path().join("f").display()));
    }
}
