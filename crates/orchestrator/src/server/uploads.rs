use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::watch;
use tokio_util::task::TaskTracker;
use tracing::Instrument;
use uuid::Uuid;

use super::retry::{Failure, retry};
use crate::peer::parents::PeerParents;

const DRAIN_LOG_INTERVAL: Duration = Duration::from_secs(10);
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
    peer_parents: OnceLock<PeerParents>,
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
        self.tasks.spawn(
            async move {
                let uploaded = uploads.upload(build_id, &parents, attempt).await;
                match &uploaded {
                    Ok(()) => tracing::info!(build = %build_id, "snapshot finished uploading successfully"),
                    Err(e) => tracing::error!(build = %build_id, error = %e, "snapshot upload did not durably land"),
                }
                let outcome = settle(&sender, &uploaded);
                uploads.forget_landed(build_id, outcome);
                finished(outcome);
            }
            .instrument(tracing::info_span!("upload snapshot")),
        );
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
        let outcome = settle(&sender, &uploaded);
        self.forget_landed(build_id, outcome);
        uploaded
    }

    fn forget_landed(&self, build_id: Uuid, outcome: Outcome) {
        if outcome == Outcome::Landed {
            self.outcomes.lock().remove(&build_id);
        }
    }

    fn register(&self, build_id: Uuid) -> watch::Sender<Outcome> {
        let (sender, receiver) = watch::channel(Outcome::Pending);
        self.outcomes.lock().insert(build_id, receiver);
        sender
    }

    pub fn follow_peers(&self, peer_parents: PeerParents) {
        let _ = self.peer_parents.set(peer_parents);
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
            None => self.wait_for_peer(build_id).await,
        }
    }

    async fn wait_for_peer(&self, build_id: Uuid) -> Outcome {
        let Some(peer_parents) = self.peer_parents.get().filter(|parents| parents.is_uploading(build_id)) else {
            return Outcome::Landed;
        };
        match peer_parents.wait_for_headers(build_id).await {
            Ok(()) => Outcome::Landed,
            Err(e) => {
                tracing::warn!(build = %build_id, error = %e, "peer upload of a parent did not land");
                Outcome::Failed
            }
        }
    }

    pub fn in_flight(&self) -> usize {
        self.tasks.len()
    }

    pub async fn drain(&self, forced: bool) {
        let in_flight = self.in_flight();
        if in_flight == 0 {
            return;
        }
        tracing::info!(uploads = in_flight, "waiting for in-flight snapshot uploads to finish");
        self.tasks.close();
        let finished = self.tasks.wait();
        tokio::pin!(finished);
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + DRAIN_LOG_INTERVAL, DRAIN_LOG_INTERVAL);
        loop {
            tokio::select! {
                () = &mut finished => {
                    tracing::info!("all in-flight snapshot uploads finished");
                    return;
                }
                () = std::future::ready(()), if forced => {
                    tracing::warn!(uploads = self.in_flight(), "shutting down with snapshot uploads still in flight");
                    return;
                }
                _ = ticks.tick() => {
                    tracing::info!(uploads = self.in_flight(), "still waiting for in-flight snapshot uploads");
                }
            }
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
        assert_eq!(uploads.outcome(id(2)), None);
        assert_eq!(uploads.wait(id(2)).await, Outcome::Landed);
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
    async fn landed_uploads_are_forgotten_and_failed_ones_are_kept() {
        let uploads = Arc::new(Uploads::default());
        uploads.start(id(1), Vec::new(), || async { Ok(()) }, |_| {});
        uploads.start(
            id(2),
            Vec::new(),
            || async { Err(Failure::Terminal("snapshot files are gone".to_owned())) },
            |_| {},
        );

        uploads.wait_all().await;

        assert_eq!(uploads.outcome(id(1)), None);
        assert_eq!(uploads.outcome(id(2)), Some(Outcome::Failed));
        assert_eq!(uploads.wait(id(1)).await, Outcome::Landed);
    }

    #[tokio::test(start_paused = true)]
    async fn in_flight_counts_the_uploads_that_have_not_ended() {
        let uploads = Arc::new(Uploads::default());
        uploads.start(id(1), Vec::new(), std::future::pending, |_| {});
        uploads.start(id(2), Vec::new(), || async { Ok(()) }, |_| {});

        tokio::time::sleep(Duration::from_secs(1)).await;

        assert_eq!(uploads.in_flight(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_forced_drain_returns_with_uploads_still_running() {
        let uploads = Arc::new(Uploads::default());
        uploads.start(id(1), Vec::new(), std::future::pending, |_| {});
        tokio::time::sleep(Duration::from_secs(1)).await;

        uploads.drain(true).await;

        assert_eq!(uploads.in_flight(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_graceful_drain_waits_for_the_uploads_to_end() {
        let uploads = Arc::new(Uploads::default());
        uploads.start(
            id(1),
            Vec::new(),
            || async {
                tokio::time::sleep(Duration::from_secs(25)).await;
                Ok(())
            },
            |_| {},
        );

        uploads.drain(false).await;

        assert_eq!(uploads.in_flight(), 0);
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

    #[tokio::test(start_paused = true)]
    async fn a_parent_that_a_peer_is_still_uploading_is_waited_for_in_storage() {
        use bytes::Bytes;

        use crate::featureflags::FeatureFlags;
        use crate::peer::switch::PeerSwitch;
        use crate::peer::testing::{FixedRegistry, local_storage};
        use crate::storage::paths::BuildPaths;

        let (_dir, storage) = local_storage();
        let flags = FeatureFlags::builder()
            .override_flag("peer-to-peer-chunk-transfer", true)
            .offline();
        let peers = PeerSwitch::new(flags);
        peers.install(
            Arc::new(FixedRegistry {
                lookup: Ok(Some("10.0.0.2:5008".to_owned())),
            }),
            "10.0.0.1:5008".to_owned(),
        );
        let parent = Uuid::new_v4();
        peers.resolve(&parent.to_string()).await;
        let uploads = Arc::new(Uploads::default());
        uploads.follow_peers(PeerParents {
            peers,
            storage: storage.clone(),
        });
        let waiting = {
            let uploads = uploads.clone();
            tokio::spawn(async move { uploads.wait(parent).await })
        };

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!waiting.is_finished(), "the headers are not in storage yet");
        let paths = BuildPaths::new(parent);
        for header in [paths.memfile_header(), paths.rootfs_header()] {
            storage.put(&header, Bytes::from_static(b"header")).await.unwrap();
        }

        assert_eq!(waiting.await.unwrap(), Outcome::Landed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_parent_nobody_is_uploading_has_landed() {
        let uploads = Arc::new(Uploads::default());

        assert_eq!(uploads.wait(id(5)).await, Outcome::Landed);
    }
}
