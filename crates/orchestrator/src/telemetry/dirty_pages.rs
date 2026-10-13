use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram};
use roaring::RoaringBitmap;

const UFFD_METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/uffd";
const SANDBOX_METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox";

static DIVERGENCE: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    global::meter(UFFD_METER_NAME)
        .u64_histogram("orchestrator.sandbox.uffd.dirty_divergence")
        .with_description(
            "Per-pause dirty-set divergence between the page tracker and the pagemap readout on sync-WP sandboxes, in pages",
        )
        .with_unit("{page}")
        .build()
});

static SOURCE: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(SANDBOX_METER_NAME)
        .u64_counter("orchestrator.sandbox.snapshot.memory.dirty_source")
        .with_description(
            "Pauses by where the memory dirty set came from (pagemap, or resident minus empty when write-protect was unconfirmed, unsupported or there was no UFFD)",
        )
        .with_unit("{pause}")
        .build()
});

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirtySource {
    Pagemap,
    Unconfirmed,
    Unsupported,
    NoUffd,
}

impl DirtySource {
    pub fn label(self) -> &'static str {
        match self {
            DirtySource::Pagemap => "pagemap",
            DirtySource::Unconfirmed => "resident_unconfirmed",
            DirtySource::Unsupported => "resident_unsupported",
            DirtySource::NoUffd => "resident_no_uffd",
        }
    }
}

pub fn record_dirty_source(source: DirtySource) {
    SOURCE.add(1, &[KeyValue::new("source", source.label())]);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Divergence {
    pub tracker_only: u64,
    pub pagemap_only: u64,
    pub pagemap_dirty: u64,
}

impl Divergence {
    pub fn between(tracker: &RoaringBitmap, pagemap: &RoaringBitmap) -> Self {
        let both = tracker.intersection_len(pagemap);
        Self {
            tracker_only: tracker.len() - both,
            pagemap_only: pagemap.len() - both,
            pagemap_dirty: pagemap.len(),
        }
    }

    pub fn record(self) {
        [
            ("tracker_only", self.tracker_only),
            ("pagemap_only", self.pagemap_only),
            ("pagemap_dirty", self.pagemap_dirty),
        ]
        .into_iter()
        .for_each(|(set, pages)| DIVERGENCE.record(pages, &[KeyValue::new("set", set)]));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn divergence_counts_each_side_like_go() {
        let tracker: RoaringBitmap = [1, 2, 3].into_iter().collect();
        let pagemap: RoaringBitmap = [2, 3, 4, 5].into_iter().collect();

        assert_eq!(
            Divergence::between(&tracker, &pagemap),
            Divergence {
                tracker_only: 1,
                pagemap_only: 2,
                pagemap_dirty: 4,
            }
        );
    }
}
