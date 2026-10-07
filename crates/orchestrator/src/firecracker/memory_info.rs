use roaring::RoaringBitmap;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub struct GuestRegion {
    pub base_host_virt_addr: u64,
    pub size: u64,
    pub offset: u64,
    pub page_size: u64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(from = "MemoryWords")]
pub struct MemoryInfo {
    pub resident: RoaringBitmap,
    pub empty: RoaringBitmap,
}

#[derive(Deserialize)]
struct MemoryWords {
    resident: Vec<u64>,
    empty: Vec<u64>,
}

impl From<MemoryWords> for MemoryInfo {
    fn from(words: MemoryWords) -> Self {
        Self {
            resident: bitmap_from_words(&words.resident),
            empty: bitmap_from_words(&words.empty),
        }
    }
}

#[derive(Deserialize)]
pub struct MemoryMappings {
    pub mappings: Vec<GuestRegion>,
}

fn bitmap_from_words(words: &[u64]) -> RoaringBitmap {
    words
        .iter()
        .enumerate()
        .flat_map(|(index, word)| {
            (0..64)
                .filter(move |bit| word & (1 << bit) != 0)
                .map(move |bit| (index * 64 + bit) as u32)
        })
        .collect()
}
