use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

use roaring::RoaringBitmap;

use crate::storage::header::DiffMetadata;

pub fn export_file_diff(source: &Path, out: &mut impl Write, block_size: u64) -> io::Result<DiffMetadata> {
    let mut file = File::open(source)?;
    let blocks = file.metadata()?.len().div_ceil(block_size);
    let mut block = vec![0u8; block_size as usize];
    let mut diff = DiffMetadata {
        dirty: RoaringBitmap::new(),
        empty: RoaringBitmap::new(),
        block_size,
    };
    for index in 0..blocks {
        block.fill(0);
        read_block(&mut file, &mut block)?;
        let index = u32::try_from(index).map_err(|_| io::Error::other("rootfs has too many blocks"))?;
        match block.iter().all(|byte| *byte == 0) {
            true => {
                diff.empty.insert(index);
            }
            false => {
                out.write_all(&block)?;
                diff.dirty.insert(index);
            }
        }
    }
    Ok(diff)
}

fn read_block(file: &mut File, block: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < block.len() {
        match file.read(&mut block[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom};

    use super::*;

    #[test]
    fn zero_blocks_are_empty_and_the_rest_are_written_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rootfs.ext4");
        let mut file = File::create(&path).unwrap();
        file.set_len(5 * 4096 + 100).unwrap();
        file.seek(SeekFrom::Start(4096)).unwrap();
        file.write_all(b"one").unwrap();
        file.seek(SeekFrom::Start(5 * 4096 + 10)).unwrap();
        file.write_all(b"tail").unwrap();

        let mut out = Vec::new();
        let diff = export_file_diff(&path, &mut out, 4096).unwrap();

        assert_eq!(diff.dirty.iter().collect::<Vec<_>>(), vec![1, 5]);
        assert_eq!(diff.empty.iter().collect::<Vec<_>>(), vec![0, 2, 3, 4]);
        assert_eq!(out.len(), 2 * 4096);
        assert_eq!(&out[..3], b"one");
        assert_eq!(&out[4096 + 10..4096 + 14], b"tail");
    }
}
