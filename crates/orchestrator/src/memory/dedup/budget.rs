use std::collections::{BTreeMap, HashMap};

use roaring::RoaringBitmap;
use uuid::Uuid;

use super::compare::DedupPlan;
use super::config::DedupBudget;
use crate::storage::header::{Header, PAGE_SIZE};

pub const DEFAULT_COMPRESS_FRAME_SIZE: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FetchKey {
    pub build_id: Uuid,
    pub window: i64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PageKind {
    #[default]
    Empty,
    Parent,
    Current,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PageInfo {
    pub kind: PageKind,
    pub key: FetchKey,
}

pub fn fetch_key_and_end(build_id: Uuid, offset: u64, window_bytes: u64) -> (FetchKey, u64) {
    let window = offset / window_bytes;
    (
        FetchKey {
            build_id,
            window: window as i64,
        },
        (window + 1) * window_bytes,
    )
}

pub fn parent_fetch_key(storage_offset: Option<(Uuid, u64)>, page_offset: u64, window_bytes: u64) -> FetchKey {
    match storage_offset {
        Some((build_id, offset)) => fetch_key_and_end(build_id, offset, window_bytes).0,
        None => FetchKey {
            build_id: Uuid::nil(),
            window: (page_offset / window_bytes) as i64,
        },
    }
}

pub fn count_external_blocks(
    candidates: &HashMap<FetchKey, RoaringBitmap>,
    base_header: &Header,
    dirty: &RoaringBitmap,
    block_size: u64,
    window_bytes: u64,
) -> HashMap<FetchKey, i64> {
    let mut counts = HashMap::new();
    if candidates.is_empty() {
        return counts;
    }
    let mut last_block: HashMap<FetchKey, u64> = HashMap::new();
    for mapping in base_header.mappings.iter().filter(|mapping| !mapping.build_id.is_nil()) {
        let end = mapping.end();
        let mut block = mapping.offset / block_size;
        while block * block_size < end {
            if !dirty.contains(block as u32) {
                let shift = |offset: u64| offset - mapping.offset + mapping.storage_offset;
                let lo = shift((block * block_size).max(mapping.offset));
                let hi = shift(((block + 1) * block_size).min(end));
                let mut offset = lo;
                while offset < hi {
                    let (key, next) = fetch_key_and_end(mapping.build_id, offset, window_bytes);
                    if candidates.contains_key(&key) && last_block.get(&key) != Some(&(block + 1)) {
                        *counts.entry(key).or_insert(0) += 1;
                        last_block.insert(key, block + 1);
                    }
                    offset = next;
                }
            }
            block += 1;
        }
    }
    counts
}

pub fn promote_cheap_frames(
    plan: &mut DedupPlan,
    parent_by_key: &HashMap<FetchKey, RoaringBitmap>,
    external_blocks: &HashMap<FetchKey, i64>,
    pages_per_block: u64,
    budget: &DedupBudget,
) {
    let max_pages = budget
        .max_pages_per_promoted_frame
        .min(budget.fetch_run_window_pages - 1);
    let fault = match budget.block_fault_pct > 0 && budget.block_fault_pct < 100 {
        true => budget.block_fault_pct as f64 / 100.0,
        false => 1.0,
    };
    for (key, pages) in parent_by_key {
        let external = external_blocks.get(key).copied().unwrap_or(0);
        let value = (1.0 - fault).powf(external as f64)
            * (1.0 - (1.0 - fault).powf(distinct_blocks(pages, pages_per_block) as f64));
        let count = pages.len();
        if count > 0 && count as f64 <= max_pages as f64 * value {
            plan.page_dirty |= pages;
            plan.promoted_frames += 1;
            plan.promoted_frame_pages += count as i64;
        }
    }
}

fn distinct_blocks(pages: &RoaringBitmap, pages_per_block: u64) -> u64 {
    let blocks: Vec<u64> = pages.iter().map(|page| u64::from(page) / pages_per_block).collect();
    blocks.chunk_by(|a, b| a == b).count() as u64
}

pub fn compact_block_windows(
    plan: &mut DedupPlan,
    base_header: &Header,
    dirty: &RoaringBitmap,
    block_size: u64,
    window_bytes: u64,
    budget: &DedupBudget,
) {
    if budget.max_fetch_windows_per_block <= 0 || budget.max_promoted_parent_pages_per_block <= 0 {
        return;
    }
    let pages_per_block = (block_size / PAGE_SIZE) as usize;
    let mut current_stored_pages: i64 = 0;
    for block in dirty.iter() {
        let absolute = u64::from(block) * block_size;
        let first_page = (absolute / PAGE_SIZE) as u32;
        let mut pages: Vec<PageInfo> = (0..pages_per_block)
            .map(|index| page_info(plan, base_header, absolute, first_page, index, window_bytes))
            .collect();
        let windower = FetchWindower {
            window_pages: budget.fetch_run_window_pages,
            current_start: current_stored_pages,
        };
        let promoted = windower.compact(
            &mut pages,
            budget.max_fetch_windows_per_block,
            budget.max_promoted_parent_pages_per_block,
        );
        if promoted > 0 {
            plan.promoted_blocks += 1;
            plan.promoted_pages += promoted as i64;
        }
        for (index, info) in pages.iter().enumerate() {
            if info.kind == PageKind::Current {
                plan.page_dirty.insert(first_page + index as u32);
                current_stored_pages += 1;
            }
        }
    }
}

fn page_info(
    plan: &DedupPlan,
    base_header: &Header,
    absolute: u64,
    first_page: u32,
    index: usize,
    window_bytes: u64,
) -> PageInfo {
    let page = first_page + index as u32;
    match (plan.page_dirty.contains(page), plan.page_empty.contains(page)) {
        (true, _) => PageInfo {
            kind: PageKind::Current,
            ..PageInfo::default()
        },
        (false, true) => PageInfo::default(),
        (false, false) => {
            let page_offset = absolute + index as u64 * PAGE_SIZE;
            let located = base_header
                .locate(page_offset)
                .ok()
                .map(|location| (location.build_id, location.storage_offset));
            PageInfo {
                kind: PageKind::Parent,
                key: parent_fetch_key(located, page_offset, window_bytes),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FetchWindower {
    pub window_pages: i64,
    pub current_start: i64,
}

impl FetchWindower {
    pub fn compact(&self, pages: &mut [PageInfo], max_windows: i64, max_promoted: i64) -> usize {
        if max_windows <= 0 || max_promoted <= 0 {
            return 0;
        }
        let current = pages.iter().filter(|page| page.kind == PageKind::Current).count() as i64;
        let groups = parent_key_groups(pages);
        let mut best = groups.len() as i64 + self.current_windows(current);
        if best <= max_windows {
            return 0;
        }
        let (mut chosen, mut promoted, mut running) = (0usize, 0usize, 0usize);
        for (index, group) in groups.iter().enumerate() {
            running += group.len();
            if running as i64 > max_promoted {
                break;
            }
            let count = (groups.len() - (index + 1)) as i64 + self.current_windows(current + running as i64);
            if count < best {
                best = count;
                chosen = index + 1;
                promoted = running;
            }
            if best <= max_windows {
                break;
            }
        }
        groups[..chosen]
            .iter()
            .flatten()
            .for_each(|index| pages[*index].kind = PageKind::Current);
        promoted
    }

    fn current_windows(&self, pages: i64) -> i64 {
        match pages {
            0 => 0,
            pages => (self.current_start + pages - 1) / self.window_pages - self.current_start / self.window_pages + 1,
        }
    }
}

fn parent_key_groups(pages: &[PageInfo]) -> Vec<Vec<usize>> {
    let by_key = pages
        .iter()
        .enumerate()
        .filter(|(_, page)| page.kind == PageKind::Parent)
        .fold(
            BTreeMap::<(Uuid, i64), Vec<usize>>::new(),
            |mut groups, (index, page)| {
                groups
                    .entry((page.key.build_id, page.key.window))
                    .or_default()
                    .push(index);
                groups
            },
        );
    let mut groups: Vec<Vec<usize>> = by_key.into_values().collect();
    groups.sort_by_key(|group| (group.len(), group[0]));
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(build_id: Uuid, window: i64) -> PageInfo {
        PageInfo {
            kind: PageKind::Parent,
            key: FetchKey { build_id, window },
        }
    }

    const CURRENT: PageInfo = PageInfo {
        kind: PageKind::Current,
        key: FetchKey {
            build_id: Uuid::nil(),
            window: 0,
        },
    };

    fn bitmap(pages: &[u32]) -> RoaringBitmap {
        pages.iter().copied().collect()
    }

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[test]
    fn a_zero_benefit_promotion_is_not_committed() {
        let windower = FetchWindower {
            window_pages: 4,
            current_start: 0,
        };
        let mut pages = [parent(id(1), 0), parent(id(1), 1)];

        assert_eq!(windower.compact(&mut pages, 1, 1), 0);
        assert_eq!(pages[0].kind, PageKind::Parent);
    }

    #[test]
    fn a_whole_key_group_of_non_adjacent_parents_is_promoted_together() {
        let windower = FetchWindower {
            window_pages: 4,
            current_start: 0,
        };
        let mut pages = [parent(id(1), 0), CURRENT, parent(id(1), 0)];

        assert_eq!(windower.compact(&mut pages, 1, 4), 2);
        assert_eq!((pages[0].kind, pages[2].kind), (PageKind::Current, PageKind::Current));
    }

    #[test]
    fn distinct_key_parents_combine_when_only_the_union_helps() {
        let windower = FetchWindower {
            window_pages: 2,
            current_start: 0,
        };
        let mut pages = [CURRENT, parent(id(1), 0), CURRENT, parent(id(2), 0)];

        assert_eq!(windower.compact(&mut pages, 2, 2), 2);
        assert_eq!((pages[1].kind, pages[3].kind), (PageKind::Current, PageKind::Current));
    }

    #[test]
    fn the_budget_is_spent_on_the_cheapest_groups_first() {
        let windower = FetchWindower {
            window_pages: 4,
            current_start: 0,
        };
        let mut pages = [parent(id(1), 0), parent(id(1), 0), parent(id(2), 0), parent(id(3), 0)];

        assert_eq!(windower.compact(&mut pages, 1, 2), 2);
        assert_eq!(
            pages.map(|page| page.kind),
            [PageKind::Parent, PageKind::Parent, PageKind::Current, PageKind::Current]
        );
    }

    fn plan() -> DedupPlan {
        DedupPlan::default()
    }

    #[test]
    fn frames_at_or_under_the_price_threshold_are_promoted() {
        let mut plan = plan();
        let parents = HashMap::from([
            (
                FetchKey {
                    build_id: id(1),
                    window: 0,
                },
                bitmap(&[10, 11, 12]),
            ),
            (
                FetchKey {
                    build_id: id(1),
                    window: 1,
                },
                bitmap(&[20]),
            ),
            (
                FetchKey {
                    build_id: id(2),
                    window: 0,
                },
                bitmap(&[30, 31]),
            ),
        ]);
        let budget = DedupBudget {
            max_pages_per_promoted_frame: 2,
            fetch_run_window_pages: 512,
            ..DedupBudget::default()
        };

        promote_cheap_frames(&mut plan, &parents, &HashMap::new(), 512, &budget);

        assert_eq!((plan.promoted_frames, plan.promoted_frame_pages), (2, 3));
        assert!(plan.page_dirty.contains(20) && plan.page_dirty.contains(30) && plan.page_dirty.contains(31));
        assert!(!plan.page_dirty.contains(10));
    }

    #[test]
    fn external_references_discount_the_expected_value() {
        let key = FetchKey {
            build_id: id(1),
            window: 0,
        };
        let parents = HashMap::from([(key, bitmap(&[0]))]);
        let budget = DedupBudget {
            max_pages_per_promoted_frame: 4,
            block_fault_pct: 50,
            fetch_run_window_pages: 512,
            ..DedupBudget::default()
        };

        let mut one_external = plan();
        promote_cheap_frames(&mut one_external, &parents, &HashMap::from([(key, 1)]), 4, &budget);
        let mut two_external = plan();
        promote_cheap_frames(&mut two_external, &parents, &HashMap::from([(key, 2)]), 4, &budget);

        assert_eq!(one_external.promoted_frames, 1);
        assert_eq!(two_external.promoted_frames, 0);
    }

    #[test]
    fn full_window_keys_are_never_promoted() {
        let mut plan = plan();
        let parents = HashMap::from([(
            FetchKey {
                build_id: id(1),
                window: 0,
            },
            bitmap(&[0, 1, 2, 3]),
        )]);
        let budget = DedupBudget {
            max_pages_per_promoted_frame: 100,
            fetch_run_window_pages: 4,
            ..DedupBudget::default()
        };

        promote_cheap_frames(&mut plan, &parents, &HashMap::new(), 512, &budget);

        assert_eq!(plan.promoted_frames, 0);
        assert!(plan.page_dirty.is_empty());
    }

    #[test]
    fn pages_without_frame_data_are_keyed_by_the_configured_window() {
        let window = 2 << 20;
        let key = |offset| parent_fetch_key(Some((id(7), offset)), 0, window);

        assert_eq!(key(0), key(window - 1));
        assert_ne!(key(0), key(window));
        assert_eq!(parent_fetch_key(None, window, window).window, 1);
    }
}
