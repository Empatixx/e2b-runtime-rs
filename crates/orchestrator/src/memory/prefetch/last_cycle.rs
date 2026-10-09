use std::collections::BTreeSet;

use uuid::Uuid;

use crate::storage::header::{BuildMapping, Header};
use crate::template::metadata::MemoryPrefetch;

const MIB: u64 = 1 << 20;

pub fn diff_mapping(header: &Header) -> Option<MemoryPrefetch> {
    let block_size = header.metadata.block_size;
    let own = header.metadata.build_id;
    if block_size == 0 {
        return None;
    }
    let has_base = header
        .mappings
        .iter()
        .any(|mapping| mapping.build_id != Uuid::nil() && mapping.build_id != own);
    let indices: BTreeSet<u64> = header
        .mappings
        .iter()
        .filter(|mapping| mapping.build_id == own && mapping.length > 0)
        .flat_map(|mapping| blocks_of(mapping, block_size))
        .collect();
    (has_base && !indices.is_empty()).then(|| MemoryPrefetch {
        indices: indices.into_iter().collect(),
        access_types: Vec::new(),
        block_size: i64::try_from(block_size).unwrap_or(i64::MAX),
    })
}

fn blocks_of(mapping: &BuildMapping, block_size: u64) -> std::ops::RangeInclusive<u64> {
    mapping.offset / block_size..=(mapping.offset + mapping.length - 1) / block_size
}

pub fn cap(mapping: Option<MemoryPrefetch>, max_mib: i64) -> Option<MemoryPrefetch> {
    let max_mib = u64::try_from(max_mib).ok();
    mapping.map(|mapping| match (max_mib, u64::try_from(mapping.block_size)) {
        (Some(max_mib), Ok(block_size)) if block_size > 0 => {
            keep_blocks(mapping, (max_mib.saturating_mul(MIB)) / block_size)
        }
        _ => mapping,
    })
}

fn keep_blocks(mut mapping: MemoryPrefetch, max_blocks: u64) -> MemoryPrefetch {
    let keep = usize::try_from(max_blocks).unwrap_or(usize::MAX);
    mapping.indices.truncate(keep);
    mapping.access_types.truncate(keep);
    mapping
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::header::Metadata;

    const BLOCK: u64 = 4096;

    fn mapping(build_id: Uuid, offset: u64, length: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length,
            build_id,
            storage_offset: 0,
        }
    }

    fn header(own: Uuid, base: Uuid, size_blocks: u64, mappings: Vec<BuildMapping>) -> Header {
        let metadata = Metadata {
            base_build_id: base,
            ..Metadata::template(own, BLOCK, size_blocks * BLOCK)
        };
        Header::new(metadata, mappings).unwrap()
    }

    fn indices(mapping: Option<MemoryPrefetch>) -> Option<Vec<u64>> {
        mapping.map(|mapping| mapping.indices)
    }

    #[test]
    fn only_the_blocks_of_the_own_build_are_mapped_in_offset_order() {
        let (own, base) = (Uuid::from_u128(2), Uuid::from_u128(1));
        let header = header(
            own,
            base,
            8,
            vec![
                mapping(base, 0, 2 * BLOCK),
                mapping(own, 2 * BLOCK, BLOCK),
                mapping(base, 3 * BLOCK, 2 * BLOCK),
                mapping(own, 5 * BLOCK, 2 * BLOCK),
                mapping(base, 7 * BLOCK, BLOCK),
            ],
        );

        let mapped = diff_mapping(&header).unwrap();

        assert_eq!(mapped.indices, vec![2, 5, 6]);
        assert_eq!(mapped.block_size, 4096);
        assert!(mapped.access_types.is_empty());
    }

    #[test]
    fn a_full_image_without_a_base_layer_has_no_diff() {
        let own = Uuid::from_u128(2);

        assert_eq!(
            diff_mapping(&header(own, own, 4, vec![mapping(own, 0, 4 * BLOCK)])),
            None
        );
    }

    #[test]
    fn nil_layers_do_not_count_as_a_base() {
        let own = Uuid::from_u128(2);
        let header = header(
            own,
            own,
            4,
            vec![mapping(Uuid::nil(), 0, 2 * BLOCK), mapping(own, 2 * BLOCK, 2 * BLOCK)],
        );

        assert_eq!(diff_mapping(&header), None);
    }

    #[test]
    fn a_header_without_own_blocks_has_no_diff() {
        let (own, base) = (Uuid::from_u128(2), Uuid::from_u128(1));

        assert_eq!(
            diff_mapping(&header(own, base, 2, vec![mapping(base, 0, 2 * BLOCK)])),
            None
        );
    }

    #[test]
    fn overlapping_entries_map_each_block_once() {
        let (own, base) = (Uuid::from_u128(2), Uuid::from_u128(1));
        let header = header(
            own,
            base,
            8,
            vec![
                mapping(base, 0, BLOCK),
                mapping(own, BLOCK, 3 * BLOCK),
                mapping(own, 2 * BLOCK, 2 * BLOCK),
            ],
        );

        assert_eq!(indices(diff_mapping(&header)), Some(vec![1, 2, 3]));
    }

    #[test]
    fn entries_unaligned_to_a_larger_block_size_still_cover_their_last_block() {
        let (own, base) = (Uuid::from_u128(2), Uuid::from_u128(1));
        let huge = 512 * BLOCK;
        let metadata = Metadata {
            base_build_id: base,
            ..Metadata::template(own, huge, 4 * huge)
        };
        let header = Header::new(
            metadata,
            vec![
                mapping(base, 0, huge - BLOCK),
                mapping(own, huge - BLOCK, 2 * BLOCK),
                mapping(base, huge + BLOCK, 3 * huge - BLOCK),
            ],
        )
        .unwrap();

        let mapped = diff_mapping(&header).unwrap();

        assert_eq!(mapped.indices, vec![0, 1]);
        assert_eq!(mapped.block_size, huge as i64);
    }

    fn blocks(count: u64) -> MemoryPrefetch {
        MemoryPrefetch {
            indices: (0..count).collect(),
            access_types: Vec::new(),
            block_size: 2 << 20,
        }
    }

    #[test]
    fn a_negative_cap_leaves_the_mapping_alone() {
        assert_eq!(cap(Some(blocks(5)), -1), Some(blocks(5)));
        assert_eq!(cap(None, 4), None);
    }

    #[test]
    fn the_cap_keeps_the_first_blocks_that_fit() {
        assert_eq!(indices(cap(Some(blocks(5)), 6)), Some(vec![0, 1, 2]));
        assert_eq!(indices(cap(Some(blocks(5)), 0)), Some(vec![]));
        assert_eq!(indices(cap(Some(blocks(2)), 6)), Some(vec![0, 1]));
    }

    #[test]
    fn a_mapping_without_a_block_size_is_not_capped() {
        let unknown = MemoryPrefetch {
            block_size: 0,
            ..blocks(3)
        };

        assert_eq!(cap(Some(unknown.clone()), 0), Some(unknown));
    }
}
