use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

pub struct SparseFile {
    file: File,
    path: PathBuf,
    size: u64,
}

impl SparseFile {
    pub fn create(path: &Path, size: u64) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().read(true).write(true).create_new(true).open(path)?;
        file.set_len(size)?;
        Ok(Self {
            file,
            path: path.to_path_buf(),
            size,
        })
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> io::Result<()> {
        self.file.read_exact_at(buf, offset)
    }

    pub fn write_at(&self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.write_all_at(data, offset)
    }

    pub fn punch_hole(&self, offset: u64, length: u64) -> io::Result<()> {
        punch_hole(&self.file, offset, length)
    }
}

impl Drop for SparseFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(target_os = "linux")]
fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    use std::os::fd::AsRawFd;

    use nix::fcntl::{FallocateFlags, fallocate};

    let mode = FallocateFlags::FALLOC_FL_PUNCH_HOLE | FallocateFlags::FALLOC_FL_KEEP_SIZE;
    fallocate(file.as_raw_fd(), mode, offset as i64, length as i64).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn punch_hole(file: &File, offset: u64, length: u64) -> io::Result<()> {
    file.write_all_at(&vec![0; length as usize], offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_back_writes_and_punched_holes_read_as_zeros() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/cache");
        let file = SparseFile::create(&path, 16384).unwrap();

        file.write_at(4096, &[7; 4096]).unwrap();
        let mut block = [0; 4096];
        file.read_at(4096, &mut block).unwrap();
        assert_eq!(block, [7; 4096]);

        file.punch_hole(4096, 4096).unwrap();
        file.read_at(4096, &mut block).unwrap();
        assert_eq!(block, [0; 4096]);
        assert_eq!(file.size(), 16384);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16384);
    }

    #[test]
    fn file_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        drop(SparseFile::create(&path, 4096).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn refuses_to_reuse_a_path_another_cache_owns() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        let _owner = SparseFile::create(&path, 4096).unwrap();
        let error = SparseFile::create(&path, 4096).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    }
}
