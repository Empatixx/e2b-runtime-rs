use std::sync::atomic::{AtomicU64, Ordering};

use opentelemetry::global;
use opentelemetry::metrics::ObservableCounter;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/chrooted";

static RESOLVE_RETRIES_EXHAUSTED: AtomicU64 = AtomicU64::new(0);
static PATHS_REFUSED: AtomicU64 = AtomicU64::new(0);
static RESOLVE_LOOPS: AtomicU64 = AtomicU64::new(0);

pub fn count_resolve_retries_exhausted() {
    RESOLVE_RETRIES_EXHAUSTED.fetch_add(1, Ordering::Relaxed);
}

pub fn count_path_refused() {
    PATHS_REFUSED.fetch_add(1, Ordering::Relaxed);
}

pub fn count_resolve_loop() {
    RESOLVE_LOOPS.fetch_add(1, Ordering::Relaxed);
}

pub struct ChrootMetrics {
    _retries_exhausted: ObservableCounter<u64>,
    _path_refused: ObservableCounter<u64>,
    _eloop: ObservableCounter<u64>,
}

impl ChrootMetrics {
    pub fn register() -> Self {
        Self {
            _retries_exhausted: observe(
                "orchestrator.chroot.resolve.retries_exhausted",
                "Path resolutions abandoned because the kernel kept detecting a rename or mount race",
                &RESOLVE_RETRIES_EXHAUSTED,
            ),
            _path_refused: observe(
                "orchestrator.chroot.path.refused",
                "Entry names refused before reaching a syscall: empty, \".\", \"..\" or containing a slash",
                &PATHS_REFUSED,
            ),
            _eloop: observe(
                "orchestrator.chroot.resolve.eloop",
                "Symlink resolutions abandoned at the hop limit",
                &RESOLVE_LOOPS,
            ),
        }
    }
}

fn observe(name: &'static str, description: &'static str, counter: &'static AtomicU64) -> ObservableCounter<u64> {
    global::meter(METER_NAME)
        .u64_observable_counter(name)
        .with_description(description)
        .with_unit("1")
        .with_callback(move |observer| observer.observe(counter.load(Ordering::Relaxed), &[]))
        .build()
}
