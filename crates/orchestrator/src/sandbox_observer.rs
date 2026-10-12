pub mod gauges;
pub mod metrics;
pub mod poll;
pub mod sandboxes;

use std::sync::Arc;

use opentelemetry::metrics::Meter;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use self::gauges::SandboxGauges;
use self::poll::{Observation, Sandboxes};
use crate::telemetry::metrics::SandboxExport;

pub struct SandboxObserver {
    poller: JoinHandle<()>,
    _gauges: SandboxGauges,
}

impl SandboxObserver {
    pub fn start(meter: &Meter, sandboxes: Sandboxes, export: Option<SandboxExport>) -> Self {
        let snapshot: Arc<Mutex<Vec<Observation>>> = Arc::default();
        Self {
            _gauges: SandboxGauges::register(meter, snapshot.clone()),
            poller: tokio::spawn(poll::run(sandboxes, snapshot, export)),
        }
    }
}

impl Drop for SandboxObserver {
    fn drop(&mut self) {
        self.poller.abort();
    }
}
