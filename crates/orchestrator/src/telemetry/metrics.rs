use std::sync::{Arc, Weak};
use std::time::Duration;

use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::metrics::data::ResourceMetrics;
use opentelemetry_sdk::metrics::exporter::PushMetricExporter;
use opentelemetry_sdk::metrics::periodic_reader_with_async_runtime::PeriodicReader;
use opentelemetry_sdk::metrics::reader::MetricReader;
use opentelemetry_sdk::metrics::{
    Aggregation, Instrument, InstrumentKind, ManualReader, Pipeline, SdkMeterProvider, Stream, Temporality,
};
use opentelemetry_sdk::runtime::Tokio;

use super::endpoint::Endpoint;
use super::server::network::{EGRESS_RETIREMENT_DURATION, SLOT_RETURN_DURATION};

const METRIC_EXPORT_PERIOD: Duration = Duration::from_secs(15);
const HISTOGRAM_MAX_BUCKETS: u32 = 160;
const LIFTED_CARDINALITY_LIMIT: usize = 20_000;
const HISTOGRAM_MAX_SCALE: i8 = 20;
const EXPLICIT_HISTOGRAMS: [&str; 2] = [SLOT_RETURN_DURATION, EGRESS_RETIREMENT_DURATION];
const EXPLICIT_BOUNDARIES: [f64; 14] = [
    0.0, 1.0, 10.0, 100.0, 500.0, 1000.0, 3000.0, 5000.0, 10000.0, 30000.0, 60000.0, 120000.0, 300000.0, 600000.0,
];

pub fn meter_provider(endpoint: &Endpoint, resource: Resource) -> Result<SdkMeterProvider, String> {
    let exporter = MetricExporter::builder()
        .with_tonic()
        .with_temporality(Temporality::LowMemory)
        .with_endpoint(endpoint.url())
        .build()
        .map_err(|e| format!("failed to create metrics exporter: {e}"))?;
    let reader = PeriodicReader::builder(exporter, Tokio)
        .with_interval(METRIC_EXPORT_PERIOD)
        .build();
    Ok(SdkMeterProvider::builder()
        .with_reader(reader)
        .with_resource(resource)
        .with_view(low_memory_streams)
        .build())
}

pub struct SandboxExport<E = MetricExporter> {
    reader: Arc<ManualReader>,
    exporter: Arc<E>,
}

impl<E> Clone for SandboxExport<E> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            exporter: self.exporter.clone(),
        }
    }
}

impl<E: PushMetricExporter> SandboxExport<E> {
    pub async fn push(&self) -> Result<(), String> {
        let mut metrics = ResourceMetrics::default();
        self.reader
            .collect(&mut metrics)
            .map_err(|e| format!("failed to collect sandbox metrics: {e}"))?;
        self.exporter
            .export(&metrics)
            .await
            .map_err(|e| format!("failed to export sandbox metrics: {e}"))
    }
}

#[derive(Debug)]
struct SharedReader(Arc<ManualReader>);

impl MetricReader for SharedReader {
    fn register_pipeline(&self, pipeline: Weak<Pipeline>) {
        self.0.register_pipeline(pipeline);
    }

    fn collect(&self, rm: &mut ResourceMetrics) -> OTelSdkResult {
        self.0.collect(rm)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn temporality(&self, kind: InstrumentKind) -> Temporality {
        self.0.temporality(kind)
    }
}

pub fn sandbox_meter_provider(
    endpoint: &Endpoint,
    resource: Resource,
) -> Result<(SdkMeterProvider, SandboxExport), String> {
    MetricExporter::builder()
        .with_tonic()
        .with_temporality(Temporality::Delta)
        .with_endpoint(endpoint.url())
        .build()
        .map_err(|e| format!("failed to create sandbox metrics exporter: {e}"))
        .map(|exporter| on_demand_provider(exporter, resource))
}

fn on_demand_provider<E: PushMetricExporter>(exporter: E, resource: Resource) -> (SdkMeterProvider, SandboxExport<E>) {
    let reader = Arc::new(ManualReader::builder().with_temporality(Temporality::Delta).build());
    let provider = SdkMeterProvider::builder()
        .with_reader(SharedReader(reader.clone()))
        .with_resource(resource)
        .with_view(lifted_cardinality)
        .build();
    let export = SandboxExport {
        reader,
        exporter: Arc::new(exporter),
    };
    (provider, export)
}

fn lifted_cardinality(_: &Instrument) -> Option<Stream> {
    Stream::builder()
        .with_cardinality_limit(LIFTED_CARDINALITY_LIMIT)
        .build()
        .ok()
}

fn low_memory_streams(instrument: &Instrument) -> Option<Stream> {
    match instrument.kind() {
        InstrumentKind::Histogram => Stream::builder()
            .with_aggregation(histogram_aggregation(instrument.name()))
            .with_cardinality_limit(LIFTED_CARDINALITY_LIMIT)
            .build()
            .ok(),
        InstrumentKind::Counter => lifted_cardinality(instrument),
        _ => None,
    }
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

    #[tokio::test]
    async fn a_sandbox_push_exports_what_the_gauges_observe_at_that_moment() {
        let exporter = InMemoryMetricExporter::default();
        let (provider, export) = on_demand_provider(exporter.clone(), Resource::builder_empty().build());
        let _gauge = provider
            .meter("test")
            .i64_observable_gauge("e2b.sandbox.cpu.total")
            .with_callback(|observer| observer.observe(2, &[]))
            .build();

        export.push().await.unwrap();

        let finished = exporter.get_finished_metrics().unwrap();
        let names: Vec<String> = finished
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .map(|metric| metric.name().to_owned())
            .collect();
        assert_eq!(names, vec!["e2b.sandbox.cpu.total"]);
    }

    #[test]
    fn delta_counters_keep_more_series_than_the_sdk_default_limit() {
        let exporter = opentelemetry_sdk::metrics::InMemoryMetricExporterBuilder::new()
            .with_temporality(Temporality::LowMemory)
            .build();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(low_memory_streams)
            .build();
        let counter = provider.meter("test").u64_counter("per.team").build();
        (0..3000).for_each(|team| counter.add(1, &[opentelemetry::KeyValue::new("team.id", team)]));

        provider.force_flush().unwrap();

        let finished = exporter.get_finished_metrics().unwrap();
        let points = finished
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .map(|metric| match metric.data() {
                AggregatedMetrics::U64(MetricData::Sum(sum)) => sum.data_points().count(),
                _ => 0,
            })
            .sum::<usize>();
        assert_eq!(points, 3000);
    }

    #[test]
    fn the_network_release_histograms_export_explicit_buckets() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .with_view(low_memory_streams)
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
