use prost_types::Timestamp;
use rustix::fs::{FileType, Stat};

use crate::go_path;

const PERMISSION_BITS: u32 = 0o777;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileInfo {
    pub name: String,
    pub kind: FileKind,
    pub permissions: u32,
    pub size: i64,
    pub uid: u32,
    pub gid: u32,
    pub accessed: Option<Timestamp>,
    pub created: Option<Timestamp>,
    pub modified: Option<Timestamp>,
}

impl FileInfo {
    pub fn from_stat(name: &str, stat: &Stat) -> Self {
        Self {
            name: go_path::base(name),
            kind: kind_of(stat.st_mode),
            permissions: stat.st_mode & PERMISSION_BITS,
            size: to_i64(stat.st_size),
            uid: stat.st_uid,
            gid: stat.st_gid,
            accessed: timestamp(to_i64(stat.st_atime), to_i64(stat.st_atime_nsec)),
            created: timestamp(to_i64(stat.st_ctime), to_i64(stat.st_ctime_nsec)),
            modified: timestamp(to_i64(stat.st_mtime), to_i64(stat.st_mtime_nsec)),
        }
    }

    pub fn is_dir(&self) -> bool {
        self.kind == FileKind::Directory
    }

    pub fn is_symlink(&self) -> bool {
        self.kind == FileKind::Symlink
    }
}

fn kind_of(raw_mode: u32) -> FileKind {
    match FileType::from_raw_mode(raw_mode) {
        FileType::RegularFile => FileKind::File,
        FileType::Directory => FileKind::Directory,
        FileType::Symlink => FileKind::Symlink,
        _ => FileKind::Other,
    }
}

fn to_i64<T: TryInto<i64>>(value: T) -> i64 {
    value.try_into().unwrap_or_default()
}

fn timestamp(seconds: i64, nanos: i64) -> Option<Timestamp> {
    (seconds != 0 || nanos != 0).then(|| Timestamp {
        seconds,
        nanos: i32::try_from(nanos).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unset_time_is_absent() {
        assert_eq!(timestamp(0, 0), None);
        assert_eq!(timestamp(0, 5), Some(Timestamp { seconds: 0, nanos: 5 }));
        assert_eq!(timestamp(7, 0), Some(Timestamp { seconds: 7, nanos: 0 }));
    }

    #[test]
    fn modes_map_to_kinds() {
        assert_eq!(kind_of(0o100644), FileKind::File);
        assert_eq!(kind_of(0o040755), FileKind::Directory);
        assert_eq!(kind_of(0o120777), FileKind::Symlink);
        assert_eq!(kind_of(0o010644), FileKind::Other);
    }
}
