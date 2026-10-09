use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::extract::Request;
use axum::http::{HeaderMap, header};
use axum::middleware::Next;
use axum::response::Response;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::Histogram;
use opentelemetry::propagation::Extractor;
use tracing::field::Empty;
use tracing::{Instrument, Span, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

const METER_NAME: &str = "go.opentelemetry.io/contrib/instrumentation/google.golang.org/grpc/otelgrpc";
const GRPC_CONTENT_TYPE: &str = "application/grpc";
const GRPC_STATUS_HEADER: &str = "grpc-status";
const GRPC_OK: i32 = 0;

static SERVER_DURATION: LazyLock<Histogram<f64>> = LazyLock::new(|| {
    global::meter(METER_NAME)
        .f64_histogram("rpc.server.duration")
        .with_description("Measures the duration of inbound RPC.")
        .with_unit("ms")
        .build()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrpcCall {
    pub service: String,
    pub method: String,
}

impl GrpcCall {
    pub fn of(request: &Request) -> Option<Self> {
        request
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .filter(|content_type| content_type.starts_with(GRPC_CONTENT_TYPE))
            .and_then(|_| Self::parse(request.uri().path()))
    }

    pub fn parse(path: &str) -> Option<Self> {
        path.strip_prefix('/')
            .and_then(|path| path.split_once('/'))
            .map(|(service, method)| Self {
                service: service.to_owned(),
                method: method.to_owned(),
            })
    }

    fn name(&self) -> String {
        format!("{}/{}", self.service, self.method)
    }

    fn span(&self) -> Span {
        info_span!(
            "grpc",
            otel.name = %self.name(),
            otel.kind = "server",
            otel.status_code = Empty,
            rpc.system = "grpc",
            rpc.service = %self.service,
            rpc.method = %self.method,
            rpc.grpc.status_code = Empty,
        )
    }

    fn finish(&self, span: &Span, status: i32, took: Duration) {
        span.record("rpc.grpc.status_code", i64::from(status));
        if is_server_error(status) {
            span.record("otel.status_code", "ERROR");
        }
        SERVER_DURATION.record(
            took.as_secs_f64() * 1000.0,
            &[
                KeyValue::new("rpc.system", "grpc"),
                KeyValue::new("rpc.service", self.service.clone()),
                KeyValue::new("rpc.method", self.method.clone()),
                KeyValue::new("rpc.grpc.status_code", i64::from(status)),
            ],
        );
    }
}

fn is_server_error(status: i32) -> bool {
    matches!(status, 2 | 4 | 12 | 13 | 14 | 15)
}

struct Headers<'a>(&'a HeaderMap);

impl Extractor for Headers<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|name| name.as_str()).collect()
    }
}

fn grpc_status(response: &Response) -> i32 {
    response
        .headers()
        .get(GRPC_STATUS_HEADER)
        .and_then(|value| value.to_str().ok())
        .and_then(|status| status.parse().ok())
        .unwrap_or(GRPC_OK)
}

pub async fn trace_grpc(request: Request, next: Next) -> Response {
    let Some(call) = GrpcCall::of(&request) else {
        return next.run(request).await;
    };
    let span = call.span();
    let parent = global::get_text_map_propagator(|propagator| propagator.extract(&Headers(request.headers())));
    let _ = span.set_parent(parent);
    let started = Instant::now();
    let response = next.run(request).instrument(span.clone()).await;
    call.finish(&span, grpc_status(&response), started.elapsed());
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_call_is_split_into_service_and_method() {
        assert_eq!(
            GrpcCall::parse("/orchestrator.SandboxService/Create"),
            Some(GrpcCall {
                service: "orchestrator.SandboxService".to_owned(),
                method: "Create".to_owned()
            })
        );
        assert_eq!(GrpcCall::parse("/health"), None);
    }

    #[test]
    fn only_server_side_codes_mark_the_span_as_an_error() {
        assert!(is_server_error(13));
        assert!(!is_server_error(5));
        assert!(!is_server_error(0));
    }
}
