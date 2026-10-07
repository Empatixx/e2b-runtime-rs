mod mapping;
mod metadata;

pub use mapping::{BuildMapping, MAPPING_SIZE};
pub use metadata::{METADATA_SIZE, Metadata};

pub const PAGE_SIZE: u64 = 4096;

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
        Ok(Self { metadata, mappings })
    }

    pub fn encode(&self) -> Vec<u8> {
        self.metadata
            .encode()
            .into_iter()
            .chain(self.mappings.iter().flat_map(BuildMapping::encode))
            .collect()
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, String> {
        let (metadata, entries) = bytes
            .split_first_chunk::<METADATA_SIZE>()
            .ok_or_else(|| format!("header too short: {} bytes", bytes.len()))?;
        let metadata = Metadata::decode(metadata);
        if !(1..=3).contains(&metadata.format_version()) {
            return Err(format!("unsupported header version {}", metadata.version));
        }
        if entries.len() % MAPPING_SIZE != 0 {
            return Err("failed to read block mapping: unexpected EOF".to_owned());
        }
        let mappings = entries.chunks_exact(MAPPING_SIZE).map(BuildMapping::decode).collect();
        Self::new(metadata, mappings)
    }
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
}
