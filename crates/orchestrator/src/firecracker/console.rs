use std::fs::File;
use std::io::Write;
use std::sync::Arc;

use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

const FLUSH_METRICS: &str = "FlushMetrics";
const REQUEST_SUCCEEDED: &str = "The request was executed successfully";

#[derive(Clone, Default)]
pub struct ConsoleTap {
    subscribers: Arc<Mutex<Option<Vec<UnboundedSender<String>>>>>,
}

#[derive(Debug, Default)]
pub struct MetricsFilter {
    skip_next_success: bool,
}

impl ConsoleTap {
    pub fn open() -> Self {
        Self {
            subscribers: Arc::new(Mutex::new(Some(Vec::new()))),
        }
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
        console: File,
    ) -> tokio::task::JoinHandle<()> {
        let console = Arc::new(Mutex::new(console));
        let tap = self.clone();
        let out = tokio::spawn(copy_lines(
            stdout,
            tap.clone(),
            console.clone(),
            Some(MetricsFilter::default()),
        ));
        let err = tokio::spawn(copy_lines(stderr, tap.clone(), console, None));
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
    console: Arc<Mutex<File>>,
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
        let _ = console.lock().write_all(line.as_bytes());
        tap.publish(line.strip_suffix('\n').unwrap_or(&line));
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
        let dir = tempfile::tempdir().unwrap();
        let console = File::create(dir.path().join("console.log")).unwrap();
        let tap = ConsoleTap::open();
        let mut lines = tap.subscribe();

        let _drained = tap.pump(&b"one\r\ntwo\n"[..], &b"err\n"[..], console);

        let mut received = Vec::new();
        while let Some(line) = lines.recv().await {
            received.push(line);
        }
        received.sort();
        assert_eq!(received, vec!["err", "one\r", "two"]);
        let written = std::fs::read_to_string(dir.path().join("console.log")).unwrap();
        assert!(written.contains("one\r\n") && written.contains("err\n"));
    }
}
