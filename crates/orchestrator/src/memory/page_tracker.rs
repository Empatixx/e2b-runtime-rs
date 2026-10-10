use std::ops::Range;

use parking_lot::RwLock;
use roaring::RoaringBitmap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageState {
    NotPresent,
    Dirty,
    Zero,
    Removed,
    Clean,
}

impl PageState {
    pub const COUNT: usize = 5;
    pub const ALL: [PageState; Self::COUNT] = [
        PageState::NotPresent,
        PageState::Dirty,
        PageState::Zero,
        PageState::Removed,
        PageState::Clean,
    ];

    pub fn label(self) -> &'static str {
        match self {
            PageState::NotPresent => "not_present",
            PageState::Dirty => "dirty",
            PageState::Zero => "zero",
            PageState::Removed => "removed",
            PageState::Clean => "clean",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageStates {
    pub dirty: RoaringBitmap,
    pub zero: RoaringBitmap,
    pub removed: RoaringBitmap,
    pub clean: RoaringBitmap,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportedPages {
    pub dirty: RoaringBitmap,
    pub empty: RoaringBitmap,
}

#[derive(Debug, Default)]
pub struct PageTracker {
    states: RwLock<PageStates>,
}

impl PageTracker {
    pub fn set_range(&self, pages: Range<u32>, state: PageState) {
        if pages.end <= pages.start {
            return;
        }
        let mut states = self.states.write();
        states.bitmaps().into_iter().for_each(|(bitmap_state, bitmap)| {
            match bitmap_state == state {
                true => bitmap.insert_range(pages.clone()),
                false => bitmap.remove_range(pages.clone()),
            };
        });
    }

    pub fn mark_installed(&self, pages: Range<u32>, state: PageState) {
        if pages.end <= pages.start || !matches!(state, PageState::Zero | PageState::Clean) {
            return;
        }
        let mut states = self.states.write();
        let adoptable: Vec<u32> = pages
            .filter(|page| matches!(states.state_of(*page), PageState::NotPresent | PageState::Removed))
            .collect();
        adoptable.into_iter().for_each(|page| {
            states.removed.remove(page);
            states.bitmap_of(state).insert(page);
        });
    }

    pub fn mark_written(&self, page: u32) -> PageState {
        let mut states = self.states.write();
        let previous = states.state_of(page);
        match previous {
            PageState::Dirty | PageState::Removed => {}
            PageState::Zero | PageState::Clean | PageState::NotPresent => {
                states.zero.remove(page);
                states.clean.remove(page);
                states.dirty.insert(page);
            }
        }
        previous
    }

    pub fn mark_written_present(&self, page: u32) -> PageState {
        let mut states = self.states.write();
        let previous = states.state_of(page);
        if previous != PageState::Dirty {
            states.zero.remove(page);
            states.removed.remove(page);
            states.clean.remove(page);
            states.dirty.insert(page);
        }
        previous
    }

    pub fn mark_exported(&self, page: u32) {
        let mut states = self.states.write();
        if states.dirty.remove(page) {
            states.clean.insert(page);
        }
    }

    pub fn state(&self, page: u32) -> PageState {
        self.states.read().state_of(page)
    }

    pub fn has_range(&self, pages: Range<u32>, state: PageState) -> bool {
        if pages.end <= pages.start {
            return pages.end == pages.start;
        }
        let wanted = u64::from(pages.end - pages.start);
        self.states
            .read()
            .bitmap(state)
            .is_some_and(|bitmap| bitmap.range_cardinality(pages) == wanted)
    }

    pub fn present(&self, pages: Range<u32>) -> bool {
        if pages.end <= pages.start {
            return pages.end == pages.start;
        }
        let wanted = u64::from(pages.end - pages.start);
        let states = self.states.read();
        [&states.dirty, &states.zero, &states.removed, &states.clean]
            .into_iter()
            .map(|bitmap| bitmap.range_cardinality(pages.clone()))
            .sum::<u64>()
            == wanted
    }

    pub fn export(&self) -> ExportedPages {
        let states = self.states.read();
        ExportedPages {
            dirty: states.dirty.clone(),
            empty: &states.zero | &states.removed,
        }
    }

    pub fn export_states(&self) -> PageStates {
        self.states.read().clone()
    }
}

impl PageStates {
    fn bitmaps(&mut self) -> [(PageState, &mut RoaringBitmap); 4] {
        [
            (PageState::Dirty, &mut self.dirty),
            (PageState::Zero, &mut self.zero),
            (PageState::Removed, &mut self.removed),
            (PageState::Clean, &mut self.clean),
        ]
    }

    fn bitmap(&self, state: PageState) -> Option<&RoaringBitmap> {
        match state {
            PageState::NotPresent => None,
            PageState::Dirty => Some(&self.dirty),
            PageState::Zero => Some(&self.zero),
            PageState::Removed => Some(&self.removed),
            PageState::Clean => Some(&self.clean),
        }
    }

    fn bitmap_of(&mut self, state: PageState) -> &mut RoaringBitmap {
        match state {
            PageState::Zero => &mut self.zero,
            PageState::Removed => &mut self.removed,
            PageState::Clean => &mut self.clean,
            PageState::Dirty | PageState::NotPresent => &mut self.dirty,
        }
    }

    fn state_of(&self, page: u32) -> PageState {
        match () {
            () if self.dirty.contains(page) => PageState::Dirty,
            () if self.zero.contains(page) => PageState::Zero,
            () if self.removed.contains(page) => PageState::Removed,
            () if self.clean.contains(page) => PageState::Clean,
            () => PageState::NotPresent,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker() -> PageTracker {
        PageTracker::default()
    }

    #[test]
    fn transitions() {
        let tracker = tracker();

        tracker.set_range(0..1, PageState::Dirty);
        assert_eq!(tracker.state(0), PageState::Dirty);

        tracker.set_range(0..1, PageState::Zero);
        assert_eq!(tracker.state(0), PageState::Zero, "dirty to zero should flip the page");
        let exported = tracker.export();
        assert!(!exported.dirty.contains(0), "dirty to zero must clear the dirty bitmap");
        assert!(exported.empty.contains(0), "dirty to zero must add to the zero bitmap");

        tracker.set_range(0..1, PageState::Dirty);
        assert_eq!(tracker.state(0), PageState::Dirty, "zero to dirty should flip back");

        tracker.set_range(0..1, PageState::NotPresent);
        assert_eq!(tracker.state(0), PageState::NotPresent, "to not-present must clear");
        let exported = tracker.export();
        assert!(!exported.dirty.contains(0));
        assert!(!exported.empty.contains(0));

        tracker.set_range(0..1, PageState::Dirty);
        tracker.set_range(0..1, PageState::Dirty);
        assert_eq!(tracker.state(0), PageState::Dirty, "dirty to dirty is idempotent");
    }

    #[test]
    fn removed_transitions() {
        let tracker = tracker();

        tracker.set_range(0..1, PageState::Dirty);
        tracker.set_range(0..1, PageState::Removed);
        assert_eq!(
            tracker.state(0),
            PageState::Removed,
            "dirty to removed should flip the page"
        );
        let exported = tracker.export();
        assert!(
            !exported.dirty.contains(0),
            "dirty to removed must clear the dirty bitmap"
        );
        assert!(exported.empty.contains(0), "removed pages export as empty");

        tracker.set_range(0..1, PageState::Zero);
        assert_eq!(
            tracker.state(0),
            PageState::Zero,
            "removed to zero (zero-fill reinstall) should flip"
        );
        assert!(tracker.export().empty.contains(0), "zero pages export as empty");

        tracker.set_range(0..1, PageState::Removed);
        tracker.set_range(0..1, PageState::Dirty);
        assert_eq!(
            tracker.state(0),
            PageState::Dirty,
            "removed to dirty (write reinstall) should flip"
        );
        let exported = tracker.export();
        assert!(exported.dirty.contains(0));
        assert!(!exported.empty.contains(0), "a dirty page must leave the empty set");

        tracker.set_range(0..1, PageState::Removed);
        tracker.set_range(0..1, PageState::NotPresent);
        assert_eq!(
            tracker.state(0),
            PageState::NotPresent,
            "to not-present must clear removed"
        );
        assert!(!tracker.export().empty.contains(0));
    }

    #[test]
    fn clean_pages_are_present_but_export_in_neither_set() {
        let tracker = tracker();

        tracker.set_range(0..3, PageState::Clean);
        assert_eq!(tracker.state(0), PageState::Clean);
        assert!(tracker.has_range(0..3, PageState::Clean));
        assert!(tracker.present(0..3), "clean pages are installed");

        let exported = tracker.export();
        assert!(
            exported.dirty.is_empty(),
            "clean is not dirty: the diff inherits the page from the parent"
        );
        assert!(
            exported.empty.is_empty(),
            "clean is not empty: the page holds source content"
        );

        tracker.set_range(0..1, PageState::Dirty);
        assert_eq!(
            tracker.state(0),
            PageState::Dirty,
            "clean to dirty promotion flips the page"
        );
        assert!(tracker.export().dirty.contains(0));
    }

    #[test]
    fn mark_installed_only_upgrades_not_present_and_removed() {
        let tracker = tracker();

        tracker.set_range(0..1, PageState::Dirty);
        tracker.set_range(1..2, PageState::Zero);
        tracker.set_range(2..3, PageState::Clean);
        tracker.set_range(3..4, PageState::Removed);

        tracker.mark_installed(0..5, PageState::Clean);

        assert_eq!(
            tracker.state(0),
            PageState::Dirty,
            "a late installer must not downgrade a promoted dirty page"
        );
        assert_eq!(tracker.state(1), PageState::Zero, "an installed zero page is untouched");
        assert_eq!(tracker.state(2), PageState::Clean);
        assert_eq!(
            tracker.state(3),
            PageState::Clean,
            "a removed page adopts the reinstall state"
        );
        assert_eq!(
            tracker.state(4),
            PageState::Clean,
            "an untracked page adopts the install state"
        );

        tracker.set_range(5..6, PageState::Removed);
        tracker.mark_installed(5..6, PageState::Zero);
        assert_eq!(
            tracker.state(5),
            PageState::Zero,
            "a zero-fill reinstall moves removed back to zero"
        );

        tracker.mark_installed(6..7, PageState::Dirty);
        assert_eq!(
            tracker.state(6),
            PageState::NotPresent,
            "mark_installed only accepts read-side states"
        );

        tracker.mark_installed(8..8, PageState::Clean);
        assert_eq!(tracker.state(8), PageState::NotPresent, "an empty range is a no-op");
    }

    #[test]
    fn mark_written_promotion_matrix() {
        let cases = [
            ("clean promotes", PageState::Clean, PageState::Clean, PageState::Dirty),
            (
                "zero promotes (a written zero page must not export as empty)",
                PageState::Zero,
                PageState::Zero,
                PageState::Dirty,
            ),
            (
                "not-present promotes (installer mid-marking; the page is present)",
                PageState::NotPresent,
                PageState::NotPresent,
                PageState::Dirty,
            ),
            ("dirty is a no-op", PageState::Dirty, PageState::Dirty, PageState::Dirty),
            (
                "removed is skipped (stale WP fault racing a REMOVE)",
                PageState::Removed,
                PageState::Removed,
                PageState::Removed,
            ),
        ];
        for (name, setup, want_previous, want) in cases {
            let tracker = tracker();
            tracker.set_range(0..1, setup);

            let previous = tracker.mark_written(0);

            assert_eq!(previous, want_previous, "{name}: previous state");
            assert_eq!(tracker.state(0), want, "{name}: final state");
        }
    }

    #[test]
    fn mark_written_present_promotes_through_removed() {
        let tracker = tracker();
        tracker.set_range(0..1, PageState::Removed);

        let previous = tracker.mark_written_present(0);

        assert_eq!(previous, PageState::Removed);
        assert_eq!(tracker.state(0), PageState::Dirty);
        assert_eq!(tracker.mark_written_present(0), PageState::Dirty);
    }

    #[test]
    fn mark_written_then_mark_installed_keeps_the_promotion() {
        let tracker = tracker();

        tracker.mark_written(0);
        tracker.mark_installed(0..1, PageState::Clean);

        assert_eq!(
            tracker.state(0),
            PageState::Dirty,
            "a late install marking must not downgrade the promotion"
        );
    }

    #[test]
    fn mark_exported_demotes_only_dirty() {
        let tracker = tracker();
        tracker.set_range(0..1, PageState::Dirty);
        tracker.set_range(1..2, PageState::Zero);
        tracker.set_range(2..3, PageState::Removed);
        tracker.set_range(3..4, PageState::Clean);

        (0..5).for_each(|page| tracker.mark_exported(page));

        assert_eq!(
            tracker.state(0),
            PageState::Clean,
            "an exported dirty page rebaselines to clean"
        );
        assert_eq!(tracker.state(1), PageState::Zero, "a zero page is untouched");
        assert_eq!(
            tracker.state(2),
            PageState::Removed,
            "a removed page must not be resurrected"
        );
        assert_eq!(tracker.state(3), PageState::Clean, "a clean page stays clean");
        assert_eq!(
            tracker.state(4),
            PageState::NotPresent,
            "an untracked page is untouched"
        );

        tracker.mark_written(0);
        assert_eq!(
            tracker.state(0),
            PageState::Dirty,
            "a write after export re-dirties for the next interval"
        );
    }

    #[test]
    fn export_states_returns_the_per_state_split() {
        let tracker = tracker();
        tracker.set_range(0..1, PageState::Dirty);
        tracker.set_range(1..2, PageState::Zero);
        tracker.set_range(2..3, PageState::Removed);
        tracker.set_range(3..4, PageState::Clean);

        let states = tracker.export_states();

        assert!(states.dirty.contains(0));
        assert!(states.zero.contains(1));
        assert!(states.removed.contains(2));
        assert!(states.clean.contains(3));
        assert_eq!(
            [
                states.dirty.len(),
                states.zero.len(),
                states.removed.len(),
                states.clean.len()
            ],
            [1, 1, 1, 1]
        );
    }

    #[test]
    fn zero_and_removed_are_distinct_states_but_share_the_empty_export() {
        let tracker = tracker();
        tracker.set_range(0..3, PageState::Zero);
        tracker.set_range(3..6, PageState::Removed);

        assert!(tracker.has_range(0..3, PageState::Zero));
        assert!(tracker.has_range(3..6, PageState::Removed));
        assert!(!tracker.has_range(0..6, PageState::Zero), "removed pages are not zero");
        assert!(
            !tracker.has_range(0..6, PageState::Removed),
            "zero pages are not removed"
        );
        assert!(tracker.present(0..6), "both states count as observed");

        let exported = tracker.export();
        assert!(exported.dirty.is_empty());
        assert_eq!(exported.empty.len(), 6, "the empty set is zero union removed");
    }

    #[test]
    fn a_partial_overlap_moves_only_the_overlapping_pages() {
        let tracker = tracker();

        tracker.set_range(0..10, PageState::Dirty);
        tracker.set_range(3..7, PageState::Zero);

        (0..3).chain(7..10).for_each(|page| {
            assert_eq!(
                tracker.state(page),
                PageState::Dirty,
                "page {page} outside the overlap stays dirty"
            );
        });
        (3..7).for_each(|page| {
            assert_eq!(
                tracker.state(page),
                PageState::Zero,
                "page {page} in the overlap moves to zero"
            );
        });
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn empty_and_inverted_ranges_are_no_ops() {
        let tracker = tracker();

        tracker.set_range(5..5, PageState::Dirty);
        tracker.set_range(7..3, PageState::Zero);

        let exported = tracker.export();
        assert!(exported.dirty.is_empty());
        assert!(exported.empty.is_empty());
    }

    #[test]
    #[allow(clippy::reversed_empty_ranges)]
    fn has_range_checks_contiguous_state_coverage() {
        let tracker = tracker();
        tracker.set_range(0..5, PageState::Dirty);
        tracker.set_range(5..10, PageState::Zero);

        assert!(tracker.has_range(0..5, PageState::Dirty));
        assert!(tracker.has_range(5..10, PageState::Zero));
        assert!(!tracker.has_range(0..5, PageState::Zero));
        assert!(!tracker.has_range(0..6, PageState::Dirty), "a gap in dirty must fail");
        assert!(
            tracker.has_range(3..3, PageState::Dirty),
            "an empty range is vacuously true"
        );
        assert!(!tracker.has_range(7..5, PageState::Dirty), "an inverted range is false");
        assert!(!tracker.has_range(0..5, PageState::NotPresent));
    }

    #[test]
    fn present_accepts_mixed_dirty_and_zero() {
        let tracker = tracker();
        tracker.set_range(0..5, PageState::Dirty);
        tracker.set_range(5..10, PageState::Zero);

        assert!(tracker.present(0..10), "mixed dirty and zero should be present");
        assert!(!tracker.present(0..11), "any not-present page in the range fails");
        assert!(tracker.present(3..3), "an empty range is vacuously true");
    }

    #[test]
    fn export_returns_clones() {
        let tracker = tracker();
        tracker.set_range(0..1, PageState::Dirty);

        let mut exported = tracker.export();
        exported.dirty.insert(42);

        assert_eq!(
            tracker.state(42),
            PageState::NotPresent,
            "mutating an export must not leak into the tracker"
        );
    }
}
