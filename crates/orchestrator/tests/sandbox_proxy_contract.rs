#[path = "proxy_contract/support.rs"]
mod support;

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use orchestrator::envd::ENVD_PORT;
use orchestrator::proto::SandboxNetworkIngressConfig;
use orchestrator::proxy::sandbox::{SandboxProxy, SandboxRoute};
use serde_json::Value;
use support::*;
use tokio::net::TcpListener;

const SANDBOX_ID: &str = "im9r2ycjiy2534qsdy1oo";
const TOKEN_HEADER: &str = "e2b-traffic-access-token";

struct Running {
    proxy: Arc<SandboxProxy>,
    address: std::net::SocketAddr,
}

async fn start(ingress: Option<SandboxNetworkIngressConfig>, max_incoming_connections: i64) -> Running {
    let lookup = Arc::new(move |sandbox_id: &str| {
        (sandbox_id == SANDBOX_ID).then(|| SandboxRoute {
            sandbox_id: SANDBOX_ID.to_owned(),
            lifecycle_id: "lifecycle-1".to_owned(),
            host_ip: Ipv4Addr::LOCALHOST,
            ingress: ingress.clone(),
        })
    });
    let proxy = Arc::new(SandboxProxy::new(lookup, Arc::new(move || max_incoming_connections)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let serving = proxy.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    Running { proxy, address }
}

impl Running {
    fn request(&self, port: u64) -> reqwest::RequestBuilder {
        reqwest::Client::new()
            .get(format!("http://{}/path", self.address))
            .header("host", format!("{port}-{SANDBOX_ID}.e2b.app"))
    }
}

fn ingress(token: Option<&str>, mask: Option<&str>, https_ports: &[u32]) -> SandboxNetworkIngressConfig {
    SandboxNetworkIngressConfig {
        traffic_access_token: token.map(str::to_owned),
        mask_request_host: mask.map(str::to_owned),
        https_ports: https_ports.to_vec(),
    }
}

#[tokio::test]
async fn proxies_a_user_port_to_the_sandbox() {
    let (backend, _server) = spawn_echo_backend().await;
    let running = start(None, -1).await;

    let body: Value = running
        .request(u64::from(backend.port()))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["uri"], "/path");
    assert_eq!(
        body["headers"]["host"],
        format!("{}-{SANDBOX_ID}.e2b.app", backend.port())
    );
}

#[tokio::test]
async fn routes_by_headers_on_ip_hosts() {
    let (backend, _server) = spawn_echo_backend().await;
    let running = start(None, -1).await;

    let response = reqwest::Client::new()
        .get(format!("http://{}/path", running.address))
        .header("e2b-sandbox-id", SANDBOX_ID)
        .header("e2b-sandbox-port", backend.port().to_string())
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn the_traffic_access_token_guards_user_ports() {
    let (backend, _server) = spawn_echo_backend().await;
    let running = start(Some(ingress(Some("secret"), None, &[])), -1).await;
    let port = u64::from(backend.port());

    let missing = running.request(port).send().await.unwrap();
    assert_eq!(missing.status(), 403);
    assert_eq!(missing.headers()["content-type"], "application/json; charset=utf-8");
    assert_eq!(missing.headers()["access-control-allow-origin"], "*");
    let body: Value = missing.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "sandboxId": SANDBOX_ID,
            "message": "Sandbox is secured with traffic access token. Token header 'e2b-traffic-access-token' is missing",
            "code": 403,
        })
    );

    let invalid = running
        .request(port)
        .header(TOKEN_HEADER, "wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(invalid.status(), 403);
    let body: Value = invalid.json().await.unwrap();
    assert_eq!(
        body["message"],
        "Sandbox is secured with traffic access token. Provided token in header 'e2b-traffic-access-token' is invalid"
    );

    let valid = running
        .request(port)
        .header(TOKEN_HEADER, "secret")
        .send()
        .await
        .unwrap();
    assert_eq!(valid.status(), 200);
}

#[tokio::test]
async fn the_traffic_access_token_page_names_the_header_for_browsers() {
    let running = start(Some(ingress(Some("secret"), None, &[])), -1).await;

    let page = running
        .request(8000)
        .header("user-agent", "Mozilla/5.0")
        .send()
        .await
        .unwrap();

    assert_eq!(page.status(), 403);
    assert_eq!(page.headers()["content-type"], "text/html; charset=utf-8");
    let html = page.text().await.unwrap();
    assert!(html.contains("Missing traffic access token"));
    assert!(html.contains(TOKEN_HEADER));
}

#[tokio::test]
async fn control_routes_are_not_reachable_through_the_envd_port() {
    let running = start(None, -1).await;

    for path in ["/init", "/freeze", "/upgrade", "/files/../init"] {
        let response = reqwest::Client::new()
            .get(format!("http://{}{path}", running.address))
            .header("host", format!("{ENVD_PORT}-{SANDBOX_ID}.e2b.app"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 404, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(
            body["message"],
            "This endpoint is reserved for the E2B control plane and is not reachable through the sandbox URL"
        );
        assert!(body.get("sandboxId").is_none());
    }
}

#[tokio::test]
async fn unknown_sandboxes_are_a_bad_gateway() {
    let running = start(None, -1).await;

    let response = reqwest::Client::new()
        .get(format!("http://{}/", running.address))
        .header("host", "8000-iunknown.e2b.app")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body,
        serde_json::json!({"sandboxId": "iunknown", "message": "The sandbox was not found", "code": 502})
    );
}

#[tokio::test]
async fn malformed_hosts_are_bad_requests() {
    let running = start(None, -1).await;
    let cases = [
        ("e2b.app", "Invalid host\n"),
        ("abc-isv6ril5xadwn1k9t2jye.e2b.app", "Invalid sandbox port\n"),
        ("8000-INVALID.e2b.app", "Invalid sandbox ID\n"),
    ];
    for (host, text) in cases {
        let response = reqwest::Client::new()
            .get(format!("http://{}/", running.address))
            .header("host", host)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400, "{host}");
        assert_eq!(response.text().await.unwrap(), text, "{host}");
    }
}

#[tokio::test]
async fn closed_ports_are_templated() {
    let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = u64::from(dead.local_addr().unwrap().port());
    drop(dead);
    let running = start(None, -1).await;

    let response = running.request(port).send().await.unwrap();

    assert_eq!(response.status(), 502);
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["message"], "The sandbox is running but port is not open");
    assert_eq!(body["port"], port);
    assert_eq!(body["sandboxId"], SANDBOX_ID);
}

#[tokio::test]
async fn masks_the_request_host_with_the_port() {
    let (backend, _server) = spawn_echo_backend().await;
    let port = u64::from(backend.port());
    let running = start(Some(ingress(None, Some("masked-${PORT}.internal"), &[])), -1).await;

    let body: Value = running.request(port).send().await.unwrap().json().await.unwrap();

    assert_eq!(body["headers"]["host"], format!("masked-{port}.internal"));
    assert_eq!(
        body["headers"]["x-forwarded-host"],
        format!("{port}-{SANDBOX_ID}.e2b.app")
    );
}

#[tokio::test]
async fn reaches_https_ports_with_self_signed_certificates() {
    let backend = spawn_tls_backend("secure backend").await;
    let port = backend.address.port();
    let running = start(Some(ingress(None, None, &[u32::from(port)])), -1).await;

    let response = running.request(u64::from(port)).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "secure backend");
}

#[tokio::test]
async fn plain_ports_do_not_speak_tls_to_the_sandbox() {
    let backend = spawn_tls_backend("secure backend").await;
    let running = start(None, -1).await;

    let response = running.request(u64::from(backend.address.port())).send().await.unwrap();

    assert_ne!(response.text().await.unwrap(), "secure backend");
}

#[tokio::test]
async fn the_incoming_connection_limit_applies_per_sandbox() {
    let (backend, _server) = spawn_echo_backend().await;
    let running = start(None, 0).await;

    let response = running.request(u64::from(backend.port())).send().await.unwrap();

    assert_eq!(response.status(), 429);
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["message"],
        "The sandbox has too many concurrent incoming connections (limit: 0)"
    );
    assert_eq!(body["connectionLimit"], 0);
}

#[tokio::test]
async fn closing_without_connections_is_immediate() {
    let running = start(None, -1).await;

    let started = std::time::Instant::now();
    running.proxy.close(false).await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
}

#[tokio::test]
async fn a_released_network_forgets_the_lifecycle_limit() {
    let (backend, _server) = spawn_echo_backend().await;
    let running = start(None, 1).await;

    let first = running.request(u64::from(backend.port())).send().await.unwrap();
    first.bytes().await.unwrap();
    running.proxy.on_network_release("lifecycle-1");
    running.proxy.remove_from_pool("lifecycle-1");

    let second = running.request(u64::from(backend.port())).send().await.unwrap();
    assert_eq!(second.status(), 200);
}

async fn failing_proxy(message: &'static str) -> Arc<SandboxProxy> {
    let proxy = Arc::new(SandboxProxy::new(Arc::new(|_| None), Arc::new(|| -1)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let serving = proxy.clone();
    tokio::spawn(async move { serving.serve(FailingClose { listener, message }).await });
    tokio::time::sleep(Duration::from_millis(50)).await;
    proxy
}

#[tokio::test]
async fn a_forced_close_returns_the_listener_error() {
    let proxy = failing_proxy("listener close failed").await;

    let error = proxy.close(true).await.unwrap_err();

    assert_eq!(error, "failed to shutdown proxy server: listener close failed");
}

#[tokio::test]
async fn a_failed_graceful_shutdown_falls_back_to_a_forced_close_and_reports_the_listener() {
    let proxy = failing_proxy("listener shutdown failed").await;

    let error = proxy.close(false).await.unwrap_err();

    assert_eq!(error, "failed to shutdown proxy server: listener shutdown failed");
}
