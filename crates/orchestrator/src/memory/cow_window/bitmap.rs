use std::sync::atomic::{AtomicU64, Ordering};

pub struct AtomicBitmap {
    words: Vec<AtomicU64>,
}

impl AtomicBitmap {
    pub fn with_bits(bits: u32) -> Self {
        let words = u64::from(bits).div_ceil(64);
        Self {
            words: (0..words).map(|_| AtomicU64::new(0)).collect(),
        }
    }

    pub fn get(&self, index: u32) -> bool {
        self.words[(index / 64) as usize].load(Ordering::Acquire) & (1 << (index % 64)) != 0
    }

    pub fn set(&self, index: u32) {
        self.words[(index / 64) as usize].fetch_or(1 << (index % 64), Ordering::Release);
    }
}
