use std::future::Future;

use tonic::Status;
use tracing::field::Empty;
use tracing::{Instrument, info_span};

const OK: &str = "OK";
const ERROR: &str = "ERROR";

pub async fn traced<T>(name: &'static str, work: impl Future<Output = Result<T, Status>>) -> Result<T, Status> {
    let span = info_span!(
        "volume",
        otel.name = name,
        otel.status_code = Empty,
        otel.status_description = Empty
    );
    let result = work.instrument(span.clone()).await;
    match &result {
        Ok(_) => span.record("otel.status_code", OK),
        Err(status) => span
            .record("otel.status_code", ERROR)
            .record("otel.status_description", status.message()),
    };
    result
}
