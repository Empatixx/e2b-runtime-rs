mod diff;
pub mod mapping;
mod metadata;

pub use diff::DiffMetadata;
pub use mapping::{BuildMapping, MAPPING_SIZE};
pub use metadata::{METADATA_SIZE, Metadata};

use uuid::Uuid;

pub const PAGE_SIZE: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Location {
    pub build_id: Uuid,
    pub storage_offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub metadata: Metadata,
    pub mappings: Vec<BuildMapping>,
}

impl Header {
    pub fn new(metadata: Metadata, mappings: Vec<BuildMapping>) -> Result<Self, String> {
        if metadata.block_size == 0 {
            return Err("block size cannot be zero".to_owned());
        }
        let mappings = if mappings.is_empty() {
            vec![whole_device(&metadata)]
        } else {
            mappings
        };
        mappings
            .iter()
            .enumerate()
            .try_for_each(|(index, mapping)| check_alignment(index, mapping))?;
        Ok(Self { metadata, mappings })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.metadata
            .encode()
            .into_iter()
            .chain(self.mappings.iter().flat_map(BuildMapping::encode))
            .collect()
    }

    pub fn serialize(&self) -> Result<Vec<u8>, String> {
        match self.metadata.supported_version() {
            true => Ok(self.encode()),
            false => Err(format!("unsupported header version {}", self.metadata.version)),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (metadata, entries) = bytes
            .split_first_chunk::<METADATA_SIZE>()
            .ok_or_else(|| format!("header too short: {} bytes", bytes.len()))?;
        let metadata = Metadata::decode(metadata);
        if !metadata.supported_version() {
            return Err(format!("unsupported header version {}", metadata.version));
        }
        let (entries, partial) = entries.as_chunks::<MAPPING_SIZE>();
        if !partial.is_empty() {
            return Err("failed to read block mapping: unexpected EOF".to_owned());
        }
        let mappings = entries.iter().map(BuildMapping::decode).collect();
        Self::new(metadata, mappings)
    }

    pub fn locate(&self, offset: u64) -> Result<Location, String> {
        let size = self.metadata.size;
        if offset >= size {
            return Err(format!("offset {offset} is out of bounds (size: {size})"));
        }
        let index = self.mappings.partition_point(|mapping| mapping.offset <= offset);
        let mapping = index
            .checked_sub(1)
            .map(|index| self.mappings[index])
            .ok_or_else(|| format!("offset {offset} has no mapping"))?;
        let shift = offset - mapping.offset;
        if shift >= mapping.length {
            return Err(format!(
                "offset {offset} is beyond the end of mapping at offset {} (ends at {})",
                mapping.offset,
                mapping.end()
            ));
        }
        Ok(Location {
            build_id: mapping.build_id,
            storage_offset: mapping.storage_offset + shift,
            length: mapping.length - shift,
        })
    }
}

fn check_alignment(index: usize, mapping: &BuildMapping) -> Result<(), String> {
    let misaligned = [
        ("offset", mapping.offset),
        ("length", mapping.length),
        ("build storage offset", mapping.storage_offset),
    ]
    .into_iter()
    .find(|(_, value)| !value.is_multiple_of(PAGE_SIZE));
    if let Some((field, value)) = misaligned {
        return Err(format!(
            "compact mapping: {field} {value} at index {index} not block-aligned to {PAGE_SIZE}"
        ));
    }
    mapping
        .offset
        .checked_add(mapping.length)
        .map(|_| ())
        .ok_or_else(|| format!("compact mapping: mapping at index {index} overflows"))
}

fn whole_device(metadata: &Metadata) -> BuildMapping {
    BuildMapping {
        offset: 0,
        length: metadata.size.div_ceil(PAGE_SIZE) * PAGE_SIZE,
        build_id: metadata.build_id,
        storage_offset: 0,
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn le(values: &[u64]) -> Vec<u8> {
        values.iter().flat_map(|value| value.to_le_bytes()).collect()
    }

    #[test]
    fn template_header_covers_the_whole_device_with_one_mapping() {
        let header = Header::new(Metadata::template(id(1), 4096, 10_000), Vec::new()).unwrap();
        assert_eq!(
            header.mappings,
            vec![BuildMapping {
                offset: 0,
                length: 12_288,
                build_id: id(1),
                storage_offset: 0,
            }]
        );
        assert_eq!(header.metadata.generation, 0);
        assert_eq!(header.metadata.base_build_id, id(1));
    }

    #[test]
    fn encodes_exactly_like_upstream_v3() {
        let metadata = Metadata::template(id(1), 4096, 8192).next_generation(id(2));
        let mappings = vec![
            BuildMapping {
                offset: 0,
                length: 4096,
                build_id: id(1),
                storage_offset: 0,
            },
            BuildMapping {
                offset: 4096,
                length: 4096,
                build_id: id(2),
                storage_offset: 0,
            },
        ];
        let expected: Vec<u8> = [
            le(&[3, 4096, 8192, 1]),
            vec![2; 16],
            vec![1; 16],
            le(&[0, 4096]),
            vec![1; 16],
            le(&[0, 4096, 4096]),
            vec![2; 16],
            le(&[0]),
        ]
        .concat();

        let header = Header::new(metadata, mappings).unwrap();
        assert_eq!(header.encode(), expected);
        assert_eq!(Header::decode(&expected).unwrap(), header);
    }

    #[test]
    fn decode_rejects_bad_input() {
        let valid = Header::new(Metadata::template(id(1), 4096, 4096), Vec::new())
            .unwrap()
            .encode();

        assert_eq!(Header::decode(&valid[..10]).unwrap_err(), "header too short: 10 bytes");
        assert_eq!(
            Header::decode(&valid[..valid.len() - 3]).unwrap_err(),
            "failed to read block mapping: unexpected EOF"
        );

        let mut future = valid.clone();
        future[0] = 9;
        assert_eq!(Header::decode(&future).unwrap_err(), "unsupported header version 9");

        let zero_block = Header::new(Metadata::template(id(1), 0, 4096), Vec::new());
        assert_eq!(zero_block.unwrap_err(), "block size cannot be zero");
    }

    #[test]
    fn serialize_refuses_versions_outside_one_to_three() {
        let metadata = Metadata {
            version: 4,
            ..Metadata::template(id(1), 4096, 4096)
        };
        let header = Header::new(metadata, Vec::new()).unwrap();

        assert_eq!(header.serialize().unwrap_err(), "unsupported header version 4");
        let supported = Header::new(Metadata::template(id(1), 4096, 4096), Vec::new()).unwrap();
        assert_eq!(supported.serialize().unwrap(), supported.encode());
    }

    use roaring::RoaringBitmap;

    fn mapping(offset: u64, length: u64, build: u8, storage_offset: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length,
            build_id: id(build),
            storage_offset,
        }
    }

    fn template(blocks: u64) -> Header {
        Header::new(Metadata::template(id(1), 4096, blocks * 4096), Vec::new()).unwrap()
    }

    fn blocks(indices: &[u32]) -> RoaringBitmap {
        indices.iter().copied().collect()
    }

    fn diff(dirty: &[u32], empty: &[u32]) -> DiffMetadata {
        DiffMetadata {
            dirty: blocks(dirty),
            empty: blocks(empty),
            block_size: 4096,
        }
    }

    #[test]
    fn diff_in_the_middle_splits_the_parent() {
        let header = diff(&[2, 3], &[]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(header.metadata.generation, 1);
        assert_eq!(header.metadata.build_id, id(2));
        assert_eq!(header.metadata.base_build_id, id(1));
        assert_eq!(
            header.mappings,
            vec![
                mapping(0, 8192, 1, 0),
                mapping(8192, 8192, 2, 0),
                mapping(16384, 16384, 1, 16384),
            ]
        );
    }

    #[test]
    fn diff_at_both_edges_and_empty_blocks() {
        let header = diff(&[0, 7], &[3]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(
            header.mappings,
            vec![
                mapping(0, 4096, 2, 0),
                mapping(4096, 8192, 1, 4096),
                mapping(12288, 4096, 0, 0),
                mapping(16384, 12288, 1, 16384),
                mapping(28672, 4096, 2, 4096),
            ]
        );
    }

    #[test]
    fn diff_covering_everything_replaces_the_parent() {
        let all: Vec<u32> = (0..8).collect();
        let header = diff(&all, &[]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(header.mappings, vec![mapping(0, 32768, 2, 0)]);
    }

    #[test]
    fn empty_diff_keeps_parent_mappings() {
        let parent = template(8);
        let header = diff(&[], &[]).to_diff_header(&parent, id(2)).unwrap();
        assert_eq!(header.mappings, parent.mappings);
        assert_eq!(header.metadata.generation, 1);
    }

    #[test]
    fn second_generation_resolves_through_one_lookup() {
        let first = diff(&[2, 3], &[]).to_diff_header(&template(8), id(2)).unwrap();
        let second = diff(&[3, 4], &[]).to_diff_header(&first, id(3)).unwrap();
        assert_eq!(
            second.mappings,
            vec![
                mapping(0, 8192, 1, 0),
                mapping(8192, 4096, 2, 0),
                mapping(12288, 8192, 3, 0),
                mapping(20480, 12288, 1, 20480),
            ]
        );
        assert_eq!(
            second.locate(9000).unwrap(),
            Location {
                build_id: id(2),
                storage_offset: 808,
                length: 3288,
            }
        );
    }

    #[test]
    fn locate_handles_the_edges() {
        let header = diff(&[2, 3], &[]).to_diff_header(&template(8), id(2)).unwrap();
        assert_eq!(
            header.locate(32767).unwrap(),
            Location {
                build_id: id(1),
                storage_offset: 32767,
                length: 1,
            }
        );
        assert_eq!(
            header.locate(32768).unwrap_err(),
            "offset 32768 is out of bounds (size: 32768)"
        );
    }

    #[test]
    fn validate_reports_gaps_and_size_mismatch() {
        assert_eq!(
            mapping::validate(&[mapping(0, 4096, 1, 0), mapping(8192, 4096, 1, 0)], 12288, 4096).unwrap_err(),
            "mapping validation failed: expected offset 4096, got 8192"
        );
        assert_eq!(
            mapping::validate(&[mapping(0, 4096, 1, 0)], 8192, 4096).unwrap_err(),
            "mapping validation failed: total 4096 != size 8192"
        );
    }

    #[test]
    fn rejects_misaligned_and_overflowing_mappings() {
        let metadata = Metadata::template(id(1), 4096, 8192);
        let new = |mapping: BuildMapping| Header::new(metadata, vec![mapping]).unwrap_err();
        assert_eq!(
            new(mapping(100, 4096, 1, 0)),
            "compact mapping: offset 100 at index 0 not block-aligned to 4096"
        );
        assert_eq!(
            new(mapping(0, 100, 1, 0)),
            "compact mapping: length 100 at index 0 not block-aligned to 4096"
        );
        assert_eq!(
            new(mapping(0, 4096, 1, 7)),
            "compact mapping: build storage offset 7 at index 0 not block-aligned to 4096"
        );
        assert_eq!(
            new(mapping(u64::MAX - 4095, 8192, 1, 0)),
            "compact mapping: mapping at index 0 overflows"
        );
    }

    #[test]
    fn locate_inside_a_gap_is_an_error_not_a_panic() {
        let header = Header::new(Metadata::template(id(1), 4096, 8192), vec![mapping(0, 4096, 1, 0)]).unwrap();
        assert_eq!(
            header.locate(5000).unwrap_err(),
            "offset 5000 is beyond the end of mapping at offset 0 (ends at 4096)"
        );
    }

    #[test]
    fn validate_checks_block_multiples_and_size() {
        assert_eq!(
            mapping::validate(&[mapping(0, 4096, 1, 0), mapping(4096, 8192, 1, 0)], 8192, 4096).unwrap_err(),
            "mapping validation failed: mapping at index 1 goes beyond the size: 4096 (current offset) + 8192 (length) > 8192 (size)"
        );
        assert_eq!(
            mapping::validate(&[mapping(0, 4096, 1, 0)], 4096, 8192).unwrap_err(),
            "mapping validation failed: mapping at index 0 has an invalid length: 4096. It should be a multiple of block size: 8192"
        );
    }

    #[test]
    fn diff_with_zero_block_size_is_rejected() {
        let zero = DiffMetadata {
            dirty: blocks(&[1]),
            empty: RoaringBitmap::new(),
            block_size: 0,
        };
        assert_eq!(
            zero.to_diff_header(&template(8), id(2)).unwrap_err(),
            "diff block size must be a positive multiple of 4096, got 0"
        );
    }
}
