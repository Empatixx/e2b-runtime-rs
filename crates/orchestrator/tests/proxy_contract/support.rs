#![allow(dead_code)]

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use axum::http::uri::{Authority, Scheme};
use bytes::Bytes;
use futures::future::BoxFuture;
use http_body_util::{BodyExt, StreamBody};
use hyper::body::{Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto;
use orchestrator::proxy::body::{BoxError, ProxyBody, empty_body};
use orchestrator::proxy::destination::Destination;
use orchestrator::proxy::handler::Resolver;
use orchestrator::proxy::limit::ConnectionLimit;
use orchestrator::proxy::server::{ProxyServer, ProxySettings};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::crypto::ring;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub const BODY_WRITE_DELAY_HEADER: &str = "body-write-delay";

pub type Handle = Arc<dyn Fn(Request<Incoming>) -> BoxFuture<'static, Response<ProxyBody>> + Send + Sync>;

pub struct Backend {
    pub address: SocketAddr,
    pub id: String,
    pub requests: Arc<AtomicU64>,
    cancelled: Arc<AtomicBool>,
    server: Option<JoinHandle<()>>,
}

impl Backend {
    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::SeqCst)
    }

    pub async fn interrupt(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(server) = self.server.take() {
            server.abort();
            drop(server.await);
        }
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        self.server.iter().for_each(JoinHandle::abort);
    }
}

pub async fn spawn_backend(id: &str) -> Backend {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_backend(listener, id, None)
}

pub async fn spawn_tls_backend(id: &str) -> Backend {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    serve_backend(listener, id, Some(tls_acceptor()))
}

pub fn serve_backend(listener: TcpListener, id: &str, tls: Option<TlsAcceptor>) -> Backend {
    let requests = Arc::new(AtomicU64::new(0));
    let cancelled = Arc::new(AtomicBool::new(false));
    let address = listener.local_addr().unwrap();
    let handle = identifying_handle(id.to_owned(), requests.clone(), cancelled.clone());
    Backend {
        address,
        id: id.to_owned(),
        requests,
        cancelled,
        server: Some(tokio::spawn(accept_loop(listener, tls, handle))),
    }
}

pub fn serve_handle(listener: TcpListener, handle: Handle) -> JoinHandle<()> {
    tokio::spawn(accept_loop(listener, None, handle))
}

fn identifying_handle(id: String, requests: Arc<AtomicU64>, cancelled: Arc<AtomicBool>) -> Handle {
    Arc::new(move |request| {
        let (id, requests, cancelled) = (id.clone(), requests.clone(), cancelled.clone());
        Box::pin(async move {
            if cancelled.load(Ordering::SeqCst) {
                return status_only(StatusCode::BAD_GATEWAY);
            }
            requests.fetch_add(1, Ordering::SeqCst);
            let delay = request
                .headers()
                .get(BODY_WRITE_DELAY_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .map_or(Duration::ZERO, Duration::from_millis);
            let chunk = futures::stream::once(async move {
                tokio::time::sleep(delay).await;
                Ok::<_, BoxError>(Frame::data(Bytes::from(id)))
            });
            Response::new(StreamBody::new(chunk).boxed_unsync())
        })
    })
}

pub fn status_only(status: StatusCode) -> Response<ProxyBody> {
    let mut response = Response::new(empty_body());
    *response.status_mut() = status;
    response
}

async fn accept_loop(listener: TcpListener, tls: Option<TlsAcceptor>, handle: Handle) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(serve_connection(stream, tls.clone(), handle.clone()));
    }
}

async fn serve_connection(stream: TcpStream, tls: Option<TlsAcceptor>, handle: Handle) {
    let service = service_fn(move |request| {
        let answer = handle(request);
        async move { Ok::<_, Infallible>(answer.await) }
    });
    let builder = auto::Builder::new(TokioExecutor::new());
    match tls {
        Some(acceptor) => {
            if let Ok(stream) = acceptor.accept(stream).await {
                drop(builder.serve_connection(TokioIo::new(stream), service).await);
            }
        }
        None => drop(
            builder
                .serve_connection_with_upgrades(TokioIo::new(stream), service)
                .await,
        ),
    }
}

pub fn tls_acceptor() -> TlsAcceptor {
    let certificates = CertificateDer::pem_slice_iter(include_bytes!("../fixtures/proxy/self-signed-cert.pem"))
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = PrivateKeyDer::from_pem_slice(include_bytes!("../fixtures/proxy/self-signed-key.pem")).unwrap();
    let config = ServerConfig::builder_with_provider(Arc::new(ring::default_provider()))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certificates, key)
        .unwrap();
    TlsAcceptor::from(Arc::new(config))
}

pub struct TestProxy {
    pub server: Arc<ProxyServer>,
    pub port: u16,
}

impl TestProxy {
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        drop(self.server.close());
    }
}

pub fn proxy_settings() -> ProxySettings {
    ProxySettings {
        max_connection_attempts: 5,
        idle_timeout: Duration::from_secs(20),
        disable_keep_alives: false,
        proxy_env: Arc::default(),
    }
}

pub async fn start_proxy(resolve: Resolver, limit: Option<ConnectionLimit>) -> TestProxy {
    start_proxy_with(proxy_settings(), resolve, limit).await
}

pub async fn start_proxy_with(settings: ProxySettings, resolve: Resolver, limit: Option<ConnectionLimit>) -> TestProxy {
    let server = ProxyServer::new(settings, resolve, limit);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let serving = server.clone();
    tokio::spawn(async move { serving.serve(listener).await });
    TestProxy { server, port }
}

pub fn destination(address: SocketAddr, connection_key: &str) -> Destination {
    Destination::builder()
        .scheme(Scheme::HTTP)
        .authority(Authority::try_from(address.to_string()).unwrap())
        .sandbox_id("test-sandbox")
        .connection_key(connection_key)
        .build()
}

pub fn fixed(destination: Destination) -> Resolver {
    Arc::new(move |_| Ok(destination.clone()))
}

pub fn refusing(refusal: orchestrator::proxy::refusal::Refusal) -> Resolver {
    Arc::new(move |_| Err(refusal.clone()))
}

pub async fn connect(proxy: &TestProxy) -> TcpStream {
    TcpStream::connect(("127.0.0.1", proxy.port)).await.unwrap()
}

pub async fn read_head(stream: &mut TcpStream) -> String {
    use tokio::io::AsyncReadExt;
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        head.push(stream.read_u8().await.unwrap());
    }
    String::from_utf8(head).unwrap()
}

pub async fn reserved_port() -> (SocketAddr, impl FnOnce() -> TcpListener) {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_reuseaddr(true).unwrap();
    socket.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    let address = socket.local_addr().unwrap();
    (address, move || socket.listen(128).unwrap())
}

pub fn echo_handle() -> Handle {
    Arc::new(|request| {
        Box::pin(async move {
            let (parts, body) = request.into_parts();
            let body = body
                .collect()
                .await
                .map(|collected| collected.to_bytes())
                .unwrap_or_default();
            let headers = parts
                .headers
                .iter()
                .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap_or_default().to_owned()))
                .collect::<std::collections::BTreeMap<_, _>>();
            let echoed = serde_json::json!({
                "method": parts.method.as_str(),
                "uri": parts.uri.to_string(),
                "headers": headers,
                "body": String::from_utf8_lossy(&body),
            });
            let mut response = Response::new(
                http_body_util::Full::new(Bytes::from(echoed.to_string()))
                    .map_err(|never| match never {})
                    .boxed_unsync(),
            );
            response
                .headers_mut()
                .insert("keep-alive", "timeout=5".parse().unwrap());
            response
        })
    })
}

pub async fn spawn_echo_backend() -> (SocketAddr, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    (listener.local_addr().unwrap(), serve_handle(listener, echo_handle()))
}

pub struct FailingClose {
    pub listener: TcpListener,
    pub message: &'static str,
}

impl orchestrator::proxy::server::Acceptor for FailingClose {
    fn accept(&self) -> BoxFuture<'_, std::io::Result<TcpStream>> {
        Box::pin(async move { self.listener.accept().await.map(|(stream, _)| stream) })
    }

    fn close(&self) -> std::io::Result<()> {
        Err(std::io::Error::other(self.message))
    }
}
