use std::fs::File;
use std::io;
use std::num::NonZeroUsize;
use std::ptr::NonNull;

use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};

pub struct ReadMapping {
    address: NonNull<u8>,
    length: usize,
}

unsafe impl Send for ReadMapping {}
unsafe impl Sync for ReadMapping {}

impl ReadMapping {
    pub fn of(file: &File, length: u64) -> io::Result<Option<Self>> {
        let Some(length) = NonZeroUsize::new(length as usize) else {
            return Ok(None);
        };
        let address = unsafe { mmap(None, length, ProtFlags::PROT_READ, MapFlags::MAP_SHARED, file, 0) }?;
        Ok(Some(Self {
            address: address.cast(),
            length: length.get(),
        }))
    }

    pub fn bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.address.as_ptr(), self.length) }
    }
}

impl Drop for ReadMapping {
    fn drop(&mut self) {
        let _ = unsafe { munmap(self.address.cast(), self.length) };
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::FileExt;

    use super::*;

    #[test]
    fn sees_writes_made_through_the_file_after_mapping() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(8192).unwrap();
        let mapping = ReadMapping::of(&file, 8192).unwrap().unwrap();

        file.write_all_at(&[9; 4096], 4096).unwrap();

        assert_eq!(&mapping.bytes()[4096..], &[9; 4096]);
        assert_eq!(&mapping.bytes()[..4096], &[0; 4096]);
    }

    #[test]
    fn an_empty_file_has_no_mapping() {
        let file = tempfile::tempfile().unwrap();
        assert!(ReadMapping::of(&file, 0).unwrap().is_none());
    }
}
