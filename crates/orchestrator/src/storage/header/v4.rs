use std::collections::BTreeMap;

use uuid::Uuid;

use super::compressed::{self, FLAG_INCOMPLETE};
use super::{BuildData, BuildMapping, Header, MAPPING_SIZE, Metadata};

#[derive(Debug)]
pub struct Serialized {
    pub bytes: Vec<u8>,
    pub block_len: usize,
}

pub fn serialize(
    metadata: &Metadata,
    builds: Option<&BTreeMap<Uuid, BuildData>>,
    mappings: &[BuildMapping],
    incomplete: bool,
) -> Result<Serialized, String> {
    let mut block = Vec::new();
    compressed::write_builds(&mut block, builds, mappings);
    block.extend_from_slice(&(mappings.len() as u32).to_le_bytes());
    mappings
        .iter()
        .for_each(|mapping| block.extend_from_slice(&mapping.encode()));
    let bytes = compressed::assemble(metadata, incomplete, &block)
        .map_err(|e| format!("failed to LZ4-compress v4 header block: {e}"))?;
    Ok(Serialized {
        bytes,
        block_len: block.len(),
    })
}

pub fn deserialize(metadata: Metadata, after_metadata: &[u8], cap: usize) -> Result<Header, String> {
    let opened = compressed::open("v4", after_metadata, cap)?;
    let mut reader = opened.block.as_slice();
    let builds = compressed::read_builds(&mut reader)?;
    let count = compressed::read_u32(&mut reader).map_err(|e| format!("failed to read mappings count: {e}"))?;
    if count as usize > reader.len() / MAPPING_SIZE {
        return Err(format!(
            "mapping count {count} exceeds remaining {} bytes",
            reader.len()
        ));
    }
    let (records, _) = reader.as_chunks::<MAPPING_SIZE>();
    let mappings = records.iter().take(count as usize).map(BuildMapping::decode).collect();
    let mut header = Header::new(metadata, mappings)?;
    header.builds = builds;
    header.incomplete_pending_upload = opened.flags & FLAG_INCOMPLETE != 0;
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::frame_table::{CompressionType, FrameSize, FrameTable};

    const BS: u64 = 4096;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn mapping(offset: u64, build: u8, storage_offset: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length: BS,
            build_id: id(build),
            storage_offset,
        }
    }

    fn v4_header(size: u64, mappings: Vec<BuildMapping>, builds: Option<BTreeMap<Uuid, BuildData>>) -> Header {
        let metadata = Metadata {
            version: 4,
            block_size: BS,
            size,
            generation: 1,
            build_id: id(1),
            base_build_id: id(2),
        };
        let mut header = Header::new(metadata, mappings).unwrap();
        header.builds = builds;
        header
    }

    fn frames(compression: CompressionType, sizes: &[(i32, i32)]) -> FrameTable {
        let sizes: Vec<_> = sizes
            .iter()
            .map(|(uncompressed, compressed)| FrameSize {
                uncompressed: *uncompressed,
                compressed: *compressed,
            })
            .collect();
        FrameTable::full(compression, &sizes)
    }

    fn round_trip(header: &Header) -> Header {
        Header::decode(&header.serialize().unwrap()).unwrap()
    }

    #[test]
    fn a_v4_header_carries_builds_checksums_and_frame_tables() {
        let checksum = [7u8; 32];
        let builds = BTreeMap::from([
            (
                id(1),
                BuildData {
                    size: 12345,
                    checksum,
                    frame_data: Some(frames(CompressionType::Lz4, &[(2048, 1024), (2048, 900)])),
                },
            ),
            (
                id(2),
                BuildData {
                    size: 67890,
                    ..BuildData::default()
                },
            ),
        ]);
        let header = v4_header(2 * BS, vec![mapping(0, 1, 0), mapping(BS, 2, 0)], Some(builds));

        let read = round_trip(&header);

        assert_eq!(read.metadata.version, 4);
        assert_eq!(read.mappings, header.mappings);
        let builds = read.builds.unwrap();
        assert_eq!(builds.len(), 2);
        assert_eq!((builds[&id(1)].size, builds[&id(1)].checksum), (12345, checksum));
        assert_eq!(builds[&id(2)].size, 67890);
        let frame_data = builds[&id(1)].frame_data.as_ref().unwrap();
        assert_eq!(frame_data.compression_type(), CompressionType::Lz4);
        assert_eq!(frame_data.num_frames(), 2);
        let located: Vec<_> = [0, 2048]
            .map(|offset| frame_data.locate_compressed(offset).unwrap())
            .iter()
            .map(|range| (range.offset, range.length))
            .collect();
        assert_eq!(located, vec![(0, 1024), (1024, 900)]);
        assert_eq!(builds[&id(2)].frame_data, None);
    }

    #[test]
    fn the_frame_table_is_trimmed_to_the_frames_the_mappings_use() {
        let builds = BTreeMap::from([(
            id(1),
            BuildData {
                frame_data: Some(frames(
                    CompressionType::Zstd,
                    &[(4096, 2000), (4096, 3000), (4096, 3500)],
                )),
                ..BuildData::default()
            },
        )]);
        let header = v4_header(BS, vec![mapping(0, 1, 8192)], Some(builds));

        let read = round_trip(&header);

        assert_eq!(read.mappings[0].storage_offset, 8192);
        let frame_data = read.builds.unwrap()[&id(1)].frame_data.clone().unwrap();
        assert_eq!(frame_data.compression_type(), CompressionType::Zstd);
        assert_eq!(frame_data.num_frames(), 1);
        let range = frame_data.locate_compressed(8192).unwrap();
        assert_eq!((range.offset, range.length), (2000 + 3000, 3500));
        assert!(frame_data.locate_compressed(0).is_err());
    }

    #[test]
    fn builds_without_frames_read_back_without_frame_data() {
        let builds = BTreeMap::from([(
            id(1),
            BuildData {
                size: 5,
                ..BuildData::default()
            },
        )]);
        let header = v4_header(BS, vec![mapping(0, 1, 0)], Some(builds));

        assert_eq!(round_trip(&header).builds.unwrap()[&id(1)].frame_data, None);
    }

    #[test]
    fn a_header_without_builds_reads_back_without_builds() {
        let header = v4_header(BS, vec![mapping(0, 1, 0)], None);

        assert_eq!(round_trip(&header).builds, None);
    }

    #[test]
    fn the_incomplete_flag_travels_in_a_v4_header() {
        let mut header = v4_header(BS, vec![mapping(0, 1, 0)], None);
        header.incomplete_pending_upload = true;

        assert!(round_trip(&header).incomplete_pending_upload);
    }

    #[test]
    fn a_build_count_beyond_the_remaining_bytes_is_refused() {
        let bytes = 2u32.to_le_bytes();

        assert_eq!(
            compressed::read_builds(&mut bytes.as_slice()).unwrap_err(),
            "build count 2 exceeds remaining 0 bytes"
        );
    }

    #[test]
    fn a_mapping_count_beyond_the_remaining_bytes_is_refused() {
        let block = [0u32.to_le_bytes(), 2u32.to_le_bytes()].concat();
        let metadata = Metadata {
            version: 4,
            block_size: BS,
            size: BS,
            generation: 0,
            build_id: id(1),
            base_build_id: id(2),
        };
        let data = compressed::assemble(&metadata, false, &block).unwrap();

        assert_eq!(
            Header::decode(&data).unwrap_err(),
            "mapping count 2 exceeds remaining 0 bytes"
        );
    }

    #[test]
    fn a_legacy_gap_between_mappings_survives_the_round_trip() {
        let mappings = vec![mapping(0, 1, 0), mapping(2 * BS, 2, 0), mapping(3 * BS, 1, BS)];
        let header = v4_header(
            4 * BS,
            mappings.clone(),
            Some(BTreeMap::from([
                (
                    id(1),
                    BuildData {
                        size: 2 * BS as i64,
                        ..BuildData::default()
                    },
                ),
                (
                    id(2),
                    BuildData {
                        size: BS as i64,
                        ..BuildData::default()
                    },
                ),
            ])),
        );

        assert_eq!(round_trip(&header).mappings, mappings);
    }

    #[test]
    fn a_block_over_the_cap_is_refused_on_read_and_accepted_with_a_higher_cap() {
        let mappings: Vec<_> = (0..1000)
            .map(|index| mapping(index * BS, if index % 2 == 0 { 1 } else { 2 }, (index / 2) * BS))
            .collect();
        let header = v4_header(1000 * BS, mappings, None);
        let data = header.serialize().unwrap();

        assert!(Header::decode_capped(&data, 4096).unwrap_err().contains("exceeds cap"));
        assert_eq!(Header::decode_capped(&data, 1 << 20).unwrap().mappings.len(), 1000);
    }
}
