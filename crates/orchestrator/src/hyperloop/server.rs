use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::Router;
use axum::extract::{ConnectInfo, DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Map, Value, json};

use super::logs::{SourceSandbox, has_stale_timestamp, set_trusted_identity, validate_sandbox_id};

pub const DEFAULT_PORT: u16 = 5010;
const MAX_UPLOAD: usize = 1 << 28;
const COLLECTOR_TIMEOUT: Duration = Duration::from_secs(10);

pub type SourceLookup = Arc<dyn Fn(IpAddr) -> Option<SourceSandbox> + Send + Sync>;

#[derive(Clone)]
struct Hyperloop {
    lookup: SourceLookup,
    collector: String,
    client: reqwest::Client,
}

pub fn router(lookup: SourceLookup, collector: String) -> Router {
    let state = Hyperloop {
        lookup,
        collector,
        client: reqwest::Client::builder()
            .timeout(COLLECTOR_TIMEOUT)
            .build()
            .unwrap_or_default(),
    };
    Router::new()
        .route("/me", get(me))
        .route("/logs", post(logs))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .with_state(state)
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

    async fn forward(&self, payload: &Map<String, Value>) -> Result<(), String> {
        let response = self
            .client
            .post(&self.collector)
            .json(payload)
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
        tracing::warn!(sandbox_id = %source.sandbox_id, "dropping envd log with a stale pre-resume timestamp");
        return api_error(StatusCode::BAD_REQUEST, "Log timestamp predates this sandbox's resume");
    }
    set_trusted_identity(&mut payload, &source);
    match hyperloop.forward(&payload).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => {
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
        let address = serve(router(Arc::new(lookup), collector)).await;
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
    async fn an_unknown_source_is_a_bad_request() {
        let address = serve(router(Arc::new(|_| None), "http://127.0.0.1:1/".to_owned())).await;

        let response = reqwest::get(format!("http://{address}/me")).await.unwrap();

        assert_eq!(response.status(), 400);
        assert_eq!(
            response.json::<Value>().await.unwrap(),
            json!({"code": 400, "message": "Error when finding source sandbox"})
        );
    }
}
