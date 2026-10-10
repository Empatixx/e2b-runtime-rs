use std::fmt;

const MAX_DESERIALIZED_FRAMES: u32 = 1024 * 1024;
const ENTRY_SIZE: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressionType {
    None,
    Zstd,
    Lz4,
}

impl CompressionType {
    pub fn code(self) -> u32 {
        match self {
            CompressionType::None => 0,
            CompressionType::Zstd => 1,
            CompressionType::Lz4 => 2,
        }
    }

    pub fn from_code(code: u32) -> Self {
        match code {
            1 => CompressionType::Zstd,
            2 => CompressionType::Lz4,
            _ => CompressionType::None,
        }
    }

    pub fn suffix(self) -> &'static str {
        match self {
            CompressionType::Zstd => ".zstd",
            CompressionType::Lz4 => ".lz4",
            CompressionType::None => "",
        }
    }
}

impl fmt::Display for CompressionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CompressionType::Zstd => "zstd",
            CompressionType::Lz4 => "lz4",
            CompressionType::None => "none",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSize {
    pub uncompressed: i32,
    pub compressed: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteRange {
    pub offset: i64,
    pub length: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FrameEntry {
    start_u: i64,
    start_c: i64,
    size_u: i32,
    size_c: i32,
}

impl FrameEntry {
    fn end_u(&self) -> i64 {
        self.start_u + i64::from(self.size_u)
    }

    fn end_c(&self) -> i64 {
        self.start_c + i64::from(self.size_c)
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.start_u.to_le_bytes());
        out.extend_from_slice(&self.start_c.to_le_bytes());
        out.extend_from_slice(&self.size_u.to_le_bytes());
        out.extend_from_slice(&self.size_c.to_le_bytes());
    }

    fn decode(bytes: &[u8]) -> Self {
        let i64_at = |at: usize| i64::from_le_bytes(bytes[at..at + 8].try_into().unwrap_or_default());
        let i32_at = |at: usize| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap_or_default());
        Self {
            start_u: i64_at(0),
            start_c: i64_at(8),
            size_u: i32_at(16),
            size_c: i32_at(20),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameTable {
    compression_type: CompressionType,
    entries: Vec<FrameEntry>,
}

impl FrameTable {
    pub fn full(compression_type: CompressionType, sizes: &[FrameSize]) -> Self {
        let mut start_u = 0i64;
        let mut start_c = 0i64;
        let entries = sizes
            .iter()
            .map(|size| {
                let entry = FrameEntry {
                    start_u,
                    start_c,
                    size_u: size.uncompressed,
                    size_c: size.compressed,
                };
                start_u += i64::from(size.uncompressed);
                start_c += i64::from(size.compressed);
                entry
            })
            .collect();
        Self {
            compression_type,
            entries,
        }
    }

    pub fn compression_type(&self) -> CompressionType {
        self.compression_type
    }

    pub fn is_compressed(&self) -> bool {
        self.compression_type != CompressionType::None
    }

    pub fn num_frames(&self) -> usize {
        self.entries.len()
    }

    pub fn uncompressed_size(&self) -> i64 {
        self.entries.iter().map(|entry| i64::from(entry.size_u)).sum()
    }

    pub fn compressed_size(&self) -> i64 {
        self.entries.iter().map(|entry| i64::from(entry.size_c)).sum()
    }

    pub fn locate_compressed(&self, offset: i64) -> Result<ByteRange, String> {
        let entry = self.locate(offset)?;
        match offset == entry.start_u {
            true => Ok(ByteRange {
                offset: entry.start_c,
                length: entry.size_c as usize,
            }),
            false => Err(format!(
                "offset {offset} is not frame-aligned (frame starts at {}); align via LocateUncompressed",
                entry.start_u
            )),
        }
    }

    pub fn locate_uncompressed(&self, offset: i64) -> Result<ByteRange, String> {
        let entry = self.locate(offset)?;
        Ok(ByteRange {
            offset: entry.start_u,
            length: entry.size_u as usize,
        })
    }

    fn locate(&self, offset: i64) -> Result<FrameEntry, String> {
        let index = self
            .entries
            .partition_point(|entry| entry.start_u <= offset)
            .checked_sub(1)
            .ok_or_else(|| format!("offset {offset} not found in frame table"))?;
        let entry = self.entries[index];
        match offset < entry.end_u() {
            true => Ok(entry),
            false => Err(format!("offset {offset} is in a gap (not covered by any frame)")),
        }
    }

    pub fn serialize(table: Option<&FrameTable>, out: &mut Vec<u8>) {
        let (compression, entries) = match table {
            Some(table) if table.is_compressed() => (table.compression_type, table.entries.as_slice()),
            _ => (CompressionType::None, [].as_slice()),
        };
        out.extend_from_slice(&compression.code().to_le_bytes());
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        entries.iter().for_each(|entry| entry.encode(out));
    }

    pub fn deserialize(reader: &mut &[u8]) -> Result<Option<FrameTable>, String> {
        let compression = read_u32(reader).map_err(|e| format!("read compression type: {e}"))?;
        let count = read_u32(reader).map_err(|e| format!("read frame count: {e}"))?;
        if compression == 0 && count > 0 {
            return Err(format!(
                "compression type is 0 but frame count is {count}: corrupted header"
            ));
        }
        if compression == 0 || count == 0 {
            return Ok(None);
        }
        if count > MAX_DESERIALIZED_FRAMES {
            return Err(format!("frame count {count} exceeds maximum {MAX_DESERIALIZED_FRAMES}"));
        }
        let length = count as usize * ENTRY_SIZE;
        if reader.len() < length {
            return Err("read frame entries: unexpected EOF".to_owned());
        }
        let (bytes, rest) = reader.split_at(length);
        *reader = rest;
        let entries: Vec<FrameEntry> = bytes
            .as_chunks::<ENTRY_SIZE>()
            .0
            .iter()
            .map(|entry| FrameEntry::decode(entry.as_slice()))
            .collect();
        validate_entries(&entries)?;
        Ok(Some(FrameTable {
            compression_type: CompressionType::from_code(compression),
            entries,
        }))
    }

    pub fn trim_to_ranges(table: Option<&FrameTable>, ranges: &[ByteRange]) -> Option<FrameTable> {
        let table = table?;
        if table.entries.is_empty() || ranges.is_empty() {
            return Some(table.clone());
        }
        let mut keep = vec![false; table.entries.len()];
        for range in ranges {
            let start = range.offset;
            let end = range.offset + range.length as i64;
            let first = table.entries.partition_point(|entry| entry.end_u() <= start);
            table.entries[first..]
                .iter()
                .take_while(|entry| entry.start_u < end)
                .enumerate()
                .for_each(|(offset, _)| keep[first + offset] = true);
        }
        let entries = table
            .entries
            .iter()
            .zip(&keep)
            .filter_map(|(entry, keep)| keep.then_some(*entry))
            .collect();
        Some(FrameTable {
            compression_type: table.compression_type,
            entries,
        })
    }
}

fn read_u32(reader: &mut &[u8]) -> Result<u32, String> {
    let (bytes, rest) = reader
        .split_first_chunk::<4>()
        .ok_or_else(|| "unexpected EOF".to_owned())?;
    *reader = rest;
    Ok(u32::from_le_bytes(*bytes))
}

fn validate_entries(entries: &[FrameEntry]) -> Result<(), String> {
    for (index, entry) in entries.iter().enumerate() {
        if entry.size_u <= 0 || entry.size_c <= 0 {
            return Err(format!(
                "frame {index} has zero or negative size: SizeU={} SizeC={}",
                entry.size_u, entry.size_c
            ));
        }
        let Some(previous) = index.checked_sub(1).map(|previous| entries[previous]) else {
            continue;
        };
        if entry.start_u < previous.end_u() {
            return Err(format!(
                "frame {index} StartU {} < previous endU {}: U-entries not sorted",
                entry.start_u,
                previous.end_u()
            ));
        }
        if entry.start_c < previous.end_c() {
            return Err(format!(
                "frame {index} StartC {} < previous endC {}: C-entries not sorted",
                entry.start_c,
                previous.end_c()
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: i64 = 1 << 20;

    fn three_frames(start_u: i64, start_c: i64) -> FrameTable {
        let entry = |start_u, start_c, size_c| FrameEntry {
            start_u,
            start_c,
            size_u: MB as i32,
            size_c,
        };
        FrameTable {
            compression_type: CompressionType::Lz4,
            entries: vec![
                entry(start_u, start_c, 500_000),
                entry(start_u + MB, start_c + 500_000, 600_000),
                entry(start_u + 2 * MB, start_c + 1_100_000, 400_000),
            ],
        }
    }

    fn four_frames() -> FrameTable {
        FrameTable::full(
            CompressionType::Lz4,
            &[500_000, 600_000, 400_000, 700_000].map(|compressed| FrameSize {
                uncompressed: MB as i32,
                compressed,
            }),
        )
    }

    fn start_us(table: &FrameTable) -> Vec<i64> {
        table.entries.iter().map(|entry| entry.start_u).collect()
    }

    #[test]
    fn the_first_byte_of_each_frame_locates_its_uncompressed_range() {
        let table = three_frames(0, 0);

        for start in [0, MB, 2 * MB] {
            let range = table.locate_uncompressed(start).unwrap();
            assert_eq!((range.offset, range.length), (start, MB as usize));
        }
    }

    #[test]
    fn the_first_byte_of_each_frame_locates_its_compressed_range() {
        let table = three_frames(0, 0);

        let located: Vec<_> = [0, MB, 2 * MB]
            .map(|offset| table.locate_compressed(offset).unwrap())
            .iter()
            .map(|range| (range.offset, range.length))
            .collect();

        assert_eq!(located, vec![(0, 500_000), (500_000, 600_000), (1_100_000, 400_000)]);
    }

    #[test]
    fn the_last_byte_of_a_frame_belongs_to_that_frame() {
        assert_eq!(three_frames(0, 0).locate_uncompressed(MB - 1).unwrap().offset, 0);
    }

    #[test]
    fn an_offset_beyond_the_end_is_an_error() {
        assert!(three_frames(0, 0).locate_compressed(3 * MB).is_err());
    }

    #[test]
    fn a_mid_frame_offset_is_refused_for_the_compressed_range() {
        let error = three_frames(0, 0).locate_compressed(MB / 2).unwrap_err();

        assert!(error.contains("frame-aligned"), "{error}");
    }

    #[test]
    fn a_table_with_a_non_zero_start_has_no_frame_before_it() {
        let table = three_frames(MB, 500_000);

        assert_eq!(table.locate_uncompressed(MB).unwrap().offset, MB);
        assert_eq!(table.locate_compressed(MB).unwrap().offset, 500_000);
        assert!(table.locate_uncompressed(0).is_err());
    }

    #[test]
    fn a_full_table_sums_its_frames() {
        let table = FrameTable::full(
            CompressionType::Zstd,
            &[
                FrameSize {
                    uncompressed: MB as i32,
                    compressed: 500_000,
                },
                FrameSize {
                    uncompressed: MB as i32,
                    compressed: 600_000,
                },
            ],
        );

        assert_eq!(table.num_frames(), 2);
        assert_eq!(table.compression_type(), CompressionType::Zstd);
        assert!(table.is_compressed());
        assert_eq!(table.uncompressed_size(), 2 * MB);
        assert_eq!(table.compressed_size(), 1_100_000);
        assert_eq!(table.entries[1].start_u, MB);
        assert_eq!(table.entries[1].start_c, 500_000);
    }

    #[test]
    fn trimming_keeps_every_frame_a_covering_range_touches() {
        let table = four_frames();

        let all = FrameTable::trim_to_ranges(
            Some(&table),
            &[ByteRange {
                offset: 0,
                length: 4 * MB as usize,
            }],
        )
        .unwrap();
        let middle = FrameTable::trim_to_ranges(
            Some(&table),
            &[ByteRange {
                offset: MB,
                length: 2 * MB as usize,
            }],
        )
        .unwrap();

        assert_eq!(all.num_frames(), 4);
        assert_eq!(start_us(&middle), vec![MB, 2 * MB]);
    }

    #[test]
    fn trimming_two_disjoint_ranges_leaves_a_sparse_table() {
        let table = four_frames();
        let ranges = [
            ByteRange {
                offset: 0,
                length: MB as usize,
            },
            ByteRange {
                offset: 3 * MB,
                length: MB as usize,
            },
        ];

        let trimmed = FrameTable::trim_to_ranges(Some(&table), &ranges).unwrap();

        assert_eq!(start_us(&trimmed), vec![0, 3 * MB]);
        assert_eq!(
            trimmed.locate_compressed(3 * MB).unwrap().offset,
            500_000 + 600_000 + 400_000
        );
        assert!(trimmed.locate_compressed(MB).is_err());
    }

    #[test]
    fn trimming_nothing_returns_nothing() {
        let range = ByteRange { offset: 0, length: 100 };

        assert_eq!(FrameTable::trim_to_ranges(None, &[range]), None);
    }

    #[test]
    fn a_table_round_trips_through_its_binary_form() {
        let table = FrameTable::full(
            CompressionType::Zstd,
            &[
                FrameSize {
                    uncompressed: 2048,
                    compressed: 1024,
                },
                FrameSize {
                    uncompressed: 4096,
                    compressed: 3500,
                },
            ],
        );
        let mut bytes = Vec::new();
        FrameTable::serialize(Some(&table), &mut bytes);

        let read = FrameTable::deserialize(&mut bytes.as_slice()).unwrap();

        assert_eq!(read, Some(table));
    }

    #[test]
    fn no_table_serializes_as_zeros_and_reads_back_as_none() {
        let mut bytes = Vec::new();
        FrameTable::serialize(None, &mut bytes);

        assert_eq!(bytes, vec![0; 8]);
        assert_eq!(FrameTable::deserialize(&mut bytes.as_slice()).unwrap(), None);
    }

    #[test]
    fn corrupted_tables_are_refused() {
        let zero_type_with_frames = [0u32.to_le_bytes(), 1u32.to_le_bytes()].concat();
        let too_many = [2u32.to_le_bytes(), (MAX_DESERIALIZED_FRAMES + 1).to_le_bytes()].concat();

        assert_eq!(
            FrameTable::deserialize(&mut zero_type_with_frames.as_slice()).unwrap_err(),
            "compression type is 0 but frame count is 1: corrupted header"
        );
        assert_eq!(
            FrameTable::deserialize(&mut too_many.as_slice()).unwrap_err(),
            "frame count 1048577 exceeds maximum 1048576"
        );
    }

    #[test]
    fn overlapping_or_empty_frames_are_refused() {
        let table = three_frames(0, 0);
        let mut overlapping = table.clone();
        overlapping.entries[1].start_u = MB - 1;
        let mut empty = table;
        empty.entries[0].size_c = 0;

        let read = |table: &FrameTable| {
            let mut bytes = Vec::new();
            FrameTable::serialize(Some(table), &mut bytes);
            FrameTable::deserialize(&mut bytes.as_slice()).unwrap_err()
        };

        assert!(read(&overlapping).contains("U-entries not sorted"));
        assert!(read(&empty).contains("zero or negative size"));
    }
}
