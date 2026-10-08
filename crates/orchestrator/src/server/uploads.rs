use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use super::retry::{Failure, retry};

const UPLOAD_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Pending,
    Landed,
    Failed,
}

#[derive(Default)]
pub struct Uploads {
    outcomes: Mutex<HashMap<Uuid, watch::Receiver<Outcome>>>,
    tasks: TaskTracker,
}

impl Uploads {
    pub fn start<F, Fut>(
        self: &Arc<Self>,
        build_id: Uuid,
        parents: Vec<Uuid>,
        attempt: F,
        finished: impl FnOnce(Outcome) + Send + 'static,
    ) where
        F: FnMut() -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), Failure>> + Send,
    {
        let sender = self.register(build_id);
        let uploads = self.clone();
        self.tasks.spawn(async move {
            let uploaded = uploads.upload(build_id, &parents, attempt).await;
            if let Err(e) = &uploaded {
                tracing::error!(build = %build_id, error = %e, "snapshot upload failed; the build stays only in this node's cache");
            }
            finished(settle(&sender, &uploaded));
        });
    }

    pub async fn run<Fut>(&self, build_id: Uuid, parents: Vec<Uuid>, attempt: Fut) -> Result<(), String>
    where
        Fut: Future<Output = Result<(), Failure>>,
    {
        let sender = self.register(build_id);
        let once = async {
            attempt.await.map_err(Failure::into_message)?;
            self.wait_parents(build_id, &parents).await
        };
        let uploaded = tokio::time::timeout(UPLOAD_TIMEOUT, once)
            .await
            .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()));
        settle(&sender, &uploaded);
        uploaded
    }

    fn register(&self, build_id: Uuid) -> watch::Sender<Outcome> {
        let (sender, receiver) = watch::channel(Outcome::Pending);
        self.outcomes.lock().insert(build_id, receiver);
        sender
    }

    pub fn outcome(&self, build_id: Uuid) -> Option<Outcome> {
        self.outcomes.lock().get(&build_id).map(|outcome| *outcome.borrow())
    }

    pub async fn wait(&self, build_id: Uuid) -> Outcome {
        let receiver = self.outcomes.lock().get(&build_id).cloned();
        match receiver {
            Some(mut receiver) => receiver
                .wait_for(|outcome| *outcome != Outcome::Pending)
                .await
                .map_or(Outcome::Failed, |outcome| *outcome),
            None => Outcome::Landed,
        }
    }

    pub async fn wait_all(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }

    async fn upload<F, Fut>(&self, build_id: Uuid, parents: &[Uuid], attempt: F) -> Result<(), String>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<(), Failure>>,
    {
        retry(attempt).await?;
        self.wait_parents(build_id, parents).await
    }

    async fn wait_parents(&self, build_id: Uuid, parents: &[Uuid]) -> Result<(), String> {
        for parent in parents.iter().filter(|parent| **parent != build_id) {
            if self.wait(*parent).await == Outcome::Failed {
                return Err(format!("parent upload {parent} failed"));
            }
        }
        Ok(())
    }
}

fn settle(sender: &watch::Sender<Outcome>, uploaded: &Result<(), String>) -> Outcome {
    let outcome = match uploaded {
        Ok(()) => Outcome::Landed,
        Err(_) => Outcome::Failed,
    };
    sender.send_replace(outcome);
    outcome
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::*;

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    #[tokio::test(start_paused = true)]
    async fn a_synchronous_upload_tries_once_and_records_its_outcome() {
        let uploads = Arc::new(Uploads::default());

        let failed = uploads
            .run(id(1), Vec::new(), async {
                Err(Failure::Retryable("bucket is gone".to_owned()))
            })
            .await;
        let landed = uploads.run(id(2), vec![id(3)], async { Ok(()) }).await;

        assert_eq!(failed, Err("bucket is gone".to_owned()));
        assert_eq!(uploads.outcome(id(1)), Some(Outcome::Failed));
        assert_eq!(landed, Ok(()));
        assert_eq!(uploads.outcome(id(2)), Some(Outcome::Landed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_synchronous_upload_gives_up_after_twenty_minutes() {
        let uploads = Arc::new(Uploads::default());

        let stalled = uploads.run(id(1), Vec::new(), std::future::pending()).await;

        assert_eq!(stalled, Err("context deadline exceeded".to_owned()));
        assert_eq!(uploads.outcome(id(1)), Some(Outcome::Failed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_child_lands_only_after_its_parent() {
        let uploads = Arc::new(Uploads::default());
        let (release, released) = oneshot::channel::<()>();
        let mut parent_gate = Some(released);
        uploads.start(
            id(1),
            Vec::new(),
            move || {
                let gate = parent_gate.take();
                async move {
                    if let Some(gate) = gate {
                        drop(gate.await);
                    }
                    Ok(())
                }
            },
            |_| {},
        );
        uploads.start(id(2), vec![id(1), id(2)], || async { Ok(()) }, |_| {});

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(uploads.outcome(id(2)), Some(Outcome::Pending));
        release.send(()).unwrap();
        assert_eq!(uploads.wait(id(2)).await, Outcome::Landed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_parent_fails_the_child_and_wait_all_sees_both_end() {
        let uploads = Arc::new(Uploads::default());
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let parent_seen = outcomes.clone();
        let child_seen = outcomes.clone();
        uploads.start(
            id(1),
            Vec::new(),
            || async { Err(Failure::Terminal("snapshot files are gone".to_owned())) },
            move |outcome| parent_seen.lock().push((1, outcome)),
        );
        uploads.start(
            id(2),
            vec![id(1)],
            || async { Ok(()) },
            move |outcome| child_seen.lock().push((2, outcome)),
        );

        uploads.wait_all().await;

        let mut seen = outcomes.lock().clone();
        seen.sort_by_key(|(build, _)| *build);
        assert_eq!(seen, vec![(1, Outcome::Failed), (2, Outcome::Failed)]);
        assert_eq!(uploads.wait(id(9)).await, Outcome::Landed);
    }
}
