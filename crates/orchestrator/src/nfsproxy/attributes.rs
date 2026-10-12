use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nfsserve::nfs::{fattr3, fileid3, ftype3, nfsstat3, nfstime3, set_atime, set_mtime, specdata3};
use rustix::fs::{FileType, Stat};
use rustix::io::Errno;

const ROOT_MODE: u32 = 0o555;
const BLOCK_SIZE: u64 = 512;

pub fn of_stat(stat: &Stat, fileid: fileid3) -> fattr3 {
    fattr3 {
        ftype: file_type(stat.st_mode),
        mode: stat.st_mode & 0o7777,
        nlink: u32::try_from(stat.st_nlink).unwrap_or(u32::MAX),
        uid: stat.st_uid,
        gid: stat.st_gid,
        size: u64::try_from(stat.st_size).unwrap_or_default(),
        used: u64::try_from(stat.st_blocks).unwrap_or_default() * BLOCK_SIZE,
        rdev: specdata3::default(),
        fsid: stat.st_dev,
        fileid,
        atime: time(stat.st_atime, stat.st_atime_nsec),
        mtime: time(stat.st_mtime, stat.st_mtime_nsec),
        ctime: time(stat.st_ctime, stat.st_ctime_nsec),
    }
}

pub fn of_root(fileid: fileid3) -> fattr3 {
    fattr3 {
        ftype: ftype3::NF3DIR,
        mode: ROOT_MODE,
        nlink: 2,
        fileid,
        ..fattr3::default()
    }
}

pub fn status(errno: Errno) -> nfsstat3 {
    match errno {
        Errno::NOENT => nfsstat3::NFS3ERR_NOENT,
        Errno::EXIST => nfsstat3::NFS3ERR_EXIST,
        Errno::NOTDIR => nfsstat3::NFS3ERR_NOTDIR,
        Errno::ISDIR => nfsstat3::NFS3ERR_ISDIR,
        Errno::ACCESS => nfsstat3::NFS3ERR_ACCES,
        Errno::PERM => nfsstat3::NFS3ERR_PERM,
        Errno::NOTEMPTY => nfsstat3::NFS3ERR_NOTEMPTY,
        Errno::NAMETOOLONG => nfsstat3::NFS3ERR_NAMETOOLONG,
        Errno::NOSPC => nfsstat3::NFS3ERR_NOSPC,
        Errno::ROFS => nfsstat3::NFS3ERR_ROFS,
        Errno::INVAL => nfsstat3::NFS3ERR_INVAL,
        Errno::XDEV => nfsstat3::NFS3ERR_XDEV,
        Errno::DQUOT => nfsstat3::NFS3ERR_DQUOT,
        Errno::FBIG => nfsstat3::NFS3ERR_FBIG,
        _ => nfsstat3::NFS3ERR_IO,
    }
}

pub fn access_time(how: set_atime) -> Option<SystemTime> {
    match how {
        set_atime::DONT_CHANGE => None,
        set_atime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
        set_atime::SET_TO_CLIENT_TIME(at) => Some(system_time(at)),
    }
}

pub fn modification_time(how: set_mtime) -> Option<SystemTime> {
    match how {
        set_mtime::DONT_CHANGE => None,
        set_mtime::SET_TO_SERVER_TIME => Some(SystemTime::now()),
        set_mtime::SET_TO_CLIENT_TIME(at) => Some(system_time(at)),
    }
}

fn file_type(mode: u32) -> ftype3 {
    match FileType::from_raw_mode(mode) {
        FileType::Directory => ftype3::NF3DIR,
        FileType::Symlink => ftype3::NF3LNK,
        FileType::BlockDevice => ftype3::NF3BLK,
        FileType::CharacterDevice => ftype3::NF3CHR,
        FileType::Socket => ftype3::NF3SOCK,
        FileType::Fifo => ftype3::NF3FIFO,
        _ => ftype3::NF3REG,
    }
}

fn time(seconds: i64, nanos: impl TryInto<u32>) -> nfstime3 {
    nfstime3 {
        seconds: u32::try_from(seconds).unwrap_or_default(),
        nseconds: nanos.try_into().unwrap_or_default(),
    }
}

fn system_time(at: nfstime3) -> SystemTime {
    UNIX_EPOCH + Duration::new(u64::from(at.seconds), at.nseconds)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errnos_become_their_nfs_status() {
        assert!(matches!(status(Errno::NOENT), nfsstat3::NFS3ERR_NOENT));
        assert!(matches!(status(Errno::NOTEMPTY), nfsstat3::NFS3ERR_NOTEMPTY));
        assert!(matches!(status(Errno::IO), nfsstat3::NFS3ERR_IO));
    }

    #[test]
    fn client_times_are_kept_and_server_times_are_now() {
        let at = nfstime3 {
            seconds: 10,
            nseconds: 5,
        };
        assert_eq!(
            access_time(set_atime::SET_TO_CLIENT_TIME(at)),
            Some(UNIX_EPOCH + Duration::new(10, 5))
        );
        assert_eq!(modification_time(set_mtime::DONT_CHANGE), None);
        assert!(access_time(set_atime::SET_TO_SERVER_TIME).is_some());
    }
}
