use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use nix::errno::Errno;

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

    pub fn adopt(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let size = file.metadata()?.len();
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

    pub fn copy_to(&self, out: &File, from: u64, to: u64, length: u64) -> io::Result<()> {
        copy_range(&self.file, out, from, to, length)
    }
}

const COPY_BUFFER: usize = 1 << 20;

fn copy_range(source: &File, out: &File, from: u64, to: u64, length: u64) -> io::Result<()> {
    let mut copied = 0;
    while copied < length {
        match copy_chunk(source, out, from + copied, to + copied, length - copied) {
            Ok(0) => {
                return Err(io::Error::other(format!(
                    "copy_file_range returned 0 with {} bytes remaining",
                    length - copied
                )));
            }
            Ok(count) => copied += count,
            Err(e) if is_unsupported(&e) => {
                return buffered_copy(source, out, from + copied, to + copied, length - copied);
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn is_unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || error
            .raw_os_error()
            .map(Errno::from_raw)
            .is_some_and(|errno| matches!(errno, Errno::EXDEV | Errno::EOPNOTSUPP | Errno::ENOSYS | Errno::EINVAL))
}

fn buffered_copy(source: &File, out: &File, from: u64, to: u64, length: u64) -> io::Result<()> {
    let mut buffer = vec![0; COPY_BUFFER.min(length as usize)];
    (0..length).step_by(COPY_BUFFER).try_for_each(|done| {
        let chunk = &mut buffer[..COPY_BUFFER.min((length - done) as usize)];
        source.read_exact_at(chunk, from + done)?;
        out.write_all_at(chunk, to + done)
    })
}

#[cfg(target_os = "linux")]
fn copy_chunk(source: &File, out: &File, from: u64, to: u64, length: u64) -> io::Result<u64> {
    use std::os::fd::AsRawFd;

    let mut read_offset = from as i64;
    let mut write_offset = to as i64;
    let copied = unsafe {
        nix::libc::copy_file_range(
            source.as_raw_fd(),
            &mut read_offset,
            out.as_raw_fd(),
            &mut write_offset,
            length as usize,
            0,
        )
    };
    Errno::result(copied).map(|count| count as u64).map_err(io::Error::from)
}

#[cfg(not(target_os = "linux"))]
fn copy_chunk(_: &File, _: &File, _: u64, _: u64, _: u64) -> io::Result<u64> {
    Err(io::ErrorKind::Unsupported.into())
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
    fn copies_a_range_to_an_explicit_offset_of_another_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = SparseFile::create(&dir.path().join("cache"), 16384).unwrap();
        file.write_at(4096, &[1; 4096]).unwrap();
        file.write_at(8192, &[2; 4096]).unwrap();
        let out = tempfile::tempfile().unwrap();

        file.copy_to(&out, 4096, 100, 8192).unwrap();

        let mut copied = vec![0; 8192];
        out.read_exact_at(&mut copied, 100).unwrap();
        assert_eq!(copied, [vec![1; 4096], vec![2; 4096]].concat());
    }

    #[test]
    fn the_buffered_copy_matches_the_kernel_copy() {
        let dir = tempfile::tempdir().unwrap();
        let file = SparseFile::create(&dir.path().join("cache"), 8192).unwrap();
        file.write_at(0, &[3; 8192]).unwrap();
        let out = tempfile::tempfile().unwrap();

        buffered_copy(&file.file, &out, 4096, 0, 4096).unwrap();

        let mut copied = vec![0; 4096];
        out.read_exact_at(&mut copied, 0).unwrap();
        assert_eq!(copied, [3; 4096]);
    }

    #[test]
    fn file_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache");
        drop(SparseFile::create(&path, 4096).unwrap());
        assert!(!path.exists());
    }

    #[test]
    fn an_adopted_file_keeps_its_bytes_and_is_removed_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("diff");
        std::fs::write(&path, [5; 8192]).unwrap();

        let file = SparseFile::adopt(&path).unwrap();
        let mut block = [0; 4096];
        file.read_at(4096, &mut block).unwrap();

        assert_eq!((file.size(), block), (8192, [5; 4096]));
        drop(file);
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
