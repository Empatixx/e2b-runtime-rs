use futures::future::BoxFuture;
use tokio::sync::oneshot;
use uuid::Uuid;

pub const CHANNEL_PREFIX: &str = "orchestrator.upload.done.";

pub type Outcome = Option<String>;

pub fn channel(build_id: Uuid) -> String {
    format!("{CHANNEL_PREFIX}{build_id}")
}

pub trait UploadDone: Send + Sync {
    fn publish(&self, build_id: Uuid, error: Option<String>) -> BoxFuture<'static, ()>;
    fn subscribe(&self, build_id: Uuid) -> BoxFuture<'static, Option<oneshot::Receiver<Outcome>>>;
}

pub fn payload(error: &Option<String>) -> &str {
    error.as_deref().unwrap_or_default()
}

pub fn outcome_of(payload: String) -> Outcome {
    (!payload.is_empty()).then_some(payload)
}

#[cfg(test)]
pub mod testing {
    use std::sync::Arc;

    use futures::FutureExt;
    use parking_lot::Mutex;

    use super::*;

    #[derive(Default)]
    pub struct Channels {
        pub published: Mutex<Vec<(Uuid, Option<String>)>>,
        pub waiting: Mutex<Vec<(Uuid, oneshot::Sender<Outcome>)>>,
    }

    impl Channels {
        pub fn fire(&self, build_id: Uuid, outcome: Outcome) {
            let mut waiting = self.waiting.lock();
            let mut index = 0;
            while index < waiting.len() {
                if waiting[index].0 == build_id {
                    let (_, sender) = waiting.remove(index);
                    let _ = sender.send(outcome.clone());
                } else {
                    index += 1;
                }
            }
        }
    }

    impl UploadDone for Arc<Channels> {
        fn publish(&self, build_id: Uuid, error: Option<String>) -> BoxFuture<'static, ()> {
            self.published.lock().push((build_id, error));
            async {}.boxed()
        }

        fn subscribe(&self, build_id: Uuid) -> BoxFuture<'static, Option<oneshot::Receiver<Outcome>>> {
            let (sender, receiver) = oneshot::channel();
            self.waiting.lock().push((build_id, sender));
            async move { Some(receiver) }.boxed()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_channel_carries_the_upstream_prefix() {
        let build_id = Uuid::nil();

        assert_eq!(
            channel(build_id),
            "orchestrator.upload.done.00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn an_empty_payload_means_success_and_anything_else_is_the_error() {
        assert_eq!(outcome_of(String::new()), None);
        assert_eq!(outcome_of("disk full".to_owned()), Some("disk full".to_owned()));
        assert_eq!(payload(&None), "");
        assert_eq!(payload(&Some("boom".to_owned())), "boom");
    }
}
