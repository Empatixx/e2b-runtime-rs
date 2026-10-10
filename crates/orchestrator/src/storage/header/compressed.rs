use std::collections::BTreeMap;
use std::io::{Read, Write};

use lz4::liblz4::BlockChecksum;
use lz4::{BlockSize, ContentChecksum, Decoder, EncoderBuilder};
use uuid::Uuid;

use super::{BuildData, BuildMapping, METADATA_SIZE, Metadata};
use crate::storage::frame_table::{ByteRange, FrameTable};

pub const FLAGS_LEN: usize = 1;
pub const SIZE_PREFIX_LEN: usize = 4;
pub const FLAG_INCOMPLETE: u8 = 1;
pub const MAX_UNCOMPRESSED_BLOCK: usize = 256 << 20;
const BUILD_INFO_SIZE: usize = 16 + 8 + 32;
const FRAME_TABLE_HEADER_SIZE: usize = 8;

pub fn assemble(metadata: &Metadata, incomplete: bool, block: &[u8]) -> Result<Vec<u8>, String> {
    let compressed = compress(block)?;
    let flags = if incomplete { FLAG_INCOMPLETE } else { 0 };
    Ok(metadata
        .encode()
        .into_iter()
        .chain([flags])
        .chain((block.len() as u32).to_le_bytes())
        .chain(compressed)
        .collect())
}

pub struct OpenedBlock {
    pub flags: u8,
    pub block: Vec<u8>,
}

pub fn open(version: &str, after_metadata: &[u8], cap: usize) -> Result<OpenedBlock, String> {
    if after_metadata.len() < FLAGS_LEN + SIZE_PREFIX_LEN {
        return Err(format!(
            "{version} header block too short for flags + size prefix: {} bytes",
            after_metadata.len()
        ));
    }
    let flags = after_metadata[0];
    let size = u32::from_le_bytes(
        after_metadata[FLAGS_LEN..FLAGS_LEN + SIZE_PREFIX_LEN]
            .try_into()
            .unwrap_or_default(),
    );
    if size as usize > cap {
        return Err(format!("{version} header uncompressed size {size} exceeds cap {cap}"));
    }
    let block = decompress(&after_metadata[FLAGS_LEN + SIZE_PREFIX_LEN..], size as usize)
        .map_err(|e| format!("failed to LZ4-decompress {version} header block: {e}"))?;
    match block.len() == size as usize {
        true => Ok(OpenedBlock { flags, block }),
        false => Err(format!(
            "{version} header decompressed size {} != prefix {size}",
            block.len()
        )),
    }
}

pub fn compress(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = EncoderBuilder::new()
        .block_size(BlockSize::Max4MB)
        .block_checksum(BlockChecksum::BlockChecksumEnabled)
        .checksum(ContentChecksum::ChecksumEnabled)
        .level(1)
        .build(Vec::with_capacity(data.len()))
        .map_err(|e| format!("lz4 options: {e}"))?;
    encoder.write_all(data).map_err(|e| format!("lz4 compress: {e}"))?;
    let (compressed, result) = encoder.finish();
    result.map_err(|e| format!("lz4 compress close: {e}"))?;
    Ok(compressed)
}

pub fn decompress(source: &[u8], expected: usize) -> Result<Vec<u8>, String> {
    let decoder = Decoder::new(source).map_err(|e| format!("lz4 decompress: {e}"))?;
    let mut block = Vec::with_capacity(expected);
    decoder
        .take(expected as u64 + 1)
        .read_to_end(&mut block)
        .map_err(|e| format!("lz4 decompress: {e}"))?;
    Ok(block)
}

pub fn write_builds(block: &mut Vec<u8>, builds: Option<&BTreeMap<Uuid, BuildData>>, mappings: &[BuildMapping]) {
    let empty = BTreeMap::new();
    let builds = builds.unwrap_or(&empty);
    block.extend_from_slice(&(builds.len() as u32).to_le_bytes());
    let ranges = relevant_ranges(mappings);
    for (id, data) in builds {
        block.extend_from_slice(id.as_bytes());
        block.extend_from_slice(&data.size.to_le_bytes());
        block.extend_from_slice(&data.checksum);
        let build_ranges = ranges.get(id).map_or(&[][..], Vec::as_slice);
        let trimmed = FrameTable::trim_to_ranges(data.frame_data.as_ref(), build_ranges);
        FrameTable::serialize(trimmed.as_ref(), block);
    }
}

fn relevant_ranges(mappings: &[BuildMapping]) -> BTreeMap<Uuid, Vec<ByteRange>> {
    let mut ranges: BTreeMap<Uuid, Vec<ByteRange>> = BTreeMap::new();
    for mapping in mappings {
        ranges.entry(mapping.build_id).or_default().push(ByteRange {
            offset: mapping.storage_offset as i64,
            length: mapping.length as usize,
        });
    }
    ranges
}

pub fn read_builds(reader: &mut &[u8]) -> Result<Option<BTreeMap<Uuid, BuildData>>, String> {
    let count = read_u32(reader).map_err(|e| format!("failed to read build count: {e}"))?;
    if count == 0 {
        return Ok(None);
    }
    let minimum = BUILD_INFO_SIZE + FRAME_TABLE_HEADER_SIZE;
    if count as usize > reader.len() / minimum {
        return Err(format!("build count {count} exceeds remaining {} bytes", reader.len()));
    }
    let mut builds = BTreeMap::new();
    for _ in 0..count {
        let (id, data) = read_build(reader)?;
        builds.insert(id, data);
    }
    Ok(Some(builds))
}

fn read_build(reader: &mut &[u8]) -> Result<(Uuid, BuildData), String> {
    let info = take(reader, BUILD_INFO_SIZE).map_err(|e| format!("failed to read build info: {e}"))?;
    let id = Uuid::from_slice(&info[..16]).map_err(|e| e.to_string())?;
    let size = i64::from_le_bytes(info[16..24].try_into().unwrap_or_default());
    let checksum: [u8; 32] = info[24..56].try_into().unwrap_or_default();
    let frame_data =
        FrameTable::deserialize(reader).map_err(|e| format!("failed to read frame table for build {id}: {e}"))?;
    Ok((
        id,
        BuildData {
            size,
            checksum,
            frame_data,
        },
    ))
}

pub fn read_u32(reader: &mut &[u8]) -> Result<u32, String> {
    let bytes = take(reader, 4)?;
    Ok(u32::from_le_bytes(bytes.try_into().unwrap_or_default()))
}

pub fn take<'a>(reader: &mut &'a [u8], length: usize) -> Result<&'a [u8], String> {
    match reader.len() >= length {
        true => {
            let (head, rest) = reader.split_at(length);
            *reader = rest;
            Ok(head)
        }
        false => Err("unexpected EOF".to_owned()),
    }
}

pub fn split_metadata(bytes: &[u8]) -> Result<(Metadata, &[u8]), String> {
    let (metadata, rest) = bytes
        .split_first_chunk::<METADATA_SIZE>()
        .ok_or_else(|| format!("header too short: {} bytes", bytes.len()))?;
    Ok((Metadata::decode(metadata), rest))
}
