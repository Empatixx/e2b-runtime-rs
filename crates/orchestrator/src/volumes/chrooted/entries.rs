use std::os::fd::AsFd;
use std::time::SystemTime;

use rustix::fs::{
    AtFlags, CWD, FileType, Gid, Mode, OFlags, Timespec, Timestamps, UTIME_OMIT, Uid, chownat, fstat, readlinkat,
    renameat, symlinkat, unlinkat, utimensat,
};
use rustix::io::Errno;

use super::Chrooted;
use super::error::{LinkError, PathError, failed, link_failed};
use super::file::ConfinedFile;
use super::names::kernel_path;
use super::syscalls::{procfs_path, retry_interrupted};
use crate::go_path;

const MAX_TEMP_FILE_ATTEMPTS: usize = 10_000;
const TEMP_FILE_PERMISSIONS: u32 = 0o600;

impl Chrooted {
    pub fn rename(&self, old: &str, new: &str) -> Result<(), RenameError> {
        let mut from = self.open_parent("rename", old)?;
        let to = self.open_parent("rename", new)?;
        from.trailing_slash = from.trailing_slash || to.trailing_slash;
        let link_error = link_failed("rename", kernel_path(old), kernel_path(new));
        from.require_directory().map_err(&link_error)?;
        retry_interrupted(|| renameat(&from.fd, from.base.as_str(), &to.fd, to.base.as_str()))
            .map_err(link_error)
            .map_err(RenameError::Link)
    }

    pub fn remove(&self, name: &str) -> Result<(), PathError> {
        let parent = self.open_parent("remove", name)?;
        parent.require_directory().map_err(failed("remove", name))?;
        let unlinked = retry_interrupted(|| unlinkat(&parent.fd, parent.base.as_str(), AtFlags::empty()));
        let Err(unlink_error) = unlinked else {
            return Ok(());
        };
        match retry_interrupted(|| unlinkat(&parent.fd, parent.base.as_str(), AtFlags::REMOVEDIR)) {
            Ok(()) => Ok(()),
            Err(Errno::NOTDIR) => Err(failed("remove", name)(unlink_error)),
            Err(rmdir_error) => Err(failed("remove", name)(rmdir_error)),
        }
    }

    pub fn symlink(&self, target: &str, link: &str) -> Result<(), RenameError> {
        let parent = self.open_parent("symlink", link)?;
        let link_error = link_failed("symlink", target.to_owned(), kernel_path(link));
        if parent.trailing_slash {
            let errno = match parent.require_directory() {
                Ok(()) | Err(Errno::NOTDIR) => Errno::EXIST,
                Err(errno) => errno,
            };
            return Err(RenameError::Link(link_error(errno)));
        }
        retry_interrupted(|| symlinkat(target, &parent.fd, parent.base.as_str()))
            .map_err(link_error)
            .map_err(RenameError::Link)
    }

    pub fn readlink(&self, link: &str) -> Result<String, PathError> {
        let fd = self.open_at("readlink", link, OFlags::PATH | OFlags::NOFOLLOW, Mode::empty())?;
        retry_interrupted(|| fstat(&fd))
            .and_then(|stat| match FileType::from_raw_mode(stat.st_mode) {
                FileType::Symlink => retry_interrupted(|| readlinkat(&fd, "", Vec::new())),
                _ => Err(Errno::INVAL),
            })
            .map(|target| target.to_string_lossy().into_owned())
            .map_err(failed("readlink", link))
    }

    pub fn lchown(&self, name: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), PathError> {
        let fd = self.open_at("lchown", name, OFlags::PATH | OFlags::NOFOLLOW, Mode::empty())?;
        retry_interrupted(|| {
            chownat(
                &fd,
                "",
                uid.map(Uid::from_raw),
                gid.map(Gid::from_raw),
                AtFlags::EMPTY_PATH,
            )
        })
        .map_err(failed("lchown", name))
    }

    pub fn chtimes(
        &self,
        name: &str,
        accessed: Option<SystemTime>,
        modified: Option<SystemTime>,
    ) -> Result<(), PathError> {
        let fd = self.open_at("chtimes", name, OFlags::PATH, Mode::empty())?;
        let times = Timestamps {
            last_access: timespec(accessed),
            last_modification: timespec(modified),
        };
        match retry_interrupted(|| utimensat(fd.as_fd(), "", &times, AtFlags::EMPTY_PATH)) {
            Err(Errno::INVAL | Errno::NOENT | Errno::OPNOTSUPP) => {
                retry_interrupted(|| utimensat(CWD, procfs_path(&fd), &times, AtFlags::empty()))
            }
            result => result,
        }
        .map_err(failed("chtimes", name))
    }

    pub fn temp_file(&self, dir: &str, prefix: &str) -> Result<ConfinedFile, PathError> {
        let dir = match dir.is_empty() {
            true => "/",
            false => dir,
        };
        for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
            let name = go_path::join(&[dir, &format!("{prefix}{}", rand::random::<u32>())]);
            match self.open_temp(&name) {
                Err(error) if error.has(Errno::EXIST) => continue,
                result => return result,
            }
        }
        Err(PathError::builder()
            .operation("createtemp")
            .path(go_path::join(&[dir, &format!("{prefix}*")]))
            .errno(Errno::EXIST)
            .build())
    }

    pub fn join(&self, elements: &[&str]) -> String {
        let joined = go_path::join(elements);
        match joined.starts_with('/') {
            true => joined,
            false => format!("/{joined}"),
        }
    }

    fn open_temp(&self, name: &str) -> Result<ConfinedFile, PathError> {
        self.open_confined(
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL,
            TEMP_FILE_PERMISSIONS,
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    Path(PathError),
    Link(LinkError),
}

impl From<PathError> for RenameError {
    fn from(error: PathError) -> Self {
        Self::Path(error)
    }
}

impl From<LinkError> for RenameError {
    fn from(error: LinkError) -> Self {
        Self::Link(error)
    }
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(error) => error.fmt(formatter),
            Self::Link(error) => error.fmt(formatter),
        }
    }
}

fn timespec(time: Option<SystemTime>) -> Timespec {
    time.and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(
            Timespec {
                tv_sec: 0,
                tv_nsec: UTIME_OMIT,
            },
            |since| Timespec {
                tv_sec: since.as_secs() as i64,
                tv_nsec: i64::from(since.subsec_nanos()),
            },
        )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    fn chroot() -> (Chrooted, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Chrooted::open(dir.path()).unwrap(), dir)
    }

    fn write_host_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn errno(error: RenameError) -> Errno {
        match error {
            RenameError::Path(error) => error.errno,
            RenameError::Link(error) => error.errno,
        }
    }

    #[test]
    fn rename_follows_the_filesystem_rules_over_a_directory() {
        let (fs, root) = chroot();
        let root = root.path();
        write_host_file(&root.join("src/file"), "x");
        fs::create_dir(root.join("empty")).unwrap();
        write_host_file(&root.join("full/file"), "y");
        write_host_file(&root.join("file"), "z");

        fs.rename("/src", "/empty").unwrap();
        assert!(!root.join("src").exists());
        assert!(root.join("empty/file").exists());

        assert_eq!(errno(fs.rename("/empty", "/full").unwrap_err()), Errno::NOTEMPTY);
        assert_eq!(errno(fs.rename("/file", "/full").unwrap_err()), Errno::ISDIR);
        assert!(root.join("file").exists());
        assert_eq!(
            fs.rename("/empty", "/full").unwrap_err().to_string(),
            "rename /empty /full: directory not empty"
        );
    }

    #[test]
    fn a_trailing_slash_requires_a_directory() {
        let (fs, root) = chroot();
        let root = root.path();
        write_host_file(&root.join("file"), "x");
        fs::create_dir(root.join("dir")).unwrap();
        symlink("dir", root.join("ld")).unwrap();

        assert_eq!(errno(fs.rename("/file/", "/other").unwrap_err()), Errno::NOTDIR);
        assert_eq!(errno(fs.rename("/file", "/other/").unwrap_err()), Errno::NOTDIR);
        assert!(root.join("file").exists());
        assert_eq!(fs.remove("/ld/").unwrap_err().errno, Errno::NOTDIR);
        assert!(root.join("ld").symlink_metadata().is_ok());
        assert_eq!(fs.remove("/file/").unwrap_err().errno, Errno::NOTDIR);
        fs.remove("/dir/").unwrap();
        assert!(!root.join("dir").exists());
        fs.mkdir("/new/", 0o755).unwrap();
        assert_eq!(errno(fs.symlink("target", "/new/").unwrap_err()), Errno::EXIST);
        assert_eq!(errno(fs.symlink("target", "/missing/").unwrap_err()), Errno::NOENT);
        assert!(root.join("missing").symlink_metadata().is_err());
    }

    #[test]
    fn remove_reports_the_error_of_the_call_that_matches_the_entry() {
        let (fs, root) = chroot();
        let root = root.path();
        write_host_file(&root.join("full/file"), "y");
        write_host_file(&root.join("file"), "z");

        fs.remove("/file").unwrap();
        assert!(!root.join("file").exists());
        assert_eq!(fs.remove("/full").unwrap_err().errno, Errno::NOTEMPTY);
        assert_eq!(fs.remove("/missing").unwrap_err().errno, Errno::NOENT);
    }

    #[test]
    fn readlink_reads_a_link_and_refuses_a_regular_file() {
        let (fs, root) = chroot();
        let root = root.path();
        write_host_file(&root.join("regular"), "x");
        fs.symlink("/regular", "/link").unwrap();

        assert_eq!(fs.readlink("/link").unwrap(), "/regular");
        assert_eq!(fs.readlink("/regular").unwrap_err().errno, Errno::INVAL);
        assert_eq!(fs.readlink("/missing").unwrap_err().errno, Errno::NOENT);
        assert_eq!(errno(fs.symlink("/regular", "/link").unwrap_err()), Errno::EXIST);
    }

    #[test]
    fn chtimes_sets_the_given_times_and_leaves_an_omitted_one() {
        let (fs, root) = chroot();
        let root = root.path();
        write_host_file(&root.join("file"), "x");
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let accessed_before = fs::metadata(root.join("file")).unwrap().accessed().unwrap();

        fs.chtimes("/file", None, Some(modified)).unwrap();

        let metadata = fs::metadata(root.join("file")).unwrap();
        assert_eq!(metadata.modified().unwrap(), modified);
        assert_eq!(metadata.accessed().unwrap(), accessed_before);
    }

    #[test]
    fn a_temp_file_is_new_inside_the_tree() {
        let (fs, root) = chroot();
        fs.mkdir("/tmp", 0o755).unwrap();

        let file = fs.temp_file("/tmp", "upload-").unwrap();

        assert!(file.name().starts_with("/tmp/upload-"), "{}", file.name());
        assert!(root.path().join(file.name().trim_start_matches('/')).exists());
        assert!(fs.temp_file("", "x").unwrap().name().starts_with("/x"));
    }

    #[test]
    fn join_is_rooted() {
        let (fs, _root) = chroot();

        assert_eq!(fs.join(&["a", "b"]), "/a/b");
        assert_eq!(fs.join(&["/a", "../b"]), "/b");
    }
}
