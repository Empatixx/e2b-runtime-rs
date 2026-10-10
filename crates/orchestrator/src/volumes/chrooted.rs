mod directory;
pub mod error;
pub mod file;
pub mod info;
pub mod metrics;
mod names;
mod parent;
mod remove;
mod resolve;
mod support;
mod syscalls;

use std::ffi::{CStr, CString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, CWD, Gid, Mode, OFlags, Uid, chmodat, chownat, fstat, mkdirat, statat};
use rustix::io::Errno;

use crate::go_os::path_error;
use crate::go_path;
use directory::entry_names;
use error::{PathError, failed};
use file::ConfinedFile;
use info::FileInfo;
use names::{check_component, display_path, kernel_path, split_parent};
use parent::Parent;
use remove::remove_all_at;
use resolve::resolve_path;
use syscalls::{fchmodat2_empty_path, procfs_path, resolve_open, retry_interrupted, syscall_mode};

pub use support::check_support;

pub struct Chrooted {
    root: OwnedFd,
    source: PathBuf,
}

impl Chrooted {
    pub fn open(source: &Path) -> Result<Self, String> {
        check_support()?;
        retry_interrupted(|| {
            rustix::fs::open(
                source,
                OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )
        })
        .map(|root| Self {
            root,
            source: source.to_path_buf(),
        })
        .map_err(|errno| path_error("open", source, &std::io::Error::from_raw_os_error(errno.raw_os_error())))
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    pub fn open_read(&self, name: &str) -> Result<ConfinedFile, PathError> {
        self.open_confined(name, OFlags::RDONLY, 0)
    }

    pub fn create_new(&self, name: &str, permissions: u32) -> Result<ConfinedFile, PathError> {
        self.open_confined(name, OFlags::CREATE | OFlags::WRONLY | OFlags::EXCL, permissions)
    }

    pub fn create_truncating(&self, name: &str, permissions: u32) -> Result<ConfinedFile, PathError> {
        self.open_confined(name, OFlags::CREATE | OFlags::WRONLY | OFlags::TRUNC, permissions)
    }

    pub fn eval_symlinks(&self, name: &str) -> Result<String, PathError> {
        resolve_path(self.root.as_fd(), name, true)
    }

    pub fn stat(&self, name: &str) -> Result<FileInfo, PathError> {
        self.info("stat", name, OFlags::empty())
    }

    pub fn lstat(&self, name: &str) -> Result<FileInfo, PathError> {
        self.info("lstat", name, OFlags::NOFOLLOW)
    }

    pub fn read_dir(&self, name: &str) -> Result<Vec<FileInfo>, PathError> {
        let directory = self.open_at("readdir", name, OFlags::RDONLY | OFlags::DIRECTORY, Mode::empty())?;
        let mut names = entry_names(&directory, usize::MAX).map_err(failed("readdir", name))?;
        names.sort();
        names
            .iter()
            .filter_map(|entry| match lstat_entry(directory.as_fd(), entry) {
                Err(Errno::NOENT) => None,
                Ok(info) => Some(Ok(info)),
                Err(errno) => Some(Err(failed(
                    "readdir",
                    &go_path::join(&[name, &entry.to_string_lossy()]),
                )(errno))),
            })
            .collect()
    }

    pub fn mkdir(&self, name: &str, permissions: u32) -> Result<(), PathError> {
        let parent = self.open_parent("mkdir", name)?;
        retry_interrupted(|| mkdirat(&parent.fd, parent.base.as_str(), syscall_mode(permissions)))
            .map_err(failed("mkdir", name))
    }

    pub fn mkdir_all(&self, name: &str, permissions: u32) -> Result<(), PathError> {
        let name = kernel_path(name);
        match self.info("mkdir", &name, OFlags::empty()) {
            Ok(info) if info.is_dir() => return Ok(()),
            Ok(_) => return Err(failed("mkdir", &name)(Errno::NOTDIR)),
            Err(_) => {}
        }
        let parent = split_parent(&name).dir;
        if parent != name {
            self.mkdir_all(&parent, permissions)?;
        }
        self.mkdir(&name, permissions).or_else(|error| {
            match self
                .info("mkdir", &name, OFlags::empty())
                .is_ok_and(|info| info.is_dir())
            {
                true => Ok(()),
                false => Err(error),
            }
        })
    }

    pub fn chmod(&self, name: &str, mode: u32) -> Result<(), PathError> {
        let fd = self.open_at("chmod", name, OFlags::PATH, Mode::empty())?;
        let bits = syscall_mode(mode);
        match retry_interrupted(|| fchmodat2_empty_path(fd.as_fd(), bits)) {
            Err(Errno::OPNOTSUPP | Errno::NOSYS) => {
                retry_interrupted(|| chmodat(CWD, procfs_path(&fd), bits, AtFlags::empty()))
            }
            result => result,
        }
        .map_err(failed("chmod", name))
    }

    pub fn chown(&self, name: &str, uid: Option<u32>, gid: Option<u32>) -> Result<(), PathError> {
        let fd = self.open_at("chown", name, OFlags::PATH, Mode::empty())?;
        retry_interrupted(|| {
            chownat(
                &fd,
                "",
                uid.map(Uid::from_raw),
                gid.map(Gid::from_raw),
                AtFlags::EMPTY_PATH,
            )
        })
        .map_err(failed("chown", name))
    }

    pub fn remove_all(&self, name: &str) -> Result<(), PathError> {
        let parent = match self.open_parent("removeall", name) {
            Err(error) if error.is_not_found() => return Ok(()),
            result => result?,
        };
        match parent.require_directory() {
            Err(Errno::NOENT) => return Ok(()),
            result => result.map_err(failed("removeall", name))?,
        }
        CString::new(parent.base.as_str())
            .map_err(|_| Errno::INVAL)
            .and_then(|base| remove_all_at(parent.fd.as_fd(), &base))
            .map_err(failed("removeall", name))
    }

    fn open_at(&self, operation: &str, name: &str, flags: OFlags, mode: Mode) -> Result<OwnedFd, PathError> {
        resolve_open(self.root.as_fd(), &kernel_path(name), flags, mode).map_err(failed(operation, name))
    }

    fn open_confined(&self, name: &str, flags: OFlags, permissions: u32) -> Result<ConfinedFile, PathError> {
        self.open_at("open", name, flags, syscall_mode(permissions & 0o777))
            .map(|fd| ConfinedFile::new(fd, display_path(name)))
    }

    fn open_parent(&self, operation: &str, name: &str) -> Result<Parent, PathError> {
        let parent = split_parent(name);
        check_component(parent.base.as_bytes()).map_err(failed(operation, name))?;
        resolve_open(
            self.root.as_fd(),
            &parent.dir,
            OFlags::PATH | OFlags::DIRECTORY,
            Mode::empty(),
        )
        .map(|fd| Parent {
            fd,
            base: parent.base,
            trailing_slash: parent.trailing_slash,
        })
        .map_err(failed(operation, name))
    }

    fn info(&self, operation: &str, name: &str, flags: OFlags) -> Result<FileInfo, PathError> {
        let fd = self.open_at(operation, name, OFlags::PATH | flags, Mode::empty())?;
        fstat(&fd)
            .map(|stat| FileInfo::from_stat(name, &stat))
            .map_err(failed(operation, name))
    }
}

fn lstat_entry(directory: BorrowedFd<'_>, name: &CStr) -> Result<FileInfo, Errno> {
    check_component(name.to_bytes())?;
    retry_interrupted(|| statat(directory, name, AtFlags::SYMLINK_NOFOLLOW))
        .map(|stat| FileInfo::from_stat(&name.to_string_lossy(), &stat))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use tempfile::TempDir;

    use super::remove::REMOVE_ALL_BATCH;
    use super::resolve::MAX_RESOLVE_STEPS;
    use super::*;
    use crate::volumes::chrooted::info::FileKind;

    fn chroot() -> (Chrooted, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        (Chrooted::open(dir.path()).unwrap(), dir)
    }

    fn write_host_file(path: &Path, content: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read_file(fs: &Chrooted, name: &str) -> String {
        let mut file = fs.open_read(name).unwrap();
        let mut content = Vec::new();
        let mut buffer = [0u8; 64];
        loop {
            match file.read(&mut buffer).unwrap() {
                0 => return String::from_utf8(content).unwrap(),
                read => content.extend_from_slice(&buffer[..read]),
            }
        }
    }

    fn names(infos: &[FileInfo]) -> Vec<&str> {
        infos.iter().map(|info| info.name.as_str()).collect()
    }

    #[test]
    fn file_systems_are_isolated() {
        let (one, _one_dir) = chroot();
        assert!(one.read_dir("/").unwrap().is_empty());

        let mut file = one.create_truncating("/test.txt", 0o644).unwrap();
        file.write_all(b"hello one").unwrap();
        drop(file);

        assert_eq!(read_file(&one, "/test.txt"), "hello one");
        assert_eq!(names(&one.read_dir("/").unwrap()), ["test.txt"]);
        let (two, _two_dir) = chroot();
        assert!(two.open_read("/test.txt").err().unwrap().is_not_found());
    }

    #[test]
    fn paths_are_relative_to_the_root() {
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("etc/passwd"), "inside");

        for name in [
            "/etc/passwd",
            "etc/passwd",
            "/../etc/passwd",
            "/../../../etc/passwd",
            "/etc/../etc/passwd",
            "//etc//passwd",
        ] {
            assert_eq!(read_file(&fs, name), "inside", "path {name:?}");
        }
        assert!(fs.open_read("/missing/../etc/passwd").err().unwrap().is_not_found());
        assert!(fs.stat("/").unwrap().is_dir());
        assert!(fs.stat("/..").unwrap().is_dir());
        assert_eq!(names(&fs.read_dir("/").unwrap()), ["etc"]);
    }

    struct Links {
        fs: Chrooted,
        dir: TempDir,
        outside: TempDir,
    }

    fn links() -> Links {
        let (fs, dir) = chroot();
        let outside = tempfile::tempdir().unwrap();
        write_host_file(&outside.path().join("secret"), "outside");
        write_host_file(
            &dir.path()
                .join(outside.path().strip_prefix("/").unwrap())
                .join("secret"),
            "inside",
        );
        write_host_file(&dir.path().join("data/file"), "data");
        fs.mkdir("/dir", 0o755).unwrap();
        let link = |target: &str, name: &str| symlink(target, dir.path().join(name)).unwrap();
        link(&outside.path().join("secret").display().to_string(), "absolute");
        link("../data/file", "dir/relative");
        link("../../../../data/file", "dir/climbing");
        link("/absolute", "chain");
        link("/data", "dirlink");
        link("/loop2", "loop1");
        link("/loop1", "loop2");
        link("/nowhere", "dangling");
        Links { fs, dir, outside }
    }

    #[test]
    fn an_absolute_link_is_anchored_at_the_root() {
        let links = links();

        assert_eq!(read_file(&links.fs, "/absolute"), "inside");
        assert_eq!(
            links.fs.eval_symlinks("/absolute").unwrap(),
            links.outside.path().join("secret").display().to_string()
        );
    }

    #[test]
    fn relative_and_climbing_links_resolve_inside_the_root() {
        let links = links();

        assert_eq!(read_file(&links.fs, "/dir/relative"), "data");
        assert_eq!(read_file(&links.fs, "/dir/climbing"), "data");
        assert_eq!(links.fs.eval_symlinks("/dir/climbing").unwrap(), "/data/file");
        assert_eq!(read_file(&links.fs, "/chain"), "inside");
    }

    #[test]
    fn a_link_to_a_directory_in_the_middle_of_a_path_is_followed() {
        let links = links();

        assert_eq!(read_file(&links.fs, "/dirlink/file"), "data");
        assert_eq!(links.fs.stat("/dirlink/file").unwrap().name, "file");
        links.fs.chmod("/dirlink/file", 0o600).unwrap();
        let host = fs::metadata(links.dir.path().join("data/file")).unwrap();
        assert_eq!(host.permissions().mode() & 0o777, 0o600);
    }

    #[test]
    fn operations_that_do_not_follow_the_final_link() {
        let links = links();

        assert_eq!(links.fs.lstat("/absolute").unwrap().kind, FileKind::Symlink);
        assert!(links.fs.create_new("/dangling", 0o644).err().unwrap().is_exist());
    }

    #[test]
    fn loops_fail() {
        let links = links();

        assert!(links.fs.stat("/loop1").is_err());
        assert!(links.fs.eval_symlinks("/loop1").unwrap_err().has(Errno::LOOP));
    }

    #[test]
    fn creating_through_a_dangling_link_creates_its_target_inside_the_root() {
        let links = links();

        assert!(links.fs.stat("/dangling").unwrap_err().is_not_found());
        links.fs.create_truncating("/dangling", 0o644).unwrap();

        assert!(links.dir.path().join("nowhere").is_file());
    }

    #[test]
    fn the_host_is_untouched() {
        let links = links();
        links.fs.create_truncating("/absolute", 0o644).unwrap();

        assert_eq!(
            fs::read_to_string(links.outside.path().join("secret")).unwrap(),
            "outside"
        );
    }

    #[test]
    fn parent_components_resolve_after_symlinks() {
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("a/b/inner"), "inner");
        write_host_file(&dir.path().join("a/victim"), "sibling of the target");
        write_host_file(&dir.path().join("victim"), "sibling of the link");
        symlink("/a/b", dir.path().join("link")).unwrap();

        assert_eq!(read_file(&fs, "/link/../victim"), "sibling of the target");
        assert_eq!(fs.eval_symlinks("/link/../victim").unwrap(), "/a/victim");
        fs.mkdir_all("/link/../x/y", 0o755).unwrap();
        assert!(dir.path().join("a/x/y").is_dir());
        assert!(!dir.path().join("x").exists());
        assert_eq!(read_file(&fs, "/../../victim"), "sibling of the link");
        assert_eq!(read_file(&fs, "/link/../../../victim"), "sibling of the link");
    }

    #[test]
    fn mkdir_all_creates_the_directory_a_path_names_even_with_trailing_dots() {
        let (fs, dir) = chroot();

        fs.mkdir_all("/x/y/.", 0o755).unwrap();
        assert!(dir.path().join("x/y").is_dir());
        fs.mkdir_all("/x/z/..", 0o755).unwrap();
        assert!(dir.path().join("x/z").is_dir());
        fs.mkdir_all("/.", 0o755).unwrap();
        fs.mkdir_all("/..", 0o755).unwrap();
        fs.mkdir_all("/x/y/./", 0o755).unwrap();
        write_host_file(&dir.path().join("file"), "");
        assert!(fs.mkdir_all("/file/.", 0o755).unwrap_err().has(Errno::NOTDIR));
    }

    #[test]
    fn creating_through_an_absolute_directory_link_stays_inside_the_root() {
        let (fs, dir) = chroot();
        fs.mkdir("/real", 0o755).unwrap();
        symlink("/real", dir.path().join("link")).unwrap();

        fs.mkdir_all("/link/a/b", 0o755).unwrap();
        assert!(dir.path().join("real/a/b").is_dir());
        let file = fs.create_truncating("/link/a/b/file", 0o666).unwrap();
        assert_eq!(file.name(), "/link/a/b/file");
        assert!(dir.path().join("real/a/b/file").is_file());
        fs.remove_all("/link/a").unwrap();
        assert!(!dir.path().join("real/a").exists());
    }

    #[test]
    fn a_file_is_named_inside_the_root() {
        let (fs, _dir) = chroot();
        fs.mkdir_all("/dir", 0o755).unwrap();

        let file = fs.create_truncating("dir//file", 0o666).unwrap();

        assert_eq!(file.name(), "/dir/file");
    }

    #[test]
    fn opening_without_create_ignores_the_mode() {
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("file"), "content");

        assert_eq!(read_file(&fs, "/file"), "content");
        assert!(fs.open_read("/missing").err().unwrap().is_not_found());
        assert!(fs.create_new("/file", 0o644).err().unwrap().is_exist());
        assert!(fs.open_read("/").is_ok());
    }

    #[test]
    fn read_dir_does_not_follow_links() {
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("d/file"), "content");
        symlink("file", dir.path().join("d/link")).unwrap();
        symlink("/nowhere", dir.path().join("d/dangling")).unwrap();
        fs.mkdir("/d/sub", 0o755).unwrap();

        let entries = fs.read_dir("/d").unwrap();

        assert_eq!(names(&entries), ["dangling", "file", "link", "sub"]);
        let kinds: Vec<FileKind> = entries.iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            [
                FileKind::Symlink,
                FileKind::File,
                FileKind::Symlink,
                FileKind::Directory
            ]
        );
        assert_eq!(entries[1].size, 7);
    }

    #[test]
    fn a_root_must_be_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        write_host_file(&dir.path().join("file"), "");

        assert!(Chrooted::open(&dir.path().join("file")).is_err());
        assert!(
            Chrooted::open(&dir.path().join("missing"))
                .err()
                .unwrap()
                .ends_with("no such file or directory")
        );
    }

    #[test]
    fn concurrent_operations_do_not_interfere() {
        const WORKERS: usize = 8;
        const FILES_PER_WORKER: usize = 50;
        let (fs, dir) = chroot();

        std::thread::scope(|scope| {
            for worker in 0..WORKERS {
                let fs = &fs;
                scope.spawn(move || {
                    let base = format!("/w/{worker}");
                    fs.mkdir_all(&base, 0o755).unwrap();
                    for index in 0..FILES_PER_WORKER {
                        let name = format!("{base}/{index}");
                        let mut file = fs.create_truncating(&name, 0o666).unwrap();
                        file.write_all(name.as_bytes()).unwrap();
                        assert_eq!(fs.stat(&name).unwrap().size, i64::try_from(name.len()).unwrap());
                    }
                });
            }
        });

        let count: usize = (0..WORKERS)
            .map(|worker| fs::read_dir(dir.path().join(format!("w/{worker}"))).unwrap().count())
            .sum();
        assert_eq!(count, WORKERS * FILES_PER_WORKER);
    }

    #[test]
    fn final_dot_components_cannot_escape() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("volume");
        fs::create_dir(&root).unwrap();
        write_host_file(&parent.path().join("sibling/file"), "host");
        write_host_file(&root.join("a/file"), "inside");
        let fs = Chrooted::open(&root).unwrap();

        for name in ["/..", "/a/../..", "/a/..", "/.", "/a/.", "/", "..", "."] {
            assert!(fs.remove_all(name).is_err(), "remove_all {name:?}");
            assert!(fs.mkdir(name, 0o755).is_err(), "mkdir {name:?}");
        }

        assert!(parent.path().join("sibling/file").is_file());
        assert!(root.join("a/file").is_file());
        write_host_file(&root.join("b/file"), "inside");
        fs.remove_all("/a/../b").unwrap();
        assert!(!root.join("b").exists());
        assert!(root.join("a/file").is_file());
    }

    #[test]
    fn resolving_through_a_file_is_not_a_directory() {
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("file"), "x");
        write_host_file(&dir.path().join("x"), "y");
        symlink("/file", dir.path().join("link")).unwrap();

        assert!(fs.eval_symlinks("/link/../x").unwrap_err().has(Errno::NOTDIR));
        assert!(fs.eval_symlinks("/file/../x").unwrap_err().has(Errno::NOTDIR));
    }

    #[test]
    fn resolution_stops_at_the_step_budget() {
        let (fs, dir) = chroot();
        fs::create_dir(dir.path().join("d")).unwrap();

        let too_long = format!("/d{}", "/.".repeat(MAX_RESOLVE_STEPS + 1));
        assert!(fs.eval_symlinks(&too_long).unwrap_err().has(Errno::NAMETOOLONG));
        assert_eq!(fs.eval_symlinks(&format!("/d{}", "/.".repeat(16))).unwrap(), "/d");
    }

    #[test]
    fn remove_all_removes_a_large_tree_and_ignores_missing_paths() {
        let (fs, dir) = chroot();
        let big = dir.path().join("big");
        fs::create_dir(&big).unwrap();
        (0..3 * REMOVE_ALL_BATCH + 7).for_each(|index| fs::write(big.join(index.to_string()), b"").unwrap());
        write_host_file(&big.join("nested/deeper/file"), "x");

        fs.remove_all("/big").unwrap();

        assert!(!big.exists());
        fs.remove_all("/big").unwrap();
        fs.remove_all("/never/existed").unwrap();
    }

    #[test]
    fn remove_all_removes_a_link_not_its_target() {
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("target/file"), "x");
        symlink("/target", dir.path().join("link")).unwrap();

        fs.remove_all("/link").unwrap();

        assert!(dir.path().join("target/file").is_file());
        assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
    }

    #[test]
    fn chown_and_chmod_change_the_entry_they_resolve_to() {
        if !nix::unistd::geteuid().is_root() {
            return;
        }
        let (fs, dir) = chroot();
        write_host_file(&dir.path().join("file"), "x");

        fs.chown("/file", Some(1234), None).unwrap();
        fs.chmod("/file", 0o640).unwrap();

        let host = fs::metadata(dir.path().join("file")).unwrap();
        assert_eq!((host.uid(), host.mode() & 0o777), (1234, 0o640));
        let info = fs.stat("/file").unwrap();
        assert_eq!((info.uid, info.permissions), (1234, 0o640));
    }

    #[test]
    fn the_host_supports_confinement() {
        assert_eq!(check_support(), Ok(()));
    }
}
