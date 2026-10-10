use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::task::AbortHandle;

use super::client::FirecrackerClient;
use crate::telemetry::fc::{BlockMetrics, NetMetrics, record_block, record_net};

const FLUSH_INTERVAL: Duration = Duration::from_secs(5);
const FRESH_LINE_TIMEOUT: Duration = Duration::from_secs(2);
const FRESH_LINE_POLL: Duration = Duration::from_millis(5);
const READER_CAPACITY: usize = 1 << 20;
const FIFO_MODE: u32 = 0o600;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
struct BalloonDelta {
    free_page_hint_count: u64,
    free_page_hint_freed: u64,
    free_page_hint_fails: u64,
    free_page_report_count: u64,
    free_page_report_freed: u64,
    free_page_report_fails: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
struct MetricsLine {
    utc_timestamp_ms: i64,
    net: NetMetrics,
    block: BlockMetrics,
    balloon: BalloonDelta,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BalloonTotals {
    pub hint_count: u64,
    pub hint_freed: u64,
    pub hint_fails: u64,
    pub report_count: u64,
    pub report_freed: u64,
    pub report_fails: u64,
}

impl BalloonTotals {
    fn plus(self, delta: BalloonDelta) -> Self {
        Self {
            hint_count: self.hint_count + delta.free_page_hint_count,
            hint_freed: self.hint_freed + delta.free_page_hint_freed,
            hint_fails: self.hint_fails + delta.free_page_hint_fails,
            report_count: self.report_count + delta.free_page_report_count,
            report_freed: self.report_freed + delta.free_page_report_freed,
            report_fails: self.report_fails + delta.free_page_report_fails,
        }
    }
}

#[derive(Default)]
struct Observed {
    balloon: Mutex<BalloonTotals>,
    line_ms: AtomicI64,
}

type Recorder = fn(&MetricsLine);

impl Observed {
    fn ingest(&self, line: &str, record: Recorder) -> Result<(), String> {
        let metrics: MetricsLine =
            serde_json::from_str(line).map_err(|e| format!("failed to parse fc metrics line: {e}"))?;
        record(&metrics);
        let mut balloon = self.balloon.lock();
        *balloon = balloon.plus(metrics.balloon);
        self.line_ms.store(metrics.utc_timestamp_ms, Ordering::SeqCst);
        Ok(())
    }
}

fn record_line(metrics: &MetricsLine) {
    record_net(&metrics.net);
    record_block(&metrics.block);
}

pub struct FirecrackerMetrics {
    observed: Arc<Observed>,
    fifo: PathBuf,
    tasks: Vec<AbortHandle>,
}

impl Drop for FirecrackerMetrics {
    fn drop(&mut self) {
        self.tasks.iter().for_each(AbortHandle::abort);
        let _ = std::fs::remove_file(&self.fifo);
    }
}

impl FirecrackerMetrics {
    pub async fn start(fifo: &Path, client: FirecrackerClient, sandbox_id: &str) -> Result<Self, String> {
        create_fifo(fifo)?;
        let receiver = tokio::net::unix::pipe::OpenOptions::new()
            .read_write(true)
            .open_receiver(fifo)
            .map_err(|e| format!("error opening fc metrics FIFO: {e}"))?;
        let observed = Arc::new(Observed::default());
        let reader = tokio::spawn(read_lines(
            receiver,
            observed.clone(),
            sandbox_id.to_owned(),
            record_line,
        ));
        let flusher = tokio::spawn(flush_periodically(client.clone(), sandbox_id.to_owned()));
        let metrics = Self {
            observed,
            fifo: fifo.to_path_buf(),
            tasks: vec![reader.abort_handle(), flusher.abort_handle()],
        };
        client
            .put("/metrics", json!({ "metrics_path": fifo }))
            .await
            .map_err(|e| format!("error setting fc metrics: {e}"))?;
        Ok(metrics)
    }

    pub fn balloon(&self) -> BalloonTotals {
        self.reader().balloon()
    }

    pub fn reader(&self) -> BalloonReader {
        BalloonReader {
            observed: self.observed.clone(),
        }
    }
}

#[derive(Clone)]
pub struct BalloonReader {
    observed: Arc<Observed>,
}

impl BalloonReader {
    pub fn balloon(&self) -> BalloonTotals {
        *self.observed.balloon.lock()
    }

    pub async fn flush_and_read(&self, client: &FirecrackerClient) -> Result<BalloonTotals, String> {
        let since = now_ms();
        client.flush_metrics().await?;
        let deadline = tokio::time::Instant::now() + FRESH_LINE_TIMEOUT;
        while self.observed.line_ms.load(Ordering::SeqCst) < since {
            if tokio::time::Instant::now() >= deadline {
                return Err("timeout waiting for fresh balloon metrics line".to_owned());
            }
            tokio::time::sleep(FRESH_LINE_POLL).await;
        }
        Ok(self.balloon())
    }
}

fn create_fifo(fifo: &Path) -> Result<(), String> {
    let _ = std::fs::remove_file(fifo);
    nix::unistd::mkfifo(fifo, nix::sys::stat::Mode::from_bits_truncate(FIFO_MODE))
        .map_err(|e| format!("error creating fc metrics FIFO: {e}"))
}

async fn read_lines(
    receiver: tokio::net::unix::pipe::Receiver,
    observed: Arc<Observed>,
    sandbox_id: String,
    record: Recorder,
) {
    let mut lines = BufReader::with_capacity(READER_CAPACITY, receiver).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Err(e) = observed.ingest(&line, record) {
                    tracing::warn!(sandbox.id = %sandbox_id, error = %e, "failed to parse fc metrics line");
                }
            }
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(sandbox.id = %sandbox_id, error = %e, "fc metrics FIFO read error");
                return;
            }
        }
    }
}

async fn flush_periodically(client: FirecrackerClient, sandbox_id: String) {
    let mut ticks = tokio::time::interval(FLUSH_INTERVAL);
    ticks.tick().await;
    loop {
        ticks.tick().await;
        if let Err(e) = client.flush_metrics().await {
            tracing::warn!(sandbox.id = %sandbox_id, error = %e, "failed to flush fc metrics");
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignore(_: &MetricsLine) {}

    #[test]
    fn balloon_counters_accumulate_the_per_flush_deltas() {
        let observed = Observed::default();
        observed
            .ingest(
                r#"{"utc_timestamp_ms":10,"balloon":{"free_page_hint_freed":4096,"free_page_hint_count":1}}"#,
                ignore,
            )
            .unwrap();
        observed
            .ingest(
                r#"{"utc_timestamp_ms":20,"balloon":{"free_page_hint_freed":8192,"free_page_report_freed":512}}"#,
                ignore,
            )
            .unwrap();

        assert_eq!(
            *observed.balloon.lock(),
            BalloonTotals {
                hint_count: 1,
                hint_freed: 12288,
                report_freed: 512,
                ..BalloonTotals::default()
            }
        );
        assert_eq!(observed.line_ms.load(Ordering::SeqCst), 20);
    }

    #[test]
    fn a_line_with_net_and_block_metrics_is_ingested_and_garbage_is_reported() {
        let observed = Observed::default();
        let line = r#"{"utc_timestamp_ms":1,"net":{"tx_bytes_count":100,"rx_fails":2},"block":{"read_bytes":4096,"execute_fails":1},"api_server":{"process_startup_time_us":5}}"#;

        assert_eq!(observed.ingest(line, ignore), Ok(()));
        assert!(observed.ingest("not json", ignore).is_err());
        assert_eq!(observed.line_ms.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn lines_written_to_the_fifo_reach_the_reader() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let fifo = dir.path().join("metrics.fifo");
        create_fifo(&fifo).unwrap();
        let receiver = tokio::net::unix::pipe::OpenOptions::new()
            .read_write(true)
            .open_receiver(&fifo)
            .unwrap();
        let observed = Arc::new(Observed::default());
        let reader = tokio::spawn(read_lines(receiver, observed.clone(), "sbx".to_owned(), ignore));

        let mut writer = std::fs::OpenOptions::new().write(true).open(&fifo).unwrap();
        writeln!(
            writer,
            r#"{{"utc_timestamp_ms":7,"balloon":{{"free_page_hint_freed":1}}}}"#
        )
        .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while observed.line_ms.load(Ordering::SeqCst) != 7 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        reader.abort();

        assert_eq!(observed.balloon.lock().hint_freed, 1);
    }
}
