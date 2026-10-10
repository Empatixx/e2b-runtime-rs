use std::sync::LazyLock;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, Meter};

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/sandbox/fc";

static METER: LazyLock<Meter> = LazyLock::new(|| global::meter(METER_NAME));

static VCPU_LIMIT: LazyLock<Counter<u64>> = LazyLock::new(|| {
    METER
        .u64_counter("orchestrator.sandbox.vcpu_limit")
        .with_description(
            "Starts whose VM has spare vCPUs, by how the CPU limit was applied (outcome=confined|cgroup_fallback|failed) on which path (path=boot|resume)",
        )
        .with_unit("{start}")
        .build()
});

pub fn record_vcpu_limit(path: &str, outcome: &str) {
    VCPU_LIMIT.add(
        1,
        &[
            KeyValue::new("path", path.to_owned()),
            KeyValue::new("outcome", outcome.to_owned()),
        ],
    );
}

fn counter(name: &'static str, description: &'static str, unit: &'static str) -> Counter<u64> {
    METER
        .u64_counter(name)
        .with_description(description)
        .with_unit(unit)
        .build()
}

fn histogram(name: &'static str, description: &'static str, unit: &'static str) -> Histogram<u64> {
    METER
        .u64_histogram(name)
        .with_description(description)
        .with_unit(unit)
        .build()
}

static NET_FAILS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.fc.net.fails",
        "Total Firecracker VMM errors transmitting or receiving data (direction=tx/rx)",
        "{error}",
    )
});
static NET_NO_AVAIL_BUFFER: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.fc.net.no_avail_buffer",
        "Total Firecracker VMM events where no virtqueue buffer was available (direction=tx/rx)",
        "{event}",
    )
});
static NET_TAP_IO_FAILS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.fc.net.tap_io_fails",
        "Total Firecracker VMM TAP I/O failures (direction=tx/rx)",
        "{error}",
    )
});
static BLOCK_FAILS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.fc.block.fails",
        "Total Firecracker VMM block device execution/event failures",
        "{error}",
    )
});
static BLOCK_NO_AVAIL_BUFFER: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.sandbox.fc.block.no_avail_buffer",
        "Total Firecracker VMM block events where no virtqueue buffer was available",
        "{event}",
    )
});
static NET_BYTES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.net.bytes",
        "Distribution of Firecracker VMM bytes per metrics flush",
        "{By}",
    )
});
static NET_PACKETS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.net.packets",
        "Distribution of Firecracker VMM packets per metrics flush",
        "{packet}",
    )
});
static NET_COUNT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.net.count",
        "Distribution of Firecracker VMM I/O operations per metrics flush",
        "{op}",
    )
});
static NET_RATE_LIMITER_THROTTLED: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.net.rate_limiter_throttled",
        "Distribution of Firecracker VMM ops throttled by rate limiter per metrics flush",
        "{op}",
    )
});
static NET_RATE_LIMITER_EVENT_COUNT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.net.rate_limiter_event_count",
        "Distribution of Firecracker VMM TX rate limiter events per metrics flush",
        "{event}",
    )
});
static NET_REMAINING_REQS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.net.remaining_reqs",
        "Distribution of Firecracker VMM TX queue remaining-request events per metrics flush",
        "{event}",
    )
});
static BLOCK_BYTES: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.bytes",
        "Distribution of Firecracker VMM block bytes per metrics flush",
        "{By}",
    )
});
static BLOCK_COUNT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.count",
        "Distribution of Firecracker VMM block I/O operations per metrics flush",
        "{op}",
    )
});
static BLOCK_QUEUE_EVENT_COUNT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.queue_event_count",
        "Distribution of Firecracker VMM block queue notifications per metrics flush",
        "{event}",
    )
});
static BLOCK_RATE_LIMITER_THROTTLED: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.rate_limiter_throttled",
        "Distribution of Firecracker VMM block ops throttled by rate limiter per metrics flush",
        "{op}",
    )
});
static BLOCK_RATE_LIMITER_EVENT_COUNT: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.rate_limiter_event_count",
        "Distribution of Firecracker VMM block rate limiter events per metrics flush",
        "{event}",
    )
});
static BLOCK_IO_ENGINE_THROTTLED: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.io_engine_throttled",
        "Distribution of Firecracker VMM block ops throttled by io_uring engine per metrics flush",
        "{op}",
    )
});
static BLOCK_REMAINING_REQS: LazyLock<Histogram<u64>> = LazyLock::new(|| {
    histogram(
        "orchestrator.sandbox.fc.block.remaining_reqs",
        "Distribution of Firecracker VMM block queue remaining-request events per metrics flush",
        "{event}",
    )
});
static BALANCE_DIRTY_PAGE_THREADS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    counter(
        "orchestrator.host.balance_dirty_pages.threads",
        "Cumulative stalled thread-polls during sandbox resume; rate() gives throttle intensity",
        "{thread}",
    )
});

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct NetMetrics {
    pub tx_bytes_count: u64,
    pub tx_packets_count: u64,
    pub tx_count: u64,
    pub tx_fails: u64,
    pub tx_rate_limiter_throttled: u64,
    pub tx_rate_limiter_event_count: u64,
    pub tx_remaining_reqs_count: u64,
    pub no_tx_avail_buffer: u64,
    pub tap_write_fails: u64,
    pub rx_bytes_count: u64,
    pub rx_packets_count: u64,
    pub rx_count: u64,
    pub rx_fails: u64,
    pub rx_rate_limiter_throttled: u64,
    pub no_rx_avail_buffer: u64,
    pub tap_read_fails: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct BlockMetrics {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_count: u64,
    pub write_count: u64,
    pub queue_event_count: u64,
    pub rate_limiter_throttled_events: u64,
    pub rate_limiter_event_count: u64,
    pub io_engine_throttled_events: u64,
    pub no_avail_buffer: u64,
    pub execute_fails: u64,
    pub event_fails: u64,
    pub remaining_reqs_count: u64,
}

fn direction(direction: &'static str) -> [KeyValue; 1] {
    [KeyValue::new("direction", direction)]
}

fn add_nonzero(counter: &Counter<u64>, value: u64, attributes: &[KeyValue]) {
    if value > 0 {
        counter.add(value, attributes);
    }
}

fn record_nonzero(histogram: &Histogram<u64>, value: u64, attributes: &[KeyValue]) {
    if value > 0 {
        histogram.record(value, attributes);
    }
}

pub fn record_net(net: &NetMetrics) {
    let (tx, rx) = (direction("tx"), direction("rx"));
    NET_BYTES.record(net.tx_bytes_count, &tx);
    NET_PACKETS.record(net.tx_packets_count, &tx);
    NET_COUNT.record(net.tx_count, &tx);
    NET_RATE_LIMITER_EVENT_COUNT.record(net.tx_rate_limiter_event_count, &tx);
    NET_REMAINING_REQS.record(net.tx_remaining_reqs_count, &tx);
    record_nonzero(&NET_RATE_LIMITER_THROTTLED, net.tx_rate_limiter_throttled, &tx);
    NET_BYTES.record(net.rx_bytes_count, &rx);
    NET_PACKETS.record(net.rx_packets_count, &rx);
    NET_COUNT.record(net.rx_count, &rx);
    record_nonzero(&NET_RATE_LIMITER_THROTTLED, net.rx_rate_limiter_throttled, &rx);
    add_nonzero(&NET_FAILS, net.tx_fails, &tx);
    add_nonzero(&NET_FAILS, net.rx_fails, &rx);
    add_nonzero(&NET_NO_AVAIL_BUFFER, net.no_tx_avail_buffer, &tx);
    add_nonzero(&NET_NO_AVAIL_BUFFER, net.no_rx_avail_buffer, &rx);
    add_nonzero(&NET_TAP_IO_FAILS, net.tap_write_fails, &tx);
    add_nonzero(&NET_TAP_IO_FAILS, net.tap_read_fails, &rx);
}

pub fn record_block(block: &BlockMetrics) {
    let (read, write) = (direction("read"), direction("write"));
    BLOCK_BYTES.record(block.read_bytes, &read);
    BLOCK_BYTES.record(block.write_bytes, &write);
    BLOCK_COUNT.record(block.read_count, &read);
    BLOCK_COUNT.record(block.write_count, &write);
    BLOCK_QUEUE_EVENT_COUNT.record(block.queue_event_count, &[]);
    BLOCK_RATE_LIMITER_EVENT_COUNT.record(block.rate_limiter_event_count, &[]);
    BLOCK_REMAINING_REQS.record(block.remaining_reqs_count, &[]);
    record_nonzero(&BLOCK_RATE_LIMITER_THROTTLED, block.rate_limiter_throttled_events, &[]);
    record_nonzero(&BLOCK_IO_ENGINE_THROTTLED, block.io_engine_throttled_events, &[]);
    add_nonzero(&BLOCK_FAILS, block.execute_fails + block.event_fails, &[]);
    add_nonzero(&BLOCK_NO_AVAIL_BUFFER, block.no_avail_buffer, &[]);
}

const DIRTY_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const BALANCE_DIRTY_PAGES_WCHAN: &str = "balance_dirty_pages";

pub fn start_dirty_page_monitor() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        tokio::spawn(async {
            let mut ticks = tokio::time::interval(DIRTY_POLL_INTERVAL);
            loop {
                ticks.tick().await;
                let stalled = tokio::task::spawn_blocking(count_balance_dirty_threads)
                    .await
                    .unwrap_or(0);
                BALANCE_DIRTY_PAGE_THREADS.add(stalled, &[]);
            }
        });
    });
}

fn count_balance_dirty_threads() -> u64 {
    std::fs::read_dir("/proc/self/task")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|task| {
            std::fs::read_to_string(task.path().join("wchan"))
                .is_ok_and(|wchan| wchan.trim() == BALANCE_DIRTY_PAGES_WCHAN)
        })
        .count() as u64
}
