use std::time::Duration;

use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, InstrumentKind, SdkMeterProvider, Stream};
use opentelemetry_sdk::runtime::Tokio;

use super::endpoint::Endpoint;

const METRIC_EXPORT_PERIOD: Duration = Duration::from_secs(15);
const HISTOGRAM_MAX_BUCKETS: u32 = 160;
const HISTOGRAM_MAX_SCALE: i8 = 20;

pub fn meter_provider(endpoint: &Endpoint, resource: Resource) -> Result<SdkMeterProvider, String> {
    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.url())
        .build()
        .map_err(|e| format!("failed to create metrics exporter: {e}"))?;
    let reader = PeriodicReader::builder(exporter, Tokio)
        .with_interval(METRIC_EXPORT_PERIOD)
        .build();
    Ok(SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .with_view(exponential_histograms)
        .build())
}

fn exponential_histograms(instrument: &Instrument) -> Option<Stream> {
    (instrument.kind() == InstrumentKind::Histogram)
        .then(|| {
            Stream::builder()
                .with_aggregation(Aggregation::Base2ExponentialHistogram {
                    max_size: HISTOGRAM_MAX_BUCKETS,
                    max_scale: HISTOGRAM_MAX_SCALE,
                    record_min_max: true,
                })
                .build()
                .ok()
        })
        .flatten()
}
