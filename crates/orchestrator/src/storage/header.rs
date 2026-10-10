mod build_data;
mod compressed;
mod diff;
pub mod mapping;
mod metadata;
mod v4;
mod v5;

use std::collections::BTreeMap;

pub use build_data::BuildData;
pub use compressed::MAX_UNCOMPRESSED_BLOCK;
pub use diff::DiffMetadata;
pub use mapping::{BuildMapping, MAPPING_SIZE};
pub use metadata::{METADATA_SIZE, Metadata, VERSION_V4, VERSION_V5};
use uuid::Uuid;
pub use v4::Serialized;

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
    pub builds: Option<BTreeMap<Uuid, BuildData>>,
    pub incomplete_pending_upload: bool,
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
        Ok(Self {
            metadata,
            mappings,
            builds: None,
            incomplete_pending_upload: false,
        })
    }

    fn encode(&self) -> Vec<u8> {
        self.metadata
            .encode()
            .into_iter()
            .chain(self.mappings.iter().flat_map(BuildMapping::encode))
            .collect()
    }

    pub fn serialize(&self) -> Result<Vec<u8>, String> {
        self.serialize_counted().map(|serialized| serialized.bytes)
    }

    pub fn serialize_counted(&self) -> Result<Serialized, String> {
        let (metadata, builds, mappings) = (&self.metadata, self.builds.as_ref(), self.mappings.as_slice());
        match metadata.format_version() {
            1..=3 => {
                let bytes = self.encode();
                Ok(Serialized {
                    block_len: bytes.len(),
                    bytes,
                })
            }
            VERSION_V4 => v4::serialize(metadata, builds, mappings, self.incomplete_pending_upload),
            VERSION_V5 => v5::serialize(metadata, builds, mappings, self.incomplete_pending_upload),
            _ => Err(format!("unsupported header version {}", metadata.version)),
        }
    }

    pub fn serialize_for_storage(&self, path: &str) -> Result<Serialized, String> {
        self.serialize_for_storage_capped(path, MAX_UNCOMPRESSED_BLOCK)
    }

    pub fn serialize_for_storage_capped(&self, path: &str, cap: usize) -> Result<Serialized, String> {
        if self.incomplete_pending_upload {
            return Err(format!("refusing to persist incomplete header for {path}"));
        }
        let serialized = self.serialize_counted().map_err(|e| format!("serialize header: {e}"))?;
        let compressed = self.metadata.format_version() >= VERSION_V4;
        match compressed && serialized.block_len > cap {
            true => Err(format!(
                "refusing to persist header for {path}: uncompressed block {} exceeds cap {cap}",
                serialized.block_len
            )),
            false => Ok(serialized),
        }
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        Self::decode_capped(bytes, MAX_UNCOMPRESSED_BLOCK)
    }

    pub fn decode_capped(bytes: &[u8], cap: usize) -> Result<Self, String> {
        let (metadata, rest) = compressed::split_metadata(bytes)?;
        match metadata.format_version() {
            VERSION_V5 => v5::deserialize(metadata, rest, cap),
            VERSION_V4 => v4::deserialize(metadata, rest, cap),
            1..=3 => Self::decode_v3(metadata, rest),
            _ => Err(format!("unsupported header version {}", metadata.version)),
        }
    }

    fn decode_v3(metadata: Metadata, entries: &[u8]) -> Result<Self, String> {
        let (entries, partial) = entries.as_chunks::<MAPPING_SIZE>();
        if !partial.is_empty() {
            return Err("failed to read block mapping: unexpected EOF".to_owned());
        }
        let mappings = entries.iter().map(BuildMapping::decode).collect();
        Self::new(metadata, mappings)
    }

    pub fn finalized(&self) -> Self {
        Self {
            incomplete_pending_upload: false,
            ..self.clone()
        }
    }

    pub fn set_build(&mut self, build_id: Uuid, data: BuildData) {
        self.builds.get_or_insert_with(BTreeMap::new).insert(build_id, data);
    }

    pub fn backfill_missing_v3_uncompressed_builds(&mut self) {
        let v4_plus = self.metadata.format_version() >= VERSION_V4;
        let own = self.metadata.build_id;
        let missing: Vec<Uuid> = self
            .mappings
            .iter()
            .map(|mapping| mapping.build_id)
            .filter(|build_id| !build_id.is_nil())
            .filter(|build_id| !v4_plus || *build_id == own)
            .filter(|build_id| self.builds.as_ref().is_none_or(|builds| !builds.contains_key(build_id)))
            .collect();
        for build_id in missing {
            self.set_build(build_id, BuildData::default());
        }
    }

    pub fn frame_data(&self, build_id: Uuid) -> Option<&crate::storage::frame_table::FrameTable> {
        self.builds
            .as_ref()
            .and_then(|builds| builds.get(&build_id))
            .and_then(|data| data.frame_data.as_ref())
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
    fn serialize_refuses_versions_outside_one_to_five() {
        let metadata = Metadata {
            version: 6,
            ..Metadata::template(id(1), 4096, 4096)
        };
        let header = Header::new(metadata, Vec::new()).unwrap();

        assert_eq!(header.serialize().unwrap_err(), "unsupported header version 6");
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

    fn v5_metadata(build: u8, size: u64) -> Metadata {
        Metadata {
            version: VERSION_V5,
            block_size: 4096,
            size,
            generation: 0,
            build_id: id(build),
            base_build_id: id(build),
        }
    }

    fn built(builds: Vec<(u8, BuildData)>) -> Option<BTreeMap<Uuid, BuildData>> {
        Some(builds.into_iter().map(|(build, data)| (id(build), data)).collect())
    }

    fn sized(size: i64) -> BuildData {
        BuildData {
            size,
            ..BuildData::default()
        }
    }

    #[test]
    fn a_v3_header_round_trips_without_builds() {
        let header = Header::new(
            Metadata {
                generation: 7,
                ..Metadata::template(id(1), 4096, 8192)
            },
            vec![mapping(0, 4096, 1, 0), mapping(4096, 4096, 2, 8192)],
        )
        .unwrap();

        let read = Header::decode(&header.serialize().unwrap()).unwrap();

        assert_eq!(read, header);
        assert_eq!(read.builds, None);
        assert_eq!(read.frame_data(id(1)), None);
    }

    #[test]
    fn a_header_with_no_mappings_decodes_to_the_default_mapping() {
        let bytes = Metadata::template(id(1), 4096, 8192).encode();

        let read = Header::decode(&bytes).unwrap();

        assert_eq!(read.mappings, vec![mapping(0, 8192, 1, 0)]);
    }

    #[test]
    fn a_header_with_a_zero_block_size_is_refused() {
        let bytes = Metadata::template(id(1), 0, 4096).encode();

        assert_eq!(Header::decode(&bytes).unwrap_err(), "block size cannot be zero");
    }

    #[test]
    fn storing_refuses_a_header_that_is_still_incomplete() {
        let mut header = Header::new(v5_metadata(1, 4096), Vec::new()).unwrap();
        header.incomplete_pending_upload = true;

        assert_eq!(
            header.serialize_for_storage("build/memfile.header").unwrap_err(),
            "refusing to persist incomplete header for build/memfile.header"
        );
        assert!(header.finalized().serialize_for_storage("build/memfile.header").is_ok());
    }

    #[test]
    fn storing_refuses_a_block_over_the_cap_but_reading_the_same_bytes_with_a_higher_cap_works() {
        let mappings: Vec<_> = (0..100)
            .map(|index| mapping(index * 4096, 4096, 1 + (index % 2) as u8, (index / 2) * 4096))
            .collect();
        let header = Header::new(v5_metadata(1, 100 * 4096), mappings).unwrap();
        let v4 = Header {
            metadata: Metadata {
                version: VERSION_V4,
                ..header.metadata
            },
            ..header
        };

        let error = v4.serialize_for_storage_capped("header", 1024).unwrap_err();

        assert!(
            error.starts_with("refusing to persist header for header: uncompressed block "),
            "{error}"
        );
        assert!(error.ends_with(" exceeds cap 1024"), "{error}");
        assert!(v4.serialize_for_storage("header").is_ok());
    }

    #[test]
    fn backfill_gives_a_v4_header_only_its_own_missing_build() {
        let mut header = Header::new(
            v5_metadata(1, 4 * 4096),
            vec![
                mapping(0, 4096, 1, 0),
                mapping(4096, 4096, 9, 0),
                mapping(8192, 4096, 10, 0),
                mapping(12288, 4096, 11, 0),
            ],
        )
        .unwrap();
        let known = BuildData {
            size: 4096,
            ..BuildData::default()
        };
        header.builds = built(vec![(9, known.clone())]);

        header.backfill_missing_v3_uncompressed_builds();

        assert_eq!(header.builds, built(vec![(1, BuildData::default()), (9, known)]));
        assert_eq!(header.frame_data(id(10)), None);
    }

    #[test]
    fn backfill_gives_a_v3_header_every_build() {
        let mut header = Header::new(
            Metadata {
                build_id: id(1),
                ..Metadata::template(id(1), 4096, 3 * 4096)
            },
            vec![
                mapping(0, 4096, 1, 0),
                mapping(4096, 4096, 2, 0),
                mapping(8192, 4096, 3, 0),
            ],
        )
        .unwrap();

        header.backfill_missing_v3_uncompressed_builds();

        assert_eq!(
            header.builds,
            built(vec![
                (1, BuildData::default()),
                (2, BuildData::default()),
                (3, BuildData::default())
            ])
        );
    }

    #[test]
    fn backfill_creates_the_builds_map_for_a_header_that_had_none() {
        let mut header = Header::new(v5_metadata(1, 4096), vec![mapping(0, 4096, 1, 0)]).unwrap();
        assert_eq!(header.builds, None);

        header.backfill_missing_v3_uncompressed_builds();

        assert_eq!(header.builds, built(vec![(1, BuildData::default())]));
    }

    #[test]
    fn an_ancestor_without_an_entry_stays_without_one_through_a_pause_and_a_reload() {
        use crate::storage::frame_table::{CompressionType, FrameSize, FrameTable};

        let known_frames = FrameTable::full(
            CompressionType::Zstd,
            &[FrameSize {
                uncompressed: 4096,
                compressed: 1024,
            }],
        );
        let mut parent = Header::new(
            v5_metadata(1, 3 * 4096),
            vec![
                mapping(0, 4096, 1, 0),
                mapping(4096, 4096, 9, 0),
                mapping(8192, 4096, 10, 0),
            ],
        )
        .unwrap();
        parent.builds = built(vec![
            (1, sized(4096)),
            (
                9,
                BuildData {
                    size: 4096,
                    frame_data: Some(known_frames),
                    ..BuildData::default()
                },
            ),
        ]);

        let mut child = diff(&[0], &[]).to_diff_header(&parent, id(20)).unwrap();
        assert!(child.incomplete_pending_upload);
        assert!(child.builds.as_ref().unwrap().contains_key(&id(9)));
        assert!(!child.builds.as_ref().unwrap().contains_key(&id(10)));
        child.set_build(id(20), sized(4096));
        let mut read = Header::decode(&child.finalized().serialize().unwrap()).unwrap();
        read.backfill_missing_v3_uncompressed_builds();

        assert_eq!(read.locate(8192).unwrap().build_id, id(10));
        assert!(!read.builds.as_ref().unwrap().contains_key(&id(10)));
        assert_eq!(read.frame_data(id(10)), None);
        assert!(read.frame_data(id(9)).is_some());
    }

    #[tokio::test]
    async fn load_stored_header_keeps_the_builds_exactly_as_written_while_load_header_backfills_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::storage::backend::Storage::local(dir.path()).unwrap();
        let mut header = Header::new(
            Metadata {
                version: VERSION_V4,
                base_build_id: id(2),
                ..v5_metadata(1, 8192)
            },
            vec![mapping(0, 4096, 1, 0), mapping(4096, 4096, 2, 0)],
        )
        .unwrap();
        header.builds = built(vec![(2, sized(4096))]);
        storage.store_header("build/memfile.header", &header).await.unwrap();

        let stored = storage.load_stored_header("build/memfile.header").await.unwrap();
        let loaded = storage.load_header("build/memfile.header").await.unwrap();

        assert_eq!(stored.builds, built(vec![(2, sized(4096))]));
        assert_eq!(loaded.builds, built(vec![(1, BuildData::default()), (2, sized(4096))]));
    }

    #[tokio::test]
    async fn a_diff_header_cannot_be_stored_until_it_is_finalized() {
        let dir = tempfile::tempdir().unwrap();
        let storage = crate::storage::backend::Storage::local(dir.path()).unwrap();
        let child = diff(&[0], &[]).to_diff_header(&template(2), id(2)).unwrap();

        assert!(storage.store_header("build/header", &child).await.is_err());
        assert!(storage.store_header("build/header", &child.finalized()).await.is_ok());
    }
}
