use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;
use uuid::Uuid;

use super::switch::PeerSwitch;
use crate::storage::backend::Storage;
use crate::storage::paths::BuildPaths;

const INITIAL_BACKOFF: Duration = Duration::from_millis(100);
const MAX_BACKOFF: Duration = Duration::from_secs(5);
const MAX_TRANSIENT_ERRORS: u32 = 3;
const BUDGET: Duration = Duration::from_secs(2 * 60 * 60);

pub struct PeerParents {
    pub peers: Arc<PeerSwitch>,
    pub storage: Storage,
}

impl PeerParents {
    pub fn is_uploading(&self, build_id: Uuid) -> bool {
        self.peers.is_active(&build_id.to_string())
    }

    pub async fn wait_for_headers(&self, build_id: Uuid) -> Result<(), String> {
        let paths = BuildPaths::new(build_id);
        let (memfile, rootfs) = (paths.memfile_header(), paths.rootfs_header());
        tokio::try_join!(self.wait_for_header(&memfile), self.wait_for_header(&rootfs)).map(|_| ())
    }

    async fn wait_for_header(&self, path: &str) -> Result<(), String> {
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
                        return Err(format!("load header {path} after {transient_errors} attempts: {e}"));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err(format!("header {path} not visible after {BUDGET:?}"));
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(MAX_BACKOFF);
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

        let waited = parents.wait_for_headers(build_id).await;

        writer.await.unwrap();
        assert_eq!(waited, Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn a_header_that_never_appears_fails_the_wait_after_the_budget() {
        let (_dir, storage) = local_storage();

        let waited = parents(storage).wait_for_headers(Uuid::new_v4()).await;

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
}
