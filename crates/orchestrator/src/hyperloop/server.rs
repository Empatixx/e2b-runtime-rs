use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, UpDownCounter};
use parking_lot::Mutex;
use serde_json::{Value, json};

use super::logs::{SourceSandbox, has_stale_timestamp, set_trusted_identity, validate_sandbox_id};
use crate::featureflags::{FeatureFlags, LogWriteConfigResolver, LogWriteDefaults};

pub const DEFAULT_PORT: u16 = 5010;
const MAX_UPLOAD: usize = 1 << 28;
const COLLECTOR_TIMEOUT: Duration = Duration::from_secs(10);
const STALE_WARNING_INTERVAL: Duration = Duration::from_secs(30);
const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/hyperloopserver/handlers";

static WRITES: LazyLock<Counter<u64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .u64_counter("orchestrator.hyperloop.log_forward.write_count")
        .with_description("Number of hyperloop log forward HTTP attempts by route and result")
        .build()
});

static SHADOW_INFLIGHT: LazyLock<UpDownCounter<i64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .i64_up_down_counter("hyperloop_log_forward_shadow_inflight")
        .with_description("Current number of in-flight best-effort shadow log forwards")
        .build()
});

fn record_write(route: &str, result: &str, reason: &str) {
    WRITES.add(
        1,
        &[
            KeyValue::new("route", route.to_owned()),
            KeyValue::new("result", result.to_owned()),
            KeyValue::new("reason", reason.to_owned()),
        ],
    );
}

pub type SourceLookup = Arc<dyn Fn(IpAddr) -> Option<SourceSandbox> + Send + Sync>;

#[derive(Clone)]
struct Hyperloop {
    lookup: SourceLookup,
    route: Arc<LogWriteConfigResolver>,
    shadow_inflight: Arc<AtomicI64>,
    stale_warning: Arc<Mutex<Option<Instant>>>,
    client: reqwest::Client,
}

pub fn router(lookup: SourceLookup, flags: FeatureFlags, collector: String) -> Router {
    Router::new()
        .route("/me", get(me))
        .route("/logs", post(logs))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .with_state(Hyperloop::new(lookup, flags, collector))
}

impl Hyperloop {
    fn new(lookup: SourceLookup, flags: FeatureFlags, collector: String) -> Self {
        Self {
            lookup,
            route: LogWriteConfigResolver::new(flags, LogWriteDefaults::from_env(&collector)),
            shadow_inflight: Arc::default(),
            stale_warning: Arc::default(),
            client: reqwest::Client::builder()
                .timeout(COLLECTOR_TIMEOUT)
                .build()
                .unwrap_or_default(),
        }
    }
}

fn unknown_source() -> Response {
    api_error(StatusCode::BAD_REQUEST, "Error when finding source sandbox")
}

fn api_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "code": status.as_u16(), "message": message }))).into_response()
}

impl Hyperloop {
    fn source(&self, remote: SocketAddr) -> Option<SourceSandbox> {
        let source = (self.lookup)(remote.ip());
        if source.is_none() {
            tracing::error!(sandbox_ip = %remote.ip(), "error finding sandbox for source addr");
        }
        source
    }

    fn warn_about_a_stale_log(&self, sandbox_id: &str) {
        let mut last = self.stale_warning.lock();
        if last.is_none_or(|warned| warned.elapsed() >= STALE_WARNING_INTERVAL) {
            *last = Some(Instant::now());
            tracing::warn!(sandbox_id = %sandbox_id, "dropping envd log with a stale pre-resume timestamp");
        }
    }

    fn try_acquire_shadow(&self, max_inflight: i64) -> bool {
        self.shadow_inflight
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current < max_inflight).then_some(current + 1)
            })
            .is_ok()
    }

    fn forward_to_shadows(&self, urls: &[String], body: &[u8], timeout: Duration, max_inflight: i64) {
        for url in urls {
            if !self.try_acquire_shadow(max_inflight) {
                record_write("shadow", "dropped", "saturated");
                continue;
            }
            SHADOW_INFLIGHT.add(1, &[]);
            let hyperloop = self.clone();
            let (url, body) = (url.clone(), body.to_vec());
            tokio::spawn(async move {
                match hyperloop.forward(&url, body, timeout).await {
                    Ok(()) => record_write("shadow", "success", ""),
                    Err(_) => record_write("shadow", "failure", "send_error"),
                }
                hyperloop.shadow_inflight.fetch_sub(1, Ordering::SeqCst);
                SHADOW_INFLIGHT.add(-1, &[]);
            });
        }
    }

    async fn forward(&self, url: &str, body: Vec<u8>, timeout: Duration) -> Result<(), String> {
        let request = self
            .client
            .post(url)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .body(body);
        let request = match timeout.is_zero() {
            true => request,
            false => request.timeout(timeout),
        };
        let response = request
            .send()
            .await
            .map_err(|e| format!("error forwarding sandbox logs: {e}"))?;
        let status = response.status();
        let _ = response.bytes().await;
        match status.is_success() {
            true => Ok(()),
            false => Err(format!(
                "error forwarding sandbox logs: unexpected HTTP status {}",
                status.as_u16()
            )),
        }
    }
}

async fn me(State(hyperloop): State<Hyperloop>, ConnectInfo(remote): ConnectInfo<SocketAddr>) -> Response {
    match hyperloop.source(remote) {
        Some(source) => Json(json!({ "sandboxID": source.sandbox_id })).into_response(),
        None => unknown_source(),
    }
}

async fn logs(
    State(hyperloop): State<Hyperloop>,
    ConnectInfo(remote): ConnectInfo<SocketAddr>,
    body: axum::body::Bytes,
) -> Response {
    let Some(source) = hyperloop.source(remote) else {
        return unknown_source();
    };
    let Ok(Value::Object(mut payload)) = serde_json::from_slice::<Value>(&body) else {
        tracing::error!(sandbox_id = %source.sandbox_id, "error when parsing sandbox logs request");
        return api_error(StatusCode::BAD_REQUEST, "Invalid body for logs");
    };
    if let Err(e) = validate_sandbox_id(&payload, &source.sandbox_id) {
        tracing::warn!(sandbox_id = %source.sandbox_id, error = %e, "error when parsing sandbox logs request");
        return api_error(StatusCode::BAD_REQUEST, "Invalid sandboxID in logs payload");
    }
    if has_stale_timestamp(&payload, source.lifecycle_started_at) {
        hyperloop.warn_about_a_stale_log(&source.sandbox_id);
        record_write("ingest", "dropped", "stale_timestamp");
        return api_error(StatusCode::BAD_REQUEST, "Log timestamp predates this sandbox's resume");
    }
    set_trusted_identity(&mut payload, &source);
    if source.team_id.is_empty() {
        record_write("ingest", "degraded", "missing_team_id");
    }
    let Ok(logs) = serde_json::to_vec(&payload) else {
        tracing::error!(sandbox_id = %source.sandbox_id, "error when parsing logs payload");
        return api_error(StatusCode::INTERNAL_SERVER_ERROR, "Error when parsing logs payload");
    };
    let route = hyperloop.route.resolve();
    hyperloop.forward_to_shadows(
        &route.shadow_urls,
        &logs,
        route.timeout,
        route.max_inflight_shadow_writes,
    );
    match hyperloop.forward(&route.primary_url, logs, route.timeout).await {
        Ok(()) => {
            record_write("primary", "success", "");
            StatusCode::OK.into_response()
        }
        Err(e) => {
            record_write("primary", "failure", "send_error");
            tracing::error!(sandbox_id = %source.sandbox_id, error = %e, "error when forwarding sandbox logs");
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "Error when forwarding sandbox logs")
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use parking_lot::Mutex;
    use tokio::net::TcpListener;

    use super::*;

    async fn serve(router: Router) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).into_future());
        address
    }

    async fn collector() -> (String, Arc<Mutex<Vec<Value>>>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let seen = received.clone();
        let router = Router::new().route(
            "/",
            post(move |Json(body): Json<Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock().push(body);
                    StatusCode::NO_CONTENT
                }
            }),
        );
        (format!("http://{}/", serve(router).await), received)
    }

    fn lookup(ip: IpAddr) -> Option<SourceSandbox> {
        (ip == IpAddr::from([127, 0, 0, 1])).then(|| SourceSandbox {
            sandbox_id: "sandbox-1".to_owned(),
            template_id: "template-1".to_owned(),
            team_id: "team-1".to_owned(),
            build_id: "build-1".to_owned(),
            lifecycle_started_at: Some(SystemTime::now()),
        })
    }

    #[tokio::test]
    async fn me_names_the_source_sandbox_and_logs_are_forwarded_with_trusted_identity() {
        let (collector, received) = collector().await;
        let address = serve(router(Arc::new(lookup), FeatureFlags::offline(), collector)).await;
        let client = reqwest::Client::new();

        let me: Value = client
            .get(format!("http://{address}/me"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let forwarded = client
            .post(format!("http://{address}/logs"))
            .json(&json!({"instanceID": "sandbox-1", "teamID": "evil", "message": "hello"}))
            .send()
            .await
            .unwrap()
            .status();
        let mismatch: Value = client
            .post(format!("http://{address}/logs"))
            .json(&json!({"instanceID": "other"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(me, json!({"sandboxID": "sandbox-1"}));
        assert_eq!(forwarded, 200);
        assert_eq!(
            received.lock().clone(),
            vec![
                json!({"instanceID": "sandbox-1", "envID": "template-1", "teamID": "team-1", "buildID": "build-1", "message": "hello"})
            ]
        );
        assert_eq!(
            mismatch,
            json!({"code": 400, "message": "Invalid sandboxID in logs payload"})
        );
    }

    #[tokio::test]
    async fn a_non_success_status_is_an_error_that_never_echoes_the_payload() {
        for status in [
            StatusCode::UNPROCESSABLE_ENTITY,
            StatusCode::BAD_GATEWAY,
            StatusCode::TOO_MANY_REQUESTS,
        ] {
            let collector = Router::new().route("/", post(move || async move { (status, "collector diagnostic") }));
            let url = format!("http://{}/", serve(collector).await);
            let hyperloop = Hyperloop::new(Arc::new(|_| None), FeatureFlags::offline(), url.clone());

            let error = hyperloop
                .forward(&url, br#"{"secret":"not-in-error"}"#.to_vec(), Duration::ZERO)
                .await
                .unwrap_err();

            assert_eq!(
                error,
                format!(
                    "error forwarding sandbox logs: unexpected HTTP status {}",
                    status.as_u16()
                )
            );
        }
        let accepted = Router::new().route("/", post(|| async { StatusCode::ACCEPTED }));
        let url = format!("http://{}/", serve(accepted).await);
        let hyperloop = Hyperloop::new(Arc::new(|_| None), FeatureFlags::offline(), url.clone());
        assert_eq!(hyperloop.forward(&url, b"{}".to_vec(), Duration::ZERO).await, Ok(()));
    }

    async fn hanging_collector() -> String {
        let collector = Router::new().route("/", post(|| async { std::future::pending::<StatusCode>().await }));
        format!("http://{}/", serve(collector).await)
    }

    #[tokio::test]
    async fn a_non_zero_timeout_aborts_a_hung_collector_promptly() {
        let url = hanging_collector().await;
        let hyperloop = Hyperloop::new(Arc::new(|_| None), FeatureFlags::offline(), url.clone());
        let started = Instant::now();

        let result = hyperloop
            .forward(&url, br#"{"msg":"slow"}"#.to_vec(), Duration::from_millis(50))
            .await;

        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[tokio::test]
    async fn a_shadow_destination_receives_the_log_without_affecting_the_response() {
        let (primary, primary_seen) = collector().await;
        let (shadow, shadow_seen) = collector().await;
        let flags = FeatureFlags::builder()
            .override_flag(
                "logs-write-config",
                json!({"mode": "primary_and_shadow", "primary_url": primary, "shadow_urls": [shadow]}),
            )
            .offline();
        let address = serve(router(Arc::new(lookup), flags, "http://127.0.0.1:1/".to_owned())).await;

        let status = reqwest::Client::new()
            .post(format!("http://{address}/logs"))
            .json(&json!({"instanceID": "sandbox-1", "message": "hello"}))
            .send()
            .await
            .unwrap()
            .status();
        tokio::time::timeout(Duration::from_secs(5), async {
            while shadow_seen.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the shadow destination receives the log");

        assert_eq!(status, 200);
        assert_eq!(primary_seen.lock().len(), 1);
        assert_eq!(shadow_seen.lock().clone(), primary_seen.lock().clone());
    }

    #[test]
    fn shadow_writes_are_bounded_by_the_in_flight_limit() {
        let hyperloop = Hyperloop::new(Arc::new(|_| None), FeatureFlags::offline(), String::new());

        assert!(hyperloop.try_acquire_shadow(2));
        assert!(hyperloop.try_acquire_shadow(2));
        assert!(!hyperloop.try_acquire_shadow(2));
        hyperloop.shadow_inflight.fetch_sub(1, Ordering::SeqCst);
        assert!(hyperloop.try_acquire_shadow(2));
    }

    #[tokio::test]
    async fn an_unknown_source_is_a_bad_request() {
        let address = serve(router(
            Arc::new(|_| None),
            FeatureFlags::offline(),
            "http://127.0.0.1:1/".to_owned(),
        ))
        .await;

        let response = reqwest::get(format!("http://{address}/me")).await.unwrap();

        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"code": 400, "message": "Error when finding source sandbox"})
        );
    }
}
