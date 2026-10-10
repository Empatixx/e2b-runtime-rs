use std::io::{self, Write};

use roaring::RoaringBitmap;

use crate::storage::header::PAGE_SIZE;

pub fn drain_dirty_pages(
    source: &mut super::compare::BlockSource,
    page_dirty: &RoaringBitmap,
    block_size: u64,
    out: &mut impl Write,
) -> io::Result<u64> {
    let pages_per_block = block_size / PAGE_SIZE;
    let mut current: Option<(u64, Vec<u8>)> = None;
    let mut written = 0;
    for page in page_dirty.iter() {
        let block = u64::from(page) / pages_per_block;
        if current.as_ref().is_none_or(|(loaded, _)| *loaded != block) {
            let bytes = source(block * block_size).map_err(io::Error::other)?;
            current = Some((block, bytes));
        }
        let within = (u64::from(page) % pages_per_block * PAGE_SIZE) as usize;
        if let Some((_, bytes)) = &current {
            out.write_all(&bytes[within..within + PAGE_SIZE as usize])?;
            written += PAGE_SIZE;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_pages_are_packed_in_page_order_across_blocks() {
        let block_size = 2 * PAGE_SIZE;
        let data: Vec<u8> = (0..4u8).flat_map(|page| vec![page; PAGE_SIZE as usize]).collect();
        let pages: RoaringBitmap = [1u32, 2].into_iter().collect();
        let mut out = Vec::new();
        let mut source = |absolute: u64| Ok(data[absolute as usize..(absolute + block_size) as usize].to_vec());

        let written = drain_dirty_pages(&mut source, &pages, block_size, &mut out).unwrap();

        assert_eq!(written, 2 * PAGE_SIZE);
        assert!(out[..PAGE_SIZE as usize].iter().all(|byte| *byte == 1));
        assert!(out[PAGE_SIZE as usize..].iter().all(|byte| *byte == 2));
    }
}
