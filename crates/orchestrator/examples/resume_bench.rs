use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use axum::serve::ListenerExt;
use orchestrator::featureflags::FeatureFlags;
use orchestrator::proto::sandbox_service_client::SandboxServiceClient;
use orchestrator::proto::{SandboxConfig, SandboxCreateRequest, SandboxDeleteRequest};
use orchestrator::server::app::App;
use orchestrator::server::config::NodeConfig;
use orchestrator::server::config::runtime::RuntimeFlags;
use orchestrator::server::node::Node;
use tonic::transport::Channel;

const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(30);
const TEARDOWN_POLL: Duration = Duration::from_millis(2);

struct Bench {
    build_id: String,
    kernel_version: String,
    firecracker_version: String,
    iterations: usize,
    parallel: usize,
}

struct Run {
    sandbox_id: String,
    elapsed: Duration,
    failure: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), String> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::WARN).init();
    let storage: PathBuf = env("BENCH_STORAGE")?.into();
    let bench = Bench {
        build_id: env("BENCH_BUILD")?,
        kernel_version: env("BENCH_KERNEL").unwrap_or_else(|_| "vmlinux-6.1.158".to_owned()),
        firecracker_version: env("BENCH_FIRECRACKER").unwrap_or_else(|_| "v1.14-0.2.0".to_owned()),
        iterations: number("BENCH_ITERATIONS", 20),
        parallel: number("BENCH_PARALLEL", 1).max(1),
    };
    let work = tempfile::tempdir().map_err(|e| e.to_string())?;
    let config = NodeConfig::from_vars(&node_vars(&storage, work.path()))?;
    let config = match std::env::var("BENCH_MAX_STARTING") {
        Ok(limit) => with_starting_limit(config, limit.parse().map_err(|_| "BENCH_MAX_STARTING is not a number")?),
        Err(_) => config,
    };
    let node = Arc::new(Node::new(&config)?);
    let app = App::new(node.clone(), None)?;
    app.start_background();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let listener = listener.tap_io(|tcp| drop(tcp.set_nodelay(true)));
    tokio::spawn(axum::serve(listener, app.router()).into_future());
    let client = SandboxServiceClient::connect(format!("http://{address}"))
        .await
        .map_err(|e| e.to_string())?;

    let runs = bench.run(client).await;
    summary(&runs);
    node.shutdown().await;
    node.close().await;
    Ok(())
}

impl Bench {
    async fn run(&self, mut client: SandboxServiceClient<Channel>) -> Vec<Run> {
        let stale_devices = connected_nbd_devices();
        let iterations: Vec<usize> = (1..=self.iterations).collect();
        let mut runs = Vec::new();
        for wave in iterations.chunks(self.parallel) {
            let resumed =
                futures::future::join_all(wave.iter().map(|iteration| self.resume(client.clone(), *iteration))).await;
            for run in resumed {
                println!(
                    "BENCHRUN {:.1} {}",
                    run.elapsed.as_secs_f64() * 1000.0,
                    run.failure.as_deref().unwrap_or("ok")
                );
                let _ = client
                    .delete(SandboxDeleteRequest {
                        sandbox_id: run.sandbox_id.clone(),
                        kill_reason: None,
                    })
                    .await;
                runs.push(run);
            }
            wait_until_torn_down(&stale_devices).await;
        }
        runs
    }

    async fn resume(&self, mut client: SandboxServiceClient<Channel>, iteration: usize) -> Run {
        let sandbox_id = format!("bench-{iteration}");
        let started = Instant::now();
        let created = client.create(self.create_request(&sandbox_id)).await;
        Run {
            sandbox_id,
            elapsed: started.elapsed(),
            failure: created.err().map(|status| status.message().to_owned()),
        }
    }

    fn create_request(&self, sandbox_id: &str) -> SandboxCreateRequest {
        let now = SystemTime::now();
        SandboxCreateRequest {
            sandbox: Some(SandboxConfig {
                sandbox_id: sandbox_id.to_owned(),
                template_id: "bench-template".to_owned(),
                build_id: self.build_id.clone(),
                team_id: "bench-team".to_owned(),
                execution_id: format!("{sandbox_id}-execution"),
                envd_version: "1.0.0".to_owned(),
                kernel_version: self.kernel_version.clone(),
                firecracker_version: self.firecracker_version.clone(),
                vcpu: 2,
                ram_mb: 1024,
                snapshot: true,
                ..SandboxConfig::default()
            }),
            start_time: Some(now.into()),
            end_time: Some((now + Duration::from_secs(3600)).into()),
            filesystem_boot: None,
        }
    }
}

fn node_vars(storage: &Path, work: &Path) -> HashMap<String, String> {
    let path = |path: PathBuf| path.display().to_string();
    std::env::vars()
        .chain([
            ("GRPC_PORT".to_owned(), "0".to_owned()),
            ("ORCHESTRATOR_SERVICES".to_owned(), "orchestrator".to_owned()),
            (
                "TEMPLATE_STORAGE_URL".to_owned(),
                format!("file://{}", path(storage.join("templates"))),
            ),
            ("FIRECRACKER_VERSIONS_DIR".to_owned(), path(storage.join("fc-versions"))),
            ("HOST_KERNELS_DIR".to_owned(), path(storage.join("kernels"))),
            ("SANDBOX_CACHE_DIR".to_owned(), path(work.join("sandboxes"))),
            ("DEFAULT_CACHE_DIR".to_owned(), path(work.join("cache"))),
            ("TEMPLATE_CACHE_DIR".to_owned(), path(work.join("cache/template"))),
        ])
        .collect()
}

fn with_starting_limit(mut config: NodeConfig, limit: i64) -> NodeConfig {
    let flags = FeatureFlags::builder()
        .override_flag("max-starting-instances-per-node", limit)
        .offline();
    config.runtime_flags = RuntimeFlags::new(flags.clone(), config.services.clone());
    config.builder.feature_flags = flags.clone();
    config.feature_flags = flags;
    config
}

async fn wait_until_torn_down(stale_devices: &[String]) {
    let deadline = Instant::now() + TEARDOWN_TIMEOUT;
    while Instant::now() < deadline
        && (firecracker_running()
            || connected_nbd_devices()
                .iter()
                .any(|device| !stale_devices.contains(device)))
    {
        tokio::time::sleep(TEARDOWN_POLL).await;
    }
}

fn firecracker_running() -> bool {
    std::fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|comm| comm.trim() == "firecracker"))
}

fn connected_nbd_devices() -> Vec<String> {
    std::fs::read_dir("/sys/block")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().join("pid").exists())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("nbd"))
        .collect()
}

fn summary(runs: &[Run]) {
    let mut warm: Vec<f64> = runs
        .iter()
        .skip(1)
        .filter(|run| run.failure.is_none())
        .map(|run| run.elapsed.as_secs_f64() * 1000.0)
        .collect();
    warm.sort_by(f64::total_cmp);
    let failed = runs.iter().filter(|run| run.failure.is_some()).count();
    let percentile = |p: f64| {
        warm.get(((p / 100.0 * warm.len() as f64).ceil() as usize).saturating_sub(1))
            .copied()
    };
    match (
        warm.first(),
        warm.last(),
        percentile(50.0),
        percentile(90.0),
        percentile(99.0),
    ) {
        (Some(min), Some(max), Some(p50), Some(p90), Some(p99)) => println!(
            "Summary ({} warm runs, {failed} failed): min {min:.1} | p50 {p50:.1} | p90 {p90:.1} | p99 {p99:.1} | max {max:.1} ms",
            warm.len()
        ),
        _ => println!("Summary: no successful warm runs ({failed} failed)"),
    }
}

fn env(name: &str) -> Result<String, String> {
    std::env::var(name).map_err(|_| format!("{name} is not set"))
}

fn number(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}
