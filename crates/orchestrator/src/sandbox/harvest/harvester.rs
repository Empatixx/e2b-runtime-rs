use std::future::Future;
use std::sync::Arc;
use std::time::SystemTime;

use tokio::time::{Duration, Instant, timeout_at};
use uuid::Uuid;

use super::{
    DEADLINE_EXCEEDED, HARVEST_REAP_TIMEOUT, HarvestFailure, HarvestInstance, HarvestOutcome, HarvestReport,
    HarvestResumer, HarvestRun, HarvestSlots, HarvestTemplates, HarvestUpload, MetadataUploader, MetadataWriteError,
    START_SLOTS_EXHAUSTED, THROWAWAY_ID_PREFIX, ThrowawayResume, UPLOAD_TOTAL_BUDGET,
};
use crate::proto::SandboxConfig;
use crate::storage::backend::ObjectMetadata;
use crate::template::metadata::{MemoryPrefetch, TemplateMetadata};

pub struct HarvestJob {
    pub config: SandboxConfig,
    pub build_id: Uuid,
    pub start: SystemTime,
    pub end: SystemTime,
    pub metadata: TemplateMetadata,
    pub upload: Arc<dyn HarvestUpload>,
    pub object_metadata: ObjectMetadata,
    pub consume: bool,
    pub deadline: Instant,
}

pub struct PrefetchHarvester {
    pub resumer: Arc<dyn HarvestResumer>,
    pub templates: Arc<dyn HarvestTemplates>,
    pub slots: Arc<dyn HarvestSlots>,
    pub uploader: Arc<dyn MetadataUploader>,
    pub persist_budget: Duration,
}

impl PrefetchHarvester {
    pub fn new(
        resumer: Arc<dyn HarvestResumer>,
        templates: Arc<dyn HarvestTemplates>,
        slots: Arc<dyn HarvestSlots>,
        uploader: Arc<dyn MetadataUploader>,
    ) -> Self {
        Self {
            resumer,
            templates,
            slots,
            uploader,
            persist_budget: UPLOAD_TOTAL_BUDGET,
        }
    }

    pub async fn run(&self, job: &HarvestJob) -> HarvestReport {
        let slot_hold_start = Instant::now();
        let mapping = self.resume_mapping(job).await;
        let slot_hold = slot_hold_start.elapsed();
        match mapping {
            Err(failure) => HarvestReport::failed(HarvestRun::failed(failure.outcome, slot_hold), failure.message),
            Ok(mapping) => self.settle(job, mapping, slot_hold).await,
        }
    }

    async fn settle(&self, job: &HarvestJob, mapping: Option<MemoryPrefetch>, slot_hold: Duration) -> HarvestReport {
        let pages = mapping.as_ref().map_or(0, |mapping| mapping.indices.len());
        let run = HarvestRun::succeeded(pages, slot_hold);
        match mapping.filter(|_| job.consume) {
            Some(mapping) => self.persist(job, mapping, run).await,
            None => HarvestReport::ok(run),
        }
    }

    async fn persist(&self, job: &HarvestJob, mapping: MemoryPrefetch, run: HarvestRun) -> HarvestReport {
        let deadline = Instant::now() + self.persist_budget;
        let wait_start = Instant::now();
        let uploaded = within(deadline, job.upload.wait()).await;
        let run = HarvestRun {
            persist_wait: wait_start.elapsed(),
            persist_attempted: true,
            ..run
        };
        if Instant::now() >= deadline {
            return HarvestReport::failed(
                HarvestRun {
                    outcome: HarvestOutcome::PersistDeadline,
                    ..run
                },
                format!("waiting for snapshot upload: {DEADLINE_EXCEEDED}"),
            );
        }
        match self.write_metadata(job, mapping, deadline, uploaded.is_ok()).await {
            Ok(()) => HarvestReport::ok(run),
            Err(error) => HarvestReport::failed(run, error),
        }
    }

    async fn write_metadata(
        &self,
        job: &HarvestJob,
        mapping: MemoryPrefetch,
        deadline: Instant,
        landed: bool,
    ) -> Result<(), String> {
        let metadata = job.metadata.with_prefetch(Some(mapping));
        let local = self.update_local(job, &metadata, deadline).await;
        if local.as_ref().is_err_and(|e| !e.committed) {
            return Err(local_message(&local));
        }
        let local_error = local.is_err().then(|| local_message(&local));
        let remote = match landed {
            true => self.reupload(job, &metadata, deadline).await,
            false => Ok(()),
        };
        let errors: Vec<String> = local_error.into_iter().chain(remote.err()).collect();
        match errors.is_empty() {
            true => Ok(()),
            false => Err(errors.join("\n")),
        }
    }

    async fn update_local(
        &self,
        job: &HarvestJob,
        metadata: &TemplateMetadata,
        deadline: Instant,
    ) -> Result<(), MetadataWriteError> {
        timeout_at(deadline, self.templates.update_metadata(job.build_id, metadata))
            .await
            .unwrap_or_else(|_| Err(MetadataWriteError::failed(DEADLINE_EXCEEDED)))
    }

    async fn reupload(&self, job: &HarvestJob, metadata: &TemplateMetadata, deadline: Instant) -> Result<(), String> {
        within(
            deadline,
            self.uploader
                .upload_metadata(job.build_id, metadata, &job.object_metadata),
        )
        .await
        .map_err(|e| format!("re-upload metadata: {e}"))
    }

    async fn resume_mapping(&self, job: &HarvestJob) -> Result<Option<MemoryPrefetch>, HarvestFailure> {
        let _slot = within(job.deadline, self.slots.acquire()).await.map_err(|e| {
            HarvestFailure::new(
                HarvestOutcome::SlotTimeout,
                format!("acquire start slot: {}", exhausted(e)),
            )
        })?;
        let hours = job.config.max_sandbox_length;
        let template = within(job.deadline, self.templates.pin(job.build_id, hours))
            .await
            .map_err(|e| HarvestFailure::new(HarvestOutcome::ResumeFailed, format!("get template: {e}")))?;
        let config = throwaway_config(&job.config, job.build_id);
        let sandbox_id = config.sandbox_id.clone();
        let resume = ThrowawayResume {
            template,
            config,
            start: job.start,
            end: job.end,
        };
        let instance = within(job.deadline, self.resumer.resume_for_harvest(resume))
            .await
            .map_err(|e| HarvestFailure::new(HarvestOutcome::ResumeFailed, format!("resume throwaway: {e}")))?;
        let collected = within(job.deadline, instance.memory_prefetch())
            .await
            .map_err(|e| HarvestFailure::new(HarvestOutcome::CollectFailed, format!("collect prefetch data: {e}")));
        reap(instance.as_ref(), &sandbox_id).await;
        collected.map(|mapping| mapping.filter(|mapping| !mapping.indices.is_empty()))
    }
}

fn exhausted(error: String) -> String {
    match error == DEADLINE_EXCEEDED {
        true => START_SLOTS_EXHAUSTED.to_owned(),
        false => error,
    }
}

fn local_message(local: &Result<(), MetadataWriteError>) -> String {
    local
        .as_ref()
        .err()
        .map_or_else(String::new, |e| format!("update local metadata: {}", e.message))
}

pub fn throwaway_config(config: &SandboxConfig, build_id: Uuid) -> SandboxConfig {
    SandboxConfig {
        sandbox_id: format!("{THROWAWAY_ID_PREFIX}{}", config.sandbox_id),
        execution_id: Uuid::new_v4().to_string(),
        build_id: build_id.to_string(),
        volume_mounts: Vec::new(),
        ..config.clone()
    }
}

async fn reap(instance: &dyn HarvestInstance, sandbox_id: &str) {
    let deadline = Instant::now() + HARVEST_REAP_TIMEOUT;
    if let Err(e) = within(deadline, instance.stop()).await {
        tracing::warn!(sandbox = %sandbox_id, error = %e, "harvest: failed to stop throwaway");
    }
    if let Err(e) = within(deadline, instance.close()).await {
        tracing::warn!(sandbox = %sandbox_id, error = %e, "harvest: failed to close throwaway");
    }
}

pub async fn within<T>(deadline: Instant, work: impl Future<Output = Result<T, String>>) -> Result<T, String> {
    timeout_at(deadline, work)
        .await
        .unwrap_or_else(|_| Err(DEADLINE_EXCEEDED.to_owned()))
}

#[cfg(test)]
pub mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use parking_lot::Mutex;

    use super::*;
    use crate::storage::backend::Storage;
    use crate::template::Template;

    #[derive(Default)]
    pub struct Script {
        pub events: Mutex<Vec<String>>,
        pub fail: Mutex<Option<&'static str>>,
        pub trace: Mutex<Option<MemoryPrefetch>>,
        pub upload_wait: Mutex<Duration>,
        pub resume_wait: Mutex<Duration>,
        pub acquire_wait: Mutex<Duration>,
        pub resumed: Mutex<Vec<SandboxConfig>>,
        pub committed: Mutex<bool>,
    }

    impl Script {
        pub fn record(&self, event: &str) {
            self.events.lock().push(event.to_owned());
        }

        pub fn events(&self) -> Vec<String> {
            self.events.lock().clone()
        }

        pub fn fails_at(&self, step: &'static str) {
            *self.fail.lock() = Some(step);
        }

        fn step(&self, step: &'static str) -> Result<(), String> {
            self.record(step);
            match *self.fail.lock() == Some(step) {
                true => Err(format!("{step} broke")),
                false => Ok(()),
            }
        }
    }

    pub struct Fakes(pub Arc<Script>);

    pub struct Slot(Arc<Script>);

    impl Drop for Slot {
        fn drop(&mut self) {
            self.0.record("release slot");
        }
    }

    struct Instance(Arc<Script>);

    #[async_trait]
    impl HarvestInstance for Instance {
        async fn memory_prefetch(&self) -> Result<Option<MemoryPrefetch>, String> {
            self.0.step("collect")?;
            Ok(self.0.trace.lock().clone())
        }

        async fn stop(&self) -> Result<(), String> {
            self.0.step("stop")
        }

        async fn close(&self) -> Result<(), String> {
            self.0.step("close")
        }
    }

    #[async_trait]
    impl HarvestResumer for Fakes {
        async fn resume_for_harvest(&self, resume: ThrowawayResume) -> Result<Box<dyn HarvestInstance>, String> {
            self.0.resumed.lock().push(resume.config);
            let wait = *self.0.resume_wait.lock();
            tokio::time::sleep(wait).await;
            self.0.step("resume")?;
            Ok(Box::new(Instance(self.0.clone())))
        }
    }

    #[async_trait]
    impl HarvestSlots for Fakes {
        async fn acquire(&self) -> Result<super::super::StartSlot, String> {
            let wait = *self.0.acquire_wait.lock();
            tokio::time::sleep(wait).await;
            self.0.step("acquire")?;
            Ok(Box::new(Slot(self.0.clone())))
        }
    }

    #[async_trait]
    impl HarvestTemplates for Fakes {
        async fn pin(&self, build_id: Uuid, _hours: i64) -> Result<Arc<Template>, String> {
            self.0.step("pin")?;
            let dir = tempfile::tempdir().unwrap().keep();
            let storage = Storage::local(&dir).unwrap();
            Ok(Arc::new(Template::new(storage, build_id, dir)))
        }

        async fn update_metadata(
            &self,
            _build_id: Uuid,
            metadata: &TemplateMetadata,
        ) -> Result<(), MetadataWriteError> {
            let pages = metadata.memory_prefetch().map_or(0, |memory| memory.indices.len());
            let committed = *self.0.committed.lock();
            self.0.step("update metadata").map_err(|message| match committed {
                true => MetadataWriteError::committed(message),
                false => MetadataWriteError::failed(message),
            })?;
            self.0.record(&format!("pages {pages}"));
            Ok(())
        }
    }

    #[async_trait]
    impl HarvestUpload for Fakes {
        async fn wait(&self) -> Result<(), String> {
            let wait = *self.0.upload_wait.lock();
            tokio::time::sleep(wait).await;
            self.0.step("upload wait")
        }
    }

    #[async_trait]
    impl MetadataUploader for Fakes {
        async fn upload_metadata(&self, _: Uuid, _: &TemplateMetadata, _: &ObjectMetadata) -> Result<(), String> {
            self.0.step("upload metadata")
        }
    }

    pub fn harvester(script: &Arc<Script>) -> PrefetchHarvester {
        let fakes = Arc::new(Fakes(script.clone()));
        PrefetchHarvester {
            persist_budget: Duration::from_secs(5),
            ..PrefetchHarvester::new(fakes.clone(), fakes.clone(), fakes.clone(), fakes)
        }
    }

    pub fn trace(pages: u64) -> MemoryPrefetch {
        MemoryPrefetch {
            indices: (0..pages).collect(),
            access_types: vec!["r".to_owned(); pages as usize],
            block_size: 4096,
        }
    }

    pub fn job(script: &Arc<Script>, consume: bool) -> HarvestJob {
        HarvestJob {
            config: SandboxConfig {
                sandbox_id: "sbx".to_owned(),
                execution_id: "execution".to_owned(),
                max_sandbox_length: 24,
                volume_mounts: vec![Default::default()],
                ..SandboxConfig::default()
            },
            build_id: Uuid::from_u128(7),
            start: SystemTime::UNIX_EPOCH,
            end: SystemTime::UNIX_EPOCH,
            metadata: TemplateMetadata::v1(),
            upload: Arc::new(Fakes(script.clone())),
            object_metadata: ObjectMetadata::default(),
            consume,
            deadline: Instant::now() + Duration::from_secs(5),
        }
    }

    fn scripted(trace_pages: u64) -> Arc<Script> {
        let script = Arc::new(Script::default());
        *script.trace.lock() = Some(trace(trace_pages));
        script
    }

    #[tokio::test]
    async fn without_consume_the_trace_is_measured_and_nothing_is_persisted() {
        let script = scripted(3);

        let report = harvester(&script).run(&job(&script, false)).await;

        assert_eq!(report.error, None);
        assert_eq!((report.run.outcome, report.run.pages), (HarvestOutcome::Success, 3));
        assert!(!report.run.persist_attempted);
        assert_eq!(
            script.events(),
            ["acquire", "pin", "resume", "collect", "stop", "close", "release slot"]
        );
    }

    #[tokio::test]
    async fn consume_persists_after_the_throwaway_is_reaped_and_the_upload_landed() {
        let script = scripted(2);

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(report.error, None);
        assert!(report.run.persist_attempted);
        assert_eq!(
            script.events(),
            [
                "acquire",
                "pin",
                "resume",
                "collect",
                "stop",
                "close",
                "release slot",
                "upload wait",
                "update metadata",
                "pages 2",
                "upload metadata",
            ]
        );
    }

    #[tokio::test]
    async fn a_failed_upload_updates_only_the_local_metadata() {
        let script = scripted(2);
        script.fails_at("upload wait");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(report.error, None);
        assert_eq!(report.run.outcome, HarvestOutcome::Success);
        let events = script.events();
        assert!(events.contains(&"update metadata".to_owned()));
        assert!(!events.contains(&"upload metadata".to_owned()));
    }

    #[tokio::test]
    async fn a_failed_local_update_skips_the_remote_upload() {
        let script = scripted(2);
        script.fails_at("update metadata");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(
            report.error.as_deref(),
            Some("update local metadata: update metadata broke")
        );
        assert!(!script.events().contains(&"upload metadata".to_owned()));
    }

    #[tokio::test]
    async fn a_committed_local_update_still_reaches_the_remote_and_reports_its_error() {
        let script = scripted(2);
        script.fails_at("update metadata");
        *script.committed.lock() = true;

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(
            report.error.as_deref(),
            Some("update local metadata: update metadata broke")
        );
        assert!(script.events().contains(&"upload metadata".to_owned()));
    }

    #[tokio::test]
    async fn a_failed_remote_upload_is_reported_but_the_harvest_stays_a_success() {
        let script = scripted(2);
        script.fails_at("upload metadata");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(
            report.error.as_deref(),
            Some("re-upload metadata: upload metadata broke")
        );
        assert_eq!(report.run.outcome, HarvestOutcome::Success);
    }

    #[tokio::test]
    async fn an_upload_outliving_the_persist_budget_leaves_the_metadata_alone() {
        let script = scripted(2);
        *script.upload_wait.lock() = Duration::from_secs(30);
        let harvester = PrefetchHarvester {
            persist_budget: Duration::from_millis(50),
            ..harvester(&script)
        };

        let report = harvester.run(&job(&script, true)).await;

        assert_eq!(report.run.outcome, HarvestOutcome::PersistDeadline);
        assert!(report.run.persist_attempted);
        assert_eq!(
            report.error.as_deref(),
            Some("waiting for snapshot upload: context deadline exceeded")
        );
        assert!(!script.events().contains(&"update metadata".to_owned()));
    }

    #[tokio::test]
    async fn an_empty_trace_persists_nothing() {
        let script = scripted(0);

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!((report.run.outcome, report.run.pages), (HarvestOutcome::Success, 0));
        assert!(!report.run.persist_attempted);
        assert!(!script.events().contains(&"upload wait".to_owned()));
    }

    #[tokio::test]
    async fn no_free_start_slot_is_a_slot_timeout() {
        let script = scripted(1);
        script.fails_at("acquire");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(report.run.outcome, HarvestOutcome::SlotTimeout);
        assert_eq!(report.error.as_deref(), Some("acquire start slot: acquire broke"));
        assert_eq!(script.events(), ["acquire"]);
    }

    #[tokio::test]
    async fn a_start_slot_not_free_before_the_deadline_reads_like_upstream_exhaustion() {
        let script = scripted(1);
        *script.acquire_wait.lock() = Duration::from_secs(30);
        let job = HarvestJob {
            deadline: Instant::now() + Duration::from_millis(50),
            ..job(&script, true)
        };

        let report = harvester(&script).run(&job).await;

        assert_eq!(report.run.outcome, HarvestOutcome::SlotTimeout);
        assert_eq!(
            report.error.as_deref(),
            Some(
                "acquire start slot: rpc error: code = ResourceExhausted desc = too many sandboxes resuming on this node, please retry"
            )
        );
    }

    #[tokio::test]
    async fn a_missing_template_fails_the_resume_and_frees_the_slot() {
        let script = scripted(1);
        script.fails_at("pin");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(report.run.outcome, HarvestOutcome::ResumeFailed);
        assert_eq!(report.error.as_deref(), Some("get template: pin broke"));
        assert_eq!(script.events(), ["acquire", "pin", "release slot"]);
    }

    #[tokio::test]
    async fn a_failed_resume_is_not_reaped_but_releases_what_it_held() {
        let script = scripted(1);
        script.fails_at("resume");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(report.run.outcome, HarvestOutcome::ResumeFailed);
        assert_eq!(report.error.as_deref(), Some("resume throwaway: resume broke"));
        assert_eq!(script.events(), ["acquire", "pin", "resume", "release slot"]);
    }

    #[tokio::test]
    async fn a_failed_collection_still_reaps_the_throwaway() {
        let script = scripted(1);
        script.fails_at("collect");

        let report = harvester(&script).run(&job(&script, true)).await;

        assert_eq!(report.run.outcome, HarvestOutcome::CollectFailed);
        assert_eq!(report.error.as_deref(), Some("collect prefetch data: collect broke"));
        assert_eq!(
            script.events(),
            ["acquire", "pin", "resume", "collect", "stop", "close", "release slot"]
        );
    }

    #[tokio::test]
    async fn a_failing_stop_does_not_skip_the_close() {
        let script = scripted(1);
        script.fails_at("stop");

        let report = harvester(&script).run(&job(&script, false)).await;

        assert_eq!(report.run.outcome, HarvestOutcome::Success);
        assert!(script.events().contains(&"close".to_owned()));
    }

    #[tokio::test]
    async fn the_harvest_deadline_cuts_a_stuck_resume_short() {
        let script = scripted(1);
        *script.resume_wait.lock() = Duration::from_secs(30);
        let job = HarvestJob {
            deadline: Instant::now() + Duration::from_millis(50),
            ..job(&script, true)
        };

        let report = harvester(&script).run(&job).await;

        assert_eq!(report.run.outcome, HarvestOutcome::ResumeFailed);
        assert_eq!(
            report.error.as_deref(),
            Some("resume throwaway: context deadline exceeded")
        );
        assert_eq!(script.events(), ["acquire", "pin", "release slot"]);
    }

    #[tokio::test]
    async fn the_throwaway_is_a_renamed_volume_free_copy_of_the_sandbox() {
        let script = scripted(1);
        let job = job(&script, false);

        harvester(&script).run(&job).await;

        let resumed = script.resumed.lock().clone();
        assert_eq!(resumed.len(), 1);
        assert_eq!(resumed[0].sandbox_id, "prefetch-harvest-sbx");
        assert_eq!(resumed[0].build_id, Uuid::from_u128(7).to_string());
        assert!(resumed[0].volume_mounts.is_empty());
        assert_ne!(resumed[0].execution_id, "execution");
        assert_eq!(resumed[0].max_sandbox_length, 24);
        assert_eq!(job.config.sandbox_id, "sbx");
        assert_eq!(job.config.volume_mounts.len(), 1);
    }
}
