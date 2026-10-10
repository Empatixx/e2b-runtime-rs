use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use hyper::Uri;
use hyper::rt::{Read, ReadBufCursor, Write};
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioIo;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Histogram, Meter};
use opentelemetry::propagation::Injector;
use parking_lot::Mutex;
use tokio::net::TcpStream;
use tonic::body::Body;
use tonic::codegen::http::{HeaderMap, HeaderName, HeaderValue, Request, Response};
use tonic::codegen::{Bytes, Service};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Status};
use tracing::field::Empty;
use tracing::{Span, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::telemetry::grpc::GrpcCall;

const GRPC_METER: &str = "github.com/e2b-dev/infra/packages/shared/pkg/grpc";
const OTELGRPC_METER: &str = "go.opentelemetry.io/contrib/instrumentation/google.golang.org/grpc/otelgrpc";
const PEER_TARGET: &str = "peer-orchestrator";
const GRPC_STATUS_HEADER: &str = "grpc-status";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectivityState {
    Idle,
    Connecting,
    Ready,
    TransientFailure,
    Shutdown,
}

impl ConnectivityState {
    pub fn name(self) -> &'static str {
        match self {
            ConnectivityState::Idle => "IDLE",
            ConnectivityState::Connecting => "CONNECTING",
            ConnectivityState::Ready => "READY",
            ConnectivityState::TransientFailure => "TRANSIENT_FAILURE",
            ConnectivityState::Shutdown => "SHUTDOWN",
        }
    }
}

struct CurrentState {
    state: ConnectivityState,
    since: Instant,
}

pub struct ConnectionObserver {
    target: String,
    state_duration: Histogram<u64>,
    current: Mutex<CurrentState>,
}

static STATE_DURATION: LazyLock<Histogram<u64>> = LazyLock::new(|| state_duration(&global::meter(GRPC_METER)));

fn state_duration(meter: &Meter) -> Histogram<u64> {
    meter
        .u64_histogram("grpc.client.connection.state.duration")
        .with_description("Time spent in each gRPC client connectivity state before transitioning")
        .with_unit("ms")
        .build()
}

impl ConnectionObserver {
    pub fn new(target: &str) -> Arc<Self> {
        Self::with_histogram(target, STATE_DURATION.clone())
    }

    pub fn with_histogram(target: &str, state_duration: Histogram<u64>) -> Arc<Self> {
        let target = match target.trim() {
            "" => "unknown",
            target => target,
        };
        Arc::new(Self {
            target: target.to_owned(),
            state_duration,
            current: Mutex::new(CurrentState {
                state: ConnectivityState::Idle,
                since: Instant::now(),
            }),
        })
    }

    pub fn transition(&self, next: ConnectivityState) {
        let mut current = self.current.lock();
        if current.state == next || current.state == ConnectivityState::Shutdown {
            return;
        }
        self.state_duration.record(
            u64::try_from(current.since.elapsed().as_millis()).unwrap_or(u64::MAX),
            &[
                KeyValue::new("grpc.target", self.target.clone()),
                KeyValue::new("grpc.connectivity.state", current.state.name()),
            ],
        );
        *current = CurrentState {
            state: next,
            since: Instant::now(),
        };
    }

    pub fn shutdown(&self) {
        self.transition(ConnectivityState::Shutdown);
    }
}

#[derive(Clone)]
struct ObservedConnector {
    inner: HttpConnector,
    observer: Arc<ConnectionObserver>,
}

impl Service<Uri> for ObservedConnector {
    type Response = ObservedIo;
    type Error = <HttpConnector as Service<Uri>>::Error;
    type Future = Pin<Box<dyn Future<Output = Result<ObservedIo, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, uri: Uri) -> Self::Future {
        let observer = self.observer.clone();
        observer.transition(ConnectivityState::Connecting);
        let connecting = self.inner.call(uri);
        Box::pin(async move {
            match connecting.await {
                Ok(io) => {
                    observer.transition(ConnectivityState::Ready);
                    Ok(ObservedIo { inner: io, observer })
                }
                Err(error) => {
                    observer.transition(ConnectivityState::TransientFailure);
                    Err(error)
                }
            }
        })
    }
}

struct ObservedIo {
    inner: TokioIo<TcpStream>,
    observer: Arc<ConnectionObserver>,
}

impl Drop for ObservedIo {
    fn drop(&mut self) {
        self.observer.transition(ConnectivityState::Idle);
    }
}

impl Read for ObservedIo {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: ReadBufCursor<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl Write for ObservedIo {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write_vectored(cx, bufs)
    }
}

static CLIENT_DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| client_duration(&global::meter(OTELGRPC_METER)));

fn client_duration(meter: &Meter) -> Histogram<f64> {
    meter
        .f64_histogram("rpc.client.duration")
        .with_description("Measures the duration of outbound RPC.")
        .with_unit("ms")
        .build()
}

#[derive(Clone)]
pub struct ObservedChannel {
    inner: Channel,
    server_address: Arc<str>,
    duration: Histogram<f64>,
}

pub struct OpenedChannel {
    pub channel: ObservedChannel,
    pub observer: Arc<ConnectionObserver>,
}

impl ObservedChannel {
    pub fn open(address: &str) -> Result<OpenedChannel, String> {
        let observer = ConnectionObserver::new(PEER_TARGET);
        Self::open_with(address, observer, CLIENT_DURATION.clone())
    }

    pub fn open_with(
        address: &str,
        observer: Arc<ConnectionObserver>,
        duration: Histogram<f64>,
    ) -> Result<OpenedChannel, String> {
        let endpoint = Endpoint::from_shared(format!("http://{address}"))
            .map_err(|e| format!("failed to dial peer {address}: {e}"))?;
        let mut http = HttpConnector::new();
        http.enforce_http(true);
        http.set_connect_timeout(Some(CONNECT_TIMEOUT));
        let connector = ObservedConnector {
            inner: http,
            observer: observer.clone(),
        };
        let inner = endpoint
            .connect_timeout(CONNECT_TIMEOUT)
            .connect_with_connector_lazy(connector);
        Ok(OpenedChannel {
            channel: Self {
                inner,
                server_address: address.into(),
                duration,
            },
            observer,
        })
    }

    pub fn plain(inner: Channel, address: &str, duration: Histogram<f64>) -> Self {
        Self {
            inner,
            server_address: address.into(),
            duration,
        }
    }
}

impl Service<Request<Body>> for ObservedChannel {
    type Response = Response<ObservedBody>;
    type Error = tonic::transport::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request<Body>) -> Self::Future {
        let pending = GrpcCall::parse(request.uri().path())
            .map(|call| PendingCall::start(call, &self.server_address, self.duration.clone()));
        if let Some(pending) = &pending {
            inject_trace_context(&pending.span, request.headers_mut());
        }
        let responding = self.inner.call(request);
        Box::pin(async move {
            let response = match responding.await {
                Ok(response) => response,
                Err(error) => {
                    if let Some(pending) = pending {
                        pending.finish(Code::Unavailable as i32);
                    }
                    return Err(error);
                }
            };
            let trailers_only = response
                .headers()
                .get(GRPC_STATUS_HEADER)
                .and_then(|value| value.to_str().ok())
                .and_then(|status| status.parse::<i32>().ok());
            let call = match (pending, trailers_only) {
                (Some(pending), Some(status)) => {
                    pending.finish(status);
                    None
                }
                (pending, _) => pending,
            };
            Ok(response.map(|inner| ObservedBody { inner, call }))
        })
    }
}

struct PendingCall {
    call: GrpcCall,
    server_address: Arc<str>,
    duration: Histogram<f64>,
    started: Instant,
    span: Span,
}

impl PendingCall {
    fn start(call: GrpcCall, server_address: &Arc<str>, duration: Histogram<f64>) -> Self {
        let span = info_span!(
            "grpc",
            otel.name = %format!("{}/{}", call.service, call.method),
            otel.kind = "client",
            otel.status_code = Empty,
            rpc.system = "grpc",
            rpc.service = %call.service,
            rpc.method = %call.method,
            rpc.grpc.status_code = Empty,
        );
        Self {
            call,
            server_address: server_address.clone(),
            duration,
            started: Instant::now(),
            span,
        }
    }

    fn finish(self, status: i32) {
        self.span.record("rpc.grpc.status_code", i64::from(status));
        if status != 0 {
            self.span.record("otel.status_code", "ERROR");
        }
        let (host, port) = self
            .server_address
            .rsplit_once(':')
            .unwrap_or((&self.server_address, ""));
        let mut attributes = vec![
            KeyValue::new("rpc.system", "grpc"),
            KeyValue::new("rpc.service", self.call.service.clone()),
            KeyValue::new("rpc.method", self.call.method.clone()),
            KeyValue::new("rpc.grpc.status_code", i64::from(status)),
            KeyValue::new("server.address", host.to_owned()),
        ];
        if let Ok(port) = port.parse::<i64>() {
            attributes.push(KeyValue::new("server.port", port));
        }
        self.duration
            .record(self.started.elapsed().as_secs_f64() * 1000.0, &attributes);
    }
}

pub struct ObservedBody {
    inner: Body,
    call: Option<PendingCall>,
}

impl ObservedBody {
    fn finish(&mut self, status: i32) {
        if let Some(call) = self.call.take() {
            call.finish(status);
        }
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        self.finish(Code::Cancelled as i32);
    }
}

impl tonic::codegen::Body for ObservedBody {
    type Data = Bytes;
    type Error = Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, Status>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(trailers) = frame.trailers_ref() {
                    self.finish(status_of(trailers));
                }
            }
            Poll::Ready(Some(Err(status))) => self.finish(status.code() as i32),
            Poll::Ready(None) => self.finish(0),
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

fn status_of(trailers: &HeaderMap) -> i32 {
    trailers
        .get(GRPC_STATUS_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|status| status.parse().ok())
        .unwrap_or(0)
}

struct HeaderInjector<'a>(&'a mut HeaderMap);

impl Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let (Ok(name), Ok(value)) = (HeaderName::from_bytes(key.as_bytes()), HeaderValue::from_str(&value)) {
            self.0.insert(name, value);
        }
    }
}

fn inject_trace_context(span: &Span, headers: &mut HeaderMap) {
    let context = span.context();
    global::get_text_map_propagator(|propagator| propagator.inject_context(&context, &mut HeaderInjector(headers)));
}
