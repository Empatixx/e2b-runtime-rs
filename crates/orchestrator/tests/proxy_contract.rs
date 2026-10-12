#[path = "proxy_contract/support.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use bytes::Bytes;
use futures::StreamExt;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::Frame;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use orchestrator::proxy::body::{BoxError, full_body};
use orchestrator::proxy::limit::ConnectionLimit;
use orchestrator::proxy::limiter::ConnectionLimiter;
use orchestrator::proxy::refusal::Refusal;
use orchestrator::proxy::server::ShutdownError;
use serde_json::Value;
use support::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const CHROME: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)";

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

async fn assert_backend_output(backend: &Backend, response: reqwest::Response) {
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), backend.id);
}

#[tokio::test]
async fn routes_to_the_target_server() {
    let backend = spawn_backend("backend-1").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;
    assert_eq!(proxy.server.total_pool_connections(), 0);
    assert_eq!(backend.request_count(), 0);

    let response = client().get(proxy.url("/hello")).send().await.unwrap();

    assert_backend_output(&backend, response).await;
    assert_eq!(backend.request_count(), 1);
    assert_eq!(proxy.server.total_pool_connections(), 1);
}

#[tokio::test]
async fn routes_to_an_https_backend_with_a_self_signed_certificate() {
    let backend = spawn_tls_backend("secure backend").await;
    let secure = orchestrator::proxy::destination::Destination::builder()
        .scheme(axum::http::uri::Scheme::HTTPS)
        .authority(axum::http::uri::Authority::try_from(backend.address.to_string()).unwrap())
        .sandbox_id("test-sandbox")
        .connection_key("https-backend")
        .insecure_skip_tls_verify(true)
        .build();
    let proxy = start_proxy(fixed(secure), None).await;

    let response = client().get(proxy.url("/")).send().await.unwrap();

    assert_backend_output(&backend, response).await;
}

#[tokio::test]
async fn rejects_an_https_backend_without_insecure_skip_tls_verify() {
    let backend = spawn_tls_backend("secure backend").await;
    let verified = orchestrator::proxy::destination::Destination::builder()
        .scheme(axum::http::uri::Scheme::HTTPS)
        .authority(axum::http::uri::Authority::try_from(backend.address.to_string()).unwrap())
        .sandbox_id("test-sandbox")
        .connection_key("https-backend-verified")
        .build();
    let proxy = start_proxy(fixed(verified), None).await;

    let response = client().get(proxy.url("/")).send().await.unwrap();

    assert_eq!(response.status(), 502);
}

async fn assert_error_template(
    refusal: Refusal,
    path: &str,
    status: u16,
    message: &str,
    title: &str,
    html_omits: &str,
) {
    let proxy = start_proxy(refusing(refusal), None).await;

    let json = client().get(proxy.url(path)).send().await.unwrap();
    assert_eq!(json.status(), status);
    assert_eq!(json.headers()["content-type"], "application/json; charset=utf-8");
    let body: Value = json.json().await.unwrap();
    assert_eq!(body["message"], message);
    assert_eq!(body["code"], status);

    let html = client()
        .get(proxy.url(path))
        .header("user-agent", CHROME)
        .send()
        .await
        .unwrap();
    assert_eq!(html.status(), status);
    assert_eq!(html.headers()["content-type"], "text/html; charset=utf-8");
    let page = html.text().await.unwrap();
    assert!(page.contains(title));
    assert!(html_omits.is_empty() || !page.contains(html_omits));
}

#[tokio::test]
async fn resume_permission_denied_is_templated() {
    assert_error_template(
        Refusal::ResumePermissionDenied {
            sandbox_id: "test-sandbox".to_owned(),
        },
        "/hello",
        403,
        "This sandbox can't be resumed with the credentials provided. Check your access token and try again.",
        "Unable to resume sandbox",
        "",
    )
    .await;
}

#[tokio::test]
async fn sandbox_still_transitioning_is_templated() {
    assert_error_template(
        Refusal::StillTransitioning {
            sandbox_id: "test-sandbox".to_owned(),
        },
        "/hello",
        409,
        "The sandbox is still transitioning. Try again in a moment.",
        "Sandbox still transitioning",
        "",
    )
    .await;
}

#[tokio::test]
async fn internal_route_is_templated_without_naming_the_sandbox() {
    assert_error_template(
        Refusal::InternalRoute {
            sandbox_id: "test-sandbox".to_owned(),
            path: "/init".to_owned(),
        },
        "/init",
        404,
        "This endpoint is reserved for the E2B control plane and is not reachable through the sandbox URL",
        "Endpoint not available",
        "test-sandbox",
    )
    .await;
    let proxy = start_proxy(
        refusing(Refusal::InternalRoute {
            sandbox_id: "test-sandbox".to_owned(),
            path: "/init".to_owned(),
        }),
        None,
    )
    .await;
    let body: Value = client()
        .get(proxy.url("/init"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["path"], "/init");
    assert!(body.get("sandboxId").is_none());
}

#[tokio::test]
async fn team_sandbox_limit_is_templated() {
    assert_error_template(
        Refusal::ResourceExhausted {
            sandbox_id: "test-sandbox".to_owned(),
            message: "rate limit hit".to_owned(),
        },
        "/hello",
        429,
        "rate limit hit",
        "Sandbox limit reached",
        "",
    )
    .await;
}

#[tokio::test]
async fn reuses_upstream_connections() {
    let backend = spawn_backend("backend-1").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;
    let client = client();

    for _ in 0..2 {
        let response = client.get(proxy.url("/hello")).send().await.unwrap();
        assert_backend_output(&backend, response).await;
    }

    assert_eq!(backend.request_count(), 2);
    assert_eq!(proxy.server.total_pool_connections(), 1);
}

#[tokio::test]
async fn removing_from_the_pool_closes_idle_connections() {
    let backend = spawn_backend("backend-1").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;

    let response = client().get(proxy.url("/hello")).send().await.unwrap();
    assert_backend_output(&backend, response).await;
    assert_eq!(proxy.server.total_pool_connections(), 1);
    assert_eq!(proxy.server.current_pool_connections(), 1);

    proxy.server.remove_from_pool(&backend.id);

    assert_eq!(proxy.server.total_pool_connections(), 1);
    assert_eq!(proxy.server.current_pool_connections(), 0);
    assert_eq!(proxy.server.pool_size(), 0);
}

#[tokio::test]
async fn removing_from_the_pool_cuts_alive_connections() {
    let backend = spawn_backend("backend-1").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;
    let url = proxy.url("/hello");
    let request = tokio::spawn(async move {
        let response = client()
            .get(url)
            .header(BODY_WRITE_DELAY_HEADER, "10000")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        response.bytes().await
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(proxy.server.total_pool_connections(), 1);
    assert_eq!(proxy.server.current_pool_connections(), 1);
    assert_eq!(backend.request_count(), 1);

    proxy.server.remove_from_pool(&backend.id);

    assert_eq!(proxy.server.total_pool_connections(), 1);
    assert_eq!(proxy.server.current_pool_connections(), 0);
    let outcome = tokio::time::timeout(Duration::from_secs(5), request)
        .await
        .unwrap()
        .unwrap();
    assert!(outcome.is_err(), "the truncated stream must surface as an error");
}

#[tokio::test]
async fn removing_from_the_pool_resets_the_sandbox_side_of_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let read_error = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n")
            .await
            .unwrap();
        stream.read_u8().await.unwrap_err()
    });
    let proxy = start_proxy(fixed(destination(address, "reset-me")), None).await;
    let url = proxy.url("/");
    let held = tokio::spawn(async move { client().get(url).send().await.unwrap().bytes().await });
    tokio::time::sleep(Duration::from_millis(500)).await;

    proxy.server.remove_from_pool("reset-me");

    let error = tokio::time::timeout(Duration::from_secs(5), read_error)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionReset);
    assert!(held.await.unwrap().is_err());
}

#[tokio::test]
async fn reusing_the_key_after_the_backend_changes_fails() {
    let mut backend1 = spawn_backend("backend-1").await;
    let address = backend1.address;
    let proxy = start_proxy(fixed(destination(address, "backend-1")), None).await;
    let client = client();
    let response = client.get(proxy.url("/hello")).send().await.unwrap();
    assert_backend_output(&backend1, response).await;
    assert_eq!(proxy.server.total_pool_connections(), 1);

    backend1.interrupt().await;
    let backend2 = serve_backend(TcpListener::bind(address).await.unwrap(), "backend-2", None);

    let response = client.get(proxy.url("/hello")).send().await.unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(backend2.request_count(), 0);
}

#[tokio::test]
async fn a_new_key_does_not_reuse_connections_when_the_backend_changes() {
    let mut backend1 = spawn_backend("backend-1").await;
    let address = backend1.address;
    let key = Arc::new(parking_lot::Mutex::new("backend-1".to_owned()));
    let resolving = key.clone();
    let proxy = start_proxy(Arc::new(move |_| Ok(destination(address, &resolving.lock()))), None).await;
    let client = client();
    let response = client.get(proxy.url("/hello")).send().await.unwrap();
    assert_backend_output(&backend1, response).await;
    assert_eq!(proxy.server.total_pool_connections(), 1);

    backend1.interrupt().await;
    let backend2 = serve_backend(TcpListener::bind(address).await.unwrap(), "backend-2", None);
    *key.lock() = "backend-2".to_owned();

    let response = client.get(proxy.url("/hello")).send().await.unwrap();
    assert_backend_output(&backend2, response).await;
    assert_eq!(backend2.request_count(), 1);
    assert_eq!(backend1.request_count(), 1);
    assert_eq!(proxy.server.total_pool_connections(), 2);
}

#[tokio::test]
async fn retries_while_the_backend_starts_up() {
    let (address, listen) = reserved_port().await;
    let proxy = start_proxy(fixed(destination(address, "delayed-backend")), None).await;
    let starting = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        serve_backend(listen(), "delayed-backend", None)
    });

    let started = Instant::now();
    let response = client().get(proxy.url("/hello")).send().await.unwrap();
    let elapsed = started.elapsed();

    let backend = starting.await.unwrap();
    assert_backend_output(&backend, response).await;
    assert!(elapsed >= Duration::from_millis(300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(backend.request_count(), 1);
    assert_eq!(proxy.server.total_pool_connections(), 1);
}

#[tokio::test]
async fn masks_the_request_host_and_keeps_the_inbound_one_in_x_forwarded_host() {
    let (address, _backend) = spawn_echo_backend().await;
    let masked = orchestrator::proxy::destination::Destination {
        mask_request_host: Some("masked.example:1234".to_owned()),
        ..destination(address, "connection-key")
    };
    let proxy = start_proxy(fixed(masked), None).await;

    let body: Value = client()
        .get(proxy.url("/"))
        .header("host", "localhost:5007")
        .header("e2b-testing", "test123")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["headers"]["host"], "masked.example:1234");
    assert_eq!(body["headers"]["x-forwarded-host"], "localhost:5007");
    assert_eq!(body["headers"]["e2b-testing"], "test123");
}

async fn raw_head_backend() -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (sent, head) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let received = read_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
        sent.send(received).ok();
    });
    (address, head)
}

#[tokio::test]
async fn writes_header_names_in_go_canonical_form() {
    let (address, head) = raw_head_backend().await;
    let masked = orchestrator::proxy::destination::Destination {
        mask_request_host: Some("masked.example:1234".to_owned()),
        ..destination(address, "canonical")
    };
    let proxy = start_proxy(fixed(masked), None).await;

    client()
        .get(proxy.url("/"))
        .header("host", "localhost:5007")
        .header("e2b-testing", "test123")
        .send()
        .await
        .unwrap();
    let head = head.await.unwrap();

    assert!(head.contains("\r\nHost: masked.example:1234\r\n"), "{head}");
    assert!(head.contains("\r\nX-Forwarded-Host: localhost:5007\r\n"), "{head}");
    assert!(head.contains("\r\nE2b-Testing: test123\r\n"), "{head}");
}

#[tokio::test]
async fn forwards_the_inbound_host_and_strips_forwarding_and_hop_by_hop_headers() {
    let (address, _backend) = spawn_echo_backend().await;
    let proxy = start_proxy(fixed(destination(address, "headers")), None).await;

    let response = client()
        .post(proxy.url("/path?query=1"))
        .header("x-forwarded-for", "1.2.3.4")
        .header("x-forwarded-host", "evil.example")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "for=1.2.3.4")
        .header("keep-alive", "timeout=5")
        .header("connection", "x-secret")
        .header("x-secret", "1")
        .header("x-custom", "kept")
        .body("payload")
        .send()
        .await
        .unwrap();

    assert!(
        response.headers().get("keep-alive").is_none(),
        "response hop-by-hop headers are stripped"
    );
    let body: Value = response.json().await.unwrap();
    let headers = &body["headers"];
    assert_eq!(headers["host"], format!("127.0.0.1:{}", proxy.port));
    assert_eq!(headers["x-custom"], "kept");
    assert_eq!(headers["user-agent"], "");
    for stripped in [
        "x-forwarded-for",
        "x-forwarded-host",
        "x-forwarded-proto",
        "forwarded",
        "keep-alive",
        "x-secret",
    ] {
        assert!(headers.get(stripped).is_none(), "{stripped}");
    }
    assert_eq!(body["uri"], "/path?query=1");
    assert_eq!(body["body"], "payload");
}

#[tokio::test]
async fn closes_upstream_connections_when_keep_alives_are_disabled() {
    let (address, _backend) = spawn_echo_backend().await;
    let mut settings = proxy_settings();
    settings.disable_keep_alives = true;
    let proxy = start_proxy_with(settings, fixed(destination(address, "no-keep-alive")), None).await;

    for _ in 0..2 {
        let body: Value = client().get(proxy.url("/")).send().await.unwrap().json().await.unwrap();
        assert_eq!(body["headers"]["connection"], "close");
    }

    assert_eq!(proxy.server.total_pool_connections(), 2);
}

fn counting_limit(max: i64, counters: &Arc<[AtomicI64; 3]>) -> ConnectionLimit {
    let (acquired, released, blocked) = (counters.clone(), counters.clone(), counters.clone());
    ConnectionLimit::builder()
        .limiter(Arc::new(ConnectionLimiter::default()))
        .max_limit(Arc::new(move || max))
        .on_acquired(Arc::new(move |_| {
            acquired[0].fetch_add(1, Ordering::SeqCst);
        }))
        .on_released(Arc::new(move |_| {
            released[1].fetch_add(1, Ordering::SeqCst);
        }))
        .on_blocked(Arc::new(move |()| {
            blocked[2].fetch_add(1, Ordering::SeqCst);
        }))
        .build()
}

#[tokio::test]
async fn connection_limit_blocks_excess_connections() {
    const MAX_CONNS: i64 = 2;
    let backend = spawn_backend("backend-limited").await;
    let counters = Arc::new([AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)]);
    let proxy = start_proxy(
        fixed(destination(backend.address, &backend.id)),
        Some(counting_limit(MAX_CONNS, &counters)),
    )
    .await;
    let held = (0..MAX_CONNS)
        .map(|_| {
            let url = proxy.url("/hello");
            tokio::spawn(async move {
                let response = client()
                    .get(url)
                    .header(BODY_WRITE_DELAY_HEADER, "2000")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                response.bytes().await.unwrap();
            })
        })
        .collect::<Vec<_>>();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let blocked = client().get(proxy.url("/hello")).send().await.unwrap();

    assert_eq!(blocked.status(), 429);
    assert_eq!(counters[0].load(Ordering::SeqCst), MAX_CONNS);
    assert_eq!(counters[2].load(Ordering::SeqCst), 1);
    for task in held {
        task.await.unwrap();
    }
    assert_eq!(counters[1].load(Ordering::SeqCst), MAX_CONNS);
    let response = client().get(proxy.url("/hello")).send().await.unwrap();
    assert_backend_output(&backend, response).await;
}

fn preflight(client: &reqwest::Client, url: String) -> reqwest::RequestBuilder {
    client
        .request(reqwest::Method::OPTIONS, url)
        .header("origin", "https://app.example.com")
        .header("access-control-request-method", "GET")
}

fn assert_answered_preflight(response: &reqwest::Response) {
    assert_eq!(response.status(), 204);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(response.headers()["access-control-allow-methods"], "*");
    assert!(!response.headers()["access-control-max-age"].is_empty());
}

#[tokio::test]
async fn destination_errors_carry_cors() {
    let cases = [
        (
            Refusal::SandboxNotFound {
                sandbox_id: "test-sandbox".to_owned(),
            },
            502,
        ),
        (
            Refusal::ResumePermissionDenied {
                sandbox_id: "test-sandbox".to_owned(),
            },
            403,
        ),
        (
            Refusal::StillTransitioning {
                sandbox_id: "test-sandbox".to_owned(),
            },
            409,
        ),
        (
            Refusal::InternalRoute {
                sandbox_id: "test-sandbox".to_owned(),
                path: "/init".to_owned(),
            },
            404,
        ),
        (
            Refusal::ResourceExhausted {
                sandbox_id: "test-sandbox".to_owned(),
                message: "rate limit hit".to_owned(),
            },
            429,
        ),
        (
            Refusal::MissingTrafficAccessToken {
                sandbox_id: "test-sandbox".to_owned(),
                header: "e2b-traffic-access-token",
            },
            403,
        ),
        (
            Refusal::InvalidTrafficAccessToken {
                sandbox_id: "test-sandbox".to_owned(),
                header: "e2b-traffic-access-token",
            },
            403,
        ),
        (Refusal::InvalidHost, 400),
        (Refusal::InvalidSandboxId, 400),
        (Refusal::InvalidSandboxPort { port: "x".to_owned() }, 400),
        (
            Refusal::MissingHeader {
                header: "E2b-Sandbox-Id",
            },
            400,
        ),
        (
            Refusal::Unexpected {
                message: "routing table on fire".to_owned(),
            },
            500,
        ),
    ];
    for (refusal, status) in cases {
        let proxy = start_proxy(refusing(refusal.clone()), None).await;
        let client = client();

        let answered = preflight(&client, proxy.url("/hello")).send().await.unwrap();
        assert_answered_preflight(&answered);

        let error = client
            .get(proxy.url("/hello"))
            .header("origin", "https://app.example.com")
            .send()
            .await
            .unwrap();
        assert_eq!(error.status(), status, "{refusal:?}");
        assert_eq!(error.headers()["access-control-allow-origin"], "*", "{refusal:?}");
    }
}

#[tokio::test]
async fn plain_routing_errors_use_the_upstream_texts() {
    let cases = [
        (Refusal::InvalidHost, "Invalid host\n"),
        (Refusal::InvalidSandboxId, "Invalid sandbox ID\n"),
        (
            Refusal::InvalidSandboxPort { port: "x".to_owned() },
            "Invalid sandbox port\n",
        ),
        (
            Refusal::MissingHeader {
                header: "E2b-Sandbox-Id",
            },
            "missing header\n",
        ),
        (
            Refusal::Unexpected {
                message: "routing table on fire".to_owned(),
            },
            "Unexpected error when routing request: routing table on fire\n",
        ),
    ];
    for (refusal, text) in cases {
        let proxy = start_proxy(refusing(refusal), None).await;
        let response = client().get(proxy.url("/")).send().await.unwrap();
        assert_eq!(response.headers()["content-type"], "text/plain; charset=utf-8");
        assert_eq!(response.text().await.unwrap(), text);
    }
}

#[tokio::test]
async fn connection_limit_refusals_carry_cors() {
    let backend = spawn_backend("backend-conn-limit").await;
    let counters = Arc::new([AtomicI64::new(0), AtomicI64::new(0), AtomicI64::new(0)]);
    let proxy = start_proxy(
        fixed(destination(backend.address, &backend.id)),
        Some(counting_limit(0, &counters)),
    )
    .await;
    let client = client();

    let answered = preflight(&client, proxy.url("/hello")).send().await.unwrap();
    assert_answered_preflight(&answered);

    let refused = client.get(proxy.url("/hello")).send().await.unwrap();
    assert_eq!(refused.status(), 429);
    assert_eq!(refused.headers()["access-control-allow-origin"], "*");
}

#[tokio::test]
async fn unreachable_upstreams_carry_cors() {
    for default_to_port_error in [true, false] {
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = dead.local_addr().unwrap();
        drop(dead);
        let unreachable = orchestrator::proxy::destination::Destination {
            sandbox_port: 3000,
            default_to_port_error,
            ..destination(address, "dead-backend")
        };
        let proxy = start_proxy_with(
            orchestrator::proxy::server::ProxySettings {
                max_connection_attempts: 1,
                ..proxy_settings()
            },
            fixed(unreachable),
            None,
        )
        .await;
        let client = client();

        let answered = preflight(&client, proxy.url("/hello")).send().await.unwrap();
        assert_answered_preflight(&answered);

        let error = client.get(proxy.url("/hello")).send().await.unwrap();
        assert_eq!(error.status(), 502);
        assert_eq!(error.headers()["access-control-allow-origin"], "*");
        let body = error.text().await.unwrap();
        match default_to_port_error {
            true => assert!(body.contains("The sandbox is running but port is not open"), "{body}"),
            false => assert_eq!(body, "Failed to route request to sandbox\n"),
        }
    }
}

#[tokio::test]
async fn reachable_upstreams_keep_their_own_cors() {
    let backend = spawn_backend("backend-cors").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;
    let client = client();

    let response = client
        .get(proxy.url("/hello"))
        .header("origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert!(response.headers().get("access-control-allow-origin").is_none());
    assert_backend_output(&backend, response).await;

    let answered = preflight(&client, proxy.url("/hello")).send().await.unwrap();
    assert_eq!(answered.status(), 200);
    assert_eq!(answered.text().await.unwrap(), backend.id);
    assert_eq!(backend.request_count(), 2);
}

async fn tunnel_backend() -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (done, finished) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: tunnel\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let (mut reader, mut writer) = stream.split();
        drop(tokio::io::copy(&mut reader, &mut writer).await);
        done.send(()).ok();
    });
    (address, finished)
}

async fn open_tunnel(proxy: &TestProxy) -> tokio::net::TcpStream {
    let mut client = connect(proxy).await;
    client
        .write_all(b"GET /stream HTTP/1.1\r\nHost: sandbox.test\r\nUpgrade: tunnel\r\nConnection: Upgrade\r\n\r\n")
        .await
        .unwrap();
    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    assert!(head.to_lowercase().contains("upgrade: tunnel"), "{head}");
    client
}

async fn echo(client: &mut tokio::net::TcpStream) {
    client.write_all(b"ping\n").await.unwrap();
    let mut got = [0u8; 5];
    tokio::time::timeout(Duration::from_secs(5), client.read_exact(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&got, b"ping\n");
}

async fn assert_tunnel_closed(finished: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(Duration::from_secs(5), finished)
        .await
        .expect("shutdown left the backend tunnel open")
        .unwrap();
}

#[tokio::test]
async fn graceful_shutdown_waits_for_upgraded_connections() {
    let (address, finished) = tunnel_backend().await;
    let proxy = start_proxy(fixed(destination(address, "upgraded-connection")), None).await;
    let mut client = open_tunnel(&proxy).await;
    echo(&mut client).await;

    let server = proxy.server.clone();
    let shutdown = tokio::spawn(async move { server.shutdown(Duration::from_secs(5)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!shutdown.is_finished(), "shutdown returned before the peer closed");
    echo(&mut client).await;
    drop(client);

    assert_eq!(shutdown.await.unwrap(), Ok(()));
    assert_tunnel_closed(finished).await;
    assert_eq!(proxy.server.current_server_connections(), 0);
}

#[tokio::test]
async fn shutdown_deadline_falls_back_to_closing_upgraded_connections() {
    let (address, finished) = tunnel_backend().await;
    let proxy = start_proxy(fixed(destination(address, "upgraded-connection")), None).await;
    let mut client = open_tunnel(&proxy).await;
    echo(&mut client).await;

    assert_eq!(
        proxy.server.shutdown(Duration::from_millis(100)).await,
        Err(ShutdownError::DeadlineExceeded)
    );
    echo(&mut client).await;
    proxy.server.close().unwrap();

    assert_tunnel_closed(finished).await;
    assert_eq!(proxy.server.current_server_connections(), 0);
}

#[tokio::test]
async fn forced_close_ends_upgraded_connections() {
    let (address, finished) = tunnel_backend().await;
    let proxy = start_proxy(fixed(destination(address, "upgraded-connection")), None).await;
    let mut client = open_tunnel(&proxy).await;
    echo(&mut client).await;

    proxy.server.close().unwrap();

    assert_tunnel_closed(finished).await;
    assert_eq!(proxy.server.current_server_connections(), 0);
}

#[tokio::test]
async fn still_switches_protocols() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: e2b\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut line = Vec::new();
        while !line.ends_with(b"\n") {
            line.push(stream.read_u8().await.unwrap());
        }
        stream.write_all(&[b"echo: ".as_slice(), &line].concat()).await.unwrap();
    });
    let proxy = start_proxy(fixed(destination(address, "upgrade-backend")), None).await;
    let mut client = connect(&proxy).await;
    client
        .write_all(b"GET /ws HTTP/1.1\r\nHost: sandbox\r\nUpgrade: e2b\r\nConnection: Upgrade\r\n\r\n")
        .await
        .unwrap();

    let head = read_head(&mut client).await;
    assert!(head.starts_with("HTTP/1.1 101"), "{head}");
    client.write_all(b"ping\n").await.unwrap();
    let mut echoed = vec![0u8; "echo: ping\n".len()];
    client.read_exact(&mut echoed).await.unwrap();

    assert_eq!(echoed, b"echo: ping\n");
}

#[tokio::test]
async fn a_backend_answering_a_different_upgrade_is_a_bad_gateway() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: other\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
    });
    let proxy = start_proxy(fixed(destination(address, "mismatch")), None).await;
    let mut client = connect(&proxy).await;
    client
        .write_all(b"GET /ws HTTP/1.1\r\nHost: sandbox\r\nUpgrade: e2b\r\nConnection: Upgrade\r\n\r\n")
        .await
        .unwrap();

    let head = read_head(&mut client).await;

    assert!(head.starts_with("HTTP/1.1 502"), "{head}");
}

fn envelope(flag: u8, payload: &[u8]) -> Vec<u8> {
    let length = u32::try_from(payload.len()).unwrap();
    [&[flag][..], &length.to_be_bytes(), payload].concat()
}

#[tokio::test]
async fn ends_a_connect_stream_when_the_sandbox_stops() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let first_frame = envelope(0, b"{}");
    let served = first_frame.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_head(&mut stream).await;
        let head = "HTTP/1.1 200 OK\r\ncontent-type: application/connect+json\r\ntransfer-encoding: chunked\r\n\r\n";
        let chunk = format!("{:x}\r\n", served.len());
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(chunk.as_bytes()).await.unwrap();
        stream.write_all(&served).await.unwrap();
        stream.write_all(b"\r\n").await.unwrap();
        tokio::time::sleep(Duration::from_secs(30)).await;
    });
    let proxy = start_proxy(fixed(destination(address, "connect-stream-backend")), None).await;
    let response = client()
        .post(proxy.url("/test.v1.Service/Stream"))
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let mut stream = response.bytes_stream();
    let mut received = Vec::new();
    while received.len() < first_frame.len() {
        received.extend_from_slice(&stream.next().await.unwrap().unwrap());
    }
    assert_eq!(received, first_frame);

    proxy.server.remove_from_pool("connect-stream-backend");

    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        received.extend_from_slice(&chunk.expect("the stream must end cleanly with an end-stream frame"));
    }
    let end = &received[first_frame.len()..];
    assert_eq!(end[0], 0b10);
    let message: Value = serde_json::from_slice(&end[5..]).unwrap();
    assert_eq!(message["error"]["code"], "unavailable");
    assert_eq!(
        message["error"]["message"],
        "the connection to sandbox test-sandbox ended before the stream completed"
    );
}

#[tokio::test]
async fn streams_request_bodies_to_the_sandbox() {
    let (address, _backend) = spawn_echo_backend().await;
    let proxy = start_proxy(fixed(destination(address, "uploads")), None).await;
    let chunks = futures::stream::iter(["one ", "two ", "three"].map(Ok::<_, std::io::Error>));

    let body: Value = client()
        .post(proxy.url("/upload"))
        .body(reqwest::Body::wrap_stream(chunks))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(body["body"], "one two three");
}

fn envelope_free_stream_backend() -> Handle {
    Arc::new(|_| {
        Box::pin(async move {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Result<Frame<Bytes>, BoxError>>();
            sender.send(Ok(Frame::data(Bytes::from("before shutdown\n")))).ok();
            tokio::spawn(async move {
                FINISH.notified().await;
                sender.send(Ok(Frame::data(Bytes::from("after shutdown\n")))).ok();
            });
            Response::new(
                StreamBody::new(tokio_stream::wrappers::UnboundedReceiverStream::new(receiver)).boxed_unsync(),
            )
        })
    })
}

static FINISH: tokio::sync::Notify = tokio::sync::Notify::const_new();

async fn h2c_request(proxy: &TestProxy) -> Response<hyper::body::Incoming> {
    let stream = connect(proxy).await;
    let (mut sender, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .uri(format!("http://127.0.0.1:{}/stream", proxy.port))
        .body(http_body_util::Empty::<Bytes>::new())
        .unwrap();
    sender.send_request(request).await.unwrap()
}

#[tokio::test]
async fn graceful_shutdown_drains_an_h2c_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _backend = serve_handle(listener, envelope_free_stream_backend());
    let proxy = start_proxy(fixed(destination(address, "h2c-stream")), None).await;
    let response = h2c_request(&proxy).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.version(), hyper::Version::HTTP_2);
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert_eq!(first, "before shutdown\n");

    let server = proxy.server.clone();
    let shutdown = tokio::spawn(async move { server.shutdown(Duration::from_secs(5)).await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!shutdown.is_finished(), "shutdown returned before the stream finished");
    FINISH.notify_one();
    let rest = body.collect().await.unwrap().to_bytes();

    assert_eq!(rest, "after shutdown\n");
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap(),
        Ok(())
    );
    assert_eq!(proxy.server.current_server_connections(), 0);
}

#[tokio::test]
async fn shutdown_releases_failed_h2c_prefaces() {
    let cases = [
        ("missing tail", "", false),
        ("partial tail", "S", false),
        ("invalid tail", "BADBAD", false),
        ("stalled preface", "", true),
        ("stalled partial tail", "S", true),
    ];
    for (name, tail, keep_open) in cases {
        let proxy = start_proxy(
            refusing(Refusal::Unexpected {
                message: "unexpected proxy request".to_owned(),
            }),
            None,
        )
        .await;
        let mut client = connect(&proxy).await;
        client
            .write_all(format!("PRI * HTTP/2.0\r\n\r\n{tail}").as_bytes())
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;

        match keep_open {
            true => {
                assert_eq!(proxy.server.current_server_connections(), 1, "{name}");
                assert_eq!(
                    proxy.server.shutdown(Duration::from_millis(100)).await,
                    Err(ShutdownError::DeadlineExceeded),
                    "{name}"
                );
                proxy.server.close().unwrap();
                let mut byte = [0u8; 1];
                let read = tokio::time::timeout(Duration::from_secs(5), client.read(&mut byte))
                    .await
                    .unwrap();
                assert_eq!(read.unwrap(), 0, "{name}");
            }
            false => {
                drop(client);
                assert_eq!(proxy.server.shutdown(Duration::from_secs(1)).await, Ok(()), "{name}");
            }
        }
        assert_eq!(proxy.server.current_server_connections(), 0, "{name}");
    }
}

async fn upload_stalled_by_the_backend(complete_upload: bool, forced: bool) {
    let (started_tx, started) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(parking_lot::Mutex::new(Some(started_tx)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _backend = serve_handle(
        listener,
        Arc::new(move |request| {
            let started_tx = started_tx.clone();
            Box::pin(async move {
                let mut body = request.into_body();
                body.frame().await;
                if let Some(sender) = started_tx.lock().take() {
                    sender.send(()).ok();
                }
                while body.frame().await.is_some() {}
                status_only(StatusCode::OK)
            })
        }),
    );
    let released = Arc::new(tokio::sync::Notify::new());
    let notifying = released.clone();
    let limit = ConnectionLimit::builder()
        .limiter(Arc::new(ConnectionLimiter::default()))
        .max_limit(Arc::new(|| 10))
        .on_released(Arc::new(move |_| notifying.notify_one()))
        .build();
    let proxy = start_proxy_with(
        orchestrator::proxy::server::ProxySettings {
            idle_timeout: Duration::from_secs(60),
            ..proxy_settings()
        },
        fixed(destination(address, "upload-lifecycle")),
        Some(limit),
    )
    .await;
    let mut client = connect(&proxy).await;
    let content_length = if complete_upload { 1 } else { 1024 };
    client
        .write_all(
            format!("POST /upload HTTP/1.1\r\nHost: sandbox.test\r\nContent-Length: {content_length}\r\n\r\nx")
                .as_bytes(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .expect("backend did not receive the upload")
        .unwrap();
    if complete_upload {
        tokio::time::timeout(Duration::from_secs(5), released.notified())
            .await
            .expect("completed upload did not finish");
    }

    let grace = if forced {
        Duration::ZERO
    } else {
        Duration::from_millis(100)
    };
    let graceful = proxy.server.shutdown(grace).await;
    if !complete_upload {
        assert!(graceful.is_err());
        proxy.server.close().unwrap();
        tokio::time::timeout(Duration::from_secs(5), released.notified())
            .await
            .expect("shutdown left the upload handler blocked");
    }
    assert_eq!(proxy.server.current_server_connections(), 0);
}

#[tokio::test]
async fn closing_interrupts_a_stalled_upload_after_the_deadline() {
    upload_stalled_by_the_backend(false, false).await;
}

#[tokio::test]
async fn forced_close_interrupts_a_stalled_upload() {
    upload_stalled_by_the_backend(false, true).await;
}

#[tokio::test]
async fn a_completed_upload_lets_shutdown_finish() {
    upload_stalled_by_the_backend(true, false).await;
}

#[tokio::test]
async fn idle_keep_alive_connections_are_closed_after_the_idle_timeout() {
    let backend = spawn_backend("backend-idle").await;
    let settings = orchestrator::proxy::server::ProxySettings {
        idle_timeout: Duration::from_millis(100),
        ..proxy_settings()
    };
    let proxy = start_proxy_with(settings, fixed(destination(backend.address, &backend.id)), None).await;
    let mut client = connect(&proxy).await;
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: sandbox.test\r\nContent-Length: 0\r\n\r\n")
        .await
        .unwrap();
    let mut response = Vec::new();
    while !response.ends_with(b"0\r\n\r\n") {
        let mut chunk = [0u8; 512];
        let read = client.read(&mut chunk).await.unwrap();
        assert!(read > 0, "closed before the response ended");
        response.extend_from_slice(&chunk[..read]);
    }
    assert_eq!(proxy.server.current_server_connections(), 1);

    let mut byte = [0u8; 1];
    let closed = tokio::time::timeout(Duration::from_secs(20), client.read(&mut byte))
        .await
        .expect("the idle connection must be closed after the idle timeout plus the downstream buffer");

    assert_eq!(closed.unwrap(), 0);
}

fn short_idle_settings() -> orchestrator::proxy::server::ProxySettings {
    orchestrator::proxy::server::ProxySettings {
        idle_timeout: Duration::from_millis(50),
        ..proxy_settings()
    }
}

#[tokio::test]
async fn a_new_connection_that_sends_nothing_is_not_timed_out() {
    let backend = spawn_backend("backend-silent").await;
    let proxy = start_proxy_with(
        short_idle_settings(),
        fixed(destination(backend.address, &backend.id)),
        None,
    )
    .await;
    let mut client = connect(&proxy).await;

    let mut byte = [0u8; 1];
    let waited = tokio::time::timeout(Duration::from_secs(12), client.read(&mut byte)).await;

    assert!(waited.is_err());
    assert_eq!(proxy.server.current_server_connections(), 1);
}

#[tokio::test]
async fn upgraded_connections_are_not_subject_to_the_idle_timeout() {
    let (address, _finished) = tunnel_backend().await;
    let proxy = start_proxy_with(
        short_idle_settings(),
        fixed(destination(address, "upgraded-connection")),
        None,
    )
    .await;
    let mut client = open_tunnel(&proxy).await;

    tokio::time::sleep(Duration::from_secs(12)).await;

    echo(&mut client).await;
}

fn fixed_body_handle(content_type: Option<&'static str>, body: &'static [u8]) -> Handle {
    Arc::new(move |_| {
        Box::pin(async move {
            let mut response = Response::new(full_body(Bytes::from_static(body)));
            if let Some(content_type) = content_type {
                response
                    .headers_mut()
                    .insert("content-type", content_type.parse().unwrap());
            }
            response
        })
    })
}

async fn content_type_through_the_proxy(declared: Option<&'static str>, body: &'static [u8]) -> Option<String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _backend = serve_handle(listener, fixed_body_handle(declared, body));
    let proxy = start_proxy(fixed(destination(address, "sniff")), None).await;
    let response = client().get(proxy.url("/")).send().await.unwrap();
    response
        .headers()
        .get("content-type")
        .map(|value| value.to_str().unwrap().to_owned())
}

#[tokio::test]
async fn sniffs_the_content_type_of_undeclared_bodies_like_go() {
    assert_eq!(
        content_type_through_the_proxy(None, b"<html>hi</html>")
            .await
            .as_deref(),
        Some("text/html; charset=utf-8")
    );
    assert_eq!(
        content_type_through_the_proxy(None, b"{\"a\":1}").await.as_deref(),
        Some("text/plain; charset=utf-8")
    );
    assert_eq!(
        content_type_through_the_proxy(None, b"\x89PNG\r\n\x1a\n")
            .await
            .as_deref(),
        Some("image/png")
    );
    assert_eq!(
        content_type_through_the_proxy(None, b"\x01\x02\x03").await.as_deref(),
        Some("application/octet-stream")
    );
}

#[tokio::test]
async fn keeps_declared_content_types_and_empty_bodies_untyped() {
    assert_eq!(
        content_type_through_the_proxy(Some("application/json"), b"<html>hi</html>")
            .await
            .as_deref(),
        Some("application/json")
    );
    assert_eq!(content_type_through_the_proxy(None, b"").await, None);
}

async fn serve_with_failing_close(message: &'static str) -> (TestProxy, tokio::task::JoinHandle<()>) {
    let server = orchestrator::proxy::server::ProxyServer::new(
        proxy_settings(),
        refusing(Refusal::Unexpected {
            message: "unused".to_owned(),
        }),
        None,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = server.clone();
    let task = tokio::spawn(async move {
        serving.serve(FailingClose { listener, message }).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    (TestProxy { server, port }, task)
}

#[tokio::test]
async fn closing_reports_a_listener_that_fails_to_close() {
    let (proxy, task) = serve_with_failing_close("listener close failed").await;

    assert_eq!(proxy.server.close(), Err("listener close failed".to_owned()));

    task.await.unwrap();
    assert_eq!(proxy.server.close(), Ok(()), "the listener is forgotten once closed");
}

#[tokio::test]
async fn shutdown_reports_a_listener_that_fails_to_close_after_draining() {
    let (proxy, task) = serve_with_failing_close("listener shutdown failed").await;

    assert_eq!(
        proxy.server.shutdown(Duration::from_secs(1)).await,
        Err(ShutdownError::Listener {
            message: "listener shutdown failed".to_owned()
        })
    );

    task.await.unwrap();
    assert_eq!(proxy.server.close(), Ok(()));
}

async fn stalled_h2c_upload(forced: bool) {
    let (started_tx, started) = tokio::sync::oneshot::channel::<()>();
    let started_tx = Arc::new(parking_lot::Mutex::new(Some(started_tx)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let _backend = serve_handle(
        listener,
        Arc::new(move |request| {
            let started_tx = started_tx.clone();
            Box::pin(async move {
                let mut body = request.into_body();
                body.frame().await;
                if let Some(sender) = started_tx.lock().take() {
                    sender.send(()).ok();
                }
                while body.frame().await.is_some() {}
                status_only(StatusCode::OK)
            })
        }),
    );
    let released = Arc::new(tokio::sync::Notify::new());
    let notifying = released.clone();
    let limit = ConnectionLimit::builder()
        .limiter(Arc::new(ConnectionLimiter::default()))
        .max_limit(Arc::new(|| 10))
        .on_released(Arc::new(move |_| notifying.notify_one()))
        .build();
    let proxy = start_proxy_with(
        orchestrator::proxy::server::ProxySettings {
            idle_timeout: Duration::from_secs(60),
            ..proxy_settings()
        },
        fixed(destination(address, "upload-lifecycle")),
        Some(limit),
    )
    .await;
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel::<Result<Frame<Bytes>, BoxError>>();
    sender.send(Ok(Frame::data(Bytes::from(vec![b'x'; 8192])))).unwrap();
    let stream = connect(&proxy).await;
    let (mut h2, connection) = hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection);
    let request = Request::builder()
        .method("POST")
        .uri(format!("http://127.0.0.1:{}/upload", proxy.port))
        .header("content-length", "8193")
        .body(StreamBody::new(tokio_stream::wrappers::UnboundedReceiverStream::new(
            receiver,
        )))
        .unwrap();
    let client = tokio::spawn(async move { h2.send_request(request).await });
    tokio::time::timeout(Duration::from_secs(5), started)
        .await
        .expect("backend did not receive the upload")
        .unwrap();

    let grace = if forced {
        Duration::ZERO
    } else {
        Duration::from_millis(100)
    };
    assert!(proxy.server.shutdown(grace).await.is_err());
    proxy.server.close().unwrap();

    tokio::time::timeout(Duration::from_secs(5), released.notified())
        .await
        .expect("shutdown left the upload handler blocked");
    assert_eq!(proxy.server.current_server_connections(), 0);
    drop(sender);
    client.abort();
}

#[tokio::test]
async fn closing_interrupts_a_stalled_h2c_upload_after_the_deadline() {
    stalled_h2c_upload(false).await;
}

#[tokio::test]
async fn forced_close_interrupts_a_stalled_h2c_upload() {
    stalled_h2c_upload(true).await;
}

#[tokio::test]
async fn an_idle_h2c_connection_is_closed_after_the_idle_timeout_even_before_any_request() {
    let backend = spawn_backend("backend-h2c-idle").await;
    let proxy = start_proxy_with(
        short_idle_settings(),
        fixed(destination(backend.address, &backend.id)),
        None,
    )
    .await;
    let stream = connect(&proxy).await;
    let (_sender, connection) = hyper::client::conn::http2::handshake::<_, _, http_body_util::Empty<Bytes>>(
        TokioExecutor::new(),
        TokioIo::new(stream),
    )
    .await
    .unwrap();

    tokio::time::timeout(Duration::from_secs(20), connection)
        .await
        .expect("the idle h2c connection must be closed")
        .ok();

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(proxy.server.current_server_connections(), 0);
}

async fn raw_exchange(proxy: &TestProxy, request: Vec<u8>) -> String {
    let mut client = connect(proxy).await;
    client.write_all(&request).await.ok();
    let mut reply = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), client.read_to_end(&mut reply))
        .await
        .unwrap()
        .ok();
    String::from_utf8_lossy(&reply).into_owned()
}

#[tokio::test]
async fn malformed_requests_get_the_go_server_replies() {
    let backend = spawn_backend("backend-malformed").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;

    assert_eq!(
        raw_exchange(&proxy, b"GARBAGE\r\n\r\n".to_vec()).await,
        "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n400 Bad Request"
    );
    let large = format!("GET / HTTP/1.1\r\nHost: x\r\nX: {}\r\n\r\n", "a".repeat(2_000_000));
    assert_eq!(
        raw_exchange(&proxy, large.into_bytes()).await,
        "HTTP/1.1 431 Request Header Fields Too Large\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n431 Request Header Fields Too Large"
    );
    let missing_host = raw_exchange(&proxy, b"GET / HTTP/1.1\r\n\r\n".to_vec()).await;
    assert!(
        missing_host.starts_with("HTTP/1.1 400 Bad Request\r\n"),
        "{missing_host}"
    );
    assert!(
        missing_host.ends_with("400 Bad Request: missing required Host header"),
        "{missing_host}"
    );
    assert_eq!(backend.request_count(), 0);
}

#[tokio::test]
async fn http_one_zero_requests_need_no_host() {
    let backend = spawn_backend("backend-http10").await;
    let proxy = start_proxy(fixed(destination(backend.address, &backend.id)), None).await;

    let reply = raw_exchange(&proxy, b"GET / HTTP/1.0\r\n\r\n".to_vec()).await;

    assert!(reply.starts_with("HTTP/1.0 200 OK"), "{reply}");
}

fn proxy_env_with(pairs: Vec<(&'static str, String)>) -> Arc<orchestrator::proxy::pool::proxy_env::ProxyEnv> {
    Arc::new(orchestrator::proxy::pool::proxy_env::ProxyEnv::from_vars(
        &move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.clone())
        },
    ))
}

async fn environment_proxy(
    reply: &'static [u8],
) -> (
    std::net::SocketAddr,
    tokio::sync::oneshot::Receiver<String>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (seen, request) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let head = read_head(&mut stream).await;
        let _ = seen.send(head);
        stream.write_all(reply).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), stream.read_u8()).await;
    });
    (address, request, serving)
}

#[tokio::test]
async fn dials_a_plain_http_upstream_through_http_proxy_from_the_environment() {
    let (proxy_address, request, _serving) =
        environment_proxy(b"HTTP/1.1 200 OK\r\ncontent-length: 8\r\nconnection: close\r\n\r\nvia-http").await;
    let upstream = destination("10.255.255.1:8080".parse().unwrap(), "through-http-proxy");
    let settings = orchestrator::proxy::server::ProxySettings {
        proxy_env: proxy_env_with(vec![("HTTP_PROXY", format!("http://{proxy_address}"))]),
        ..proxy_settings()
    };
    let proxy = start_proxy_with(settings, fixed(upstream), None).await;

    let response = client().get(proxy.url("/hello")).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "via-http");
    let head = request.await.unwrap();
    assert!(
        head.starts_with("GET http://10.255.255.1:8080/hello HTTP/1.1\r\n"),
        "{head}"
    );
}

#[tokio::test]
async fn dials_an_https_upstream_through_a_connect_tunnel_from_the_environment() {
    let backend = spawn_tls_backend("tunnelled").await;
    let (proxy_address, request, _serving) = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (seen, request) = tokio::sync::oneshot::channel();
        let target = backend.address;
        let serving = tokio::spawn(async move {
            let (mut inbound, _) = listener.accept().await.unwrap();
            let head = read_head(&mut inbound).await;
            let _ = seen.send(head);
            let mut outbound = tokio::net::TcpStream::connect(target).await.unwrap();
            inbound
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
        });
        (address, request, serving)
    };
    let secure = orchestrator::proxy::destination::Destination::builder()
        .scheme(axum::http::uri::Scheme::HTTPS)
        .authority(axum::http::uri::Authority::try_from("10.255.255.2:443").unwrap())
        .sandbox_id("test-sandbox")
        .connection_key("through-connect")
        .insecure_skip_tls_verify(true)
        .build();
    let settings = orchestrator::proxy::server::ProxySettings {
        proxy_env: proxy_env_with(vec![("HTTPS_PROXY", format!("http://{proxy_address}"))]),
        ..proxy_settings()
    };
    let proxy = start_proxy_with(settings, fixed(secure), None).await;

    let response = client().get(proxy.url("/")).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "tunnelled");
    let head = request.await.unwrap();
    assert!(head.starts_with("CONNECT 10.255.255.2:443 HTTP/1.1\r\n"), "{head}");
}

#[tokio::test]
async fn loopback_upstreams_bypass_the_environment_proxy() {
    let backend = spawn_backend("direct").await;
    let dead = reserved_port().await.0;
    let settings = orchestrator::proxy::server::ProxySettings {
        proxy_env: proxy_env_with(vec![("HTTP_PROXY", format!("http://{dead}"))]),
        ..proxy_settings()
    };
    let proxy = start_proxy_with(settings, fixed(destination(backend.address, &backend.id)), None).await;

    let response = client().get(proxy.url("/")).send().await.unwrap();

    assert_backend_output(&backend, response).await;
}

#[tokio::test]
async fn no_proxy_entries_keep_matching_upstreams_off_the_environment_proxy() {
    let (proxy_address, mut request, _serving) =
        environment_proxy(b"HTTP/1.1 500 no\r\ncontent-length: 0\r\n\r\n").await;
    let settings = orchestrator::proxy::server::ProxySettings {
        proxy_env: proxy_env_with(vec![
            ("HTTP_PROXY", format!("http://{proxy_address}")),
            ("NO_PROXY", "10.255.255.0/24".to_owned()),
        ]),
        ..proxy_settings()
    };
    let proxy = start_proxy_with(
        settings,
        fixed(destination("10.255.255.3:8080".parse().unwrap(), "no-proxy")),
        None,
    )
    .await;
    let url = proxy.url("/");
    let pending = tokio::spawn(async move { client().get(url).send().await });

    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        request.try_recv().is_err(),
        "the proxy named in HTTP_PROXY must not have been asked"
    );
    pending.abort();
}
