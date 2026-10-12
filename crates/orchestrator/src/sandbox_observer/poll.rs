use crate::sandbox_logs::{SandboxMeta, external};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use futures::future::OptionFuture;
use opentelemetry::KeyValue;
use parking_lot::Mutex;

use super::metrics::{
    MIN_ENVD_FOR_CACHE_METRICS, MIN_ENVD_FOR_DISK_METRICS, MIN_ENVD_FOR_METRICS, MIN_ENVD_FOR_METRICS_TIMESTAMP,
    SandboxMetrics, sandbox_memory,
};
use crate::envd::body::decode_result;
use crate::envd::version::is_at_least;
use crate::telemetry::metrics::SandboxExport;

const EXPORT_PERIOD: Duration = Duration::from_secs(5);
const GET_METRICS_TIMEOUT: Duration = Duration::from_millis(100);
const PARALLELISM_FACTOR: usize = 5;
const MAX_ACCEPTABLE_CLOCK_DRIFT_SECONDS: i64 = 5;
const MEMORY_THRESHOLD_PERCENT: f64 = 80.0;
const CPU_THRESHOLD_PERCENT: f64 = 80.0;
const CPU_WRITE_STUCK_MS: i64 = 10_000;

pub type Sandboxes = Arc<dyn Fn() -> Vec<ObservedSandbox> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct ObservedSandbox {
    pub sandbox_id: String,
    pub team_id: String,
    pub template_id: String,
    pub build_id: String,
    pub sandbox_type: String,
    pub envd_version: String,
    pub envd: SocketAddr,
    pub access_token: Option<String>,
    pub started_at: SystemTime,
}

impl ObservedSandbox {
    fn log_meta(&self) -> SandboxMeta {
        SandboxMeta {
            sandbox_id: self.sandbox_id.clone(),
            template_id: self.template_id.clone(),
            team_id: self.team_id.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Observation {
    pub attributes: Vec<KeyValue>,
    pub metrics: SandboxMetrics,
    pub memory: Option<(i64, i64)>,
    pub cache: bool,
    pub disk: bool,
    pub cpu_hotplug: bool,
}

pub async fn run(sandboxes: Sandboxes, snapshot: Arc<Mutex<Vec<Observation>>>, export: Option<SandboxExport>) {
    let client = reqwest::Client::new();
    let mut stuck: HashMap<String, bool> = HashMap::new();
    let mut ticks = tokio::time::interval(EXPORT_PERIOD);
    loop {
        ticks.tick().await;
        let running = sandboxes();
        stuck.retain(|id, _| running.iter().any(|sandbox| &sandbox.sandbox_id == id));
        let fetched = fetch_all(&client, running).await;
        let observed = fetched
            .into_iter()
            .filter_map(|(sandbox, metrics)| observation(&sandbox, metrics, &mut stuck))
            .collect();
        *snapshot.lock() = observed;
        if let Some(Err(e)) = OptionFuture::from(export.as_ref().map(SandboxExport::push)).await {
            tracing::error!(error = %e, "error exporting sandbox metrics");
        }
    }
}

async fn fetch_all(
    client: &reqwest::Client,
    running: Vec<ObservedSandbox>,
) -> Vec<(ObservedSandbox, Result<SandboxMetrics, String>)> {
    let limit = running.len().div_ceil(PARALLELISM_FACTOR).max(1);
    let fetched: Vec<_> = futures::stream::iter(running.into_iter().filter(reports_metrics))
        .map(|sandbox| async move {
            let metrics = fetch(client, &sandbox).await;
            (sandbox, metrics)
        })
        .buffer_unordered(limit)
        .collect()
        .await;
    if let Some(error) = fetched.iter().find_map(|(_, metrics)| metrics.as_ref().err()) {
        tracing::warn!(error = %error, "error during observing sandbox metrics");
    }
    fetched
}

fn reports_metrics(sandbox: &ObservedSandbox) -> bool {
    is_at_least(&sandbox.envd_version, MIN_ENVD_FOR_METRICS).unwrap_or_else(|e| {
        tracing::error!(error = %e, sandbox_id = %sandbox.sandbox_id, "Failed to check envd version");
        false
    })
}

async fn fetch(client: &reqwest::Client, sandbox: &ObservedSandbox) -> Result<SandboxMetrics, String> {
    let request = client
        .get(format!("http://{}/metrics", sandbox.envd))
        .timeout(GET_METRICS_TIMEOUT);
    let request = match &sandbox.access_token {
        Some(token) => request.header("X-Access-Token", token),
        None => request,
    };
    let response = request.send().await.map_err(|e| e.to_string())?;
    match response.status() {
        reqwest::StatusCode::OK => decode_result(response.bytes_stream()).await,
        status => Err(format!("unexpected status code: {}", status.as_u16())),
    }
}

fn observation(
    sandbox: &ObservedSandbox,
    metrics: Result<SandboxMetrics, String>,
    stuck: &mut HashMap<String, bool>,
) -> Option<Observation> {
    let metrics = metrics.ok()?;
    warn_on_clock_drift(sandbox, &metrics);
    let cpu_hotplug = metrics.reports_cpu_hotplug();
    if cpu_hotplug {
        warn_when_cpu_write_gets_stuck(sandbox, &metrics, stuck);
    }
    let memory = sandbox_memory(&sandbox.envd_version, &metrics).unwrap_or_else(|e| {
        tracing::error!(error = %e, sandbox_id = %sandbox.sandbox_id, "Failed to check envd version for memory metrics");
        None
    });
    warn_on_thresholds(sandbox, &metrics, memory);
    Some(Observation {
        attributes: attributes(sandbox),
        cache: at_least(sandbox, MIN_ENVD_FOR_CACHE_METRICS, "cache metrics"),
        disk: at_least(sandbox, MIN_ENVD_FOR_DISK_METRICS, "disk metrics"),
        memory,
        cpu_hotplug,
        metrics,
    })
}

fn attributes(sandbox: &ObservedSandbox) -> Vec<KeyValue> {
    vec![
        KeyValue::new("sandbox_id", sandbox.sandbox_id.clone()),
        KeyValue::new("team_id", sandbox.team_id.clone()),
        KeyValue::new("build_id", sandbox.build_id.clone()),
        KeyValue::new("sandbox_type", sandbox.sandbox_type.clone()),
    ]
}

fn at_least(sandbox: &ObservedSandbox, minimum: &str, what: &str) -> bool {
    is_at_least(&sandbox.envd_version, minimum).unwrap_or_else(|e| {
        tracing::error!(error = %e, sandbox_id = %sandbox.sandbox_id, "Failed to check envd version for {what}");
        false
    })
}

fn warn_on_clock_drift(sandbox: &ObservedSandbox, metrics: &SandboxMetrics) {
    if !at_least(sandbox, MIN_ENVD_FOR_METRICS_TIMESTAMP, "timestamp in metrics") {
        return;
    }
    let host_seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs() as i64);
    let drift = (host_seconds - metrics.timestamp).abs();
    if drift > MAX_ACCEPTABLE_CLOCK_DRIFT_SECONDS {
        tracing::warn!(
            sandbox_id = %sandbox.sandbox_id,
            team_id = %sandbox.team_id,
            template_id = %sandbox.template_id,
            envd_version = %sandbox.envd_version,
            sandbox_start = %crate::go_time::rfc3339(sandbox.started_at),
            clock_host = host_seconds,
            clock_sbx = metrics.timestamp,
            clock_drift_seconds = drift as f64,
            "Significant clock drift detected between sandbox and host"
        );
    }
}

fn warn_when_cpu_write_gets_stuck(
    sandbox: &ObservedSandbox,
    metrics: &SandboxMetrics,
    stuck: &mut HashMap<String, bool>,
) {
    let now_stuck = metrics.cpu_write_pending_ms >= CPU_WRITE_STUCK_MS;
    let was_stuck = stuck.insert(sandbox.sandbox_id.clone(), now_stuck).unwrap_or(false);
    if now_stuck && !was_stuck {
        external(&sandbox.log_meta()).warn(
            "CPU hotplug write stuck in the guest kernel",
            vec![
                ("cpu_write_pending_ms", json!(metrics.cpu_write_pending_ms)),
                ("cpu_online", json!(metrics.cpu_count)),
                ("cpu_target", json!(metrics.cpu_target)),
            ],
        );
    }
}

fn warn_on_thresholds(sandbox: &ObservedSandbox, metrics: &SandboxMetrics, memory: Option<(i64, i64)>) {
    if let Some((total, used)) = memory {
        let used_percent = (used as f64 / total as f64 * 10_000.0).floor() / 100.0;
        if used_percent >= MEMORY_THRESHOLD_PERCENT {
            external(&sandbox.log_meta()).warn(
                "Memory usage threshold exceeded",
                vec![
                    ("mem_used_percent", json!(used_percent as f32)),
                    ("mem_threshold_percent", json!(MEMORY_THRESHOLD_PERCENT as f32)),
                ],
            );
        }
    }
    if metrics.cpu_used_percent >= CPU_THRESHOLD_PERCENT {
        external(&sandbox.log_meta()).warn(
            "CPU usage threshold exceeded",
            vec![
                ("cpu_used_percent", json!(metrics.cpu_used_percent as f32)),
                ("cpu_threshold_percent", json!(CPU_THRESHOLD_PERCENT as f32)),
            ],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandbox(id: &str) -> ObservedSandbox {
        ObservedSandbox {
            sandbox_id: id.to_owned(),
            team_id: "team".to_owned(),
            template_id: "template".to_owned(),
            build_id: "build".to_owned(),
            sandbox_type: "sandbox".to_owned(),
            envd_version: "0.9.0".to_owned(),
            envd: SocketAddr::from(([127, 0, 0, 1], 49983)),
            access_token: None,
            started_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn pending(ms: i64) -> SandboxMetrics {
        SandboxMetrics {
            cpu_count: 2,
            cpu_possible: 16,
            cpu_target: 2,
            cpu_write_pending_ms: ms,
            ..SandboxMetrics::default()
        }
    }

    #[test]
    fn a_stuck_write_is_remembered_so_each_episode_is_logged_once() {
        let sandbox = sandbox("sbx");
        let mut stuck = HashMap::new();

        for (ms, want) in [(0, false), (10_000, true), (20_000, true), (0, false), (10_001, true)] {
            warn_when_cpu_write_gets_stuck(&sandbox, &pending(ms), &mut stuck);

            assert_eq!(stuck.get("sbx"), Some(&want), "{ms} ms pending");
        }
    }

    #[test]
    fn an_observation_carries_the_sandbox_identity_and_the_hotplug_state() {
        let mut stuck = HashMap::new();

        let observed = observation(&sandbox("sbx"), Ok(pending(0)), &mut stuck).unwrap();

        assert!(observed.cpu_hotplug);
        assert_eq!(
            observed
                .attributes
                .iter()
                .map(|attribute| (attribute.key.to_string(), attribute.value.as_str().into_owned()))
                .collect::<Vec<_>>(),
            [
                ("sandbox_id".to_owned(), "sbx".to_owned()),
                ("team_id".to_owned(), "team".to_owned()),
                ("build_id".to_owned(), "build".to_owned()),
                ("sandbox_type".to_owned(), "sandbox".to_owned()),
            ]
        );
        assert_eq!(observed.memory, Some((0, 0)));
    }

    #[test]
    fn a_failed_poll_observes_nothing() {
        assert!(observation(&sandbox("sbx"), Err("timeout".to_owned()), &mut HashMap::new()).is_none());
    }

    #[test]
    fn an_envd_older_than_the_metrics_endpoint_is_not_polled() {
        let old = ObservedSandbox {
            envd_version: "0.1.4".to_owned(),
            ..sandbox("old")
        };

        assert!(!reports_metrics(&old));
        assert!(reports_metrics(&sandbox("new")));
    }
}
