use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::Mutex;
use uuid::Uuid;

use super::registry::PeerRegistry;
use super::resolver::{BaseReason, Resolution, Resolver};
use crate::featureflags::{FeatureFlags, PEER_TO_PEER_CHUNK_TRANSFER};
use crate::server::retry::TOTAL_BUDGET;

const UPLOADED_BUILDS_TTL: Duration = Duration::from_secs(60 * 60);
const REGISTRY_KEY_GRACE: Duration = Duration::from_secs(2 * 60);

struct Active {
    registry: Arc<dyn PeerRegistry>,
    resolver: Resolver,
}

pub struct PeerSwitch {
    flags: FeatureFlags,
    active: OnceLock<Active>,
    registered: Mutex<HashSet<Uuid>>,
    uploaded_builds: moka::sync::Cache<String, ()>,
}

impl PeerSwitch {
    pub fn new(flags: FeatureFlags) -> Arc<Self> {
        Arc::new(Self {
            flags,
            active: OnceLock::new(),
            registered: Mutex::default(),
            uploaded_builds: moka::sync::Cache::builder().time_to_live(UPLOADED_BUILDS_TTL).build(),
        })
    }

    pub fn install(&self, registry: Arc<dyn PeerRegistry>, self_address: String) {
        let resolver = Resolver::new(registry.clone(), self_address);
        let _ = self.active.set(Active { registry, resolver });
    }

    pub fn enabled(&self) -> bool {
        self.active.get().is_some() && self.flags.get(&PEER_TO_PEER_CHUNK_TRANSFER, &[])
    }

    pub async fn resolve(&self, build_id: &str) -> Resolution {
        match self.active.get() {
            Some(active) => active.resolver.resolve(build_id).await,
            None => Resolution::Base(BaseReason::NoPeer),
        }
    }

    pub fn is_active(&self, build_id: &str) -> bool {
        self.active
            .get()
            .is_some_and(|active| active.resolver.is_active(build_id))
    }

    pub fn purge(&self, build_id: &str) {
        self.active
            .get()
            .iter()
            .for_each(|active| active.resolver.purge(build_id));
    }

    pub fn close(&self) {
        self.active.get().iter().for_each(|active| active.resolver.close());
    }

    pub fn uploaded(&self, build_id: &str) -> bool {
        self.uploaded_builds.contains_key(build_id)
    }

    pub async fn register(&self, build_id: Uuid) {
        let Some(active) = self.active.get().filter(|_| self.enabled()) else {
            return;
        };
        self.registered.lock().insert(build_id);
        if let Err(e) = active
            .registry
            .register(&build_id.to_string(), TOTAL_BUDGET + REGISTRY_KEY_GRACE)
            .await
        {
            tracing::warn!(build_id = %build_id, error = %e, "failed to register peer address for routing");
        }
    }

    pub async fn finish(&self, build_id: Uuid, landed: bool) {
        if !self.registered.lock().remove(&build_id) {
            return;
        }
        if landed {
            self.uploaded_builds.insert(build_id.to_string(), ());
        }
        let Some(active) = self.active.get() else { return };
        if let Err(e) = active.registry.unregister(&build_id.to_string()).await {
            tracing::warn!(build_id = %build_id, error = %e, "failed to unregister peer address from routing");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::testing::RecordingRegistry;

    fn switch(flag_on: bool) -> (Arc<PeerSwitch>, Arc<RecordingRegistry>) {
        let flags = FeatureFlags::builder()
            .override_flag("peer-to-peer-chunk-transfer", flag_on)
            .offline();
        let switch = PeerSwitch::new(flags);
        let registry = Arc::new(RecordingRegistry::default());
        switch.install(registry.clone(), "10.0.0.1:5008".to_owned());
        (switch, registry)
    }

    #[test]
    fn a_switch_without_a_registry_is_disabled_whatever_the_flag_says() {
        let flags = FeatureFlags::builder()
            .override_flag("peer-to-peer-chunk-transfer", true)
            .offline();

        assert!(!PeerSwitch::new(flags).enabled());
    }

    #[test]
    fn the_flag_decides_once_a_registry_is_installed() {
        assert!(switch(true).0.enabled());
        assert!(!switch(false).0.enabled());
    }

    #[tokio::test]
    async fn register_advertises_the_build_for_the_whole_retry_window_plus_two_minutes() {
        let (switch, registry) = switch(true);
        let build_id = Uuid::from_u128(1);

        switch.register(build_id).await;

        assert_eq!(*registry.calls.lock(), vec![format!("register {build_id} 7320s")]);
    }

    #[tokio::test]
    async fn register_does_nothing_while_the_flag_is_off() {
        let (switch, registry) = switch(false);
        let build_id = Uuid::from_u128(1);

        switch.register(build_id).await;
        switch.finish(build_id, true).await;

        assert!(registry.calls.lock().is_empty());
        assert!(!switch.uploaded(&build_id.to_string()));
    }

    #[tokio::test]
    async fn a_landed_upload_unregisters_and_marks_the_build_uploaded() {
        let (switch, registry) = switch(true);
        let build_id = Uuid::from_u128(1);
        switch.register(build_id).await;

        switch.finish(build_id, true).await;

        assert_eq!(registry.calls.lock().last().unwrap(), &format!("unregister {build_id}"));
        assert!(switch.uploaded(&build_id.to_string()));
    }

    #[tokio::test]
    async fn a_failed_upload_unregisters_without_marking_the_build_uploaded() {
        let (switch, registry) = switch(true);
        let build_id = Uuid::from_u128(1);
        switch.register(build_id).await;

        switch.finish(build_id, false).await;

        assert_eq!(registry.calls.lock().last().unwrap(), &format!("unregister {build_id}"));
        assert!(!switch.uploaded(&build_id.to_string()));
    }

    #[tokio::test]
    async fn finishing_a_build_that_was_never_registered_does_nothing() {
        let (switch, registry) = switch(true);
        let build_id = Uuid::from_u128(1);

        switch.finish(build_id, true).await;

        assert!(registry.calls.lock().is_empty());
        assert!(!switch.uploaded(&build_id.to_string()));
    }

    #[tokio::test]
    async fn finishing_twice_unregisters_once() {
        let (switch, registry) = switch(true);
        let build_id = Uuid::from_u128(1);
        switch.register(build_id).await;
        switch.finish(build_id, true).await;
        let calls = registry.calls.lock().len();

        switch.finish(build_id, true).await;

        assert_eq!(registry.calls.lock().len(), calls);
    }
}
