use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::path::{Component, Path, PathBuf};

pub fn os_error(op: &str, path: &Path, err: &io::Error) -> String {
    format!("{op} {}: {}", path.display(), reason(err))
}

pub fn reason(err: &io::Error) -> String {
    err.raw_os_error()
        .map(|code| nix::errno::Errno::from_raw(code).desc())
        .map(|desc| {
            let first = desc.chars().take(1).flat_map(char::to_lowercase);
            first.chain(desc.chars().skip(1)).collect()
        })
        .unwrap_or_else(|| err.to_string())
}

pub fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
            _ => {}
        }
    }
    out
}

pub fn eval_symlinks(path: &Path) -> io::Result<PathBuf> {
    let mut pending = names(path);
    let mut resolved = PathBuf::from("/");
    let mut links = 0;

    while let Some(name) = pending.pop_front() {
        if name == ".." {
            resolved.pop();
            continue;
        }

        resolved.push(&name);
        let meta = std::fs::symlink_metadata(&resolved)
            .map_err(|e| io::Error::new(e.kind(), os_error("lstat", &resolved, &e)))?;

        if !meta.is_symlink() {
            if !meta.is_dir() && !pending.is_empty() {
                return Err(io::Error::new(io::ErrorKind::NotADirectory, "not a directory"));
            }
            continue;
        }

        links += 1;
        if links > 255 {
            return Err(io::Error::other("EvalSymlinks: too many links"));
        }

        let target =
            std::fs::read_link(&resolved).map_err(|e| io::Error::new(e.kind(), os_error("readlink", &resolved, &e)))?;
        resolved.pop();
        if target.is_absolute() {
            resolved = PathBuf::from("/");
        }
        pending = names(&target).into_iter().chain(pending).collect();
    }

    Ok(clean(&resolved))
}

fn names(path: &Path) -> VecDeque<OsString> {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name.to_owned()),
            Component::ParentDir => Some("..".into()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;

    #[test]
    fn error_text() {
        let err = io::Error::from_raw_os_error(nix::libc::ENOENT);
        assert_eq!(
            os_error("lstat", Path::new("/x"), &err),
            "lstat /x: no such file or directory"
        );
    }

    #[test]
    fn clean_paths() {
        assert_eq!(clean(Path::new("/a/./b/../c/")), Path::new("/a/c"));
        assert_eq!(clean(Path::new("/../..")), Path::new("/"));
    }

    #[test]
    fn follows_absolute_and_relative_links() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir(&target).unwrap();
        symlink(&target, tmp.path().join("link3")).unwrap();
        symlink("link3", tmp.path().join("link2")).unwrap();
        symlink(tmp.path().join("link2"), tmp.path().join("link1")).unwrap();

        let want = std::fs::canonicalize(&target).unwrap();
        assert_eq!(eval_symlinks(&tmp.path().join("link1")).unwrap(), want);
        assert_eq!(eval_symlinks(&tmp.path().join("link1/../target/.")).unwrap(), want);
    }

    #[test]
    fn missing_names_the_first_missing_part() {
        let err = eval_symlinks(Path::new("/definitely/not/here")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "lstat /definitely: no such file or directory");
    }

    #[test]
    fn file_in_the_middle() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("f");
        std::fs::write(&file, b"").unwrap();
        let err = eval_symlinks(&file.join("x")).unwrap_err();
        assert_eq!(err.to_string(), "not a directory");
    }

    #[test]
    fn cycle() {
        let tmp = tempfile::tempdir().unwrap();
        symlink(tmp.path().join("b"), tmp.path().join("a")).unwrap();
        symlink(tmp.path().join("a"), tmp.path().join("b")).unwrap();
        let err = eval_symlinks(&tmp.path().join("a")).unwrap_err();
        assert_eq!(err.to_string(), "EvalSymlinks: too many links");
    }
}
