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

    pub fn decode(bytes: &[u8]) -> Self {
        Self {
            offset: u64_at(bytes, 0),
            length: u64_at(bytes, 8),
            build_id: uuid_at(bytes, 16),
            storage_offset: u64_at(bytes, 32),
        }
    }
}
