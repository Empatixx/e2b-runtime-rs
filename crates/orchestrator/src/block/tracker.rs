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
        let wanted = u64::from(blocks.end.saturating_sub(blocks.start));
        self.data.range_cardinality(blocks.clone()) + self.zero.range_cardinality(blocks) == wanted
    }

    pub fn data(&self) -> &RoaringBitmap {
        &self.data
    }

    pub fn data_runs(&self) -> Vec<Range<u32>> {
        runs(&self.data)
    }

    pub fn zero(&self) -> &RoaringBitmap {
        &self.zero
    }
}

pub(super) fn runs(blocks: &RoaringBitmap) -> Vec<Range<u32>> {
    blocks.iter().fold(Vec::new(), |mut runs, index| {
        match runs.last_mut() {
            Some(last) if last.end == index => last.end += 1,
            _ => runs.push(index..index + 1),
        }
        runs
    })
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

    #[test]
    fn is_present_counts_a_range() {
        let mut tracker = BlockTracker::default();
        tracker.set(10..20, BlockState::Data);
        tracker.set(20..30, BlockState::Zero);

        assert!(tracker.is_present(10..30));
        assert!(tracker.is_present(15..16));
        assert!(!tracker.is_present(9..30));
        assert!(!tracker.is_present(25..31));
        assert!(tracker.is_present(5..5));
    }

    #[test]
    fn data_runs_join_neighbouring_blocks() {
        let mut tracker = BlockTracker::default();
        tracker.set(0..3, BlockState::Data);
        tracker.set(5..6, BlockState::Data);
        tracker.set(3..4, BlockState::Zero);

        assert_eq!(tracker.data_runs(), vec![0..3, 5..6]);
    }
}
