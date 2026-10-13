use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::host::mmds::MmdsOpts;
use crate::util::rate_limit::RateLimit;

const EXPORTER_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_LOG_LINE_BYTES: usize = 192 << 10;
const MAX_BUFFERED_BYTES: usize = 8 << 20;
const LOG_FLOOR: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Buffer {
    lines: VecDeque<Vec<u8>>,
    bytes: usize,
}

struct LimitedLog {
    limit: RateLimit,
    format: fn(&str, i64) -> String,
}

pub struct LogExporter {
    client: reqwest::Client,
    buffer: Mutex<Buffer>,
    opts: Mutex<Option<Arc<MmdsOpts>>>,
    trigger: Notify,
    started: AtomicBool,
    json_error: LimitedLog,
    send_error: LimitedLog,
    oversized: LimitedLog,
}

impl LimitedLog {
    fn new(format: fn(&str, i64) -> String) -> Self {
        Self {
            limit: RateLimit::new(LOG_FLOOR),
            format,
        }
    }

    fn log(&self, detail: &str) {
        if let Some(suppressed) = self.limit.allow() {
            let stamp = chrono::Local::now().format("%Y/%m/%d %H:%M:%S");
            eprintln!("{stamp} {}", (self.format)(detail, suppressed));
        }
    }
}

impl Default for LogExporter {
    fn default() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(EXPORTER_TIMEOUT)
                .pool_max_idle_per_host(0)
                .build()
                .unwrap_or_default(),
            buffer: Mutex::default(),
            opts: Mutex::new(None),
            trigger: Notify::new(),
            started: AtomicBool::new(false),
            json_error: LimitedLog::new(|e, n| {
                format!("error adding instance logging options to JSON: {e} ({n} suppressed since last log)")
            }),
            send_error: LimitedLog::new(|e, n| {
                format!("error sending instance logs: {e} ({n} suppressed since last log)")
            }),
            oversized: LimitedLog::new(|e, n| {
                format!("dropped log line exceeding {e} bytes ({n} suppressed since last log)")
            }),
        }
    }
}

impl LogExporter {
    pub fn write(&self, line: &[u8]) {
        if line.len() > MAX_LOG_LINE_BYTES {
            self.oversized.log(&MAX_LOG_LINE_BYTES.to_string());
            return;
        }
        let mut buffer = self.buffer.lock();
        while buffer.bytes + line.len() > MAX_BUFFERED_BYTES {
            let Some(oldest) = buffer.lines.pop_front() else {
                break;
            };
            buffer.bytes -= oldest.len();
        }
        buffer.bytes += line.len();
        buffer.lines.push_back(line.to_vec());
        drop(buffer);
        self.trigger.notify_one();
    }

    pub fn set_opts(self: &Arc<Self>, opts: MmdsOpts) {
        *self.opts.lock() = Some(Arc::new(opts));
        if !self.started.swap(true, Ordering::SeqCst) {
            tokio::spawn(self.clone().run());
        }
    }

    pub async fn flush_and_purge(&self, budget: Duration) -> Result<(), String> {
        let lines = self.take_all();
        let Some(opts) = self.opts.lock().clone() else {
            return Ok(());
        };
        let deadline = tokio::time::Instant::now() + budget;
        let mut first_error = None;
        for line in lines {
            if tokio::time::Instant::now() >= deadline {
                first_error.get_or_insert("context deadline exceeded".to_owned());
                break;
            }
            let sent = match with_opts(&line, &opts) {
                Ok(body) => tokio::time::timeout_at(deadline, self.send(body, &opts.logs_collector_address))
                    .await
                    .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()))
                    .map_err(|e| format!("send instance logs: {e}")),
                Err(e) => Err(format!("add instance logging options to JSON: {e}")),
            };
            if let Err(e) = sent {
                first_error.get_or_insert(e);
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn run(self: Arc<Self>) {
        loop {
            self.trigger.notified().await;
            let lines = self.take_all();
            let Some(opts) = self.opts.lock().clone() else {
                continue;
            };
            for line in lines {
                match with_opts(&line, &opts) {
                    Ok(body) => {
                        if let Err(e) = self.send(body, &opts.logs_collector_address).await {
                            self.send_error.log(&e);
                        }
                    }
                    Err(e) => self.json_error.log(&e),
                }
            }
        }
    }

    fn take_all(&self) -> Vec<Vec<u8>> {
        let mut buffer = self.buffer.lock();
        buffer.bytes = 0;
        buffer.lines.drain(..).collect()
    }

    async fn send(&self, body: String, address: &str) -> Result<(), String> {
        if address.is_empty() {
            return Ok(());
        }
        let response = self
            .client
            .post(address)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        match status.is_success() {
            true => Ok(()),
            false => Err(format!(
                "collector returned {} {}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            )),
        }
    }
}

fn with_opts(line: &[u8], opts: &MmdsOpts) -> Result<String, String> {
    let mut parsed: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(line).map_err(|e| e.to_string())?;
    parsed.insert("instanceID".to_owned(), opts.sandbox_id.clone().into());
    parsed.insert("envID".to_owned(), opts.template_id.clone().into());
    serde_json::to_string(&parsed).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> MmdsOpts {
        MmdsOpts {
            sandbox_id: "sbx".to_owned(),
            template_id: "tpl".to_owned(),
            ..MmdsOpts::default()
        }
    }

    #[test]
    fn lines_get_the_instance_ids_and_sorted_keys() {
        assert_eq!(
            with_opts(br#"{"level":"info","message":"a<b"}"#, &opts()).unwrap(),
            r#"{"envID":"tpl","instanceID":"sbx","level":"info","message":"a<b"}"#
        );
        assert!(with_opts(b"not json", &opts()).is_err());
    }

    #[test]
    fn the_buffer_keeps_the_newest_lines_within_its_cap() {
        let exporter = LogExporter::default();
        let line = vec![b'x'; MAX_LOG_LINE_BYTES];
        (0..60).for_each(|_| exporter.write(&line));
        exporter.write(&vec![b'y'; MAX_LOG_LINE_BYTES + 1]);

        let kept = exporter.take_all();
        assert_eq!(kept.len(), MAX_BUFFERED_BYTES / MAX_LOG_LINE_BYTES);
        assert!(exporter.take_all().is_empty());
    }

    #[tokio::test]
    async fn flush_without_opts_only_purges() {
        let exporter = LogExporter::default();
        exporter.write(b"{}");
        assert_eq!(exporter.flush_and_purge(Duration::ZERO).await, Ok(()));
        assert!(exporter.take_all().is_empty());
    }
}
