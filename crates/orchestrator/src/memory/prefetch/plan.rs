use super::last_cycle::{cap, diff_mapping};
use super::source::PrefetchSettings;
use crate::storage::header::Header;
use crate::template::metadata::MemoryPrefetch;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrefetchPlan {
    pub init: Option<MemoryPrefetch>,
    pub last_cycle: Option<MemoryPrefetch>,
}

impl PrefetchPlan {
    pub fn resolve(settings: PrefetchSettings, init_trace: Option<MemoryPrefetch>, memfile: &Header) -> Self {
        Self {
            init: init_trace.filter(|_| settings.source.uses_init()),
            last_cycle: settings
                .source
                .uses_last_cycle()
                .then(|| cap(diff_mapping(memfile), settings.last_cycle_max_mib))
                .flatten(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.init.is_none() && self.last_cycle.is_none()
    }

    pub fn init_blocks(&self) -> usize {
        blocks(self.init.as_ref())
    }

    pub fn last_cycle_blocks(&self) -> usize {
        blocks(self.last_cycle.as_ref())
    }
}

fn blocks(mapping: Option<&MemoryPrefetch>) -> usize {
    mapping.map_or(0, |mapping| mapping.indices.len())
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::memory::prefetch::source::PrefetchSource;
    use crate::storage::header::{BuildMapping, Metadata};

    const BLOCK: u64 = 4096;

    fn layered() -> Header {
        let (own, base) = (Uuid::from_u128(2), Uuid::from_u128(1));
        let mapping = |build_id, offset, blocks: u64| BuildMapping {
            offset: offset * BLOCK,
            length: blocks * BLOCK,
            build_id,
            storage_offset: 0,
        };
        let metadata = Metadata {
            base_build_id: base,
            ..Metadata::template(own, BLOCK, 8 * BLOCK)
        };
        Header::new(
            metadata,
            vec![mapping(base, 0, 4), mapping(own, 4, 3), mapping(base, 7, 1)],
        )
        .unwrap()
    }

    fn trace() -> MemoryPrefetch {
        MemoryPrefetch {
            indices: vec![1, 0],
            access_types: vec!["r".to_owned(), "r".to_owned()],
            block_size: BLOCK as i64,
        }
    }

    fn settings(source: &str, last_cycle_max_mib: i64) -> PrefetchSettings {
        PrefetchSettings {
            source: PrefetchSource::parse(source),
            last_cycle_max_mib,
        }
    }

    fn resolved(source: &str) -> PrefetchPlan {
        PrefetchPlan::resolve(settings(source, -1), Some(trace()), &layered())
    }

    #[test]
    fn off_resolves_nothing() {
        assert!(resolved("off").is_empty());
    }

    #[test]
    fn init_replays_only_the_recorded_trace() {
        let plan = resolved("init");

        assert_eq!((plan.init, plan.last_cycle), (Some(trace()), None));
    }

    #[test]
    fn last_cycle_replays_only_the_pause_diff() {
        let plan = resolved("last-cycle");

        assert_eq!(plan.init, None);
        assert_eq!(plan.last_cycle.map(|mapping| mapping.indices), Some(vec![4, 5, 6]));
    }

    #[test]
    fn both_resolves_the_trace_and_the_diff() {
        let plan = resolved("both");

        assert_eq!((plan.init_blocks(), plan.last_cycle_blocks()), (2, 3));
    }

    #[test]
    fn a_missing_trace_leaves_only_the_diff() {
        let plan = PrefetchPlan::resolve(settings("both", -1), None, &layered());

        assert_eq!((plan.init_blocks(), plan.last_cycle_blocks()), (0, 3));
    }

    #[test]
    fn the_cap_bounds_only_the_diff() {
        let plan = PrefetchPlan::resolve(settings("both", 0), Some(trace()), &layered());

        assert_eq!((plan.init_blocks(), plan.last_cycle_blocks()), (2, 0));
        assert!(!plan.is_empty());
    }
}
