use roaring::RoaringBitmap;
use uuid::Uuid;

use super::mapping::{from_blocks, merge, normalize, validate};
use super::{Header, PAGE_SIZE};

#[derive(Debug, Clone, Default)]
pub struct DiffMetadata {
    pub dirty: RoaringBitmap,
    pub empty: RoaringBitmap,
    pub block_size: u64,
}

impl DiffMetadata {
    pub fn to_diff_header(&self, parent: &Header, build_id: Uuid) -> Result<Header, String> {
        if self.block_size == 0 || !self.block_size.is_multiple_of(PAGE_SIZE) {
            return Err(format!(
                "diff block size must be a positive multiple of {PAGE_SIZE}, got {}",
                self.block_size
            ));
        }
        let dirty = from_blocks(build_id, &self.dirty, self.block_size);
        let empty = from_blocks(Uuid::nil(), &self.empty, self.block_size);
        let mappings = normalize(&merge(&parent.mappings, &merge(&dirty, &empty)));

        let header = Header::new(parent.metadata.next_generation(build_id), mappings)?;
        validate(&header.mappings, header.metadata.size, PAGE_SIZE)?;
        Ok(header)
    }
}
