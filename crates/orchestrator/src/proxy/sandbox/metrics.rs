use std::sync::Arc;

use opentelemetry::global;
use opentelemetry::metrics::{Counter, Histogram, ObservableUpDownCounter};

use crate::proxy::server::ProxyServer;

const METER_NAME: &str = "github.com/e2b-dev/infra/packages/orchestrator/pkg/proxy";

#[derive(Clone)]
pub struct ConnectionRecorder {
    connections_per_sandbox: Histogram<u64>,
    connection_duration: Histogram<u64>,
    connections_blocked: Counter<u64>,
}

impl ConnectionRecorder {
    pub fn register() -> Self {
        let meter = global::meter(METER_NAME);
        Self {
            connections_per_sandbox: meter
                .u64_histogram("orchestrator.proxy.connections.per_sandbox")
                .with_description("Number of active ingress proxy connections per sandbox")
                .with_unit("{connection}")
                .build(),
            connection_duration: meter
                .u64_histogram("orchestrator.proxy.connection.duration")
                .with_description("Duration of ingress proxy connections")
                .with_unit("ms")
                .build(),
            connections_blocked: meter
                .u64_counter("orchestrator.proxy.connections.blocked.total")
                .with_description("Total number of ingress proxy connections blocked by connection limit")
                .with_unit("{connection}")
                .build(),
        }
    }

    pub fn on_acquired(&self) -> Arc<dyn Fn(i64) + Send + Sync> {
        let histogram = self.connections_per_sandbox.clone();
        Arc::new(move |count| histogram.record(u64::try_from(count).unwrap_or_default(), &[]))
    }

    pub fn on_released(&self) -> Arc<dyn Fn(u64) + Send + Sync> {
        let histogram = self.connection_duration.clone();
        Arc::new(move |millis| histogram.record(millis, &[]))
    }

    pub fn on_blocked(&self) -> Arc<dyn Fn(()) + Send + Sync> {
        let counter = self.connections_blocked.clone();
        Arc::new(move |()| counter.add(1, &[]))
    }
}

pub struct ProxyMetrics {
    _server_connections: ObservableUpDownCounter<i64>,
    _pool_connections: ObservableUpDownCounter<i64>,
    _pool_size: ObservableUpDownCounter<i64>,
}

impl ProxyMetrics {
    pub fn register(server: &Arc<ProxyServer>) -> Self {
        let meter = global::meter(METER_NAME);
        let (for_server, for_pool, for_size) = (server.clone(), server.clone(), server.clone());
        Self {
            _server_connections: meter
                .i64_observable_up_down_counter("orchestrator.proxy.server.connections.open")
                .with_description("Open connections to the orchestrator proxy from client proxies.")
                .with_unit("{connection}")
                .with_callback(move |observer| observer.observe(for_server.current_server_connections(), &[]))
                .build(),
            _pool_connections: meter
                .i64_observable_up_down_counter("orchestrator.proxy.pool.connections.open")
                .with_description("Open connections from the orchestrator proxy to sandboxes.")
                .with_unit("{connection}")
                .with_callback(move |observer| observer.observe(for_pool.current_pool_connections(), &[]))
                .build(),
            _pool_size: meter
                .i64_observable_up_down_counter("orchestrator.proxy.pool.size")
                .with_description("Size of the orchestrator proxy pool.")
                .with_unit("{transport}")
                .with_callback(move |observer| {
                    observer.observe(i64::try_from(for_size.pool_size()).unwrap_or(i64::MAX), &[])
                })
                .build(),
        }
    }
}
