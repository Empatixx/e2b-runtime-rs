mod entry;
mod scoped;

use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::AtFlags;
use nix::sys::stat::{Mode, SFlag, makedev, mknod};
use nix::unistd::{Gid, Uid, fchownat};
use tar::{Archive, Entry, EntryType};

use entry::{EntryMeta, Times, set_times};
use scoped::{clean_local, resolve_dir};

const WHITEOUT_PREFIX: &[u8] = b".wh.";
const OPAQUE_WHITEOUT: &[u8] = b".wh..wh..opq";
const OPAQUE_XATTR: &str = "trusted.overlay.opaque";
const PERMISSION_BITS: u32 = 0o7777;
const FILE_PERMISSION_BITS: u32 = 0o777;

struct DirectoryTimes {
    path: PathBuf,
    times: Times,
}

struct Target {
    parent: PathBuf,
    name: std::ffi::OsString,
}

impl Target {
    fn path(&self) -> PathBuf {
        self.parent.join(&self.name)
    }
}

pub fn untar(reader: impl Read, root: &Path) -> io::Result<()> {
    let mut archive = Archive::new(reader);
    let mut directories = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        if let Some(directory) = apply(&mut entry, root)? {
            directories.push(directory);
        }
    }
    directories
        .iter()
        .try_for_each(|directory| set_times(&directory.path, directory.times, true))
}

fn apply<R: Read>(entry: &mut Entry<R>, root: &Path) -> io::Result<Option<DirectoryTimes>> {
    let meta = EntryMeta::read(entry)?;
    if meta.kind == EntryType::XGlobalHeader {
        return Ok(None);
    }
    let raw_name = entry.path_bytes().into_owned();
    let Some(mut parts) = clean_local(&raw_name).map_err(|()| invalid_entry_name(&raw_name))? else {
        return Ok(None);
    };
    let name = parts.pop().ok_or_else(|| invalid_entry_name(&raw_name))?;
    let target = Target {
        parent: resolve_dir(root, &parts, true)?,
        name,
    };
    remove_existing(&target.path(), meta.kind)?;
    if !convert_whiteout(&target, &meta, &raw_name)? {
        return Ok(None);
    }
    let link = link_target(entry, &meta, root)?;
    create(entry, &meta, &target.path(), link.as_deref())?;
    finish(&meta, &target.path(), link.as_deref())
}

fn invalid_entry_name(name: &[u8]) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("invalid entry name {:?}", String::from_utf8_lossy(name)),
    )
}

fn remove_existing(path: &Path, kind: EntryType) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(existing) if existing.is_dir() && kind == EntryType::Directory => Ok(()),
        Ok(existing) if existing.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn convert_whiteout(target: &Target, meta: &EntryMeta, raw_name: &[u8]) -> io::Result<bool> {
    let base = target.name.as_bytes();
    if [b".wh.".as_slice(), b".wh..", b".wh..."].contains(&base) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid whiteout entry {:?}", String::from_utf8_lossy(raw_name)),
        ));
    }
    if base == OPAQUE_WHITEOUT {
        return xattr::set(&target.parent, OPAQUE_XATTR, b"y")
            .map(|()| false)
            .map_err(|e| {
                io::Error::new(
                    e.kind(),
                    format!("fsetxattr('{}', {OPAQUE_XATTR}=y): {e}", target.parent.display()),
                )
            });
    }
    let Some(hidden) = base.strip_prefix(WHITEOUT_PREFIX) else {
        return Ok(true);
    };
    let path = target.parent.join(std::ffi::OsStr::from_bytes(hidden));
    mknod(&path, SFlag::S_IFCHR, Mode::empty(), makedev(0, 0)).map_err(|e| {
        io::Error::new(
            io::Error::from(e).kind(),
            format!("failed to mknod('{}', S_IFCHR, 0): {e}", path.display()),
        )
    })?;
    if meta.uid != 0 || meta.gid != 0 {
        chown(&path, meta)?;
    }
    Ok(false)
}

fn link_target<R: Read>(entry: &Entry<R>, meta: &EntryMeta, root: &Path) -> io::Result<Option<PathBuf>> {
    let Some(link) = entry.link_name_bytes().map(|name| name.into_owned()) else {
        return Ok(None);
    };
    match meta.kind {
        EntryType::Link => hard_link_target(&link, root).map(Some),
        _ => Ok(Some(PathBuf::from(std::ffi::OsStr::from_bytes(&link)))),
    }
}

fn hard_link_target(link: &[u8], root: &Path) -> io::Result<PathBuf> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid hardlink target {:?}", String::from_utf8_lossy(link)),
        )
    };
    let mut parts = clean_local(link).map_err(|()| invalid())?.ok_or_else(invalid)?;
    let name = parts.pop().ok_or_else(invalid)?;
    resolve_dir(root, &parts, false).map(|parent| parent.join(name))
}

fn create<R: Read>(entry: &mut Entry<R>, meta: &EntryMeta, path: &Path, link: Option<&Path>) -> io::Result<()> {
    match meta.kind {
        EntryType::Directory => match path.is_dir() {
            true => Ok(()),
            false => std::fs::DirBuilder::new()
                .mode(meta.mode & FILE_PERMISSION_BITS)
                .create(path),
        },
        EntryType::Regular | EntryType::GNUSparse => write_file(entry, path, meta.mode & FILE_PERMISSION_BITS),
        EntryType::Symlink => std::os::unix::fs::symlink(link.unwrap_or(Path::new("")), path),
        EntryType::Link => std::fs::hard_link(link.unwrap_or(Path::new("")), path),
        EntryType::Char => make_node(meta, path, SFlag::S_IFCHR),
        EntryType::Block => make_node(meta, path, SFlag::S_IFBLK),
        EntryType::Fifo => make_node(meta, path, SFlag::S_IFIFO),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unhandled tar header type {}", other.as_byte()),
        )),
    }
}

fn write_file<R: Read>(entry: &mut Entry<R>, path: &Path, mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    io::copy(entry, &mut file).map(drop)
}

fn make_node(meta: &EntryMeta, path: &Path, kind: SFlag) -> io::Result<()> {
    mknod(
        path,
        kind,
        Mode::from_bits_truncate(meta.mode & PERMISSION_BITS),
        makedev(meta.device_major.into(), meta.device_minor.into()),
    )
    .map_err(io::Error::from)
}

fn finish(meta: &EntryMeta, path: &Path, link: Option<&Path>) -> io::Result<Option<DirectoryTimes>> {
    chown(path, meta)?;
    meta.apply_xattrs(path)?;
    let follows = match meta.kind {
        EntryType::Symlink => false,
        EntryType::Link => !link.is_some_and(is_symlink),
        _ => true,
    };
    if follows {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(meta.mode & PERMISSION_BITS))?;
    }
    match (meta.kind, follows) {
        (EntryType::Directory, _) => Ok(Some(DirectoryTimes {
            path: path.to_path_buf(),
            times: meta.times,
        })),
        (EntryType::Symlink, _) => set_times(path, meta.times, false).map(|()| None),
        (_, true) => set_times(path, meta.times, true).map(|()| None),
        (_, false) => Ok(None),
    }
}

fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_symlink())
}

fn chown(path: &Path, meta: &EntryMeta) -> io::Result<()> {
    fchownat(
        None,
        path,
        Some(Uid::from_raw(meta.uid)),
        Some(Gid::from_raw(meta.gid)),
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .map_err(|e| {
        io::Error::new(
            io::Error::from(e).kind(),
            format!(
                "failed to Lchown {:?} for UID {}, GID {}: {}",
                path.display(),
                meta.uid,
                meta.gid,
                io::Error::from(e)
            ),
        )
    })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileTypeExt;

    use tar::{Builder, Header};

    use super::*;

    fn tar(entries: &[(&str, EntryType, &[u8], &str)]) -> Vec<u8> {
        let builder = entries
            .iter()
            .fold(Builder::new(Vec::new()), |mut builder, (path, kind, data, link)| {
                let mut header = Header::new_gnu();
                header.set_entry_type(*kind);
                header.set_mode(if *kind == EntryType::Directory { 0o755 } else { 0o600 });
                header.set_uid(0);
                header.set_gid(0);
                header.set_mtime(1_700_000_000);
                header.set_size(data.len() as u64);
                let raw = header.as_gnu_mut().unwrap();
                raw.name[..path.len()].copy_from_slice(path.as_bytes());
                raw.linkname[..link.len()].copy_from_slice(link.as_bytes());
                header.set_cksum();
                builder.append(&header, *data).unwrap();
                builder
            });
        builder.into_inner().unwrap()
    }

    fn is_root() -> bool {
        nix::unistd::geteuid().is_root()
    }

    #[test]
    fn whiteouts_become_character_devices() {
        if !is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let lower = root.path().join("lower");
        let upper = root.path().join("upper");
        std::fs::create_dir_all(&lower).unwrap();
        std::fs::create_dir_all(&upper).unwrap();

        untar(
            tar(&[("stale-file.txt", EntryType::Regular, b"old-version", "")]).as_slice(),
            &lower,
        )
        .unwrap();
        untar(
            tar(&[(".wh.stale-file.txt", EntryType::Regular, b"", "")]).as_slice(),
            &upper,
        )
        .unwrap();

        assert!(!upper.join(".wh.stale-file.txt").exists());
        assert!(
            std::fs::symlink_metadata(upper.join("stale-file.txt"))
                .unwrap()
                .file_type()
                .is_char_device()
        );
        assert_eq!(std::fs::read(lower.join("stale-file.txt")).unwrap(), b"old-version");
    }

    #[test]
    fn escaping_relative_symlinks_are_kept_verbatim() {
        if !is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let layer = tar(&[
            ("nix/", EntryType::Directory, b"", ""),
            ("nix/store/", EntryType::Directory, b"", ""),
            ("nix/store/.links/", EntryType::Directory, b"", ""),
            ("etc/", EntryType::Directory, b"", ""),
            ("etc/environment", EntryType::Regular, b"PATH=/usr/bin\n", ""),
            (
                "nix/store/.links/abc",
                EntryType::Symlink,
                b"",
                "../../../../../etc/environment",
            ),
            (
                "nix/store/link-within",
                EntryType::Symlink,
                b"",
                "../../etc/environment",
            ),
        ]);

        untar(layer.as_slice(), root.path()).unwrap();

        assert_eq!(
            std::fs::read_link(root.path().join("nix/store/.links/abc")).unwrap(),
            PathBuf::from("../../../../../etc/environment")
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("nix/store/link-within")).unwrap(),
            "PATH=/usr/bin\n"
        );
    }

    #[test]
    fn opaque_dirs_hardlinks_and_confined_paths() {
        if !is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let layer = tar(&[
            ("dir/", EntryType::Directory, b"", ""),
            ("dir/.wh..wh..opq", EntryType::Regular, b"", ""),
            ("dir/file", EntryType::Regular, b"data", ""),
            ("dir/hard", EntryType::Link, b"", "/dir/file"),
        ]);

        untar(layer.as_slice(), root.path()).unwrap();

        assert_eq!(
            xattr::get(root.path().join("dir"), OPAQUE_XATTR).unwrap(),
            Some(b"y".to_vec())
        );
        assert_eq!(std::fs::read(root.path().join("dir/hard")).unwrap(), b"data");
        assert_eq!(
            std::fs::metadata(root.path().join("dir/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn escaping_names_and_unknown_types_fail_like_moby() {
        let root = tempfile::tempdir().unwrap();
        let escaping = untar(
            tar(&[("../../escape", EntryType::Regular, b"x", "")]).as_slice(),
            root.path(),
        );
        let continuous = untar(tar(&[("cont", EntryType::Continuous, b"", "")]).as_slice(), root.path());
        let bad_link = untar(
            tar(&[("h", EntryType::Link, b"", "../outside")]).as_slice(),
            root.path(),
        );
        let bad_whiteout = untar(tar(&[(".wh..", EntryType::Regular, b"", "")]).as_slice(), root.path());

        assert_eq!(escaping.unwrap_err().to_string(), "invalid entry name \"../../escape\"");
        assert_eq!(continuous.unwrap_err().to_string(), "unhandled tar header type 55");
        assert_eq!(
            bad_link.unwrap_err().to_string(),
            "invalid hardlink target \"../outside\""
        );
        assert_eq!(
            bad_whiteout.unwrap_err().to_string(),
            "invalid whiteout entry \".wh..\""
        );
        assert!(!root.path().join("escape").exists());
    }

    #[test]
    fn long_link_targets_setuid_hardlinks_and_global_headers() {
        if !is_root() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let long_target = format!("/{}", "deep/".repeat(30));
        let mut builder = Builder::new(Vec::new());
        let mut global = Header::new_ustar();
        global.set_entry_type(EntryType::XGlobalHeader);
        global.set_size(0);
        global.set_cksum();
        builder.append(&global, std::io::empty()).unwrap();
        let mut file = Header::new_gnu();
        file.set_entry_type(EntryType::Regular);
        file.set_mode(0o4755);
        file.set_uid(0);
        file.set_gid(0);
        file.set_size(2);
        builder.append_data(&mut file, "bin/tool", b"hi".as_slice()).unwrap();
        let mut hard = Header::new_gnu();
        hard.set_entry_type(EntryType::Link);
        hard.set_mode(0o4755);
        hard.set_uid(0);
        hard.set_gid(0);
        hard.set_size(0);
        builder.append_link(&mut hard, "bin/alias", "bin/tool").unwrap();
        let mut link = Header::new_gnu();
        link.set_entry_type(EntryType::Symlink);
        link.set_mode(0o777);
        link.set_uid(0);
        link.set_gid(0);
        link.set_size(0);
        builder.append_link(&mut link, "long", &long_target).unwrap();
        let layer = builder.into_inner().unwrap();

        untar(layer.as_slice(), root.path()).unwrap();

        assert_eq!(
            std::fs::read_link(root.path().join("long")).unwrap(),
            PathBuf::from(&long_target)
        );
        let mode = std::fs::metadata(root.path().join("bin/alias"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o4755);
        assert_eq!(
            std::fs::metadata(root.path().join("bin")).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
}
