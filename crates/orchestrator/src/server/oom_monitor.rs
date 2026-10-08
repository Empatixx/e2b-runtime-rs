use std::net::SocketAddr;
use std::time::Duration;

use tokio::task::JoinHandle;

use crate::oom::{EnvdMetrics, OomKill, Watermark};

const SEED_TIMEOUT: Duration = Duration::from_secs(2);
const SEED_ATTEMPTS: u32 = 5;
const SEED_RETRY_DELAY: Duration = Duration::from_secs(2);
const POLL_TIMEOUT: Duration = Duration::from_millis(100);
const POLL_PERIOD: Duration = Duration::from_secs(5);

pub struct OomMonitor {
    task: JoinHandle<()>,
}

impl OomMonitor {
    pub fn start(envd: SocketAddr, access_token: Option<String>, sandbox_id: String) -> Self {
        Self {
            task: tokio::spawn(watch(envd, access_token, sandbox_id)),
        }
    }
}

impl Drop for OomMonitor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn watch(envd: SocketAddr, access_token: Option<String>, sandbox_id: String) {
    let client = reqwest::Client::new();
    let fetch = |timeout: Duration| {
        let request = client.get(format!("http://{envd}/metrics")).timeout(timeout);
        let request = match &access_token {
            Some(token) => request.header("X-Access-Token", token),
            None => request,
        };
        async move {
            let metrics: EnvdMetrics = request
                .send()
                .await
                .map_err(|e| e.to_string())?
                .json()
                .await
                .map_err(|e| e.to_string())?;
            metrics.oom_kills.ok_or_else(|| "unknown".to_owned())
        }
    };
    let mut watermark = Watermark::default();
    seed(&mut watermark, &fetch, &sandbox_id).await;
    let mut polls = tokio::time::interval(POLL_PERIOD);
    loop {
        polls.tick().await;
        if let Ok(kills) = fetch(POLL_TIMEOUT).await {
            report(&mut watermark, &kills, &sandbox_id);
        }
    }
}

async fn seed<F, Fut>(watermark: &mut Watermark, fetch: &F, sandbox_id: &str)
where
    F: Fn(Duration) -> Fut,
    Fut: Future<Output = Result<Vec<OomKill>, String>>,
{
    for attempt in 0..SEED_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(SEED_RETRY_DELAY).await;
        }
        match fetch(SEED_TIMEOUT).await {
            Ok(kills) => return watermark.seed(&kills),
            Err(e) if e == "unknown" => {
                tracing::debug!(
                    sandbox_id,
                    "envd doesn't know the guest's OOM kills, none will be logged until it does"
                );
            }
            Err(e) => {
                tracing::warn!(sandbox_id, error = %e, "Failed to seed the OOM kill watermark, kills until the first metrics poll will not be logged");
            }
        }
    }
    watermark.seed_failed();
}

fn report(watermark: &mut Watermark, kills: &[OomKill], sandbox_id: &str) {
    let unseen = watermark.unseen(kills);
    unseen
        .kills
        .iter()
        .for_each(|kill| tracing::warn!(sandbox_id, process = %kill.process, "Out of memory: process was killed"));
    if unseen.more > 0 {
        tracing::warn!(
            sandbox_id,
            processes = unseen.more,
            "Out of memory: at least this many more processes were killed"
        );
    }
}
