use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tonic::transport::Channel;
use tracing::Instrument;

use crate::proto::chunk_service_client::ChunkServiceClient;

pub type PeerReply<T> = Result<Option<T>, String>;

#[derive(Clone, bon::Builder)]
pub struct PeerHandle {
    pub client: ChunkServiceClient<Channel>,
    #[builder(into)]
    pub build_id: String,
    #[builder(into)]
    pub name: String,
    pub uploaded: Arc<AtomicBool>,
}

impl PeerHandle {
    pub fn switched_to_storage(&self) -> bool {
        self.uploaded.load(Ordering::SeqCst)
    }

    pub async fn try_peer<T, Fut>(&self, operation: &'static str, attempt: impl FnOnce() -> Fut) -> PeerReply<T>
    where
        Fut: Future<Output = PeerReply<T>>,
    {
        if self.switched_to_storage() {
            return Ok(None);
        }
        let span = tracing::info_span!("peer", operation, file_name = %self.name, peer_hit = tracing::field::Empty);
        let reply = attempt().instrument(span.clone()).await;
        span.record("peer_hit", matches!(reply, Ok(Some(_)) | Err(_)));
        reply
    }
}
