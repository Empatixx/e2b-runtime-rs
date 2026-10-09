use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio::time::Instant;
use uuid::Uuid;

use super::harvester::{HarvestJob, PrefetchHarvester, within};
use super::{HarvestOutcome, HarvestReport, HarvestRun, HarvestUpload, clamp_harvest_timeout_ms};
use crate::block::build_file::Ready;
use crate::memory::prefetch::flags::PrefetchFlags;
use crate::proto::SandboxConfig;
use crate::sandbox::config::Held;
use crate::storage::backend::ObjectMetadata;
use crate::template::metadata::TemplateMetadata;

pub struct PauseHarvest {
    pub config: SandboxConfig,
    pub build_id: Uuid,
    pub start: SystemTime,
    pub end: SystemTime,
    pub metadata: TemplateMetadata,
    pub upload: Arc<dyn HarvestUpload>,
    pub object_metadata: ObjectMetadata,
    pub memfile_ready: Ready,
    pub work: Option<Held>,
}

#[derive(Clone)]
pub struct PrefetchHarvest {
    harvester: Arc<PrefetchHarvester>,
}

impl PrefetchHarvest {
    pub fn new(harvester: Arc<PrefetchHarvester>) -> Self {
        Self { harvester }
    }

    pub fn after_pause(&self, flags: &dyn PrefetchFlags, pause: PauseHarvest) -> Option<JoinHandle<HarvestReport>> {
        if !flags.pause_resume_prefetch_harvest() {
            return None;
        }
        let timeout = harvest_timeout(flags.pause_resume_prefetch_harvest_timeout_ms());
        let consume = flags.pause_resume_prefetch_consume();
        let harvester = self.harvester.clone();
        Some(tokio::spawn(harvest(harvester, pause, timeout, consume)))
    }
}

fn harvest_timeout(timeout_ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(clamp_harvest_timeout_ms(timeout_ms)).unwrap_or_default())
}

async fn harvest(
    harvester: Arc<PrefetchHarvester>,
    mut pause: PauseHarvest,
    timeout: Duration,
    consume: bool,
) -> HarvestReport {
    let _work = pause.work.take();
    let deadline = Instant::now() + timeout;
    let sealed = within(deadline, pause.memfile_ready.clone()).await;
    let sandbox_id = pause.config.sandbox_id.clone();
    let build_id = pause.build_id;
    let report = match sealed {
        Ok(()) => harvester.run(&job_of(pause, consume, deadline)).await,
        Err(e) => HarvestReport::failed(
            HarvestRun::failed(HarvestOutcome::Skipped, Duration::ZERO),
            format!("waiting for rootfs seal: {e}"),
        ),
    };
    log_failure(&sandbox_id, build_id, &report);
    report
}

fn job_of(pause: PauseHarvest, consume: bool, deadline: Instant) -> HarvestJob {
    HarvestJob {
        config: pause.config,
        build_id: pause.build_id,
        start: pause.start,
        end: pause.end,
        metadata: pause.metadata,
        upload: pause.upload,
        object_metadata: pause.object_metadata,
        consume,
        deadline,
    }
}

fn log_failure(sandbox_id: &str, build_id: Uuid, report: &HarvestReport) {
    if let Some(error) = &report.error {
        tracing::warn!(sandbox = %sandbox_id, build = %build_id, %error, "pause-resume prefetch harvest failed");
    }
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    use super::super::harvester::tests::{Fakes, Script, harvester, trace};
    use super::*;
    use crate::block::build_file::ready_now;
    use crate::memory::prefetch::flags::fixed::FixedPrefetchFlags;
    use crate::template::metadata::TemplateMetadata;

    fn flags(harvest: bool, consume: bool, timeout_ms: i64) -> FixedPrefetchFlags {
        FixedPrefetchFlags {
            harvest,
            consume,
            harvest_timeout_ms: timeout_ms,
            ..FixedPrefetchFlags::default()
        }
    }

    fn pause(script: &Arc<Script>, memfile_ready: Ready) -> PauseHarvest {
        PauseHarvest {
            config: SandboxConfig {
                sandbox_id: "sbx".to_owned(),
                ..SandboxConfig::default()
            },
            build_id: Uuid::from_u128(7),
            start: SystemTime::UNIX_EPOCH,
            end: SystemTime::UNIX_EPOCH,
            metadata: TemplateMetadata::v1(),
            upload: Arc::new(Fakes(script.clone())),
            object_metadata: ObjectMetadata::default(),
            memfile_ready,
            work: None,
        }
    }

    fn scripted() -> Arc<Script> {
        let script = Arc::new(Script::default());
        *script.trace.lock() = Some(trace(2));
        script
    }

    fn harvesting(script: &Arc<Script>) -> PrefetchHarvest {
        PrefetchHarvest::new(Arc::new(harvester(script)))
    }

    #[tokio::test]
    async fn the_harvest_is_off_by_default() {
        let script = scripted();

        let started = harvesting(&script).after_pause(&FixedPrefetchFlags::default(), pause(&script, ready_now()));

        assert!(started.is_none());
        assert!(script.events().is_empty());
    }

    #[tokio::test]
    async fn an_enabled_harvest_runs_without_persisting_unless_consume_is_on() {
        let script = scripted();

        let report = harvesting(&script)
            .after_pause(&flags(true, false, 15_000), pause(&script, ready_now()))
            .unwrap()
            .await
            .unwrap();

        assert_eq!(report.error, None);
        assert_eq!(report.run.pages, 2);
        assert!(!script.events().contains(&"update metadata".to_owned()));
    }

    #[tokio::test]
    async fn consume_persists_the_mapping() {
        let script = scripted();

        harvesting(&script)
            .after_pause(&flags(true, true, 15_000), pause(&script, ready_now()))
            .unwrap()
            .await
            .unwrap();

        assert!(script.events().contains(&"upload metadata".to_owned()));
    }

    #[tokio::test]
    async fn a_failed_seal_skips_the_harvest() {
        let script = scripted();
        let failed = futures::future::ready(Err("disk full".to_owned())).boxed().shared();

        let report = harvesting(&script)
            .after_pause(&flags(true, true, 15_000), pause(&script, failed))
            .unwrap()
            .await
            .unwrap();

        assert_eq!(report.run.outcome, HarvestOutcome::Skipped);
        assert_eq!(report.error.as_deref(), Some("waiting for rootfs seal: disk full"));
        assert!(script.events().is_empty());
    }

    #[test]
    fn the_timeout_never_drops_below_one_second() {
        assert_eq!(harvest_timeout(0), Duration::from_secs(1));
        assert_eq!(harvest_timeout(-5), Duration::from_secs(1));
        assert_eq!(harvest_timeout(2500), Duration::from_millis(2500));
    }
}
