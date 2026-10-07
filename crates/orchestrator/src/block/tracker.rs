use std::ops::Range;

use roaring::RoaringBitmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    Data,
    Zero,
}

#[derive(Debug, Clone, Default)]
pub struct BlockTracker {
    data: RoaringBitmap,
    zero: RoaringBitmap,
}

impl BlockTracker {
    pub fn set(&mut self, blocks: Range<u32>, state: BlockState) {
        let (target, other) = match state {
            BlockState::Data => (&mut self.data, &mut self.zero),
            BlockState::Zero => (&mut self.zero, &mut self.data),
        };
        other.remove_range(blocks.clone());
        target.insert_range(blocks);
    }

    pub fn state(&self, block: u32) -> Option<BlockState> {
        if self.data.contains(block) {
            Some(BlockState::Data)
        } else if self.zero.contains(block) {
            Some(BlockState::Zero)
        } else {
            None
        }
    }

    pub fn is_present(&self, blocks: Range<u32>) -> bool {
        blocks.into_iter().all(|block| self.state(block).is_some())
    }

    pub fn data(&self) -> &RoaringBitmap {
        &self.data
    }

    pub fn zero(&self) -> &RoaringBitmap {
        &self.zero
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_replace_each_other_and_presence_needs_every_block() {
        let mut tracker = BlockTracker::default();
        tracker.set(0..2, BlockState::Data);
        tracker.set(1..3, BlockState::Zero);

        assert_eq!(tracker.state(0), Some(BlockState::Data));
        assert_eq!(tracker.state(1), Some(BlockState::Zero));
        assert_eq!(tracker.state(3), None);
        assert!(tracker.is_present(0..3));
        assert!(!tracker.is_present(2..4));
        assert!(tracker.is_present(5..5));
        assert_eq!(tracker.data().iter().collect::<Vec<_>>(), vec![0]);
        assert_eq!(tracker.zero().iter().collect::<Vec<_>>(), vec![1, 2]);
    }
}
