use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use parking_lot::Mutex;

use crate::network::slot::Slot;

pub struct SlotPool {
    free: Mutex<BTreeSet<u32>>,
}

impl SlotPool {
    pub fn new(indices: RangeInclusive<u32>) -> Self {
        Self {
            free: Mutex::new(indices.collect()),
        }
    }

    pub fn acquire(&self) -> Result<Slot, String> {
        let index = self.free.lock().pop_first().ok_or("no free network slot")?;
        Slot::new(index)
    }

    pub fn release(&self, slot: Slot) {
        self.free.lock().insert(slot.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_unique_until_released() {
        let pool = SlotPool::new(7..=8);

        let first = pool.acquire().unwrap();
        let second = pool.acquire().unwrap();
        assert_ne!(first, second);
        assert_eq!(pool.acquire().unwrap_err(), "no free network slot");

        pool.release(first);
        assert_eq!(pool.acquire().unwrap(), first);
    }
}
