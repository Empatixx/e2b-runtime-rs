use std::sync::{Arc, LazyLock};

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Meter, ObservableGauge, ObservableUpDownCounter};

use crate::proto::SandboxConfig;
use crate::server::admission::MaxSandboxes;
use crate::server::status::NodeStatus;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/server";
const MIB: i64 = 1024 * 1024;

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static SNAPSHOT_UPLOAD_FAILED: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.snapshot.upload.failed")
        .with_description("Number of pause-snapshot uploads that never landed durably")
        .with_unit("{snapshot}")
        .build()
});

pub fn count_failed_snapshot_upload(filesystem_only: bool) {
    SNAPSHOT_UPLOAD_FAILED.add(1, &[KeyValue::new("fs_only", filesystem_only)]);
}

pub type RunningConfigs = Arc<dyn Fn() -> Vec<SandboxConfig> + Send + Sync>;

#[derive(Clone)]
pub struct NodeSource {
    pub running: RunningConfigs,
    pub max_sandboxes: MaxSandboxes,
    pub status: Arc<NodeStatus>,
    pub version: String,
    pub commit: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Allocated {
    vcpus: i64,
    memory_bytes: i64,
    disk_bytes: i64,
}

impl Allocated {
    fn of(configs: &[SandboxConfig]) -> Self {
        configs.iter().fold(Self::default(), |sum, config| Self {
            vcpus: sum.vcpus + config.vcpu,
            memory_bytes: sum.memory_bytes + config.ram_mb * MIB,
            disk_bytes: sum.disk_bytes + config.total_disk_size_mb * MIB,
        })
    }
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
        Self {
            _sandboxes: sandboxes(&source),
            _limit: limit(&source),
            _status: status(&source),
            _cpu: allocated(
                &source,
                "orchestrator.sandbox.cpu.allocated",
                "Total vCPUs allocated to running sandboxes on the orchestrator node.",
                "{count}",
                |allocated| allocated.vcpus,
            ),
            _memory: allocated(
                &source,
                "orchestrator.sandbox.memory.allocated",
                "Total memory allocated to running sandboxes on the orchestrator node.",
                "{By}",
                |allocated| allocated.memory_bytes,
            ),
            _disk: allocated(
                &source,
                "orchestrator.sandbox.disk.allocated",
                "Total disk space allocated to running sandboxes on the orchestrator node.",
                "{By}",
                |allocated| allocated.disk_bytes,
            ),
        }
    }
}

fn sandboxes(source: &NodeSource) -> ObservableUpDownCounter<i64> {
    let running = source.running.clone();
    METER
        .i64_observable_up_down_counter("orchestrator.env.sandbox.running")
        .with_description("Counter of running sandboxes on the orchestrator.")
        .with_unit("{sandbox}")
        .with_callback(move |observer| observer.observe(running().len() as i64, &[]))
        .build()
}

fn limit(source: &NodeSource) -> ObservableGauge<i64> {
    let max_sandboxes = source.max_sandboxes.clone();
    METER
        .i64_observable_gauge("orchestrator.sandbox.limit")
        .with_description("Configured maximum number of running sandboxes on the orchestrator node.")
        .with_unit("{sandbox}")
        .with_callback(move |observer| observer.observe(max_sandboxes.load(), &[]))
        .build()
}

fn status(source: &NodeSource) -> ObservableGauge<i64> {
    let source = source.clone();
    METER
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

fn allocated(
    source: &NodeSource,
    name: &'static str,
    description: &'static str,
    unit: &'static str,
    pick: fn(&Allocated) -> i64,
) -> ObservableGauge<i64> {
    let running = source.running.clone();
    METER
        .i64_observable_gauge(name)
        .with_description(description)
        .with_unit(unit)
        .with_callback(move |observer| observer.observe(pick(&Allocated::of(&running())), &[]))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vcpu: i64, ram_mb: i64, total_disk_size_mb: i64) -> SandboxConfig {
        SandboxConfig {
            vcpu,
            ram_mb,
            total_disk_size_mb,
            ..SandboxConfig::default()
        }
    }

    #[test]
    fn allocation_sums_vcpus_and_converts_megabytes_to_bytes() {
        let allocated = Allocated::of(&[config(2, 1024, 5000), config(4, 512, 3000)]);

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
        let source = NodeSource {
            running: Arc::new(Vec::new),
            max_sandboxes: MaxSandboxes::new(10),
            status: Arc::default(),
            version: "0.1.0".to_owned(),
            commit: "abc".to_owned(),
        };

        let attributes = status_attributes(&source);

        assert_eq!(
            attributes.map(|attribute| (attribute.key.to_string(), attribute.value.as_str().into_owned())),
            [
                ("status".to_owned(), "Healthy".to_owned()),
                ("version".to_owned(), "0.1.0".to_owned()),
                ("commit".to_owned(), "abc".to_owned())
            ]
        );
    }
}
