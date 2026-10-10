use tracing::{Instrument, info_span};

use super::Node;
use super::prefetch::pause_contexts;
use crate::envd::reclaim::ReclaimSettings;
use crate::featureflags::FlagContext;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;

pub fn reclaim_contexts(config: &SandboxConfig) -> Vec<FlagContext> {
    pause_contexts(config)
        .into_iter()
        .chain([FlagContext::template(&config.template_id)])
        .collect()
}

impl Node {
    pub async fn reclaim_before_pause(&self, sandbox: &Sandbox, contexts: &[FlagContext], skip_heap_collapse: bool) {
        let settings = ReclaimSettings::resolve(&self.feature_flags, contexts);
        sandbox
            .guest_reclaim()
            .before_pause(&settings, skip_heap_collapse)
            .instrument(info_span!("envd-reclaim"))
            .await;
    }
}
