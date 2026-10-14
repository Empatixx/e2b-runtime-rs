use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

use crate::sandbox::Sandboxes;

const SANDBOX_ID_HEADER: &str = "e2b-sandbox-id";
const SANDBOX_PORT_HEADER: &str = "e2b-sandbox-port";

#[derive(Clone)]
struct ProxyState {
    sandboxes: Arc<Sandboxes>,
    client: Client<HttpConnector, Body>,
}

#[derive(Debug, PartialEq)]
struct Target {
    sandbox_id: String,
    port: u16,
}

pub fn router(sandboxes: Arc<Sandboxes>) -> Router {
    let client = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .build_http();
    Router::new()
        .fallback(forward)
        .with_state(ProxyState { sandboxes, client })
}

async fn forward(State(state): State<ProxyState>, mut request: Request) -> Response {
    let Some(target) = target_from_headers(request.headers()) else {
        return (StatusCode::BAD_REQUEST, "missing sandbox routing").into_response();
    };
    let Some(address) = state.sandboxes.guest_address(&target.sandbox_id).await else {
        return (
            StatusCode::BAD_GATEWAY,
            format!("sandbox {} is not running", target.sandbox_id),
        )
            .into_response();
    };
    *request.uri_mut() = guest_uri(request.uri(), &format!("{address}:{}", target.port));
    match state.client.request(request).await {
        Ok(response) => response.map(Body::new),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

fn target_from_headers(headers: &HeaderMap) -> Option<Target> {
    routing_headers(headers).or_else(|| headers.get("host")?.to_str().ok().and_then(target_from_host))
}

fn routing_headers(headers: &HeaderMap) -> Option<Target> {
    let header = |name| headers.get(name)?.to_str().ok();
    Some(Target {
        sandbox_id: header(SANDBOX_ID_HEADER)?.to_owned(),
        port: header(SANDBOX_PORT_HEADER)?.parse().ok()?,
    })
}

fn target_from_host(host: &str) -> Option<Target> {
    let label = host.split('.').next()?;
    let (port, sandbox_id) = label.split_once('-')?;
    Some(Target {
        sandbox_id: sandbox_id.to_owned(),
        port: port.parse().ok()?,
    })
}

fn guest_uri(original: &Uri, authority: &str) -> Uri {
    let path = original.path_and_query().map_or("/", |path| path.as_str());
    format!("http://{authority}{path}").parse().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_target_from_host_name() {
        let target = target_from_host("49983-iabc.localhost:3001").unwrap();
        assert_eq!(
            target,
            Target {
                sandbox_id: "iabc".into(),
                port: 49983
            }
        );
    }
}
