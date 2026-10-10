use std::sync::Arc;

use opentelemetry::metrics::{Meter, ObservableGauge};
use parking_lot::Mutex;

use super::poll::Observation;

type Snapshot = Arc<Mutex<Vec<Observation>>>;

struct IntGauge {
    name: &'static str,
    description: &'static str,
    unit: &'static str,
    pick: fn(&Observation) -> Option<i64>,
}

const INT_GAUGES: [IntGauge; 10] = [
    IntGauge {
        name: "e2b.sandbox.cpu.total",
        description: "Amount of CPU available to the sandbox.",
        unit: "{count}",
        pick: |observation| Some(observation.metrics.cpu_count),
    },
    IntGauge {
        name: "e2b.sandbox.cpu.possible",
        description: "Number of CPUs the sandbox guest can bring online.",
        unit: "{count}",
        pick: |observation| observation.cpu_hotplug.then_some(observation.metrics.cpu_possible),
    },
    IntGauge {
        name: "e2b.sandbox.cpu.target",
        description: "Number of online CPUs last requested from the sandbox guest, 0 when none was.",
        unit: "{count}",
        pick: |observation| observation.cpu_hotplug.then_some(observation.metrics.cpu_target),
    },
    IntGauge {
        name: "e2b.sandbox.cpu.target_attempts",
        description: "Attempts the sandbox guest made at reaching the requested CPU count.",
        unit: "{attempt}",
        pick: |observation| {
            observation
                .cpu_hotplug
                .then_some(observation.metrics.cpu_target_attempts)
        },
    },
    IntGauge {
        name: "e2b.sandbox.cpu.write_pending",
        description: "How long a CPU online or offline write has been running in the sandbox guest, 0 when none is.",
        unit: "ms",
        pick: |observation| {
            observation
                .cpu_hotplug
                .then_some(observation.metrics.cpu_write_pending_ms)
        },
    },
    IntGauge {
        name: "e2b.sandbox.ram.total",
        description: "Amount of RAM available to the sandbox.",
        unit: "{By}",
        pick: |observation| observation.memory.map(|(total, _)| total),
    },
    IntGauge {
        name: "e2b.sandbox.ram.used",
        description: "Amount of RAM used by the sandbox.",
        unit: "{By}",
        pick: |observation| observation.memory.map(|(_, used)| used),
    },
    IntGauge {
        name: "e2b.sandbox.ram.cache",
        description: "Amount of RAM used by the page cache in the sandbox.",
        unit: "{By}",
        pick: |observation| observation.cache.then_some(observation.metrics.mem_cache),
    },
    IntGauge {
        name: "e2b.sandbox.disk.total",
        description: "Amount of disk space available to the sandbox.",
        unit: "{By}",
        pick: |observation| observation.disk.then_some(observation.metrics.disk_total),
    },
    IntGauge {
        name: "e2b.sandbox.disk.used",
        description: "Amount of disk space used by the sandbox.",
        unit: "{By}",
        pick: |observation| observation.disk.then_some(observation.metrics.disk_used),
    },
];

pub struct SandboxGauges {
    _ints: Vec<ObservableGauge<i64>>,
    _cpu_used: ObservableGauge<f64>,
}

impl SandboxGauges {
    pub fn register(meter: &Meter, snapshot: Snapshot) -> Self {
        Self {
            _ints: INT_GAUGES
                .iter()
                .map(|gauge| int_gauge(meter, gauge, snapshot.clone()))
                .collect(),
            _cpu_used: cpu_used(meter, snapshot),
        }
    }
}

fn int_gauge(meter: &Meter, gauge: &'static IntGauge, snapshot: Snapshot) -> ObservableGauge<i64> {
    meter
        .i64_observable_gauge(gauge.name)
        .with_description(gauge.description)
        .with_unit(gauge.unit)
        .with_callback(move |observer| {
            snapshot.lock().iter().for_each(|observation| {
                (gauge.pick)(observation)
                    .iter()
                    .for_each(|value| observer.observe(*value, &observation.attributes))
            })
        })
        .build()
}

fn cpu_used(meter: &Meter, snapshot: Snapshot) -> ObservableGauge<f64> {
    meter
        .f64_observable_gauge("e2b.sandbox.cpu.used")
        .with_description("Amount of CPU used by the sandbox.")
        .with_unit("{percent}")
        .with_callback(move |observer| {
            snapshot
                .lock()
                .iter()
                .for_each(|observation| observer.observe(observation.metrics.cpu_used_percent, &observation.attributes))
        })
        .build()
}

#[cfg(test)]
mod tests {
    use opentelemetry::KeyValue;
    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;
    use crate::sandbox_observer::metrics::SandboxMetrics;

    fn observation(metrics: SandboxMetrics, hotplug: bool, memory: Option<(i64, i64)>) -> Observation {
        Observation {
            attributes: vec![KeyValue::new("sandbox_id", "sbx")],
            cpu_hotplug: hotplug,
            memory,
            cache: false,
            disk: false,
            metrics,
        }
    }

    fn exported(observations: Vec<Observation>) -> Vec<(String, String, i64)> {
        let exporter = InMemoryMetricExporter::default();
        let provider = SdkMeterProvider::builder()
            .with_reader(PeriodicReader::builder(exporter.clone()).build())
            .build();
        let snapshot: Snapshot = Arc::new(Mutex::new(observations));
        let _gauges = SandboxGauges::register(&provider.meter("test"), snapshot);

        provider.force_flush().unwrap();

        let mut values: Vec<(String, String, i64)> = exporter
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
                _ => None,
            })
            .collect();
        values.sort();
        values
    }

    #[test]
    fn an_envd_without_hotplug_reports_only_the_online_cpus() {
        let metrics = SandboxMetrics {
            cpu_count: 2,
            ..SandboxMetrics::default()
        };

        let values = exported(vec![observation(metrics, false, None)]);

        assert_eq!(
            values,
            vec![("e2b.sandbox.cpu.total".to_owned(), "{count}".to_owned(), 2)]
        );
    }

    #[test]
    fn an_envd_with_hotplug_reports_possible_target_attempts_and_the_pending_write() {
        let metrics = SandboxMetrics {
            cpu_count: 2,
            cpu_possible: 16,
            cpu_target: 2,
            cpu_target_attempts: 3,
            cpu_write_pending_ms: 12_000,
            ..SandboxMetrics::default()
        };

        let values = exported(vec![observation(metrics, true, Some((4096, 1024)))]);

        assert_eq!(
            values,
            vec![
                ("e2b.sandbox.cpu.possible".to_owned(), "{count}".to_owned(), 16),
                ("e2b.sandbox.cpu.target".to_owned(), "{count}".to_owned(), 2),
                ("e2b.sandbox.cpu.target_attempts".to_owned(), "{attempt}".to_owned(), 3),
                ("e2b.sandbox.cpu.total".to_owned(), "{count}".to_owned(), 2),
                ("e2b.sandbox.cpu.write_pending".to_owned(), "ms".to_owned(), 12_000),
                ("e2b.sandbox.ram.total".to_owned(), "{By}".to_owned(), 4096),
                ("e2b.sandbox.ram.used".to_owned(), "{By}".to_owned(), 1024),
            ]
        );
    }
}
