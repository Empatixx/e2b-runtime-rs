use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const FLUSH_METRICS: &str = "FlushMetrics";
const REQUEST_SUCCEEDED: &str = "The request was executed successfully";
const TAIL_BYTES: usize = 2000;

#[derive(Clone, Default)]
pub struct ConsoleTap {
    subscribers: Arc<Mutex<Option<Vec<UnboundedSender<String>>>>>,
    tail: Arc<Mutex<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout,
    Stderr,
}

#[derive(Debug, Default)]
pub struct MetricsFilter {
    skip_next_success: bool,
}

impl ConsoleTap {
    pub fn open() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Some(Vec::new()))),
            tail: Arc::default(),
        }
    }

    pub fn tail(&self) -> String {
        self.tail.lock().trim().to_owned()
    }

    fn remember(&self, line: &str) {
        let mut tail = self.tail.lock();
        tail.push_str(line);
        tail.push('\n');
        let excess = tail.len().saturating_sub(TAIL_BYTES);
        let cut = (excess..=tail.len()).find(|at| tail.is_char_boundary(*at)).unwrap_or(0);
        tail.drain(..cut);
    }

    pub fn subscribe(&self) -> UnboundedReceiver<String> {
        let (sender, receiver) = unbounded_channel();
        if let Some(subscribers) = self.subscribers.lock().as_mut() {
            subscribers.push(sender);
        }
        receiver
    }

    fn publish(&self, line: &str) {
        if let Some(subscribers) = self.subscribers.lock().as_mut() {
            subscribers.retain(|subscriber| subscriber.send(line.to_owned()).is_ok());
        }
    }

    fn close(&self) {
        self.subscribers.lock().take();
    }

    pub fn pump(
        &self,
        stdout: impl AsyncRead + Unpin + Send + 'static,
        stderr: impl AsyncRead + Unpin + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let tap = self.clone();
        let out = tokio::spawn(copy_lines(
            stdout,
            tap.clone(),
            Stream::Stdout,
            Some(MetricsFilter::default()),
        ));
        let err = tokio::spawn(copy_lines(stderr, tap.clone(), Stream::Stderr, None));
        tokio::spawn(async move {
            let _ = tokio::join!(out, err);
            tap.close();
        })
    }
}

impl MetricsFilter {
    pub fn keeps(&mut self, line: &str) -> bool {
        if line.contains(FLUSH_METRICS) {
            self.skip_next_success = true;
            return false;
        }
        if self.skip_next_success && line.contains(REQUEST_SUCCEEDED) {
            self.skip_next_success = false;
            return false;
        }
        true
    }
}

async fn copy_lines(
    stream: impl AsyncRead + Unpin,
    tap: ConsoleTap,
    source: Stream,
    mut filter: Option<MetricsFilter>,
) {
    let mut reader = BufReader::new(stream);
    let mut buffer = Vec::new();
    while reader.read_until(b'\n', &mut buffer).await.is_ok_and(|read| read > 0) {
        let line = String::from_utf8_lossy(&buffer).into_owned();
        buffer.clear();
        if filter.as_mut().is_some_and(|filter| !filter.keeps(&line)) {
            continue;
        }
        let line = line.strip_suffix('\n').unwrap_or(&line);
        log_line(source, line);
        tap.remember(line);
        tap.publish(line);
    }
}

fn log_line(source: Stream, line: &str) {
    match source {
        Stream::Stdout => tracing::info!(target: "firecracker", "{line}"),
        Stream::Stderr => tracing::error!(target: "firecracker", "{line}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flush_metrics_and_its_reply_are_dropped() {
        let mut filter = MetricsFilter::default();
        let kept: Vec<&str> = [
            "boot",
            "[anonymous-instance:fc_api] The API server received a Put request on \"/vm\" FlushMetrics",
            "[anonymous-instance:fc_api] The request was executed successfully. Status code: 204 No Content.",
            "The request was executed successfully",
            "done",
        ]
        .into_iter()
        .filter(|line| filter.keeps(line))
        .collect();

        assert_eq!(kept, vec!["boot", "The request was executed successfully", "done"]);
    }

    #[tokio::test]
    async fn subscribers_get_lines_until_the_streams_end() {
        let tap = ConsoleTap::open();
        let mut lines = tap.subscribe();

        let _drained = tap.pump(&b"one\r\ntwo\n"[..], &b"err\n"[..]);

        let mut received = Vec::new();
        while let Some(line) = lines.recv().await {
            received.push(line);
        }
        received.sort();
        assert_eq!(received, vec!["err", "one\r", "two"]);
        let tail = tap.tail();
        assert!(tail.contains("one\r\n") && tail.contains("err"));
    }

    #[test]
    fn the_tail_keeps_only_the_last_bytes() {
        let tap = ConsoleTap::open();
        (0..1000).for_each(|n| tap.remember(&format!("line {n}")));

        let tail = tap.tail();

        assert!(tail.len() <= TAIL_BYTES);
        assert!(tail.ends_with("line 999"));
        assert!(!tail.contains("line 0\n"));
    }
}
