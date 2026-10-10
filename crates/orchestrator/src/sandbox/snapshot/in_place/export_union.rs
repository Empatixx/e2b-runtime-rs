use parking_lot::Mutex;
use roaring::RoaringBitmap;

#[derive(Debug, Default)]
pub struct ExportBaseline {
    exported: Mutex<Option<RoaringBitmap>>,
}

impl ExportBaseline {
    pub fn apply(&self, dirty: &mut RoaringBitmap, empty: &mut RoaringBitmap, advance: bool) {
        let mut exported = self.exported.lock();
        if let Some(baseline) = exported.as_ref() {
            *dirty |= baseline - &*empty;
            *empty -= &*dirty;
        }
        if advance {
            *exported = Some(dirty.clone());
        }
    }

    pub fn snapshot(&self) -> Option<RoaringBitmap> {
        self.exported.lock().clone()
    }

    #[cfg(test)]
    pub fn seed(&self, pages: RoaringBitmap) {
        *self.exported.lock() = Some(pages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bitmap(pages: impl IntoIterator<Item = u32>) -> RoaringBitmap {
        pages.into_iter().collect()
    }

    #[test]
    fn freed_pages_stay_empty_while_still_present_baseline_pages_are_re_exported() {
        let baseline = ExportBaseline::default();
        baseline.seed(bitmap([0, 1, 2]));
        let mut dirty = bitmap([3]);
        let mut empty = bitmap([1]);

        baseline.apply(&mut dirty, &mut empty, true);

        assert_eq!(
            dirty.iter().collect::<Vec<_>>(),
            vec![0, 2, 3],
            "still-present baseline pages re-export; the freed page must not"
        );
        assert_eq!(empty.iter().collect::<Vec<_>>(), vec![1], "the freed page stays empty");
        assert_eq!(
            baseline.snapshot().unwrap().iter().collect::<Vec<_>>(),
            vec![0, 2, 3],
            "the advanced baseline drops the freed page"
        );
    }

    #[test]
    fn without_a_baseline_the_readout_is_untouched_and_a_destroy_path_does_not_advance() {
        let baseline = ExportBaseline::default();
        let mut dirty = bitmap([5]);
        let mut empty = bitmap([6]);

        baseline.apply(&mut dirty, &mut empty, false);

        assert_eq!((dirty.len(), empty.len()), (1, 1));
        assert!(baseline.snapshot().is_none());
    }

    #[test]
    fn the_union_applies_to_a_destroy_path_pause_without_moving_the_baseline() {
        let baseline = ExportBaseline::default();
        baseline.seed(bitmap([0, 1]));
        let mut dirty = bitmap([2]);
        let mut empty = RoaringBitmap::new();

        baseline.apply(&mut dirty, &mut empty, false);

        assert_eq!(dirty.iter().collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(baseline.snapshot().unwrap().iter().collect::<Vec<_>>(), vec![0, 1]);
    }
}
