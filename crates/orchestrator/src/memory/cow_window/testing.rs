use std::borrow::Cow;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

use parking_lot::Mutex;

use super::io::{PageSink, PageSource};

pub const PAGE_SIZE: u64 = 4096;

pub fn pseudo_random(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..length)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

#[derive(Clone)]
pub struct MemorySource {
    bytes: Arc<Mutex<Vec<u8>>>,
}

impl MemorySource {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(Mutex::new(bytes)),
        }
    }

    pub fn overwrite_page(&self, page: u32, value: u8) {
        let start = page as usize * PAGE_SIZE as usize;
        self.bytes.lock()[start..start + PAGE_SIZE as usize].fill(value);
    }

    pub fn overwrite_first_byte(&self, page: u32, value: u8) {
        self.bytes.lock()[page as usize * PAGE_SIZE as usize] = value;
    }
}

impl PageSource for MemorySource {
    fn read_page(&self, offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>> {
        let bytes = self.bytes.lock();
        Ok(Cow::Owned(bytes[offset as usize..(offset + length) as usize].to_vec()))
    }
}

pub struct LiveMemory {
    bytes: UnsafeCell<Vec<u8>>,
}

unsafe impl Sync for LiveMemory {}

impl LiveMemory {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: UnsafeCell::new(bytes),
        }
    }

    pub fn overwrite_page(&self, page: u32, value: u8) {
        let start = page as usize * PAGE_SIZE as usize;
        let bytes = unsafe { &mut *self.bytes.get() };
        bytes[start..start + PAGE_SIZE as usize].fill(value);
    }
}

impl PageSource for LiveMemory {
    fn read_page(&self, offset: u64, length: u64) -> io::Result<Cow<'_, [u8]>> {
        let bytes = unsafe { &*self.bytes.get() };
        Ok(Cow::Borrowed(&bytes[offset as usize..(offset + length) as usize]))
    }
}

pub struct MemorySink {
    data: Mutex<Vec<u8>>,
    writes: Mutex<HashMap<u64, usize>>,
    fail_at: Mutex<Option<u64>>,
}

impl MemorySink {
    pub fn new(size: u64) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(vec![0; size as usize]),
            writes: Mutex::default(),
            fail_at: Mutex::default(),
        })
    }

    pub fn fail_at(&self, offset: u64) {
        *self.fail_at.lock() = Some(offset);
    }

    pub fn data(&self) -> Vec<u8> {
        self.data.lock().clone()
    }

    pub fn write_count(&self, offset: u64) -> usize {
        self.writes.lock().get(&offset).copied().unwrap_or(0)
    }

    pub fn written_offsets(&self) -> usize {
        self.writes.lock().len()
    }
}

impl PageSink for MemorySink {
    fn write_page(&self, data: &[u8], offset: u64) -> io::Result<()> {
        if *self.fail_at.lock() == Some(offset) {
            return Err(io::Error::other("injected sink failure"));
        }
        *self.writes.lock().entry(offset).or_insert(0) += 1;
        self.data.lock()[offset as usize..offset as usize + data.len()].copy_from_slice(data);
        Ok(())
    }
}
