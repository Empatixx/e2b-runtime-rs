use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;
use tonic::Request;
use uuid::Uuid;

use super::artifacts::print_artifact_sizes;
use super::guest::Guest;
use super::options::{PauseOptions, RunOptions};
use super::report::{COMMAND_LABELS, PAUSE_LABELS, Run, Timings, fmt_dur, print_results, print_timings};
use crate::envd::ENVD_PORT;
use crate::proto::sandbox_service_server::SandboxService;
use crate::proto::{
    SandboxCheckpointRequest, SandboxConfig, SandboxCreateRequest, SandboxDeleteRequest, SandboxPauseRequest,
};
use crate::server::node::Node;
use crate::server::service::SandboxServer;
use crate::server::uploads::Outcome;
use crate::storage::spec::StorageSpec;
use crate::template::metadata::TemplateMetadata;

const ACCESS_TOKEN: &str = "local";
const TEAM_ID: &str = "local";
const RAM_MB: i64 = 512;
const LIFETIME: Duration = Duration::from_secs(24 * 60 * 60);
const STOP_POLL: Duration = Duration::from_millis(10);
const STOP_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Runner {
    pub node: Arc<Node>,
    pub server: SandboxServer,
    pub build_id: String,
    pub metadata: TemplateMetadata,
    pub template_storage: StorageSpec,
    pub sandbox: SandboxSettings,
    pub cold: bool,
    pub no_prefetch: bool,
    pub no_egress: bool,
    pub shell: bool,
    pub reboot: bool,
    pub force_reboot: bool,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SandboxSettings {
    pub vcpu: i64,
    pub max_vcpus: i64,
    pub kernel_version: String,
    pub firecracker_version: String,
    pub envd_version: String,
}

pub struct Started {
    pub id: String,
    pub guest: Guest,
}

impl Runner {
    pub async fn start_sandbox(&self, iteration: Option<i64>) -> Result<Started, String> {
        if self.reboot && !self.force_reboot && !self.metadata.filesystem_only {
            return Err(format!(
                "refusing to reboot build {}: not a filesystem-only snapshot and the request did not demand a filesystem boot",
                self.build_id
            ));
        }
        self.start_from(&self.build_id, sandbox_id(iteration), LIFETIME).await
    }

    pub async fn start_from(&self, build_id: &str, id: String, lifetime: Duration) -> Result<Started, String> {
        let request = SandboxCreateRequest {
            sandbox: Some(self.config(build_id, &id)),
            start_time: Some(SystemTime::now().into()),
            end_time: Some((SystemTime::now() + lifetime).into()),
            filesystem_boot: Some(self.force_reboot),
        };
        self.server
            .create(Request::new(request))
            .await
            .map_err(|status| status.message().to_owned())?;
        let slot = self
            .node
            .registry
            .get(&id)
            .map(|handle| handle.info.lock().slot)
            .ok_or_else(|| format!("sandbox {id} is not running after the create"))?;
        if self.no_egress {
            self.node.network.deny_egress(slot).await?;
        }
        let guest = self
            .guest_of(&id)
            .ok_or_else(|| format!("sandbox {id} is not running after the create"))?;
        Ok(Started { guest, id })
    }

    pub fn guest_of(&self, id: &str) -> Option<Guest> {
        self.node.registry.get(id).map(|handle| {
            let slot = handle.info.lock().slot;
            Guest {
                envd: SocketAddr::from((slot.host_ip(), ENVD_PORT)),
                access_token: ACCESS_TOKEN.to_owned(),
                namespace: slot.namespace(),
            }
        })
    }

    fn config(&self, build_id: &str, id: &str) -> SandboxConfig {
        let nanos = now_nanos();
        SandboxConfig {
            template_id: build_id.to_owned(),
            build_id: build_id.to_owned(),
            base_template_id: build_id.to_owned(),
            kernel_version: self.sandbox.kernel_version.clone(),
            firecracker_version: self.sandbox.firecracker_version.clone(),
            sandbox_id: id.to_owned(),
            envd_version: self.sandbox.envd_version.clone(),
            vcpu: self.sandbox.vcpu,
            max_vcpus: self.sandbox.max_vcpus,
            ram_mb: RAM_MB,
            team_id: TEAM_ID.to_owned(),
            snapshot: !self.reboot && !self.force_reboot,
            envd_access_token: Some(ACCESS_TOKEN.to_owned()),
            execution_id: format!("exec-{nanos}"),
            ..SandboxConfig::default()
        }
    }

    pub async fn close(&self, id: &str) {
        let _ = self
            .server
            .delete(Request::new(SandboxDeleteRequest {
                sandbox_id: id.to_owned(),
                kill_reason: None,
            }))
            .await;
        let stopped = tokio::time::timeout(STOP_TIMEOUT, async {
            while self.node.registry.get(id).is_some() {
                tokio::time::sleep(STOP_POLL).await;
            }
        });
        let _ = stopped.await;
    }

    pub async fn interactive(&self) -> Result<(), String> {
        println!("🚀 Starting...");
        let started_at = Instant::now();
        let started = self.start_sandbox(None).await?;
        println!("✅ Running (resumed in {:?})", started_at.elapsed());
        println!("{}", started.guest.ssh_hint());
        let result = match self.shell {
            true => super::shell::attach(&started.guest, &self.cancel).await,
            false => {
                println!("Ctrl+C to stop");
                self.cancel.cancelled().await;
                Ok(())
            }
        };
        println!("🧹 Cleanup...");
        self.close(&started.id).await;
        result
    }

    pub async fn benchmark(&self, iterations: i64) -> Result<(), String> {
        let mut results = Vec::new();
        let mut last_error = None;
        for iteration in 0..iterations {
            if self.cancel.is_cancelled() {
                break;
            }
            self.cold_start(iteration).await?;
            print!("\r[{}/{iterations}] Running...    ", iteration + 1);
            flush();
            let started_at = Instant::now();
            let started = self.start_sandbox(Some(iteration)).await;
            let duration = started_at.elapsed();
            if let Ok(started) = &started {
                self.close(&started.id).await;
            }
            let error = started.err();
            results.push(Run {
                duration,
                error: error.clone(),
            });
            if let Some(error) = error {
                println!("\r[{}/{iterations}] ❌ Failed", iteration + 1);
                last_error = Some(error);
                break;
            }
        }
        print!("\r                    \r");
        print_results(&results);
        last_error.map_or(Ok(()), Err)
    }

    pub async fn command_mode(&self, options: &RunOptions) -> Result<(), String> {
        match options.iterations > 0 {
            true => self.command_benchmark(options).await,
            false => self.command_once(options, true).await.error.map_or(Ok(()), Err),
        }
    }

    async fn command_once(&self, options: &RunOptions, verbose: bool) -> Timings {
        if verbose {
            println!("🚀 Starting sandbox...");
        }
        let started_at = Instant::now();
        let started = match self.start_sandbox(None).await {
            Ok(started) => started,
            Err(e) => return failed(started_at.elapsed(), e),
        };
        let resume = started_at.elapsed();
        if verbose {
            println!("✅ Sandbox resumed in {resume:?}");
            println!("🔧 Running: {}", options.cmd);
        }
        let command_started = Instant::now();
        let ran = started.guest.run(&options.cmd).await;
        let timings = Timings {
            resume,
            work: command_started.elapsed(),
            total: started_at.elapsed(),
            error: ran.err(),
        };
        if verbose {
            match &timings.error {
                Some(error) => println!("❌ Command failed: {error}"),
                None => print_breakdown(&[
                    ("Resume: ", timings.resume),
                    ("Command:", timings.work),
                    ("Total:  ", timings.total),
                ]),
            }
        }
        self.close(&started.id).await;
        timings
    }

    async fn command_benchmark(&self, options: &RunOptions) -> Result<(), String> {
        println!("📦 Benchmarking command: {}", options.cmd);
        println!("   Iterations: {}, Cold start: {}\n", options.iterations, self.cold);
        let results = self
            .repeat(options.iterations, |_| self.command_once(options, false))
            .await?;
        print_timings(&results, &COMMAND_LABELS);
        first_error(&results)
    }

    pub async fn pause_mode(&self, options: &PauseOptions) -> Result<(), String> {
        match options.immediate && options.iterations > 0 {
            true => self.pause_benchmark(options).await,
            false => self.pause_once(options, true).await.error.map_or(Ok(()), Err),
        }
    }

    async fn pause_benchmark(&self, options: &PauseOptions) -> Result<(), String> {
        println!("📦 Benchmarking pause (resume -> snapshot)");
        println!("   Iterations: {}, Cold start: {}\n", options.iterations, self.cold);
        let results = self
            .repeat(options.iterations, |_| {
                let iteration = PauseOptions {
                    new_build_id: Uuid::new_v4().to_string(),
                    ..options.clone()
                };
                async move { self.pause_once(&iteration, false).await }
            })
            .await?;
        print_timings(&results, &PAUSE_LABELS);
        first_error(&results)
    }

    async fn repeat<F, Fut>(&self, iterations: i64, mut once: F) -> Result<Vec<Timings>, String>
    where
        F: FnMut(i64) -> Fut,
        Fut: Future<Output = Timings>,
    {
        let mut results = Vec::new();
        for iteration in 0..iterations {
            if self.cancel.is_cancelled() {
                break;
            }
            self.cold_start(iteration).await?;
            print!("\r[{}/{iterations}] Running...    ", iteration + 1);
            flush();
            let timings = once(iteration).await;
            let failure = timings.error.clone();
            results.push(timings);
            if let Some(error) = failure {
                println!("\r[{}/{iterations}] ❌ Failed: {error}", iteration + 1);
                break;
            }
        }
        print!("\r                         \r");
        Ok(results)
    }

    async fn pause_once(&self, options: &PauseOptions, verbose: bool) -> Timings {
        if verbose {
            println!("🚀 Starting sandbox...");
        }
        let started_at = Instant::now();
        let started = match self.start_sandbox(None).await {
            Ok(started) => started,
            Err(e) => return failed(started_at.elapsed(), e),
        };
        let resume = started_at.elapsed();
        if verbose {
            println!("✅ Sandbox resumed in {resume:?}");
        }
        if let Err(e) = self.pause_trigger(&started, options, verbose).await {
            self.close(&started.id).await;
            return failed(resume, e);
        }
        if verbose {
            println!("⏸️  Pausing sandbox (new build: {})...", options.new_build_id);
        }
        if let Err(e) = started.guest.sync_and_drop_caches().await
            && verbose
        {
            println!("⚠️  Warning: sync/drop_caches failed: {e}");
        }
        let pause_started = Instant::now();
        let paused = self.snapshot(&started, options).await;
        let timings = Timings {
            resume,
            work: pause_started.elapsed(),
            total: started_at.elapsed(),
            error: paused.as_ref().err().map(|e| format!("failed to pause: {e}")),
        };
        if options.in_place || paused.is_err() {
            self.close(&started.id).await;
        }
        if timings.error.is_some() || !verbose {
            return timings;
        }
        print_breakdown(&[
            ("Resume:", timings.resume),
            ("Pause: ", timings.work),
            ("Total: ", timings.total),
        ]);
        match self.finish_upload(options).await {
            Ok(()) => timings,
            Err(e) => Timings {
                error: Some(e),
                ..timings
            },
        }
    }

    async fn pause_trigger(&self, started: &Started, options: &PauseOptions, verbose: bool) -> Result<(), String> {
        if !options.command.is_empty() {
            if verbose {
                println!("🔧 Running command: {}", options.command);
            }
            started
                .guest
                .run(&options.command)
                .await
                .map_err(|e| format!("command failed: {e}"))?;
            if verbose {
                println!("✅ Command completed successfully");
            }
            return Ok(());
        }
        if !options.command_signal.is_empty() {
            if verbose {
                println!("🔧 Starting command: {}", options.command_signal);
            }
            let running = started.guest.run_in_background(&options.command_signal);
            return started
                .guest
                .wait_for_pause_signal("SIGUSR1", Some(running), &self.cancel)
                .await;
        }
        if !options.signal_name.is_empty() {
            return started
                .guest
                .wait_for_pause_signal(&options.signal_name, None, &self.cancel)
                .await;
        }
        Ok(())
    }

    async fn snapshot(&self, started: &Started, options: &PauseOptions) -> Result<(), String> {
        match options.in_place {
            true => self
                .server
                .checkpoint(Request::new(SandboxCheckpointRequest {
                    sandbox_id: started.id.clone(),
                    build_id: options.new_build_id.clone(),
                    metadata: Default::default(),
                    filesystem_only: options.fs_only,
                }))
                .await
                .map(drop),
            false => self
                .server
                .pause(Request::new(SandboxPauseRequest {
                    sandbox_id: started.id.clone(),
                    template_id: self.build_id.clone(),
                    build_id: options.new_build_id.clone(),
                    filesystem_only: options.fs_only,
                }))
                .await
                .map(drop),
        }
        .map_err(|status| status.message().to_owned())
    }

    async fn finish_upload(&self, options: &PauseOptions) -> Result<(), String> {
        match options.remote_storage {
            true => println!("📤 Uploading snapshot..."),
            false => println!("💾 Saving snapshot to local storage..."),
        }
        let build_id = Uuid::parse_str(&options.new_build_id).map_err(|e| e.to_string())?;
        if self.node.uploads.wait(build_id).await == Outcome::Failed {
            return Err("failed to upload snapshot".to_owned());
        }
        println!("✅ Snapshot uploaded successfully");
        println!("\n✅ Build finished: {}", options.new_build_id);
        print_artifact_sizes(&self.template_storage, &options.new_build_id);
        match options.optimize {
            true => super::optimize::collect_prefetch(self, options)
                .await
                .map_err(|e| format!("prefetch optimization: {e}")),
            false => Ok(()),
        }
    }

    async fn cold_start(&self, iteration: i64) -> Result<(), String> {
        if !self.cold || iteration == 0 {
            return Ok(());
        }
        self.node.templates.invalidate_all();
        self.node.store.clear();
        drop_page_cache().map_err(|e| format!("drop page cache: {e}"))?;
        self.mask_prefetch().await;
        Ok(())
    }

    pub async fn mask_prefetch(&self) {
        if self.no_prefetch {
            self.drop_prefetch(&self.build_id).await;
        }
    }

    pub async fn drop_prefetch(&self, build_id: &str) {
        let Ok(build_id) = Uuid::parse_str(build_id) else {
            return;
        };
        let template = self.node.templates.get(build_id);
        if let Ok(metadata) = template.metadata().await {
            template.update_metadata(TemplateMetadata {
                prefetch: None,
                ..metadata
            });
        }
    }
}

fn sandbox_id(iteration: Option<i64>) -> String {
    match iteration {
        Some(iteration) => format!("sbx-{}-{iteration}", now_nanos()),
        None => format!("sbx-{}", now_nanos()),
    }
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos())
}

fn failed(resume: Duration, error: String) -> Timings {
    Timings {
        resume,
        error: Some(error),
        ..Timings::default()
    }
}

fn first_error(results: &[Timings]) -> Result<(), String> {
    results
        .iter()
        .find_map(|timings| timings.error.clone())
        .map_or(Ok(()), Err)
}

fn print_breakdown(lines: &[(&str, Duration)]) {
    println!();
    println!("📊 Timing breakdown:");
    lines
        .iter()
        .for_each(|(label, duration)| println!("   {label} {}", fmt_dur(*duration)));
}

fn flush() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
}

fn drop_page_cache() -> std::io::Result<()> {
    rustix::fs::sync();
    std::fs::write("/proc/sys/vm/drop_caches", "3")
}
