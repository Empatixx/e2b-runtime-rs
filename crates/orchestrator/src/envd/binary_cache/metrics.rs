use std::sync::{Arc, LazyLock};
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/envdbin";

pub const READS_NAME: &str = "orchestrator.envd.binary_cache.reads";
pub const DELIVERIES_NAME: &str = "orchestrator.envd.binary_cache.deliveries";
pub const WARMS_NAME: &str = "orchestrator.envd.binary_cache.warms";
pub const SOURCE_STAT_NAME: &str = "orchestrator.envd.binary_cache.source_stat.duration";

const READS_DESCRIPTION: &str = "Host envd binary cache lookups on the resume path, by what the lookup found (hit|miss), upgrade path, and on a miss its cause (not_warmed|stalled|cancelled|unreadable|no_cache)";
const DELIVERIES_DESCRIPTION: &str =
    "Host envd binary reads at delivery time, by outcome (copy|stale) and upgrade path";
const WARMS_DESCRIPTION: &str =
    "Host envd binary cache warms, by result (ok|superseded|suppressed|pinned|already_warming|bad_target|failed)";
const SOURCE_STAT_DESCRIPTION: &str =
    "Wall-time of the resume path's stat of the host envd binary on its mount, by result (ok|error)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Live,
    Offline,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Op::Live => "live",
            Op::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Hit,
    Copy,
    Miss,
    Stale,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Hit => "hit",
            Outcome::Copy => "copy",
            Outcome::Miss => "miss",
            Outcome::Stale => "stale",
        }
    }
}

pub const READ_OUTCOMES: [Outcome; 2] = [Outcome::Hit, Outcome::Miss];
pub const DELIVERY_OUTCOMES: [Outcome; 2] = [Outcome::Copy, Outcome::Stale];

pub struct CacheMetrics {
    reads: Counter<u64>,
    deliveries: Counter<u64>,
    warms: Counter<u64>,
    source_stat: Histogram<u64>,
}

static GLOBAL: LazyLock<Arc<CacheMetrics>> = LazyLock::new(|| Arc::new(CacheMetrics::new(&global::meter(METER_NAME))));

impl CacheMetrics {
    pub fn new(meter: &Meter) -> Self {
        let counter = |name: &'static str, description: &'static str, unit: &'static str| {
            meter
                .u64_counter(name)
                .with_description(description)
                .with_unit(unit)
                .build()
        };
        Self {
            reads: counter(READS_NAME, READS_DESCRIPTION, "{read}"),
            deliveries: counter(DELIVERIES_NAME, DELIVERIES_DESCRIPTION, "{read}"),
            warms: counter(WARMS_NAME, WARMS_DESCRIPTION, "{warm}"),
            source_stat: meter
                .u64_histogram(SOURCE_STAT_NAME)
                .with_description(SOURCE_STAT_DESCRIPTION)
                .with_unit("ms")
                .build(),
        }
    }

    pub fn global() -> Arc<Self> {
        GLOBAL.clone()
    }

    pub fn read(&self, op: Op, outcome: Outcome, cause: Option<&str>) {
        let mut attributes = vec![
            KeyValue::new("outcome", outcome.as_str()),
            KeyValue::new("op", op.as_str()),
        ];
        attributes.extend(cause.map(|cause| KeyValue::new("cause", cause.to_owned())));
        self.reads.add(1, &attributes);
    }

    pub fn delivery(&self, op: Op, outcome: Outcome) {
        self.deliveries.add(
            1,
            &[
                KeyValue::new("outcome", outcome.as_str()),
                KeyValue::new("op", op.as_str()),
            ],
        );
    }

    pub fn warm(&self, result: &str) {
        self.warms.add(1, &[KeyValue::new("result", result.to_owned())]);
    }

    pub fn source_stat(&self, took: Duration, result: &'static str) {
        self.source_stat.record(
            u64::try_from(took.as_millis()).unwrap_or(u64::MAX),
            &[KeyValue::new("result", result)],
        );
    }
}

#[cfg(test)]
pub mod testing {
    use std::collections::BTreeMap;

    use opentelemetry::metrics::MeterProvider;
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData};
    use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

    use super::*;

    pub struct Collected {
        exporter: InMemoryMetricExporter,
        provider: SdkMeterProvider,
        pub metrics: Arc<CacheMetrics>,
    }

    impl Default for Collected {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Collected {
        pub fn new() -> Self {
            let exporter = InMemoryMetricExporter::default();
            let provider = SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(exporter.clone()).build())
                .build();
            let metrics = Arc::new(CacheMetrics::new(&provider.meter(METER_NAME)));
            Self {
                exporter,
                provider,
                metrics,
            }
        }

        fn flushed(&self) -> Vec<opentelemetry_sdk::metrics::data::ResourceMetrics> {
            self.provider.force_flush().unwrap();
            let all = self.exporter.get_finished_metrics().unwrap();
            self.exporter.reset();
            all
        }

        pub fn by_attribute(&self, metric: &str, attribute: &str) -> BTreeMap<String, u64> {
            let mut totals = BTreeMap::new();
            for resource in self.flushed() {
                for scope in resource.scope_metrics() {
                    for data in scope.metrics().filter(|data| data.name() == metric) {
                        if let AggregatedMetrics::U64(MetricData::Sum(sum)) = data.data() {
                            for point in sum.data_points() {
                                let label = point
                                    .attributes()
                                    .find(|kv| kv.key.as_str() == attribute)
                                    .map(|kv| kv.value.to_string())
                                    .unwrap_or_default();
                                *totals.entry(label).or_insert(0) += point.value();
                            }
                        }
                    }
                }
            }
            totals
        }
    }

    pub fn published_description(metric: &str) -> String {
        let collected = Collected::new();
        match metric {
            READS_NAME => collected.metrics.read(Op::Live, Outcome::Hit, None),
            DELIVERIES_NAME => collected.metrics.delivery(Op::Live, Outcome::Copy),
            WARMS_NAME => collected.metrics.warm("ok"),
            other => panic!("unknown counter {other}"),
        }
        collected.provider.force_flush().unwrap();
        collected
            .exporter
            .get_finished_metrics()
            .unwrap()
            .iter()
            .flat_map(|resource| resource.scope_metrics())
            .flat_map(|scope| scope.metrics())
            .find(|data| data.name() == metric)
            .map(|data| data.description().to_owned())
            .unwrap_or_default()
    }

    pub fn warm_results() -> Vec<&'static str> {
        crate::envd::binary_cache::error::WARM_RESULTS.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::testing::*;
    use super::*;

    #[test]
    fn the_published_descriptions_name_every_label_the_code_emits() {
        let read_labels: Vec<&str> = READ_OUTCOMES.iter().map(|o| o.as_str()).collect();
        let delivery_labels: Vec<&str> = DELIVERY_OUTCOMES.iter().map(|o| o.as_str()).collect();
        for (name, labels) in [
            (READS_NAME, read_labels),
            (DELIVERIES_NAME, delivery_labels),
            (WARMS_NAME, warm_results()),
        ] {
            let description = published_description(name);
            assert!(!description.is_empty(), "an unlabelled metric ships silently: {name}");
            for label in labels {
                assert!(
                    description.contains(label),
                    "{name} emits {label:?} but its published description does not name it"
                );
            }
        }
    }

    #[test]
    fn a_read_carries_its_cause_only_on_a_miss() {
        let collected = Collected::new();

        collected.metrics.read(Op::Live, Outcome::Hit, None);
        collected.metrics.read(Op::Live, Outcome::Miss, Some("not_warmed"));
        collected.metrics.read(Op::Live, Outcome::Miss, Some("stalled"));

        let causes = collected.by_attribute(READS_NAME, "cause");
        assert_eq!(
            causes,
            BTreeMap::from([
                (String::new(), 1),
                ("not_warmed".to_owned(), 1),
                ("stalled".to_owned(), 1)
            ])
        );
    }
}
