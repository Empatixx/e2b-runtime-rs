use std::collections::VecDeque;
use std::ffi::OsString;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Component, Path, PathBuf};

const MAX_LINKS: usize = 255;
const NEW_DIR_MODE: u32 = 0o755;

pub fn clean_relative(path: &Path) -> Vec<OsString> {
    path.components().fold(Vec::new(), |mut parts, component| {
        match component {
            Component::Normal(name) => parts.push(name.to_os_string()),
            Component::ParentDir => {
                parts.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
        parts
    })
}

pub fn resolve_dir(root: &Path, parts: &[OsString], create: bool) -> io::Result<PathBuf> {
    let mut resolved: Vec<OsString> = Vec::new();
    let mut pending: VecDeque<OsString> = parts.iter().cloned().collect();
    let mut links = 0;
    while let Some(name) = pending.pop_front() {
        if name == ".." {
            resolved.pop();
            continue;
        }
        if name.is_empty() || name == "." {
            continue;
        }
        let candidate = join(root, &resolved).join(&name);
        match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.is_symlink() => {
                links += 1;
                if links > MAX_LINKS {
                    return Err(io::Error::from_raw_os_error(nix::libc::ELOOP));
                }
                let target = std::fs::read_link(&candidate)?;
                if target.is_absolute() {
                    resolved.clear();
                }
                let mut target_parts: VecDeque<OsString> = target
                    .components()
                    .filter_map(|component| match component {
                        Component::Normal(name) => Some(name.to_os_string()),
                        Component::ParentDir => Some(OsString::from("..")),
                        _ => None,
                    })
                    .collect();
                target_parts.extend(pending);
                pending = target_parts;
            }
            Ok(metadata) if metadata.is_dir() => resolved.push(name),
            Ok(_) => return Err(io::Error::from_raw_os_error(nix::libc::ENOTDIR)),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                std::fs::DirBuilder::new().mode(NEW_DIR_MODE).create(&candidate)?;
                resolved.push(name);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(join(root, &resolved))
}

fn join(root: &Path, parts: &[OsString]) -> PathBuf {
    parts.iter().fold(root.to_path_buf(), |path, part| path.join(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning_never_climbs_above_the_root() {
        assert_eq!(
            clean_relative(Path::new("/a/../../b/./c")),
            vec![OsString::from("b"), OsString::from("c")]
        );
        assert!(clean_relative(Path::new("./")).is_empty());
    }

    #[test]
    fn symlinked_parents_resolve_inside_the_root() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("real")).unwrap();
        std::os::unix::fs::symlink("/real", root.path().join("abs")).unwrap();
        std::os::unix::fs::symlink("../../../..", root.path().join("real/up")).unwrap();

        let through_absolute = resolve_dir(root.path(), &clean_relative(Path::new("abs/new")), true).unwrap();
        let escaping = resolve_dir(root.path(), &clean_relative(Path::new("real/up/etc")), true).unwrap();

        assert_eq!(through_absolute, root.path().join("real/new"));
        assert_eq!(escaping, root.path().join("etc"));
        assert!(root.path().join("real/new").is_dir());
    }
}
