#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    pub first_index: u64,
    pub blocks: usize,
}

impl Extent {
    pub fn offset(&self, block_size: u64) -> u64 {
        self.first_index * block_size
    }

    pub fn bytes(&self, block_size: u64) -> usize {
        self.blocks * block_size as usize
    }

    pub fn block_offsets(&self, block_size: u64) -> impl Iterator<Item = u64> {
        let first = self.offset(block_size);
        (0..self.blocks as u64).map(move |block| first + block * block_size)
    }
}

pub fn coalesce_indices(indices: &[u64], max_blocks: usize) -> Vec<Extent> {
    let max_blocks = max_blocks.max(1);
    let mut extents: Vec<Extent> = Vec::with_capacity(indices.len());
    for &index in indices {
        match extents.last_mut() {
            Some(last) if last.blocks < max_blocks && last.first_index + last.blocks as u64 == index => {
                last.blocks += 1;
            }
            _ => extents.push(Extent {
                first_index: index,
                blocks: 1,
            }),
        }
    }
    extents
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(first_index: u64, blocks: usize) -> Extent {
        Extent { first_index, blocks }
    }

    #[test]
    fn a_cap_of_one_keeps_every_block_apart() {
        assert_eq!(
            coalesce_indices(&[1, 2, 3], 1),
            vec![extent(1, 1), extent(2, 1), extent(3, 1)]
        );
        assert_eq!(coalesce_indices(&[1, 2], 0), vec![extent(1, 1), extent(2, 1)]);
    }

    #[test]
    fn contiguous_runs_merge_up_to_the_cap() {
        assert_eq!(
            coalesce_indices(&[4, 5, 6, 7, 8, 20, 21, 9], 3),
            vec![extent(4, 3), extent(7, 2), extent(20, 2), extent(9, 1)]
        );
    }

    #[test]
    fn repeated_or_descending_indices_do_not_merge() {
        assert_eq!(
            coalesce_indices(&[3, 3, 2, 3], 8),
            vec![extent(3, 1), extent(3, 1), extent(2, 2)]
        );
    }

    #[test]
    fn an_extent_spans_its_blocks() {
        let extent = extent(2, 3);

        assert_eq!(extent.offset(4096), 8192);
        assert_eq!(extent.bytes(4096), 12288);
        assert_eq!(extent.block_offsets(4096).collect::<Vec<_>>(), vec![8192, 12288, 16384]);
    }
}
