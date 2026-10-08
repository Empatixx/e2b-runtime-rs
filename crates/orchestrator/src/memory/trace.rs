use std::collections::BTreeMap;

use crate::template::metadata::MemoryPrefetch;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Prefetch,
}

impl Access {
    pub fn code(&self) -> &'static str {
        match self {
            Access::Read => "r",
            Access::Write => "w",
            Access::Prefetch => "p",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Touch {
    pub order: u64,
    pub access: Access,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trace {
    pub block_size: u64,
    pub touches: BTreeMap<u64, Touch>,
}

#[derive(Debug)]
pub struct PrefetchTracker {
    trace: Trace,
    next_order: u64,
    tracking: bool,
}

impl PrefetchTracker {
    pub fn new(block_size: u64) -> Self {
        Self {
            trace: Trace {
                block_size,
                touches: BTreeMap::new(),
            },
            next_order: 1,
            tracking: true,
        }
    }

    pub fn record(&mut self, index: u64, access: Access) {
        if !self.tracking || self.trace.touches.contains_key(&index) {
            return;
        }
        self.trace.touches.insert(
            index,
            Touch {
                order: self.next_order,
                access,
            },
        );
        self.next_order += 1;
    }

    pub fn collect(&mut self) -> Trace {
        self.tracking = false;
        self.trace.clone()
    }
}

impl Trace {
    pub fn intersect(runs: &[Trace]) -> Option<Trace> {
        let (first, others) = runs.split_first()?;
        let touches: BTreeMap<u64, Touch> = first
            .touches
            .iter()
            .filter_map(|(index, touch)| {
                let matching: Vec<&Touch> = others.iter().filter_map(|run| run.touches.get(index)).collect();
                (matching.len() == others.len()).then(|| (*index, merged(touch, &matching)))
            })
            .collect();
        (!touches.is_empty()).then_some(Trace {
            block_size: first.block_size,
            touches,
        })
    }

    pub fn to_mapping(&self) -> MemoryPrefetch {
        let mut ordered: Vec<(&u64, &Touch)> = self.touches.iter().collect();
        ordered.sort_by_key(|(index, touch)| (touch.order, **index));
        MemoryPrefetch {
            indices: ordered.iter().map(|(index, _)| **index).collect(),
            access_types: ordered
                .iter()
                .map(|(_, touch)| touch.access.code().to_owned())
                .collect(),
            block_size: self.block_size as i64,
        }
    }
}

fn merged(first: &Touch, others: &[&Touch]) -> Touch {
    let runs = others.len() as u64 + 1;
    let order = (first.order + others.iter().map(|touch| touch.order).sum::<u64>()) / runs;
    let access = match others.iter().all(|touch| touch.access == first.access) {
        true => first.access,
        false => Access::Read,
    };
    Touch { order, access }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(touches: &[(u64, u64, Access)]) -> Trace {
        Trace {
            block_size: 4096,
            touches: touches
                .iter()
                .map(|(index, order, access)| {
                    (
                        *index,
                        Touch {
                            order: *order,
                            access: *access,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn only_the_first_touch_counts_and_collecting_stops_tracking() {
        let mut tracker = PrefetchTracker::new(4096);
        tracker.record(9, Access::Read);
        tracker.record(3, Access::Write);
        tracker.record(9, Access::Write);

        let collected = tracker.collect();
        tracker.record(5, Access::Read);

        assert_eq!(collected, trace(&[(9, 1, Access::Read), (3, 2, Access::Write)]));
        assert_eq!(tracker.collect(), collected);
    }

    #[test]
    fn runs_intersect_by_index_with_averaged_order_and_read_on_disagreement() {
        let first = trace(&[(1, 1, Access::Write), (2, 2, Access::Read), (7, 3, Access::Read)]);
        let second = trace(&[(2, 1, Access::Read), (1, 4, Access::Read), (8, 2, Access::Read)]);

        let common = Trace::intersect(&[first, second]).unwrap();

        assert_eq!(common, trace(&[(1, 2, Access::Read), (2, 1, Access::Read)]));
        assert_eq!(
            common.to_mapping(),
            MemoryPrefetch {
                indices: vec![2, 1],
                access_types: vec!["r".to_owned(), "r".to_owned()],
                block_size: 4096,
            }
        );
        assert_eq!(
            Trace::intersect(&[trace(&[(1, 1, Access::Read)]), trace(&[(2, 1, Access::Read)])]),
            None
        );
    }
}
