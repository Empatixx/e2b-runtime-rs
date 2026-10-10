use std::collections::HashMap;

use roaring::RoaringBitmap;
use uuid::Uuid;

use super::budget::{
    DEFAULT_COMPRESS_FRAME_SIZE, FetchKey, compact_block_windows, count_external_blocks, parent_fetch_key,
    promote_cheap_frames,
};
use super::config::DedupBudget;
use crate::storage::header::{Header, PAGE_SIZE};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DedupPlan {
    pub page_dirty: RoaringBitmap,
    pub page_empty: RoaringBitmap,
    pub exported_size: u64,
    pub promoted_blocks: i64,
    pub promoted_pages: i64,
    pub parent_frames: i64,
    pub promoted_frames: i64,
    pub promoted_frame_pages: i64,
}

pub trait BasePages {
    fn header(&self) -> &Header;
    fn page(&self, offset: u64) -> Result<Vec<u8>, String>;
    fn is_cached(&self, offset: u64) -> bool;
}

pub type BlockSource<'a> = dyn FnMut(u64) -> Result<Vec<u8>, String> + 'a;

pub struct Comparison<'a> {
    pub dirty: &'a RoaringBitmap,
    pub block_size: u64,
    pub best_effort: bool,
    pub budget: DedupBudget,
}

pub fn compare(source: &mut BlockSource, base: &dyn BasePages, comparison: &Comparison) -> Result<DedupPlan, String> {
    let budget = DedupBudget {
        fetch_run_window_pages: match comparison.budget.fetch_run_window_pages {
            pages if pages > 0 => pages,
            _ => (DEFAULT_COMPRESS_FRAME_SIZE / PAGE_SIZE) as i64,
        },
        ..comparison.budget
    };
    let window_bytes = budget.fetch_run_window_pages as u64 * PAGE_SIZE;
    let block_size = comparison.block_size;
    let mut plan = DedupPlan::default();
    let mut parent_by_key: HashMap<FetchKey, RoaringBitmap> = HashMap::new();

    for block in comparison.dirty.iter() {
        let absolute = u64::from(block) * block_size;
        plan.exported_size += block_size;
        let bytes = source(absolute)?;
        for (index, page) in bytes.chunks(PAGE_SIZE as usize).enumerate() {
            let page_offset = absolute + index as u64 * PAGE_SIZE;
            let page_index = (page_offset / PAGE_SIZE) as u32;
            match classify(page, page_offset, base, comparison.best_effort)? {
                Classified::Empty => drop(plan.page_empty.insert(page_index)),
                Classified::Stored => drop(plan.page_dirty.insert(page_index)),
                Classified::Parent(located) => {
                    let key = parent_fetch_key(located, page_offset, window_bytes);
                    parent_by_key.entry(key).or_default().insert(page_index);
                }
            }
        }
    }

    plan.parent_frames = parent_by_key.len() as i64;
    if budget.max_pages_per_promoted_frame > 0 {
        let external = count_external_blocks(
            &parent_by_key,
            base.header(),
            comparison.dirty,
            block_size,
            window_bytes,
        );
        promote_cheap_frames(&mut plan, &parent_by_key, &external, block_size / PAGE_SIZE, &budget);
    }
    compact_block_windows(
        &mut plan,
        base.header(),
        comparison.dirty,
        block_size,
        window_bytes,
        &budget,
    );
    Ok(plan)
}

enum Classified {
    Empty,
    Stored,
    Parent(Option<(Uuid, u64)>),
}

fn classify(page: &[u8], page_offset: u64, base: &dyn BasePages, best_effort: bool) -> Result<Classified, String> {
    if page.iter().all(|byte| *byte == 0) {
        return Ok(Classified::Empty);
    }
    let located = base
        .header()
        .locate(page_offset)
        .ok()
        .map(|location| (location, (location.build_id, location.storage_offset)));
    if located.is_some_and(|(location, _)| location.build_id.is_nil() && location.length >= PAGE_SIZE) {
        return Ok(Classified::Stored);
    }
    if best_effort && !base.is_cached(page_offset) {
        return Ok(Classified::Stored);
    }
    let base_page = base
        .page(page_offset)
        .map_err(|e| format!("slice base at {page_offset}: {e}"))?;
    match base_page == page {
        true => Ok(Classified::Parent(located.map(|(_, mapped)| mapped))),
        false => Ok(Classified::Stored),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::header::Metadata;

    struct FakeBase {
        data: Vec<u8>,
        header: Header,
        cached: bool,
    }

    impl BasePages for FakeBase {
        fn header(&self) -> &Header {
            &self.header
        }

        fn page(&self, offset: u64) -> Result<Vec<u8>, String> {
            Ok(self.data[offset as usize..(offset + PAGE_SIZE) as usize].to_vec())
        }

        fn is_cached(&self, _: u64) -> bool {
            self.cached
        }
    }

    const PAGE: usize = PAGE_SIZE as usize;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len)
            .map(|index| (index as u32).wrapping_mul(2654435761).to_le_bytes()[3] | 1)
            .collect()
    }

    fn base(data: Vec<u8>, block_size: u64) -> FakeBase {
        let build = Uuid::from_u128(1);
        let size = data.len() as u64;
        FakeBase {
            header: Header::new(
                Metadata::template(build, block_size, size),
                vec![crate::storage::header::BuildMapping {
                    offset: 0,
                    length: size,
                    build_id: build,
                    storage_offset: 0,
                }],
            )
            .unwrap(),
            data,
            cached: true,
        }
    }

    fn run(src: &[u8], base: &FakeBase, dirty: &[u32], block_size: u64, budget: DedupBudget) -> DedupPlan {
        let dirty: RoaringBitmap = dirty.iter().copied().collect();
        let mut source = |absolute: u64| Ok(src[absolute as usize..(absolute + block_size) as usize].to_vec());
        compare(
            &mut source,
            base,
            &Comparison {
                dirty: &dirty,
                block_size,
                best_effort: false,
                budget,
            },
        )
        .unwrap()
    }

    #[test]
    fn zero_pages_are_empty_changed_pages_stored_and_equal_pages_left_to_the_parent() {
        let block_size = 4 * PAGE_SIZE;
        let base_data = pattern(4 * PAGE);
        let mut src = base_data.clone();
        src[0] ^= 0xFF;
        src[2 * PAGE..3 * PAGE].fill(0);
        let base = base(base_data, block_size);

        let plan = run(&src, &base, &[0], block_size, DedupBudget::default());

        assert_eq!(plan.page_dirty.iter().collect::<Vec<_>>(), vec![0]);
        assert_eq!(plan.page_empty.iter().collect::<Vec<_>>(), vec![2]);
        assert_eq!(plan.exported_size, block_size);
        assert_eq!(plan.parent_frames, 1);
    }

    #[test]
    fn an_unbacked_parent_hole_stores_the_page() {
        let block_size = 4 * PAGE_SIZE;
        let data = pattern(4 * PAGE);
        let mut hole = base(data.clone(), block_size);
        hole.header = Header::new(
            Metadata::template(Uuid::from_u128(1), block_size, 4 * PAGE_SIZE),
            vec![crate::storage::header::BuildMapping {
                offset: 0,
                length: 4 * PAGE_SIZE,
                build_id: Uuid::nil(),
                storage_offset: 0,
            }],
        )
        .unwrap();

        let plan = run(&data, &hole, &[0], block_size, DedupBudget::default());

        assert_eq!(plan.page_dirty.len(), 4);
    }

    #[test]
    fn best_effort_stores_pages_whose_parent_is_not_cached() {
        let block_size = 2 * PAGE_SIZE;
        let data = pattern(2 * PAGE);
        let mut uncached = base(data.clone(), block_size);
        uncached.cached = false;
        let dirty: RoaringBitmap = [0u32].into_iter().collect();
        let mut source = |absolute: u64| Ok(data[absolute as usize..(absolute + block_size) as usize].to_vec());

        let plan = compare(
            &mut source,
            &uncached,
            &Comparison {
                dirty: &dirty,
                block_size,
                best_effort: true,
                budget: DedupBudget::default(),
            },
        )
        .unwrap();

        assert_eq!(plan.page_dirty.len(), 2);
    }

    #[test]
    fn the_per_block_budget_promotes_a_small_parent_run() {
        let block_size = 4 * PAGE_SIZE;
        let base_data = pattern(4 * PAGE);
        let mut src = base_data.clone();
        src[0] ^= 0xFF;
        src[2 * PAGE] ^= 0xFF;
        src[3 * PAGE..4 * PAGE].fill(0);
        let base = base(base_data, block_size);
        let budget = DedupBudget {
            max_fetch_windows_per_block: 1,
            max_promoted_parent_pages_per_block: 1,
            fetch_run_window_pages: 4,
            ..DedupBudget::default()
        };

        let plan = run(&src, &base, &[0], block_size, budget);

        assert_eq!(plan.page_dirty.len(), 3);
        assert_eq!(plan.page_empty.len(), 1);
    }

    #[test]
    fn the_per_block_and_global_budgets_compose() {
        let block_size = 4 * PAGE_SIZE;
        let base_data = pattern(8 * PAGE);
        let mut src = base_data.clone();
        src[0..PAGE].fill(0);
        src[2 * PAGE..4 * PAGE].fill(0);
        src[4 * PAGE] ^= 0xFF;
        src[5 * PAGE..6 * PAGE].fill(0);
        let base = base(base_data, block_size);
        let budget = DedupBudget {
            max_fetch_windows_per_block: 1,
            max_promoted_parent_pages_per_block: 2,
            max_pages_per_promoted_frame: 1,
            fetch_run_window_pages: 4,
            ..DedupBudget::default()
        };

        let plan = run(&src, &base, &[0, 1], block_size, budget);

        assert_eq!(plan.page_dirty.iter().collect::<Vec<_>>(), vec![1, 4, 6, 7]);
        assert_eq!(plan.page_empty.len(), 4);
    }

    #[test]
    fn a_frame_referenced_outside_the_diff_is_not_promoted() {
        let block_size = 4 * PAGE_SIZE;
        let base_data = pattern(8 * PAGE);
        let mut src = base_data.clone();
        src[0] ^= 0xFF;
        let base = base(base_data, block_size);
        let budget = DedupBudget {
            max_pages_per_promoted_frame: 4,
            fetch_run_window_pages: 8,
            ..DedupBudget::default()
        };

        let plan = run(&src, &base, &[0], block_size, budget);

        assert_eq!(plan.page_dirty.iter().collect::<Vec<_>>(), vec![0]);
    }
}
