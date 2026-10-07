use roaring::RoaringBitmap;
use uuid::Uuid;

use super::Header;
use super::mapping::{from_blocks, merge, normalize, validate};

#[derive(Debug, Clone, Default)]
pub struct DiffMetadata {
    pub dirty: RoaringBitmap,
    pub empty: RoaringBitmap,
    pub block_size: u64,
}

impl DiffMetadata {
    pub fn to_diff_header(&self, parent: &Header, build_id: Uuid) -> Result<Header, String> {
        let dirty = from_blocks(build_id, &self.dirty, self.block_size);
        let empty = from_blocks(Uuid::nil(), &self.empty, self.block_size);
        let mappings = normalize(&merge(&parent.mappings, &merge(&dirty, &empty)));

        let header = Header::new(parent.metadata.next_generation(build_id), mappings)?;
        validate(&header.mappings, header.metadata.size)?;
        Ok(header)
    }
}
