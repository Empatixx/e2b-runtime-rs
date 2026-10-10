use std::ops::Range;

use roaring::RoaringBitmap;
use uuid::Uuid;

use super::metadata::{u64_at, uuid_at};

pub const MAPPING_SIZE: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuildMapping {
    pub offset: u64,
    pub length: u64,
    pub build_id: Uuid,
    pub storage_offset: u64,
}

impl BuildMapping {
    pub fn end(&self) -> u64 {
        self.offset + self.length
    }

    pub fn encode(&self) -> Vec<u8> {
        [self.offset, self.length]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .chain(*self.build_id.as_bytes())
            .chain(self.storage_offset.to_le_bytes())
            .collect()
    }

    pub fn decode(bytes: &[u8; MAPPING_SIZE]) -> Self {
        Self {
            offset: u64_at(bytes, 0),
            length: u64_at(bytes, 8),
            build_id: uuid_at(bytes, 16),
            storage_offset: u64_at(bytes, 32),
        }
    }
}

impl BuildMapping {
    pub fn slice(&self, start: u64, end: u64) -> Self {
        Self {
            offset: start,
            length: end - start,
            build_id: self.build_id,
            storage_offset: self.storage_offset + (start - self.offset),
        }
    }

    fn continues_with(&self, next: &BuildMapping) -> bool {
        self.build_id == next.build_id
            && (next.build_id.is_nil() || next.storage_offset == self.storage_offset + self.length)
    }
}

pub fn merge(base: &[BuildMapping], diff: &[BuildMapping]) -> Vec<BuildMapping> {
    let mut merged: Vec<BuildMapping> = base
        .iter()
        .flat_map(|mapping| uncovered_parts(mapping, diff))
        .chain(diff.iter().copied())
        .filter(|mapping| mapping.length > 0)
        .collect();
    merged.sort_by_key(|mapping| mapping.offset);
    merged
}

fn uncovered_parts(mapping: &BuildMapping, diff: &[BuildMapping]) -> Vec<BuildMapping> {
    let first = diff.partition_point(|cover| cover.end() <= mapping.offset);
    let covers = diff[first..].iter().take_while(|cover| cover.offset < mapping.end());

    let mut parts = Vec::new();
    let mut cursor = mapping.offset;
    for cover in covers {
        if cover.offset > cursor {
            parts.push(mapping.slice(cursor, cover.offset));
        }
        cursor = cursor.max(cover.end());
    }
    if cursor < mapping.end() {
        parts.push(mapping.slice(cursor, mapping.end()));
    }
    parts
}

pub fn normalize(mappings: &[BuildMapping]) -> Vec<BuildMapping> {
    mappings
        .iter()
        .fold(Vec::new(), |mut result: Vec<BuildMapping>, mapping| {
            match result.last_mut() {
                Some(last) if last.continues_with(mapping) => last.length += mapping.length,
                _ => result.push(*mapping),
            }
            result
        })
}

pub fn from_blocks(build_id: Uuid, blocks: &RoaringBitmap, block_size: u64) -> Vec<BuildMapping> {
    let mut storage_offset = 0;
    block_ranges(blocks)
        .into_iter()
        .map(|range| {
            let mapping = BuildMapping {
                offset: u64::from(range.start) * block_size,
                length: u64::from(range.end - range.start) * block_size,
                build_id,
                storage_offset,
            };
            storage_offset += mapping.length;
            mapping
        })
        .collect()
}

pub fn identity_from_blocks(build_id: Uuid, blocks: &RoaringBitmap, block_size: u64) -> Vec<BuildMapping> {
    block_ranges(blocks)
        .into_iter()
        .map(|range| BuildMapping {
            offset: u64::from(range.start) * block_size,
            length: u64::from(range.end - range.start) * block_size,
            build_id,
            storage_offset: u64::from(range.start) * block_size,
        })
        .collect()
}

fn block_ranges(blocks: &RoaringBitmap) -> Vec<Range<u32>> {
    blocks.iter().fold(Vec::new(), |mut ranges: Vec<Range<u32>>, block| {
        match ranges.last_mut() {
            Some(range) if range.end == block => range.end += 1,
            _ => ranges.push(block..block + 1),
        }
        ranges
    })
}

pub fn validate(mappings: &[BuildMapping], size: u64, block_size: u64) -> Result<(), String> {
    let total = mappings.iter().enumerate().try_fold(0, |expected, (index, mapping)| {
        check_entry(index, mapping, expected, size, block_size).map(|()| expected + mapping.length)
    })?;
    if total != size {
        return Err(format!("mapping validation failed: total {total} != size {size}"));
    }
    Ok(())
}

fn check_entry(index: usize, mapping: &BuildMapping, expected: u64, size: u64, block_size: u64) -> Result<(), String> {
    if mapping.offset != expected {
        return Err(format!(
            "mapping validation failed: expected offset {expected}, got {}",
            mapping.offset
        ));
    }
    if !mapping.length.is_multiple_of(block_size) {
        return Err(format!(
            "mapping validation failed: mapping at index {index} has an invalid length: {}. It should be a multiple of block size: {block_size}",
            mapping.length
        ));
    }
    if expected + mapping.length > size {
        return Err(format!(
            "mapping validation failed: mapping at index {index} goes beyond the size: {expected} (current offset) + {} (length) > {size} (size)",
            mapping.length
        ));
    }
    Ok(())
}
