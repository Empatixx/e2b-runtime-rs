use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::task::JoinHandle;

use crate::envd::body::decode_result;
use crate::oom::{EnvdMetrics, OomKill, Watermark};
use crate::sandbox_logs::{SandboxMeta, external};
use serde_json::json;

const SEED_TIMEOUT: Duration = Duration::from_secs(2);
const SEED_ATTEMPTS: u32 = 5;
const SEED_RETRY_DELAY: Duration = Duration::from_secs(2);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const POLL_PERIOD: Duration = Duration::from_secs(5);
const UNKNOWN: &str = "envd doesn't know the guest's OOM kills";

pub struct OomMonitor {
    task: JoinHandle<()>,
}

pub type SharedWatermark = Arc<Mutex<Watermark>>;

impl OomMonitor {
    pub fn start(
        envd: SocketAddr,
        access_token: Option<String>,
        meta: SandboxMeta,
        watermark: SharedWatermark,
    ) -> Self {
        Self {
            task: tokio::spawn(watch(envd, access_token, meta, watermark)),
        }
    }
}

impl Drop for OomMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn watch(envd: SocketAddr, access_token: Option<String>, meta: SandboxMeta, watermark: SharedWatermark) {
    let client = reqwest::Client::new();
    let fetch = |timeout: Duration| {
        let request = client.get(format!("http://{envd}/metrics")).timeout(timeout);
        let request = match &access_token {
            Some(token) => request.header("X-Access-Token", token),
            None => request,
        };
        async move {
            let response = request.send().await.map_err(|e| e.to_string())?;
            let metrics: EnvdMetrics = decode_result(response.bytes_stream()).await?;
            metrics.oom_kills.ok_or_else(|| UNKNOWN.to_owned())
        }
    };
    if !watermark.lock().seed_done() {
        match seed(&fetch).await {
            Ok(kills) => watermark.lock().seed(&kills),
            Err(e) => {
                watermark.lock().seed_failed();
                log_seed_failure(&e, &meta.sandbox_id);
            }
        }
    }
    let mut polls = tokio::time::interval(POLL_PERIOD);
    loop {
        polls.tick().await;
        if let Ok(kills) = fetch(POLL_TIMEOUT).await {
            report(&mut watermark.lock(), &kills, &meta);
        }
    }
}

async fn seed<F, Fut>(fetch: &F) -> Result<Vec<OomKill>, String>
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = Result<Vec<OomKill>, String>>,
{
    let mut last = String::new();
    for attempt in 0..SEED_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(SEED_RETRY_DELAY).await;
        }
        match fetch(SEED_TIMEOUT).await {
            Ok(kills) => return Ok(kills),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn log_seed_failure(error: &str, sandbox_id: &str) {
    match error {
        UNKNOWN => tracing::debug!(
            sandbox_id,
            "envd doesn't know the guest's OOM kills, none will be logged until it does"
        ),
        error => {
            tracing::warn!(sandbox_id, error = %error, "Failed to seed the OOM kill watermark, kills until the first metrics poll will not be logged")
        }
    }
}

fn report(watermark: &mut Watermark, kills: &[OomKill], meta: &SandboxMeta) {
    let unseen = watermark.unseen(kills);
    let log = external(meta);
    unseen.kills.iter().for_each(|kill| {
        log.warn(
            "Out of memory: process was killed",
            vec![("process", json!(kill.process))],
        )
    });
    if unseen.more > 0 {
        log.warn(
            "Out of memory: at least this many more processes were killed",
            vec![("processes", json!(unseen.more))],
        );
    }
}
