use std::sync::Arc;

use super::{SandboxLookup, SandboxRoute};
use crate::server::lifecycle::SandboxInfo;
use crate::server::node::Node;

impl SandboxRoute {
    pub fn of(info: &SandboxInfo) -> Self {
        Self {
            sandbox_id: info.config.sandbox_id.clone(),
            lifecycle_id: info.lifecycle.lifecycle_id.clone(),
            host_ip: info.host_ip,
            ingress: info.config.network.as_ref().and_then(|network| network.ingress.clone()),
        }
    }
}

pub fn lookup_in(node: Arc<Node>) -> SandboxLookup {
    Arc::new(move |sandbox_id| {
        node.registry
            .get(sandbox_id)
            .map(|handle| SandboxRoute::of(&handle.info.lock()))
    })
}
