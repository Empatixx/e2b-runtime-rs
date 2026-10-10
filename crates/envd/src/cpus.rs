pub mod hotplug;
pub mod mask;
pub mod sysfs;
pub mod target_file;

use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct State {
    pub online: usize,
    pub possible: usize,
    pub target: usize,
    pub attempts: usize,
    pub write_pending: Duration,
}

pub trait CpuManager: Send + Sync {
    fn set_target(&self, count: i64) -> Result<(), String>;
    fn status(&self) -> State;
    fn start(self: Arc<Self>);
}

pub struct NoopCpus;

impl CpuManager for NoopCpus {
    fn set_target(&self, _count: i64) -> Result<(), String> {
        Ok(())
    }

    fn status(&self) -> State {
        State::default()
    }

    fn start(self: Arc<Self>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_noop_manager_accepts_any_target_and_reports_nothing() {
        let noop = NoopCpus;
        assert_eq!(noop.set_target(64), Ok(()));
        assert_eq!(noop.status(), State::default());
    }
}
