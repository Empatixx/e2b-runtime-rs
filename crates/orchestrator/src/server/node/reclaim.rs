use tracing::{Instrument, info_span};

use super::Node;
use super::prefetch::pause_contexts;
use crate::envd::reclaim::ReclaimSettings;
use crate::featureflags::{FREE_PAGE_HINTING_CONFIG, FlagContext};
use crate::firecracker::version::supports_free_page_hinting;
use crate::proto::SandboxConfig;
use crate::sandbox::Sandbox;
use crate::sandbox::hinting::PrePauseHint;

pub struct PausePreparation<'a> {
    pub contexts: &'a [FlagContext],
    pub filesystem_only: bool,
    pub use_case: &'static str,
    pub firecracker_version: &'a str,
}

pub fn reclaim_contexts(config: &SandboxConfig) -> Vec<FlagContext> {
    pause_contexts(config)
        .into_iter()
        .chain([FlagContext::template(&config.template_id)])
        .collect()
}

impl Node {
    pub async fn prepare_for_pause(&self, sandbox: &Sandbox, preparation: PausePreparation<'_>) {
        let settings = ReclaimSettings::resolve(&self.feature_flags, preparation.contexts);
        sandbox
            .guest_reclaim()
            .before_pause(&settings, preparation.filesystem_only)
            .instrument(info_span!("envd-reclaim"))
            .await;
        if preparation.filesystem_only {
            return;
        }
        let hint = PrePauseHint::from_flag(
            &self.feature_flags.get(&FREE_PAGE_HINTING_CONFIG, preparation.contexts),
            preparation.use_case,
            supports_free_page_hinting(preparation.firecracker_version),
        );
        sandbox
            .pre_pause_hint_drain(hint)
            .instrument(info_span!("drain-balloon"))
            .await;
    }
}
