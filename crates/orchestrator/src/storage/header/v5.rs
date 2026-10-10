use std::collections::{BTreeMap, HashMap};

use uuid::Uuid;

use super::compressed::{self, FLAG_INCOMPLETE};
use super::v4::Serialized;
use super::{BuildData, BuildMapping, Header, Metadata, PAGE_SIZE};

const MAX_ENTRIES: u32 = 8 << 20;
const MAX_BUILDS: u32 = u16::MAX as u32;
const MAX_BLOCK_IDX: u64 = u32::MAX as u64;
const ENCODED_ENTRY_BYTES: usize = 3 * 4 + 2;
const COMPACT_ENTRY_BYTES: usize = 2 * 4 + 2;

pub fn serialize(
    metadata: &Metadata,
    builds: Option<&BTreeMap<Uuid, BuildData>>,
    mappings: &[BuildMapping],
    incomplete: bool,
) -> Result<Serialized, String> {
    let mut block = Vec::new();
    compressed::write_builds(&mut block, builds, mappings);
    write_mapping_section(&mut block, mappings);
    let bytes = compressed::assemble(metadata, incomplete, &block)
        .map_err(|e| format!("failed to LZ4-compress v5 header block: {e}"))?;
    Ok(Serialized {
        bytes,
        block_len: block.len(),
    })
}

fn write_mapping_section(block: &mut Vec<u8>, mappings: &[BuildMapping]) {
    let mapped: Vec<&BuildMapping> = mappings.iter().filter(|mapping| !mapping.build_id.is_nil()).collect();
    let mut table: Vec<Uuid> = Vec::new();
    let mut index_of: HashMap<Uuid, u64> = HashMap::new();
    for mapping in &mapped {
        index_of.entry(mapping.build_id).or_insert_with(|| {
            table.push(mapping.build_id);
            table.len() as u64 - 1
        });
    }
    block.extend_from_slice(&(table.len() as u32).to_le_bytes());
    table.iter().for_each(|id| block.extend_from_slice(id.as_bytes()));
    block.extend_from_slice(&(mapped.len() as u32).to_le_bytes());

    let mut previous_offset = 0u64;
    for mapping in &mapped {
        let offset = mapping.offset / PAGE_SIZE;
        put_uvarint(block, offset.wrapping_sub(previous_offset));
        previous_offset = offset;
    }
    mapped
        .iter()
        .for_each(|mapping| put_uvarint(block, mapping.length / PAGE_SIZE));
    let mut previous_storage = 0i64;
    for mapping in &mapped {
        let storage = (mapping.storage_offset / PAGE_SIZE) as i64;
        put_varint(block, storage - previous_storage);
        previous_storage = storage;
    }
    mapped
        .iter()
        .for_each(|mapping| put_uvarint(block, index_of[&mapping.build_id]));
}

pub fn deserialize(metadata: Metadata, after_metadata: &[u8], cap: usize) -> Result<Header, String> {
    if metadata.block_size == 0 {
        return Err("v5 header has zero block size".to_owned());
    }
    let opened = compressed::open("v5", after_metadata, cap)?;
    let mut reader = opened.block.as_slice();
    let builds = compressed::read_builds(&mut reader)?;
    let mappings = read_mapping_section(&mut reader, metadata.size, cap)?;
    Ok(Header {
        metadata,
        mappings,
        builds,
        incomplete_pending_upload: opened.flags & FLAG_INCOMPLETE != 0,
    })
}

struct Columns {
    offsets: Vec<u64>,
    lengths: Vec<u64>,
    storage: Vec<u64>,
    build_indices: Vec<usize>,
}

fn read_mapping_section(reader: &mut &[u8], size: u64, cap: usize) -> Result<Vec<BuildMapping>, String> {
    let size_blocks = size.div_ceil(PAGE_SIZE);
    if size_blocks > MAX_BLOCK_IDX {
        return Err(format!("mapping size block {size_blocks} exceeds uint32"));
    }
    let builds = read_build_table(reader)?;
    let count = compressed::read_u32(reader).map_err(|e| format!("failed to read mapping count: {e}"))?;
    if count > MAX_ENTRIES {
        return Err(format!("mapping count {count} exceeds maximum {MAX_ENTRIES}"));
    }
    if count == 0 {
        return Ok(match size {
            0 => Vec::new(),
            _ => vec![gap(0, size_blocks)],
        });
    }
    let by_bytes = reader.len() / 4;
    let by_memory = cap / (ENCODED_ENTRY_BYTES + 2 * COMPACT_ENTRY_BYTES);
    if count as usize > by_bytes.min(by_memory) {
        return Err(format!(
            "mapping count {count} exceeds remaining {} bytes",
            reader.len()
        ));
    }
    let columns = read_columns(reader, count as usize, builds.len())?;
    reconstruct(&columns, &builds, size_blocks)
}

fn read_build_table(reader: &mut &[u8]) -> Result<Vec<Uuid>, String> {
    let count = compressed::read_u32(reader).map_err(|e| format!("failed to read mapping build count: {e}"))?;
    if count > MAX_BUILDS {
        return Err(format!("mapping build count {count} exceeds maximum {MAX_BUILDS}"));
    }
    if count as usize > reader.len() / 16 {
        return Err(format!(
            "mapping build count {count} exceeds remaining {} bytes",
            reader.len()
        ));
    }
    (0..count)
        .map(|index| {
            let bytes =
                compressed::take(reader, 16).map_err(|e| format!("failed to read mapping build id {index}: {e}"))?;
            Uuid::from_slice(bytes).map_err(|e| e.to_string())
        })
        .collect()
}

fn read_columns(reader: &mut &[u8], count: usize, builds: usize) -> Result<Columns, String> {
    let mut offsets = Vec::with_capacity(count);
    let mut previous = 0u64;
    for index in 0..count {
        let delta = get_uvarint(reader).map_err(|e| format!("failed to read offset delta {index}: {e}"))?;
        if delta > MAX_BLOCK_IDX {
            return Err(format!("offset delta {delta} at entry {index} exceeds uint32"));
        }
        previous += delta;
        if previous > MAX_BLOCK_IDX {
            return Err(format!("offset block {previous} at entry {index} exceeds uint32"));
        }
        offsets.push(previous);
    }
    let mut lengths = Vec::with_capacity(count);
    for index in 0..count {
        let length = get_uvarint(reader).map_err(|e| format!("failed to read length {index}: {e}"))?;
        if length > MAX_BLOCK_IDX {
            return Err(format!("length block {length} at entry {index} exceeds uint32"));
        }
        lengths.push(length);
    }
    let mut storage = Vec::with_capacity(count);
    let mut previous = 0i64;
    for index in 0..count {
        let delta = get_varint(reader).map_err(|e| format!("failed to read storage delta {index}: {e}"))?;
        previous = previous.saturating_add(delta);
        if previous < 0 || previous as u64 > MAX_BLOCK_IDX {
            return Err(format!("storage block {previous} at entry {index} out of range"));
        }
        storage.push(previous as u64);
    }
    let mut build_indices = Vec::with_capacity(count);
    for index in 0..count {
        let value = get_uvarint(reader).map_err(|e| format!("failed to read build index {index}: {e}"))?;
        if value >= builds as u64 {
            return Err(format!(
                "build index {value} at entry {index} out of range ({builds} builds)"
            ));
        }
        build_indices.push(value as usize);
    }
    Ok(Columns {
        offsets,
        lengths,
        storage,
        build_indices,
    })
}

fn reconstruct(columns: &Columns, builds: &[Uuid], size_blocks: u64) -> Result<Vec<BuildMapping>, String> {
    let mut mappings = Vec::with_capacity(columns.offsets.len() * 2 + 1);
    let mut current = 0u64;
    for index in 0..columns.offsets.len() {
        let (offset, length) = (columns.offsets[index], columns.lengths[index]);
        if length == 0 {
            return Err(format!("zero-length mapping at entry {index}"));
        }
        if offset < current {
            return Err(format!(
                "mapping offset {offset} at entry {index} overlaps previous end {current}"
            ));
        }
        if offset > current {
            mappings.push(gap(current, offset));
        }
        let end = offset + length;
        if end > size_blocks {
            return Err(format!(
                "mapping end block {end} at entry {index} exceeds size {size_blocks}"
            ));
        }
        mappings.push(BuildMapping {
            offset: offset * PAGE_SIZE,
            length: length * PAGE_SIZE,
            build_id: builds[columns.build_indices[index]],
            storage_offset: columns.storage[index] * PAGE_SIZE,
        });
        current = end;
    }
    if current < size_blocks {
        mappings.push(gap(current, size_blocks));
    }
    Ok(mappings)
}

fn gap(start_block: u64, end_block: u64) -> BuildMapping {
    BuildMapping {
        offset: start_block * PAGE_SIZE,
        length: (end_block - start_block) * PAGE_SIZE,
        build_id: Uuid::nil(),
        storage_offset: 0,
    }
}

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_varint(out: &mut Vec<u8>, value: i64) {
    put_uvarint(out, ((value << 1) ^ (value >> 63)) as u64);
}

fn get_uvarint(reader: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = compressed::take(reader, 1)?[0];
        if shift == 63 && byte > 1 {
            return Err("varint overflows a 64-bit integer".to_owned());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("varint overflows a 64-bit integer".to_owned())
}

fn get_varint(reader: &mut &[u8]) -> Result<i64, String> {
    let unsigned = get_uvarint(reader)?;
    Ok(((unsigned >> 1) as i64) ^ -((unsigned & 1) as i64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::frame_table::{CompressionType, FrameSize, FrameTable};

    const BS: u64 = 4096;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn map(offset: u64, length: u64, build: Uuid, storage_offset: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length,
            build_id: build,
            storage_offset,
        }
    }

    fn v5_header(metadata: Metadata, mappings: Vec<BuildMapping>, builds: Option<BTreeMap<Uuid, BuildData>>) -> Header {
        let mut header = Header::new(Metadata { version: 5, ..metadata }, mappings).unwrap();
        header.builds = builds;
        header
    }

    fn meta(block_size: u64, size: u64) -> Metadata {
        Metadata {
            version: 5,
            block_size,
            size,
            generation: 3,
            build_id: id(1),
            base_build_id: id(2),
        }
    }

    fn sized(size: i64) -> BuildData {
        BuildData {
            size,
            ..BuildData::default()
        }
    }

    fn round_trip(header: &Header) -> Header {
        Header::decode(&header.serialize().unwrap()).unwrap()
    }

    #[test]
    fn page_granular_mappings_survive_a_hugepage_block_size() {
        let hugepage = 2 << 20;
        let mappings = vec![
            map(0, BS, id(1), 0),
            map(BS, BS, id(2), 0),
            map(2 * BS, 2 * BS, id(1), BS),
        ];
        let header = v5_header(
            meta(hugepage, 4 * BS),
            mappings.clone(),
            Some(BTreeMap::from([(id(1), sized(100)), (id(2), sized(50))])),
        );

        let read = round_trip(&header);

        assert_eq!(read.mappings, mappings);
        assert_eq!(read.metadata.block_size, hugepage);
    }

    #[test]
    fn nil_regions_are_dropped_on_the_wire_and_rebuilt_on_read() {
        let mappings = vec![
            map(0, 2 * BS, id(1), 0),
            map(2 * BS, BS, Uuid::nil(), 0),
            map(3 * BS, BS, id(2), 0),
            map(4 * BS, 2 * BS, id(1), 2 * BS),
        ];
        let checksum = [9u8; 32];
        let builds = BTreeMap::from([
            (
                id(1),
                BuildData {
                    size: 100,
                    checksum,
                    frame_data: Some(FrameTable::full(
                        CompressionType::Zstd,
                        &[
                            FrameSize {
                                uncompressed: BS as i32,
                                compressed: 1000,
                            },
                            FrameSize {
                                uncompressed: 2 * BS as i32,
                                compressed: 2000,
                            },
                        ],
                    )),
                },
            ),
            (id(2), sized(200)),
        ]);
        let header = v5_header(meta(BS, 6 * BS), mappings.clone(), Some(builds));

        let read = round_trip(&header);

        assert_eq!(read.metadata.version, 5);
        assert_eq!(read.mappings, mappings);
        let builds = read.builds.unwrap();
        assert_eq!(builds.len(), 2);
        assert_eq!((builds[&id(1)].size, builds[&id(1)].checksum), (100, checksum));
        assert_eq!(builds[&id(2)].size, 200);
        assert_eq!(
            builds[&id(1)].frame_data.as_ref().unwrap().compression_type(),
            CompressionType::Zstd
        );
    }

    #[test]
    fn a_size_that_is_not_page_aligned_rounds_the_mapping_up() {
        let header = v5_header(meta(BS, BS + 1), Vec::new(), None);

        let read = round_trip(&header);

        assert_eq!(read.metadata.size, BS + 1);
        assert_eq!(read.mappings[0].length, 2 * BS);
    }

    #[test]
    fn v5_and_v4_read_back_the_same_mappings_and_builds() {
        let mappings = vec![
            map(0, BS, id(1), 0),
            map(BS, BS, id(2), 0),
            map(2 * BS, BS, id(1), BS),
            map(3 * BS, BS, id(3), 0),
        ];
        let builds = BTreeMap::from([(id(1), sized(10)), (id(2), sized(20)), (id(3), sized(30))]);
        let v5 = v5_header(meta(BS, 4 * BS), mappings.clone(), Some(builds.clone()));
        let v4 = Header {
            metadata: Metadata {
                version: 4,
                ..v5.metadata
            },
            ..v5.clone()
        };

        let (read_v4, read_v5) = (round_trip(&v4), round_trip(&v5));

        assert_eq!(read_v4.mappings, read_v5.mappings);
        assert_eq!(read_v4.builds, read_v5.builds);
    }

    fn alternating(runs: u64) -> (Vec<BuildMapping>, u64) {
        let (mut offset, mut storage_a, mut storage_b) = (0, 0, 0);
        let mappings = (0..runs)
            .map(|index| {
                let (build, storage) = match index % 2 {
                    0 => (id(1), &mut storage_a),
                    _ => (id(2), &mut storage_b),
                };
                let mapping = map(offset, BS, build, *storage);
                offset += BS;
                *storage += BS;
                mapping
            })
            .collect();
        (mappings, offset)
    }

    #[test]
    fn v5_is_at_least_twice_as_small_as_v4_on_a_fragmented_header() {
        let (mappings, size) = alternating(50_000);
        let builds = BTreeMap::from([(id(1), sized(1)), (id(2), sized(1))]);
        let v5 = v5_header(meta(BS, size), mappings, Some(builds));
        let v4 = Header {
            metadata: Metadata {
                version: 4,
                ..v5.metadata
            },
            ..v5.clone()
        };

        let (v4_bytes, v5_bytes) = (v4.serialize().unwrap(), v5.serialize().unwrap());

        assert!(
            v5_bytes.len() < v4_bytes.len() / 2,
            "v4={} v5={}",
            v4_bytes.len(),
            v5_bytes.len()
        );
        assert_eq!(Header::decode(&v5_bytes).unwrap().mappings, v5.mappings);
    }

    #[test]
    fn alternating_mapped_and_nil_entries_rebuild_contiguously() {
        let mappings = vec![
            map(0, BS, Uuid::nil(), 0),
            map(BS, BS, id(1), 0),
            map(2 * BS, BS, Uuid::nil(), 0),
            map(3 * BS, BS, id(1), BS),
        ];
        let header = v5_header(
            meta(BS, 4 * BS),
            mappings.clone(),
            Some(BTreeMap::from([(id(1), sized(2 * BS as i64))])),
        );

        assert_eq!(round_trip(&header).mappings, mappings);
    }

    #[test]
    fn an_uncompressed_size_prefix_over_the_cap_is_refused() {
        let header = v5_header(meta(BS, BS), Vec::new(), None);
        let mut data = header.serialize().unwrap();
        let prefix = crate::storage::header::METADATA_SIZE + compressed::FLAGS_LEN;
        data[prefix..prefix + 4].fill(0xFF);

        assert!(Header::decode(&data).unwrap_err().contains("exceeds cap"));
    }

    #[test]
    fn a_large_sparse_mapping_round_trips_entry_for_entry() {
        let (mut offset, mut storage_a, mut storage_b) = (0, 0, 0);
        let mut mappings = Vec::new();
        for index in 0..60_000u64 {
            let length = BS * (1 + index % 3);
            mappings.push(match index % 3 {
                0 => {
                    let mapping = map(offset, length, id(1), storage_a);
                    storage_a += length;
                    mapping
                }
                1 => map(offset, length, Uuid::nil(), 0),
                _ => {
                    let mapping = map(offset, length, id(2), storage_b);
                    storage_b += length;
                    mapping
                }
            });
            offset += length;
        }
        let header = v5_header(
            meta(BS, offset),
            mappings.clone(),
            Some(BTreeMap::from([
                (id(1), sized(storage_a as i64)),
                (id(2), sized(storage_b as i64)),
            ])),
        );

        let read = round_trip(&header);

        assert_eq!(read.mappings, mappings);
        crate::storage::header::mapping::validate(&read.mappings, offset, BS).unwrap();
    }

    #[test]
    fn a_zero_block_size_is_refused() {
        let header = v5_header(meta(BS, BS), Vec::new(), None);
        let mut data = header.serialize().unwrap();
        data[8..16].fill(0);

        assert_eq!(Header::decode(&data).unwrap_err(), "v5 header has zero block size");
    }

    #[test]
    fn varints_follow_the_go_encoding() {
        let mut bytes = Vec::new();
        put_uvarint(&mut bytes, 300);
        put_varint(&mut bytes, -3);
        put_varint(&mut bytes, 3);

        assert_eq!(bytes, vec![0xAC, 0x02, 0x05, 0x06]);
        let mut reader = bytes.as_slice();
        assert_eq!(get_uvarint(&mut reader).unwrap(), 300);
        assert_eq!(get_varint(&mut reader).unwrap(), -3);
        assert_eq!(get_varint(&mut reader).unwrap(), 3);
    }
}
