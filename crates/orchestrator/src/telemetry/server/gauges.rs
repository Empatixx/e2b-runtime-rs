use std::sync::Arc;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Meter, ObservableGauge, ObservableUpDownCounter};

use super::METER;
use crate::server::admission::MaxSandboxes;
use crate::server::status::NodeStatus;

const MIB: i64 = 1024 * 1024;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Allocated {
    pub vcpus: i64,
    pub memory_bytes: i64,
    pub disk_bytes: i64,
}

impl Allocated {
    pub fn with(self, vcpus: i64, ram_mb: i64, total_disk_size_mb: i64) -> Self {
        Self {
            vcpus: self.vcpus + vcpus,
            memory_bytes: self.memory_bytes + ram_mb * MIB,
            disk_bytes: self.disk_bytes + total_disk_size_mb * MIB,
        }
    }
}

#[derive(Clone)]
pub struct NodeSource {
    pub sandboxes: Arc<dyn Fn() -> usize + Send + Sync>,
    pub allocated: Arc<dyn Fn() -> Allocated + Send + Sync>,
    pub max_sandboxes: MaxSandboxes,
    pub status: Arc<NodeStatus>,
    pub version: String,
    pub commit: String,
}

pub struct NodeGauges {
    _sandboxes: ObservableUpDownCounter<i64>,
    _limit: ObservableGauge<i64>,
    _status: ObservableGauge<i64>,
    _cpu: ObservableGauge<i64>,
    _memory: ObservableGauge<i64>,
    _disk: ObservableGauge<i64>,
}

impl NodeGauges {
    pub fn register(source: NodeSource) -> Self {
        Self::register_on(&METER, source)
    }

    pub fn register_on(meter: &Meter, source: NodeSource) -> Self {
        Self {
            _sandboxes: sandboxes(meter, &source),
            _limit: limit(meter, &source),
            _status: status(meter, &source),
            _cpu: allocated(
                meter,
                &source,
                Allocation {
                    name: "orchestrator.sandbox.cpu.allocated",
                    description: "Total vCPUs allocated to running sandboxes on the orchestrator node.",
                    unit: "{count}",
                    pick: |allocated| allocated.vcpus,
                },
            ),
            _memory: allocated(
                meter,
                &source,
                Allocation {
                    name: "orchestrator.sandbox.memory.allocated",
                    description: "Total memory allocated to running sandboxes on the orchestrator node.",
                    unit: "{By}",
                    pick: |allocated| allocated.memory_bytes,
                },
            ),
            _disk: allocated(
                meter,
                &source,
                Allocation {
                    name: "orchestrator.sandbox.disk.allocated",
                    description: "Total disk space allocated to running sandboxes on the orchestrator node.",
                    unit: "{By}",
                    pick: |allocated| allocated.disk_bytes,
                },
            ),
        }
    }
}

struct Allocation {
    name: &'static str,
    description: &'static str,
    unit: &'static str,
    pick: fn(&Allocated) -> i64,
}

fn sandboxes(meter: &Meter, source: &NodeSource) -> ObservableUpDownCounter<i64> {
    let sandboxes = source.sandboxes.clone();
    meter
        .i64_observable_up_down_counter("orchestrator.env.sandbox.running")
        .with_description("Counter of running sandboxes on the orchestrator.")
        .with_unit("{sandbox}")
        .with_callback(move |observer| observer.observe(i64::try_from(sandboxes()).unwrap_or(i64::MAX), &[]))
        .build()
}

fn limit(meter: &Meter, source: &NodeSource) -> ObservableGauge<i64> {
    let max_sandboxes = source.max_sandboxes.clone();
    meter
        .i64_observable_gauge("orchestrator.sandbox.limit")
        .with_description("Configured maximum number of running sandboxes on the orchestrator node.")
        .with_unit("{sandbox}")
        .with_callback(move |observer| observer.observe(max_sandboxes.load(), &[]))
        .build()
}

fn status(meter: &Meter, source: &NodeSource) -> ObservableGauge<i64> {
    let source = source.clone();
    meter
        .i64_observable_gauge("orchestrator.status")
        .with_description("Self-reported orchestrator status (always 1, labelled with status and version).")
        .with_unit("{orchestrator}")
        .with_callback(move |observer| observer.observe(1, &status_attributes(&source)))
        .build()
}

fn status_attributes(source: &NodeSource) -> [KeyValue; 3] {
    [
        KeyValue::new("status", source.status.state().status.as_str_name()),
        KeyValue::new("version", source.version.clone()),
        KeyValue::new("commit", source.commit.clone()),
    ]
}

fn allocated(meter: &Meter, source: &NodeSource, allocation: Allocation) -> ObservableGauge<i64> {
    let allocated = source.allocated.clone();
    meter
        .i64_observable_gauge(allocation.name)
        .with_description(allocation.description)
        .with_unit(allocation.unit)
        .with_callback(move |observer| observer.observe((allocation.pick)(&allocated()), &[]))
        .build()
}

#[cfg(test)]
mod tests {
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;

    fn source() -> NodeSource {
        NodeSource {
            sandboxes: Arc::new(|| 2),
            allocated: Arc::new(|| Allocated::default().with(2, 1024, 5000).with(4, 512, 3000)),
            max_sandboxes: MaxSandboxes::new(10),
            status: Arc::default(),
            version: "0.1.0".to_owned(),
            commit: "abc".to_owned(),
        }
    }

    fn gauge_values(exporter: &InMemoryMetricExporter) -> Vec<(String, String, i64)> {
        exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .filter_map(|metric| match metric.data() {
                AggregatedMetrics::I64(MetricData::Gauge(gauge)) => Some((
                    metric.name().to_owned(),
                    metric.unit().to_owned(),
                    gauge.data_points().map(|point| point.value()).sum(),
                )),
                AggregatedMetrics::I64(MetricData::Sum(sum)) => Some((
                    metric.name().to_owned(),
                    metric.unit().to_owned(),
                    sum.data_points().map(|point| point.value()).sum(),
                )),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn allocation_sums_vcpus_and_converts_megabytes_to_bytes() {
        let allocated = Allocated::default().with(2, 1024, 5000).with(4, 512, 3000);

        assert_eq!(
            allocated,
            Allocated {
                vcpus: 6,
                memory_bytes: 1536 * MIB,
                disk_bytes: 8000 * MIB
            }
        );
    }

    #[test]
    fn the_status_gauge_is_labelled_with_status_version_and_commit() {
        let attributes = status_attributes(&source());

        assert_eq!(
            attributes.map(|attribute| (attribute.key.to_string(), attribute.value.as_str().into_owned())),
            [
                ("status".to_owned(), "Healthy".to_owned()),
                ("version".to_owned(), "0.1.0".to_owned()),
                ("commit".to_owned(), "abc".to_owned())
            ]
        );
    }

    #[test]
    fn the_node_gauges_report_their_upstream_names_units_and_values() {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let _gauges = NodeGauges::register_on(&provider.meter("test"), source());

        provider.force_flush().unwrap();

        let mut values = gauge_values(&exporter);
        values.sort();
        assert_eq!(
            values,
            vec![
                ("orchestrator.env.sandbox.running".to_owned(), "{sandbox}".to_owned(), 2),
                ("orchestrator.sandbox.cpu.allocated".to_owned(), "{count}".to_owned(), 6),
                (
                    "orchestrator.sandbox.disk.allocated".to_owned(),
                    "{By}".to_owned(),
                    8000 * MIB
                ),
                ("orchestrator.sandbox.limit".to_owned(), "{sandbox}".to_owned(), 10),
                (
                    "orchestrator.sandbox.memory.allocated".to_owned(),
                    "{By}".to_owned(),
                    1536 * MIB
                ),
                ("orchestrator.status".to_owned(), "{orchestrator}".to_owned(), 1),
            ]
        );
    }
}
