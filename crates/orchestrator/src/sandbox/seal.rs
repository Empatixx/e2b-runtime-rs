use std::sync::Arc;

use futures::FutureExt;
use parking_lot::Mutex;

use crate::block::build_file::Ready;

#[derive(Clone, Default)]
pub struct SealSlot {
    ready: Arc<Mutex<Option<Ready>>>,
}

impl SealSlot {
    pub fn set(&self, ready: Ready) {
        *self.ready.lock() = Some(ready);
    }

    pub fn current(&self) -> Option<Ready> {
        self.ready.lock().clone()
    }

    pub fn pending(&self) -> bool {
        self.current().is_some_and(|ready| ready.now_or_never().is_none())
    }

    pub async fn wait(&self) -> Result<(), String> {
        match self.current() {
            Some(ready) => ready.await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::build_file::ready_now;

    #[test]
    fn a_slot_is_pending_only_while_its_seal_runs() {
        let slot = SealSlot::default();
        assert!(!slot.pending(), "an empty slot has nothing to wait for");

        let (finish, finished) = futures::channel::oneshot::channel::<()>();
        slot.set(finished.map(|_| Ok(())).boxed().shared());
        assert!(slot.pending());

        finish.send(()).unwrap();
        assert!(!slot.pending());
        slot.set(ready_now());
        assert!(!slot.pending());
    }
}
