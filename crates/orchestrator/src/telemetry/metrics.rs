use std::time::Duration;

use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::metrics::{Aggregation, Instrument, InstrumentKind, SdkMeterProvider, Stream};
use opentelemetry_sdk::runtime::Tokio;

use super::endpoint::Endpoint;
use super::server::network::{EGRESS_RETIREMENT_DURATION, SLOT_RETURN_DURATION};

const METRIC_EXPORT_PERIOD: Duration = Duration::from_secs(15);
const HISTOGRAM_MAX_BUCKETS: u32 = 160;
const HISTOGRAM_MAX_SCALE: i8 = 20;
const EXPLICIT_HISTOGRAMS: [&str; 2] = [SLOT_RETURN_DURATION, EGRESS_RETIREMENT_DURATION];
const EXPLICIT_BOUNDARIES: [f64; 14] = [
    0.0, 1.0, 10.0, 100.0, 500.0, 1000.0, 3000.0, 5000.0, 10000.0, 30000.0, 60000.0, 120000.0, 300000.0, 600000.0,
];

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
                .with_aggregation(histogram_aggregation(instrument.name()))
                .build()
                .ok()
        })
        .flatten()
}

fn histogram_aggregation(name: &str) -> Aggregation {
    match EXPLICIT_HISTOGRAMS.contains(&name) {
        true => Aggregation::ExplicitBucketHistogram {
            boundaries: EXPLICIT_BOUNDARIES.to_vec(),
            record_min_max: true,
        },
        false => Aggregation::Base2ExponentialHistogram {
            max_size: HISTOGRAM_MAX_BUCKETS,
            max_scale: HISTOGRAM_MAX_SCALE,
            record_min_max: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader};

    use super::*;

    #[test]
    fn the_network_release_histograms_export_explicit_buckets() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(exponential_histograms)
            .build();
        let meter = provider.meter("test");
        [SLOT_RETURN_DURATION, EGRESS_RETIREMENT_DURATION]
            .into_iter()
            .for_each(|name| meter.u64_histogram(name).build().record(60_001, &[]));
        meter.u64_histogram("other.duration").build().record(5, &[]);

        provider.force_flush().unwrap();

        let finished = exporter.get_finished_metrics().unwrap();
        let metrics: Vec<_> = finished
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .collect();
        let explicit: Vec<_> = metrics
            .iter()
            .filter_map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Histogram(histogram)) => Some((
                    metric.name().to_owned(),
                    histogram.data_points().map(|point| point.sum()).sum::<u64>(),
                    histogram
                        .data_points()
                        .flat_map(|point| point.bounds().collect::<Vec<_>>())
                        .collect::<Vec<_>>(),
                )),
                _ => None,
            })
            .collect();
        assert_eq!(explicit.len(), 2);
        explicit.iter().for_each(|(name, sum, bounds)| {
            assert!(EXPLICIT_HISTOGRAMS.contains(&name.as_str()));
            assert_eq!(*sum, 60_001);
            assert!(bounds.contains(&60_000.0) && bounds.contains(&120_000.0));
        });
        assert!(metrics.iter().any(|metric| {
            metric.name() == "other.duration"
                && matches!(
                    metric.data(),
                    AggregatedMetrics::U64(MetricData::ExponentialHistogram(_))
                )
        }));
    }
}
