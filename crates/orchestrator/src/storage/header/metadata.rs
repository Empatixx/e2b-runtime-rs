use uuid::Uuid;

pub const METADATA_SIZE: usize = 64;
pub const TEMPLATE_VERSION: u64 = 3;
pub const VERSION_V4: u64 = 4;
pub const VERSION_V5: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub version: u64,
    pub block_size: u64,
    pub size: u64,
    pub generation: u64,
    pub build_id: Uuid,
    pub base_build_id: Uuid,
}

impl Metadata {
    pub fn template(build_id: Uuid, block_size: u64, size: u64) -> Self {
        Self {
            version: TEMPLATE_VERSION,
            block_size,
            size,
            generation: 0,
            build_id,
            base_build_id: build_id,
        }
    }

    pub fn next_generation(&self, build_id: Uuid) -> Self {
        Self {
            generation: self.generation + 1,
            build_id,
            ..*self
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        [self.version, self.block_size, self.size, self.generation]
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .chain(*self.build_id.as_bytes())
            .chain(*self.base_build_id.as_bytes())
            .collect()
    }

    pub fn decode(bytes: &[u8; METADATA_SIZE]) -> Self {
        Self {
            version: u64_at(bytes, 0),
            block_size: u64_at(bytes, 8),
            size: u64_at(bytes, 16),
            generation: u64_at(bytes, 24),
            build_id: uuid_at(bytes, 32),
            base_build_id: uuid_at(bytes, 48),
        }
    }

    pub fn format_version(&self) -> u64 {
        self.version & 0xFFFF
    }

    pub fn supported_version(&self) -> bool {
        (1..=VERSION_V5).contains(&self.format_version())
    }
}

pub fn u64_at(bytes: &[u8], start: usize) -> u64 {
    u64::from_le_bytes(bytes[start..start + 8].try_into().unwrap_or_default())
}

pub fn uuid_at(bytes: &[u8], start: usize) -> Uuid {
    Uuid::from_bytes(bytes[start..start + 16].try_into().unwrap_or_default())
}
