use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tonic::transport::{Channel, Endpoint};

use super::registry::PeerRegistry;
use crate::proto::chunk_service_client::ChunkServiceClient;

const PEER_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub struct PeerRoute {
    pub client: ChunkServiceClient<Channel>,
    pub uploaded: Arc<AtomicBool>,
    pub address: String,
}

pub enum Resolution {
    Peer(Box<PeerRoute>),
    Base(BaseReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseReason {
    RedisError,
    NoPeer,
    SelfAddress,
    DialError,
    Uploaded,
}

impl BaseReason {
    pub fn label(self) -> &'static str {
        match self {
            BaseReason::RedisError => "redis_error",
            BaseReason::NoPeer => "no_peer",
            BaseReason::SelfAddress => "self",
            BaseReason::DialError => "dial_error",
            BaseReason::Uploaded => "uploaded",
        }
    }
}

pub struct Resolver {
    registry: Arc<dyn PeerRegistry>,
    self_address: String,
    channels: Mutex<HashMap<String, Channel>>,
    uploaded_builds: Mutex<HashMap<String, Arc<AtomicBool>>>,
}

impl Resolver {
    pub fn new(registry: Arc<dyn PeerRegistry>, self_address: String) -> Self {
        Self {
            registry,
            self_address,
            channels: Mutex::default(),
            uploaded_builds: Mutex::default(),
        }
    }

    pub async fn resolve(&self, build_id: &str) -> Resolution {
        if self.switched_to_storage(build_id) {
            return Resolution::Base(BaseReason::Uploaded);
        }
        match self.registry.lookup(build_id).await {
            Err(_) => Resolution::Base(BaseReason::RedisError),
            Ok(None) => Resolution::Base(BaseReason::NoPeer),
            Ok(Some(address)) if address == self.self_address => Resolution::Base(BaseReason::SelfAddress),
            Ok(Some(address)) => self.route_to(build_id, address),
        }
    }

    pub fn is_active(&self, build_id: &str) -> bool {
        self.uploaded_builds
            .lock()
            .get(build_id)
            .is_some_and(|uploaded| !uploaded.load(Ordering::SeqCst))
    }

    pub fn purge(&self, build_id: &str) {
        self.uploaded_builds.lock().remove(build_id);
    }

    pub fn close(&self) {
        self.channels.lock().clear();
    }

    fn switched_to_storage(&self, build_id: &str) -> bool {
        self.uploaded_builds
            .lock()
            .get(build_id)
            .is_some_and(|uploaded| uploaded.load(Ordering::SeqCst))
    }

    fn route_to(&self, build_id: &str, address: String) -> Resolution {
        match self.channel(&address) {
            Err(_) => Resolution::Base(BaseReason::DialError),
            Ok(channel) => {
                let uploaded = self.uploaded_flag(build_id);
                match uploaded.load(Ordering::SeqCst) {
                    true => Resolution::Base(BaseReason::Uploaded),
                    false => Resolution::Peer(Box::new(PeerRoute {
                        client: ChunkServiceClient::new(channel),
                        uploaded,
                        address,
                    })),
                }
            }
        }
    }

    fn uploaded_flag(&self, build_id: &str) -> Arc<AtomicBool> {
        self.uploaded_builds
            .lock()
            .entry(build_id.to_owned())
            .or_default()
            .clone()
    }

    fn channel(&self, address: &str) -> Result<Channel, String> {
        let mut channels = self.channels.lock();
        if let Some(channel) = channels.get(address) {
            return Ok(channel.clone());
        }
        let channel = Endpoint::from_shared(format!("http://{address}"))
            .map(|endpoint| endpoint.connect_timeout(PEER_CONNECT_TIMEOUT).connect_lazy())
            .map_err(|e| format!("failed to dial peer {address}: {e}"))?;
        channels.insert(address.to_owned(), channel.clone());
        Ok(channel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::testing::FixedRegistry;

    fn resolver(lookup: Result<Option<String>, String>) -> Resolver {
        Resolver::new(Arc::new(FixedRegistry { lookup }), "10.0.0.1:5008".to_owned())
    }

    async fn reason(resolver: &Resolver) -> Option<BaseReason> {
        match resolver.resolve("build-1").await {
            Resolution::Base(reason) => Some(reason),
            Resolution::Peer(_) => None,
        }
    }

    #[tokio::test]
    async fn a_registry_failure_resolves_to_the_base_with_a_redis_error() {
        let resolver = resolver(Err("redis is down".to_owned()));

        assert_eq!(reason(&resolver).await, Some(BaseReason::RedisError));
        assert!(!resolver.is_active("build-1"));
    }

    #[tokio::test]
    async fn a_build_without_a_peer_resolves_to_the_base() {
        let resolver = resolver(Ok(None));

        assert_eq!(reason(&resolver).await, Some(BaseReason::NoPeer));
        assert!(!resolver.is_active("build-1"));
    }

    #[tokio::test]
    async fn this_node_never_routes_to_itself() {
        let resolver = resolver(Ok(Some("10.0.0.1:5008".to_owned())));

        assert_eq!(reason(&resolver).await, Some(BaseReason::SelfAddress));
    }

    #[tokio::test]
    async fn another_node_is_a_peer_and_makes_the_build_active() {
        let resolver = resolver(Ok(Some("10.0.0.2:5008".to_owned())));

        assert_eq!(reason(&resolver).await, None);
        assert!(resolver.is_active("build-1"));
    }

    #[tokio::test]
    async fn a_build_that_switched_to_storage_resolves_to_the_base_without_asking_the_registry() {
        let resolver = resolver(Ok(Some("10.0.0.2:5008".to_owned())));
        let Resolution::Peer(route) = resolver.resolve("build-1").await else {
            panic!("expected a peer");
        };

        route.uploaded.store(true, Ordering::SeqCst);

        assert_eq!(reason(&resolver).await, Some(BaseReason::Uploaded));
        assert!(!resolver.is_active("build-1"));
    }

    #[tokio::test]
    async fn purging_a_build_forgets_that_it_was_peer_served() {
        let resolver = resolver(Ok(Some("10.0.0.2:5008".to_owned())));
        reason(&resolver).await;

        resolver.purge("build-1");

        assert!(!resolver.is_active("build-1"));
    }

    #[tokio::test]
    async fn an_address_that_is_not_a_uri_resolves_to_the_base_with_a_dial_error() {
        let resolver = resolver(Ok(Some("not a host".to_owned())));

        assert_eq!(reason(&resolver).await, Some(BaseReason::DialError));
    }
}
