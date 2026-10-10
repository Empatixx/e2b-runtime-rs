use roaring::RoaringBitmap;
use uuid::Uuid;

use super::mapping::{from_blocks, merge, normalize, validate};
use std::collections::BTreeMap;

use super::{BuildData, BuildMapping, Header, Metadata, PAGE_SIZE};

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

        let mut header = Header::new(parent.metadata.next_generation(build_id), mappings)?;
        header.builds = parent
            .builds
            .as_ref()
            .map(|builds| inherited_builds(builds, &header.mappings));
        header.incomplete_pending_upload = true;
        validate(&header.mappings, header.metadata.size, PAGE_SIZE)?;
        Ok(header)
    }

    pub fn to_resized_diff_header(&self, parent: &Header, build_id: Uuid, new_size: u64) -> Result<Header, String> {
        let resized_parent = Header {
            metadata: Metadata {
                size: new_size,
                ..parent.metadata
            },
            ..parent.clone()
        };
        let header = self.to_diff_header(&resized_parent, build_id)?;
        validate(&header.mappings, header.metadata.size, PAGE_SIZE)
            .map_err(|e| format!("invalid resized header mappings: {e}"))?;
        Ok(header)
    }
}

fn inherited_builds(parent: &BTreeMap<Uuid, BuildData>, mappings: &[BuildMapping]) -> BTreeMap<Uuid, BuildData> {
    mappings
        .iter()
        .filter_map(|mapping| {
            parent
                .get(&mapping.build_id)
                .map(|data| (mapping.build_id, data.clone()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: u64 = PAGE_SIZE;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn blocks(indices: &[u32]) -> RoaringBitmap {
        indices.iter().copied().collect()
    }

    fn source() -> Header {
        Header::new(Metadata::template(id(1), BLOCK, 4 * BLOCK), Vec::new()).unwrap()
    }

    #[test]
    fn a_resized_diff_header_covers_the_grown_device() {
        let diff = DiffMetadata {
            dirty: blocks(&[4]),
            empty: blocks(&[5, 6, 7]),
            block_size: BLOCK,
        };
        let source = source();

        let grown = diff.to_resized_diff_header(&source, id(2), 8 * BLOCK).unwrap();

        assert_eq!(source.metadata.size, 4 * BLOCK);
        assert_eq!(grown.metadata.size, 8 * BLOCK);
        assert_eq!(grown.metadata.generation, 1);
        let owners: Vec<Uuid> = [2, 4, 6]
            .iter()
            .map(|block| grown.locate(block * BLOCK).unwrap().build_id)
            .collect();
        assert_eq!(owners, vec![id(1), id(2), Uuid::nil()]);
    }

    #[test]
    fn a_resized_diff_header_refuses_an_uncovered_tail() {
        let diff = DiffMetadata {
            dirty: blocks(&[4]),
            empty: RoaringBitmap::new(),
            block_size: BLOCK,
        };

        let error = diff.to_resized_diff_header(&source(), id(2), 8 * BLOCK).unwrap_err();

        assert!(error.contains("mapping"), "{error}");
    }
}
