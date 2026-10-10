use std::sync::Arc;
use std::time::SystemTime;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::SandboxInfo;
use crate::featureflags::FREE_PAGE_HINTING_CONFIG;
use crate::firecracker::version::supports_free_page_hinting;
use crate::sandbox::Sandbox;
use crate::sandbox::hinting::HintTelemetry;
use crate::sandbox::hinting::periodic::{CheckpointMarks, ConfigSource, HintGates, PeriodicConfig, PeriodicHinting};
use crate::server::node::Node;
use crate::server::node::reclaim::reclaim_contexts;

pub struct HintLoop {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl HintLoop {
    pub fn start(
        sandbox: &Sandbox,
        node: &Node,
        info: &SandboxInfo,
        checkpoints: Arc<CheckpointMarks>,
    ) -> Option<Self> {
        let config = config_source(node, info);
        if !node.feature_flags.live() && !config().enabled() {
            return None;
        }
        let supported = supports_free_page_hinting(&info.config.firecracker_version);
        let cancel = CancellationToken::new();
        let hinting = PeriodicHinting {
            runner: sandbox.hint_runner(supported)?,
            hinting: sandbox.hinting().clone(),
            signals: sandbox.hint_signals(started_instant(info.started_at), checkpoints),
            gates: HintGates::production(supported),
            config,
            record: Arc::new(HintTelemetry),
            sandbox_id: sandbox.id().to_owned(),
            cancel: cancel.clone(),
            unresponsive: Default::default(),
            warned: Default::default(),
        };
        let task = tokio::spawn(async move { hinting.run().await });
        Some(Self { cancel, task })
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

fn config_source(node: &Node, info: &SandboxInfo) -> ConfigSource {
    let flags = node.feature_flags.clone();
    let contexts = reclaim_contexts(&info.config);
    Box::new(move || PeriodicConfig::from_flag(&flags.get(&FREE_PAGE_HINTING_CONFIG, &contexts)))
}

fn started_instant(started_at: SystemTime) -> Instant {
    let now = Instant::now();
    SystemTime::now()
        .duration_since(started_at)
        .ok()
        .and_then(|ago| now.checked_sub(ago))
        .unwrap_or(now)
}
