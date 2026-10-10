use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, UpDownCounter};

use crate::featureflags::{FeatureFlags, LogWriteConfig, LogWriteConfigResolver, LogWriteDefaults};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/shared/pkg/logger";
const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_MAX_INFLIGHT_SHADOW_WRITES: i64 = 1024;

static WRITES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .u64_counter("log_writer_write_count")
        .with_description("Number of log writer HTTP attempts by route and result")
        .build()
});

static SHADOW_INFLIGHT: LazyLock<UpDownCounter<i64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .i64_up_down_counter("log_writer_shadow_inflight")
        .with_description("Current number of in-flight best-effort shadow log writes")
        .build()
});

fn record_write(route: &'static str, result: &'static str, reason: &'static str) {
    WRITES.add(
        1,
        &[
            KeyValue::new("route", route),
            KeyValue::new("result", result),
            KeyValue::new("reason", reason),
        ],
    );
}

pub struct LogWriter {
    url: String,
    client: reqwest::Client,
    route: Arc<LogWriteConfigResolver>,
    shadow_inflight: AtomicI64,
}

impl LogWriter {
    pub fn new(collector: &str, flags: FeatureFlags) -> Arc<Self> {
        Arc::new(Self {
            url: collector.to_owned(),
            client: reqwest::Client::builder()
                .timeout(CLIENT_TIMEOUT)
                .build()
                .unwrap_or_default(),
            route: LogWriteConfigResolver::new(flags, LogWriteDefaults::from_env(collector)),
            shadow_inflight: AtomicI64::new(0),
        })
    }

    pub fn write(self: &Arc<Self>, line: Vec<u8>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let writer = self.clone();
        runtime.spawn(async move {
            if let Err(e) = writer.route_line(line.clone()).await {
                eprintln!("error routing log line ({} bytes): {e}", line.len());
            }
        });
    }

    async fn route_line(self: &Arc<Self>, line: Vec<u8>) -> Result<(), String> {
        let route = self.route.resolve();
        let primary = match route.primary_url.is_empty() {
            true => self.url.clone(),
            false => route.primary_url.clone(),
        };
        self.send_to_shadows(&route, &line);
        match self.send(&primary, line, route.timeout).await {
            Ok(()) => {
                record_write("primary", "success", "");
                Ok(())
            }
            Err(e) => {
                record_write("primary", "failure", "send_error");
                Err(e)
            }
        }
    }

    fn send_to_shadows(self: &Arc<Self>, route: &LogWriteConfig, line: &[u8]) {
        let max_inflight = match route.max_inflight_shadow_writes {
            limit if limit <= 0 => DEFAULT_MAX_INFLIGHT_SHADOW_WRITES,
            limit => limit,
        };
        for url in &route.shadow_urls {
            if !self.try_acquire_shadow(max_inflight) {
                record_write("shadow", "dropped", "saturated");
                continue;
            }
            SHADOW_INFLIGHT.add(1, &[]);
            let writer = self.clone();
            let (url, line, timeout) = (url.clone(), line.to_vec(), route.timeout);
            tokio::spawn(async move {
                match writer.send(&url, line, timeout).await {
                    Ok(()) => record_write("shadow", "success", ""),
                    Err(_) => record_write("shadow", "failure", "send_error"),
                }
                writer.shadow_inflight.fetch_sub(1, Ordering::SeqCst);
                SHADOW_INFLIGHT.add(-1, &[]);
            });
        }
    }

    fn try_acquire_shadow(&self, max_inflight: i64) -> bool {
        self.shadow_inflight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < max_inflight).then_some(current + 1)
            })
            .is_ok()
    }

    async fn send(&self, url: &str, line: Vec<u8>, timeout: Duration) -> Result<(), String> {
        let request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(line);
        let request = match timeout.is_zero() {
            true => request,
            false => request.timeout(timeout),
        };
        let response = request.send().await.map_err(|e| format!("error sending logs: {e}"))?;
        let status = response.status();
        let drained = response
            .bytes()
            .await
            .map(drop)
            .map_err(|e| format!("error draining response body: {e}"));
        match status.is_success() {
            true => drained,
            false => Err(format!(
                "error sending logs: unexpected HTTP status {}",
                status.as_u16()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::post;
    use parking_lot::Mutex;
    use tokio::net::TcpListener;

    use super::*;

    async fn collector() -> (String, Arc<Mutex<Vec<String>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        let router = Router::new().route(
            "/",
            post(move |body: String| {
                let sink = sink.clone();
                async move {
                    sink.lock().push(body);
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        (format!("http://{address}/"), received)
    }

    #[tokio::test]
    async fn a_line_goes_to_the_collector_when_the_flag_routes_nowhere_else() {
        let (url, received) = collector().await;
        let writer = LogWriter::new(&url, FeatureFlags::offline());

        writer.write(b"{\"message\":\"hi\"}".to_vec());

        tokio::time::timeout(Duration::from_secs(5), async {
            while received.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(*received.lock(), vec!["{\"message\":\"hi\"}".to_owned()]);
    }
}
