mod scoped;

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use nix::fcntl::AtFlags;
use nix::sys::stat::{Mode, SFlag, UtimensatFlags, makedev, mknod, utimensat};
use nix::sys::time::TimeSpec;
use nix::unistd::{Gid, Uid, fchownat, mkfifo};
use tar::{Archive, Entry, EntryType, Header};

use scoped::{clean_relative, resolve_dir};

const WHITEOUT_PREFIX: &str = ".wh.";
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";
const OPAQUE_XATTR: &str = "trusted.overlay.opaque";
const XATTR_PAX_PREFIX: &str = "SCHILY.xattr.";
const PERMISSION_BITS: u32 = 0o7777;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Owner {
    uid: u32,
    gid: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Times {
    accessed: i64,
    modified: i64,
}

struct DirectoryTimes {
    path: PathBuf,
    times: Times,
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
        .rev()
        .try_for_each(|directory| set_times(&directory.path, directory.times))
}

fn apply<R: Read>(entry: &mut Entry<R>, root: &Path) -> io::Result<Option<DirectoryTimes>> {
    let mut parts = clean_relative(&entry.path()?);
    let Some(name) = parts.pop() else {
        return Ok(None);
    };
    let header = entry.header().clone();
    let parent = resolve_dir(root, &parts, true)?;
    let name_text = name.to_string_lossy().into_owned();
    if name_text == OPAQUE_WHITEOUT {
        xattr::set(&parent, OPAQUE_XATTR, b"y")?;
        return Ok(None);
    }
    if let Some(hidden) = name_text.strip_prefix(WHITEOUT_PREFIX) {
        return whiteout(&parent.join(hidden), &owner(&header)?).map(|()| None);
    }
    let path = parent.join(&name);
    remove_conflicting(&path, header.entry_type())?;
    create(entry, &header, &path, root)?;
    finish(entry, &header, &path)
}

fn whiteout(path: &Path, owner: &Owner) -> io::Result<()> {
    mknod(path, SFlag::S_IFCHR, Mode::empty(), makedev(0, 0))?;
    chown(path, owner)
}

fn remove_conflicting(path: &Path, kind: EntryType) -> io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(existing) if existing.is_dir() && kind == EntryType::Directory => Ok(()),
        Ok(existing) if existing.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

fn create<R: Read>(entry: &mut Entry<R>, header: &Header, path: &Path, root: &Path) -> io::Result<()> {
    let mode = header.mode()? & PERMISSION_BITS;
    match header.entry_type() {
        EntryType::Directory => match path.is_dir() {
            true => Ok(()),
            false => std::fs::create_dir(path),
        },
        EntryType::Regular | EntryType::Continuous => write_file(entry, path, mode),
        EntryType::Symlink => std::os::unix::fs::symlink(link_name(header)?, path),
        EntryType::Link => std::fs::hard_link(hard_link_target(header, root)?, path),
        EntryType::Char => make_device(header, path, SFlag::S_IFCHR, mode),
        EntryType::Block => make_device(header, path, SFlag::S_IFBLK, mode),
        EntryType::Fifo => mkfifo(path, Mode::from_bits_truncate(mode)).map_err(io::Error::from),
        _ => Ok(()),
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

fn make_device(header: &Header, path: &Path, kind: SFlag, mode: u32) -> io::Result<()> {
    let major = header.device_major()?.unwrap_or_default();
    let minor = header.device_minor()?.unwrap_or_default();
    mknod(
        path,
        kind,
        Mode::from_bits_truncate(mode),
        makedev(major.into(), minor.into()),
    )
    .map_err(io::Error::from)
}

fn link_name(header: &Header) -> io::Result<PathBuf> {
    header
        .link_name()?
        .map(|name| name.into_owned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "link entry without a target"))
}

fn hard_link_target(header: &Header, root: &Path) -> io::Result<PathBuf> {
    let mut parts: Vec<OsString> = clean_relative(&link_name(header)?);
    let name = parts
        .pop()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "hard link to the layer root"))?;
    resolve_dir(root, &parts, false).map(|parent| parent.join(name))
}

fn finish<R: Read>(entry: &mut Entry<R>, header: &Header, path: &Path) -> io::Result<Option<DirectoryTimes>> {
    let kind = header.entry_type();
    chown(path, &owner(header)?)?;
    if kind != EntryType::Symlink && kind != EntryType::Link {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(header.mode()? & PERMISSION_BITS))?;
    }
    apply_xattrs(entry, path)?;
    let times = times(header)?;
    match kind {
        EntryType::Directory => Ok(Some(DirectoryTimes {
            path: path.to_path_buf(),
            times,
        })),
        EntryType::Link => Ok(None),
        _ => set_times(path, times).map(|()| None),
    }
}

fn apply_xattrs<R: Read>(entry: &mut Entry<R>, path: &Path) -> io::Result<()> {
    let Some(extensions) = entry.pax_extensions()? else {
        return Ok(());
    };
    extensions.filter_map(Result::ok).try_for_each(|extension| {
        let Some(name) = extension.key().ok().and_then(|key| key.strip_prefix(XATTR_PAX_PREFIX)) else {
            return Ok(());
        };
        match xattr::set(path, name, extension.value_bytes()) {
            Err(e) if name.starts_with("security.") && is_unsupported(&e) => Ok(()),
            outcome => outcome,
        }
    })
}

fn is_unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(nix::libc::EOPNOTSUPP) | Some(nix::libc::EPERM)
    )
}

fn owner(header: &Header) -> io::Result<Owner> {
    let raw = header.as_old();
    Ok(Owner {
        uid: numeric(header.uid(), &raw.uid)? as u32,
        gid: numeric(header.gid(), &raw.gid)? as u32,
    })
}

fn numeric(parsed: io::Result<u64>, field: &[u8]) -> io::Result<u64> {
    match parsed {
        Err(_) if field.iter().all(|byte| matches!(byte, 0 | b' ')) => Ok(0),
        parsed => parsed,
    }
}

fn chown(path: &Path, owner: &Owner) -> io::Result<()> {
    fchownat(
        None,
        path,
        Some(Uid::from_raw(owner.uid)),
        Some(Gid::from_raw(owner.gid)),
        AtFlags::AT_SYMLINK_NOFOLLOW,
    )
    .map_err(io::Error::from)
}

fn times(header: &Header) -> io::Result<Times> {
    let modified = numeric(header.mtime(), &header.as_old().mtime)? as i64;
    let accessed = header
        .as_gnu()
        .and_then(|gnu| gnu.atime().ok())
        .map_or(modified, |atime| atime as i64);
    Ok(Times { accessed, modified })
}

fn set_times(path: &Path, times: Times) -> io::Result<()> {
    utimensat(
        None,
        path,
        &TimeSpec::new(times.accessed, 0),
        &TimeSpec::new(times.modified, 0),
        UtimensatFlags::NoFollowSymlink,
    )
    .map_err(io::Error::from)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileTypeExt;

    use tar::Builder;

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
            ("dir/hard", EntryType::Link, b"", "dir/file"),
            ("../../escape", EntryType::Regular, b"x", ""),
        ]);

        untar(layer.as_slice(), root.path()).unwrap();

        assert_eq!(
            xattr::get(root.path().join("dir"), OPAQUE_XATTR).unwrap(),
            Some(b"y".to_vec())
        );
        assert_eq!(std::fs::read(root.path().join("dir/hard")).unwrap(), b"data");
        assert!(root.path().join("escape").is_file());
        assert_eq!(
            std::fs::metadata(root.path().join("dir/file"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
