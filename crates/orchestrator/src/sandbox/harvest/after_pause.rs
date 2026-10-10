use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::Instrument;
use uuid::Uuid;

use super::harvester::{HarvestJob, PrefetchHarvester};
use super::{HarvestOutcome, HarvestReport, HarvestRun, HarvestUpload, clamp_harvest_timeout_ms};
use crate::memory::prefetch::flags::PrefetchFlags;
use crate::proto::SandboxConfig;
use crate::sandbox::config::Held;
use crate::storage::backend::ObjectMetadata;
use crate::telemetry::server::prefetch::PATH_PAUSE;
use crate::template::Template;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarvestSettings {
    pub timeout: Duration,
    pub consume: bool,
}

impl HarvestSettings {
    pub fn from_flags(flags: &dyn PrefetchFlags) -> Option<Self> {
        flags.pause_resume_prefetch_harvest().then(|| Self {
            timeout: harvest_timeout(flags.pause_resume_prefetch_harvest_timeout_ms()),
            consume: flags.pause_resume_prefetch_consume(),
        })
    }
}

pub struct PauseHarvest {
    pub config: SandboxConfig,
    pub build_id: Uuid,
    pub start: SystemTime,
    pub end: SystemTime,
    pub template: Arc<Template>,
    pub upload: Arc<dyn HarvestUpload>,
    pub object_metadata: ObjectMetadata,
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

    pub fn after_pause(&self, settings: HarvestSettings, pause: PauseHarvest) -> JoinHandle<HarvestReport> {
        let span = tracing::info_span!(
            parent: None,
            "harvest-resume-prefetch",
            build_id = %pause.build_id,
            consume = settings.consume,
            path = PATH_PAUSE
        );
        tokio::spawn(harvest(self.harvester.clone(), pause, settings).instrument(span))
    }
}

fn harvest_timeout(timeout_ms: i64) -> Duration {
    Duration::from_millis(u64::try_from(clamp_harvest_timeout_ms(timeout_ms)).unwrap_or_default())
}

async fn harvest(
    harvester: Arc<PrefetchHarvester>,
    mut pause: PauseHarvest,
    settings: HarvestSettings,
) -> HarvestReport {
    let _work = pause.work.take();
    let deadline = Instant::now() + settings.timeout;
    let sandbox_id = pause.config.sandbox_id.clone();
    let team_id = pause.config.team_id.clone();
    let build_id = pause.build_id;
    let report = match job_of(pause, settings.consume, deadline).await {
        Ok(job) => harvester.run(&job).await,
        Err(e) => HarvestReport::failed(HarvestRun::failed(HarvestOutcome::Skipped, Duration::ZERO), e),
    };
    report.run.record_metrics(&team_id, Duration::ZERO);
    log_failure(&sandbox_id, build_id, &report);
    report
}

async fn job_of(pause: PauseHarvest, consume: bool, deadline: Instant) -> Result<HarvestJob, String> {
    let metadata = pause
        .template
        .metadata()
        .await
        .map_err(|e| format!("get template metadata: {e}"))?;
    Ok(HarvestJob {
        config: pause.config,
        build_id: pause.build_id,
        start: pause.start,
        end: pause.end,
        metadata,
        upload: pause.upload,
        object_metadata: pause.object_metadata,
        consume,
        deadline,
    })
}

fn log_failure(sandbox_id: &str, build_id: Uuid, report: &HarvestReport) {
    if let Some(error) = &report.error {
        tracing::warn!(sandbox = %sandbox_id, build = %build_id, path = PATH_PAUSE, %error, "resume prefetch harvest failed");
    }
}

#[cfg(test)]
mod tests {
    use super::super::harvester::tests::{Fakes, Script, harvester, trace};
    use super::*;
    use crate::memory::prefetch::flags::fixed::FixedPrefetchFlags;
    use crate::storage::backend::Storage;

    fn flags(harvest: bool, consume: bool, timeout_ms: i64) -> FixedPrefetchFlags {
        FixedPrefetchFlags {
            harvest,
            consume,
            harvest_timeout_ms: timeout_ms,
            ..FixedPrefetchFlags::default()
        }
    }

    fn pause(script: &Arc<Script>) -> PauseHarvest {
        let dir = tempfile::tempdir().unwrap().keep();
        let template = Template::new(Storage::local(&dir).unwrap(), Uuid::from_u128(7), dir);
        PauseHarvest {
            config: SandboxConfig {
                sandbox_id: "sbx".to_owned(),
                ..SandboxConfig::default()
            },
            build_id: Uuid::from_u128(7),
            start: SystemTime::UNIX_EPOCH,
            end: SystemTime::UNIX_EPOCH,
            template: Arc::new(template),
            upload: Arc::new(Fakes(script.clone())),
            object_metadata: ObjectMetadata::default(),
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

    async fn harvested(script: &Arc<Script>, flags: FixedPrefetchFlags) -> HarvestReport {
        let settings = HarvestSettings::from_flags(&flags).unwrap();
        harvesting(script).after_pause(settings, pause(script)).await.unwrap()
    }

    #[test]
    fn the_harvest_is_off_by_default() {
        assert_eq!(HarvestSettings::from_flags(&FixedPrefetchFlags::default()), None);
    }

    #[test]
    fn the_settings_carry_the_clamped_timeout_and_the_consume_gate() {
        assert_eq!(
            HarvestSettings::from_flags(&flags(true, true, 0)),
            Some(HarvestSettings {
                timeout: Duration::from_secs(1),
                consume: true
            })
        );
        assert_eq!(
            HarvestSettings::from_flags(&flags(true, false, 2500)).map(|settings| settings.timeout),
            Some(Duration::from_millis(2500))
        );
    }

    #[tokio::test]
    async fn an_enabled_harvest_runs_without_persisting_unless_consume_is_on() {
        let script = scripted();

        let report = harvested(&script, flags(true, false, 15_000)).await;

        assert_eq!(report.error, None);
        assert_eq!(report.run.pages, 2);
        assert!(!script.events().contains(&"update metadata".to_owned()));
    }

    #[tokio::test]
    async fn consume_persists_the_mapping() {
        let script = scripted();

        harvested(&script, flags(true, true, 15_000)).await;

        assert!(script.events().contains(&"upload metadata".to_owned()));
    }

    #[test]
    fn the_timeout_never_drops_below_one_second() {
        assert_eq!(harvest_timeout(0), Duration::from_secs(1));
        assert_eq!(harvest_timeout(-5), Duration::from_secs(1));
        assert_eq!(harvest_timeout(2500), Duration::from_millis(2500));
    }
}
