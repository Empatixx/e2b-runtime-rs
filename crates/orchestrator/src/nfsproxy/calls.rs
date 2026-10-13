use std::convert::Infallible;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use nfsserve::nfs::nfsstat3;
use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};
use tracing::field::Empty;
use tracing::{Instrument, Span};
use uuid::Uuid;

use crate::server::config::nfs_proxy::NfsProxySettings;
use crate::volumes::chrooted::RenameError;
use crate::volumes::chrooted::error::PathError;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/nfsproxy/metrics";
const CALLS_NAME: &str = "orchestrator.nfsproxy.calls.total";
const DURATION_NAME: &str = "orchestrator.nfsproxy.call.duration";
const SUCCESS: &str = "success";
const CLIENT_ERROR: &str = "client_error";
const OTHER_ERROR: &str = "other_error";

struct CallMetrics {
    calls: Counter<u64>,
    duration: Histogram<u64>,
}

static GLOBAL: LazyLock<Arc<CallMetrics>> = LazyLock::new(|| Arc::new(CallMetrics::new(&global::meter(METER_NAME))));

impl CallMetrics {
    fn new(meter: &Meter) -> Self {
        Self {
            calls: meter
                .u64_counter(CALLS_NAME)
                .with_description("Total number of calls to the NFS proxy")
                .with_unit("1")
                .build(),
            duration: meter
                .u64_histogram(DURATION_NAME)
                .with_description("Duration of calls to the NFS proxy")
                .with_unit("us")
                .build(),
        }
    }

    fn count(&self, operation: &'static str, failure: Option<&impl Failure>, elapsed: Duration) {
        let attributes = [
            KeyValue::new("operation", operation),
            KeyValue::new("result", result_of(failure)),
        ];
        self.calls.add(1, &attributes);
        self.duration
            .record(u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX), &attributes);
    }
}

pub trait Failure {
    fn by_client(&self) -> bool;
    fn describe(&self) -> String;
}

impl Failure for PathError {
    fn by_client(&self) -> bool {
        self.is_not_found() || self.is_exist()
    }

    fn describe(&self) -> String {
        self.to_string()
    }
}

impl Failure for RenameError {
    fn by_client(&self) -> bool {
        match self {
            RenameError::Path(path) => path.by_client(),
            RenameError::Link(link) => link.errno == rustix::io::Errno::NOENT || link.errno == rustix::io::Errno::EXIST,
        }
    }

    fn describe(&self) -> String {
        self.to_string()
    }
}

impl Failure for nfsstat3 {
    fn by_client(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        format!("{self:?}")
    }
}

impl Failure for Infallible {
    fn by_client(&self) -> bool {
        match *self {}
    }

    fn describe(&self) -> String {
        match *self {}
    }
}

struct Recording {
    operation: &'static str,
    span: Option<Span>,
    request_id: Option<Uuid>,
    start: Instant,
}

#[derive(Default, Clone)]
pub struct Calls {
    metrics: Option<Arc<CallMetrics>>,
    logging: bool,
    tracing: bool,
    record_stats: bool,
    record_handles: bool,
}

impl Calls {
    pub fn new(settings: &NfsProxySettings) -> Self {
        Self {
            metrics: settings.metrics.then(|| GLOBAL.clone()),
            logging: settings.logging,
            tracing: settings.tracing,
            record_stats: settings.record_stat_calls,
            record_handles: settings.record_handle_calls,
        }
    }

    pub fn record<T, E: Failure>(
        &self,
        operation: &'static str,
        subject: &str,
        call: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        let recording = self.begin(operation, subject);
        let outcome = match &recording.span {
            Some(span) => span.in_scope(call),
            None => call(),
        };
        self.finish(recording, subject, &outcome);
        outcome
    }

    pub async fn record_async<T, E: Failure>(
        &self,
        operation: &'static str,
        subject: &str,
        call: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let recording = self.begin(operation, subject);
        let outcome = match recording.span.clone() {
            Some(span) => call.instrument(span).await,
            None => call.await,
        };
        self.finish(recording, subject, &outcome);
        outcome
    }

    fn begin(&self, operation: &'static str, subject: &str) -> Recording {
        Recording {
            operation,
            span: self.tracing.then(|| {
                tracing::info_span!(
                    "nfs proxy call",
                    otel.name = operation,
                    nfs.filename = subject,
                    otel.status_code = Empty,
                    otel.status_message = Empty,
                    error = Empty,
                )
            }),
            request_id: self.logging.then(|| log_start(operation)),
            start: Instant::now(),
        }
    }

    fn finish<T, E: Failure>(&self, recording: Recording, subject: &str, outcome: &Result<T, E>) {
        let elapsed = recording.start.elapsed();
        let failure = outcome.as_ref().err();
        if let Some(metrics) = &self.metrics {
            metrics.count(recording.operation, failure, elapsed);
        }
        if let Some(request_id) = recording.request_id {
            log_end(recording.operation, subject, request_id, elapsed, failure);
        }
        if let (Some(span), Some(failure)) = (&recording.span, failure) {
            mark_failed(span, failure);
        }
    }

    pub fn stat<T, E: Failure>(
        &self,
        operation: &'static str,
        subject: &str,
        call: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        match self.record_stats {
            true => self.record(operation, subject, call),
            false => call(),
        }
    }

    pub fn handle<T, E: Failure>(
        &self,
        operation: &'static str,
        subject: &str,
        call: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E> {
        match self.record_handles {
            true => self.record(operation, subject, call),
            false => call(),
        }
    }
}

fn result_of(failure: Option<&impl Failure>) -> &'static str {
    match failure {
        None => SUCCESS,
        Some(failure) if failure.by_client() => CLIENT_ERROR,
        Some(_) => OTHER_ERROR,
    }
}

fn log_start(operation: &'static str) -> Uuid {
    let request_id = Uuid::new_v4();
    tracing::debug!(requestID = %request_id, operation, "[nfs proxy] {operation}: start");
    request_id
}

fn log_end(
    operation: &'static str,
    subject: &str,
    request_id: Uuid,
    elapsed: Duration,
    failure: Option<&impl Failure>,
) {
    match failure {
        None => {
            tracing::debug!(requestID = %request_id, dur = ?elapsed, args = subject, "[nfs proxy] {operation}: end")
        }
        Some(failure) => tracing::warn!(
            requestID = %request_id,
            dur = ?elapsed,
            args = subject,
            error = failure.describe(),
            "[nfs proxy] {operation}: end"
        ),
    }
}

fn mark_failed(span: &Span, failure: &impl Failure) {
    let description = failure.describe();
    span.record("error", description.as_str());
    if !failure.by_client() {
        span.record("otel.status_code", "ERROR");
        span.record("otel.status_message", description.as_str());
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};
    use rustix::io::Errno;

    use super::*;

    fn counted(exporter: &InMemoryMetricExporter) -> Vec<(String, String, u64)> {
        let mut points: Vec<(String, String, u64)> = exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|resource| resource.scope_metrics().collect::<Vec<_>>())
            .flat_map(|scope| scope.metrics().collect::<Vec<_>>())
            .filter(|metric| metric.name() == CALLS_NAME)
            .flat_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum
                    .data_points()
                    .map(|point| {
                        let attribute = |key: &str| {
                            point
                                .attributes()
                                .find(|kv| kv.key.as_str() == key)
                                .map(|kv| kv.value.to_string())
                                .unwrap_or_default()
                        };
                        (attribute("operation"), attribute("result"), point.value())
                    })
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect();
        points.sort();
        points
    }

    #[test]
    fn calls_are_counted_by_operation_and_result_like_go() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let calls = Calls {
            metrics: Some(Arc::new(CallMetrics::new(&provider.meter(METER_NAME)))),
            ..Calls::default()
        };

        calls.record("FS.ReadDir", "/", || Ok::<_, PathError>(())).unwrap();
        calls
            .record("FS.Remove", "/x", || Err::<(), _>(path_error(Errno::NOENT)))
            .unwrap_err();
        calls
            .record("FS.Remove", "/y", || Err::<(), _>(path_error(Errno::IO)))
            .unwrap_err();
        calls.stat("FS.Lstat", "/", || Ok::<_, PathError>(())).unwrap();
        provider.force_flush().unwrap();

        assert_eq!(
            counted(&exporter),
            vec![
                ("FS.ReadDir".to_owned(), "success".to_owned(), 1),
                ("FS.Remove".to_owned(), "client_error".to_owned(), 1),
                ("FS.Remove".to_owned(), "other_error".to_owned(), 1),
            ]
        );
    }

    fn path_error(errno: Errno) -> PathError {
        PathError::builder().operation("lstat").path("/x").errno(errno).build()
    }

    #[test]
    fn missing_and_existing_paths_are_client_errors_like_go() {
        assert_eq!(result_of(None::<&PathError>), SUCCESS);
        assert_eq!(result_of(Some(&path_error(Errno::NOENT))), CLIENT_ERROR);
        assert_eq!(result_of(Some(&path_error(Errno::EXIST))), CLIENT_ERROR);
        assert_eq!(result_of(Some(&path_error(Errno::NOTEMPTY))), CLIENT_ERROR);
        assert_eq!(result_of(Some(&path_error(Errno::ACCESS))), OTHER_ERROR);
        assert_eq!(result_of(Some(&nfsstat3::NFS3ERR_STALE)), OTHER_ERROR);
    }

    #[test]
    fn stat_and_handle_calls_run_even_when_not_recorded() {
        let calls = Calls::default();
        assert_eq!(calls.stat("FS.Lstat", "/x", || Ok::<_, Infallible>(1)), Ok(1));
        assert!(matches!(
            calls.handle("NFS.FromHandle", "", || Err::<u8, _>(nfsstat3::NFS3ERR_STALE)),
            Err(nfsstat3::NFS3ERR_STALE)
        ));
    }
}
