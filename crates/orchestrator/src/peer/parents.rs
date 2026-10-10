use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::time::Instant;
use uuid::Uuid;

use super::switch::PeerSwitch;
use super::upload_done::{Outcome, UploadDone};
use crate::storage::backend::Storage;
use crate::storage::paths::BuildPaths;
use crate::telemetry::header_poll::record_header_poll_wait;

const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const MAX_TRANSIENT_ERRORS: u32 = 3;
const BUDGET: Duration = Duration::from_secs(2 * 60 * 60);

pub struct PeerParents {
    pub peers: Arc<PeerSwitch>,
    pub storage: Storage,
}

type Hints<'a> = Option<&'a Arc<dyn UploadDone>>;

impl PeerParents {
    pub fn is_uploading(&self, build_id: Uuid) -> bool {
        self.peers.is_active(&build_id.to_string())
    }

    pub async fn wait_for_headers(&self, build_id: Uuid, hints: Hints<'_>) -> Result<(), String> {
        let paths = BuildPaths::new(build_id);
        let (memfile, rootfs) = (paths.memfile_header(), paths.rootfs_header());
        tokio::try_join!(
            self.wait_for_header(build_id, "memfile", &memfile, hints),
            self.wait_for_header(build_id, "rootfs", &rootfs, hints)
        )
        .map(|_| ())
    }

    async fn wait_for_header(
        &self,
        build_id: Uuid,
        file_type: &str,
        path: &str,
        hints: Hints<'_>,
    ) -> Result<(), String> {
        let started = Instant::now();
        let waited = self.poll(build_id, file_type, path, hints).await;
        let result = match &waited {
            Ok(()) => "ok",
            Err(Failure::Transient(_)) => "transient_errors",
            Err(Failure::Deadline(_)) => "deadline_exceeded",
            Err(Failure::UploadFailed(_)) => "upload_failed",
        };
        record_header_poll_wait(file_type, result, started.elapsed());
        waited.map_err(Failure::into_message)
    }

    async fn poll(&self, build_id: Uuid, file_type: &str, path: &str, hints: Hints<'_>) -> Result<(), Failure> {
        let mut hint = match hints {
            Some(hints) => hints.subscribe(build_id).await,
            None => None,
        };
        let deadline = Instant::now() + BUDGET;
        let mut backoff = INITIAL_BACKOFF;
        let mut transient_errors = 0;
        loop {
            match self.storage.exists_direct(path).await {
                Ok(true) => return Ok(()),
                Ok(false) => transient_errors = 0,
                Err(e) => {
                    transient_errors += 1;
                    if transient_errors >= MAX_TRANSIENT_ERRORS {
                        return Err(Failure::Transient(format!(
                            "load header {path} after {transient_errors} attempts: {e}"
                        )));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(Failure::Deadline(format!("header {path} not visible after {BUDGET:?}")));
            }
            tokio::select! {
                signalled = hinted(&mut hint) => match signalled {
                    Some(error) => return Err(Failure::UploadFailed(format!(
                        "upload signaled failure for {build_id}/{file_type}: {error}"
                    ))),
                    None => backoff = INITIAL_BACKOFF,
                },
                () = tokio::time::sleep(backoff) => backoff = (backoff * 2).min(MAX_BACKOFF),
            }
        }
    }
}

enum Failure {
    Transient(String),
    Deadline(String),
    UploadFailed(String),
}

impl Failure {
    fn into_message(self) -> String {
        match self {
            Failure::Transient(message) | Failure::Deadline(message) | Failure::UploadFailed(message) => message,
        }
    }
}

async fn hinted(hint: &mut Option<oneshot::Receiver<Outcome>>) -> Option<String> {
    let Some(receiver) = hint.as_mut() else {
        return std::future::pending().await;
    };
    match receiver.await {
        Ok(outcome) => {
            *hint = None;
            outcome
        }
        Err(_) => {
            *hint = None;
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::featureflags::FeatureFlags;
    use crate::peer::testing::{FixedRegistry, local_storage};

    fn parents(storage: Storage) -> PeerParents {
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
        PeerParents { peers, storage }
    }

    async fn put_headers(storage: &Storage, build_id: Uuid) {
        let paths = BuildPaths::new(build_id);
        for path in [paths.memfile_header(), paths.rootfs_header()] {
            storage.put(&path, Bytes::from_static(b"header")).await.unwrap();
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_wait_ends_when_both_headers_are_visible_in_storage() {
        let (_dir, storage) = local_storage();
        let build_id = Uuid::new_v4();
        let parents = parents(storage.clone());
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(3)).await;
            put_headers(&storage, build_id).await;
        });

        let waited = parents.wait_for_headers(build_id, None).await;

        writer.await.unwrap();
        assert_eq!(waited, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_header_that_never_appears_fails_the_wait_after_the_budget() {
        let (_dir, storage) = local_storage();

        let waited = parents(storage).wait_for_headers(Uuid::new_v4(), None).await;

        let error = waited.unwrap_err();
        assert!(
            error.starts_with("header ") && error.ends_with(" not visible after 7200s"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_build_is_uploading_while_this_node_reads_it_from_a_peer() {
        let (_dir, storage) = local_storage();
        let build_id = Uuid::new_v4();
        let parents = parents(storage);

        assert!(!parents.is_uploading(build_id));
        parents.peers.resolve(&build_id.to_string()).await;

        assert!(parents.is_uploading(build_id));
    }

    fn hints(channels: &Arc<crate::peer::upload_done::testing::Channels>) -> Arc<dyn UploadDone> {
        Arc::new(channels.clone())
    }

    #[tokio::test(start_paused = true)]
    async fn an_upload_failure_signal_ends_the_wait_at_once() {
        let (_dir, storage) = local_storage();
        let build_id = Uuid::new_v4();
        let channels = Arc::new(crate::peer::upload_done::testing::Channels::default());
        let hints = hints(&channels);
        let parents = parents(storage);
        let signaller = tokio::spawn({
            let channels = channels.clone();
            async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                channels.fire(build_id, Some("disk full".to_owned()));
            }
        });

        let waited = parents.wait_for_headers(build_id, Some(&hints)).await;

        signaller.await.unwrap();
        let error = waited.unwrap_err();
        assert!(
            error.starts_with("upload signaled failure for ") && error.ends_with(": disk full"),
            "{error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_success_signal_polls_storage_without_waiting_out_the_backoff() {
        let (_dir, storage) = local_storage();
        let build_id = Uuid::new_v4();
        let channels = Arc::new(crate::peer::upload_done::testing::Channels::default());
        let hints = hints(&channels);
        let parents = parents(storage.clone());
        let started = Instant::now();
        let writer = tokio::spawn({
            let channels = channels.clone();
            async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                put_headers(&storage, build_id).await;
                channels.fire(build_id, None);
            }
        });

        let waited = parents.wait_for_headers(build_id, Some(&hints)).await;

        writer.await.unwrap();
        assert_eq!(waited, Ok(()));
        assert!(
            started.elapsed() < Duration::from_secs(66),
            "the hint should have cut the final backoff of up to 5 s"
        );
    }
}
