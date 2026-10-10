use prost_types::Timestamp;

use super::chrooted::Chrooted;
use super::chrooted::info::{FileInfo, FileKind};
use crate::proto::{self, FileType};

#[derive(Debug, Clone, PartialEq)]
pub struct EntryInfo {
    pub name: String,
    pub kind: FileKind,
    pub path: String,
    pub size: i64,
    pub permissions: u32,
    pub uid: u32,
    pub gid: u32,
    pub accessed: Option<Timestamp>,
    pub created: Option<Timestamp>,
    pub modified: Option<Timestamp>,
    pub symlink_target: Option<String>,
}

struct ResolvedSymlink {
    target: String,
    info: Option<FileInfo>,
}

impl Chrooted {
    pub fn get_entry(&self, path: &str) -> Result<EntryInfo, super::chrooted::error::PathError> {
        self.lstat(path).map(|info| self.entry_info(path, &info))
    }

    pub fn entry_info(&self, path: &str, info: &FileInfo) -> EntryInfo {
        let symlink = info.is_symlink().then(|| self.resolve_symlink(path));
        let (kind, permissions) = match &symlink {
            None => (info.kind, info.permissions),
            Some(ResolvedSymlink { info: None, .. }) => (FileKind::Other, 0),
            Some(ResolvedSymlink { info: Some(target), .. }) => (target.kind, target.permissions),
        };
        EntryInfo {
            name: info.name.clone(),
            kind,
            path: path.to_owned(),
            size: info.size,
            permissions,
            uid: info.uid,
            gid: info.gid,
            accessed: info.accessed,
            created: info.created,
            modified: info.modified,
            symlink_target: symlink.map(|symlink| symlink.target),
        }
    }

    fn resolve_symlink(&self, path: &str) -> ResolvedSymlink {
        match self.eval_symlinks(path) {
            Err(_) => ResolvedSymlink {
                target: path.to_owned(),
                info: None,
            },
            Ok(target) => ResolvedSymlink {
                info: self.stat(&target).ok(),
                target,
            },
        }
    }
}

impl From<EntryInfo> for proto::EntryInfo {
    fn from(entry: EntryInfo) -> Self {
        Self {
            name: entry.name,
            r#type: FileType::from(entry.kind).into(),
            path: entry.path,
            size: entry.size,
            mode: entry.permissions,
            uid: entry.uid,
            gid: entry.gid,
            modified_time: entry.modified,
            symlink_target: entry.symlink_target,
            created_time: entry.created,
            accessed_time: entry.accessed,
        }
    }
}

impl From<FileKind> for FileType {
    fn from(kind: FileKind) -> Self {
        match kind {
            FileKind::Directory => FileType::Directory,
            FileKind::File => FileType::File,
            FileKind::Symlink => FileType::Symlink,
            FileKind::Other => FileType::Unspecified,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    fn symlinked_volume() -> Option<(Chrooted, tempfile::TempDir)> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("target"), "content").unwrap();
        std::os::unix::fs::symlink("/target", dir.path().join("link")).unwrap();
        std::os::unix::fs::symlink("/nowhere", dir.path().join("dangling")).unwrap();
        std::os::unix::fs::symlink("/loop2", dir.path().join("loop1")).unwrap();
        std::os::unix::fs::symlink("/loop1", dir.path().join("loop2")).unwrap();
        std::os::unix::fs::symlink("link", dir.path().join("chain")).unwrap();
        std::fs::create_dir(dir.path().join("dir")).unwrap();
        std::os::unix::fs::symlink("/dir", dir.path().join("dirlink")).unwrap();
        Some((Chrooted::open(dir.path()).unwrap(), dir))
    }

    #[test]
    fn a_regular_entry_has_no_symlink_target() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry = fs.get_entry("/target").unwrap();

        assert_eq!(entry.symlink_target, None);
        assert_eq!((entry.size, entry.kind, entry.permissions), (7, FileKind::File, 0o644));
    }

    #[test]
    fn a_link_reports_where_it_resolves_inside_the_tree() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry = fs.get_entry("/link").unwrap();

        assert_eq!(entry.symlink_target.as_deref(), Some("/target"));
        assert_eq!(
            (entry.name.as_str(), entry.kind, entry.permissions),
            ("link", FileKind::File, 0o644)
        );
        assert_eq!(entry.size, 7);
    }

    #[test]
    fn a_chain_of_links_reports_the_final_target() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry = fs.get_entry("/chain").unwrap();

        assert_eq!(entry.symlink_target.as_deref(), Some("/target"));
        assert_eq!(entry.kind, FileKind::File);
    }

    #[test]
    fn a_link_to_a_directory_is_a_directory() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry = fs.get_entry("/dirlink").unwrap();

        assert_eq!(
            (entry.kind, entry.symlink_target.as_deref()),
            (FileKind::Directory, Some("/dir"))
        );
    }

    #[test]
    fn a_dangling_link_reports_itself_and_an_unknown_type() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry = fs.get_entry("/dangling").unwrap();

        assert_eq!(entry.symlink_target.as_deref(), Some("/dangling"));
        assert_eq!((entry.kind, entry.permissions), (FileKind::Other, 0));
    }

    #[test]
    fn a_cyclic_link_reports_itself_and_an_unknown_type() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry = fs.get_entry("/loop1").unwrap();

        assert_eq!(entry.symlink_target.as_deref(), Some("/loop1"));
        assert_eq!(entry.kind, FileKind::Other);
    }

    #[test]
    fn a_missing_entry_is_not_found() {
        let (fs, _dir) = symlinked_volume().unwrap();

        assert!(fs.get_entry("/missing").unwrap_err().is_not_found());
    }

    #[test]
    fn permissions_are_reported_as_they_are() {
        let dir = tempfile::tempdir().unwrap();
        let fs = Chrooted::open(dir.path()).unwrap();
        for (name, mode) in [
            ("read-only", 0o444),
            ("executable", 0o755),
            ("write-only", 0o200),
            ("none", 0o000),
        ] {
            std::fs::write(dir.path().join(name), "test").unwrap();
            std::fs::set_permissions(dir.path().join(name), std::fs::Permissions::from_mode(mode)).unwrap();

            assert_eq!(fs.get_entry(&format!("/{name}")).unwrap().permissions, mode, "{name}");
        }
    }

    #[test]
    fn an_empty_file_has_no_size() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.txt"), "").unwrap();

        let entry = Chrooted::open(dir.path()).unwrap().get_entry("/empty.txt").unwrap();

        assert_eq!(
            (entry.name.as_str(), entry.size, entry.kind),
            ("empty.txt", 0, FileKind::File)
        );
    }

    #[test]
    fn times_and_owner_come_from_the_stat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("file"), "test").unwrap();
        let metadata = std::fs::metadata(dir.path().join("file")).unwrap();

        let entry = Chrooted::open(dir.path()).unwrap().get_entry("/file").unwrap();

        assert_eq!((entry.uid, entry.gid), (metadata.uid(), metadata.gid()));
        assert_eq!(entry.modified.unwrap().seconds, metadata.mtime());
        assert_eq!(entry.accessed.unwrap().seconds, metadata.atime());
        assert_eq!(entry.created.unwrap().seconds, metadata.ctime());
    }

    #[test]
    fn the_proto_entry_carries_every_field() {
        let (fs, _dir) = symlinked_volume().unwrap();

        let entry: proto::EntryInfo = fs.get_entry("/link").unwrap().into();

        assert_eq!(entry.path, "/link");
        assert_eq!(entry.r#type, FileType::File as i32);
        assert_eq!(entry.symlink_target.as_deref(), Some("/target"));
        assert!(entry.modified_time.is_some());
    }
}
