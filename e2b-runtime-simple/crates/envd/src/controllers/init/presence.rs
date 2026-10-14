use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub trait Presence: Send + Sync {
    fn gone(&self) -> bool;
}

#[derive(Default)]
pub struct Client {
    gone: AtomicBool,
}

pub struct Watch {
    client: Arc<Client>,
}

impl Presence for Client {
    fn gone(&self) -> bool {
        self.gone.load(Ordering::SeqCst)
    }
}

impl Client {
    pub fn watch(self: &Arc<Self>) -> Watch {
        Watch { client: self.clone() }
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.client.gone.store(true, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_client_is_gone_once_its_watch_is_dropped() {
        let client = Arc::new(Client::default());
        let watch = client.watch();
        assert!(!client.gone());

        drop(watch);

        assert!(client.gone());
    }
}
