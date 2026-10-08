use futures::FutureExt;
use futures::future::BoxFuture;

use super::batcher::{Batcher, BatcherOptions};
use super::endpoint::{Endpoint, Writer};
use super::rows::{EventRow, HostStatRow, INSERT_EVENTS, INSERT_HOST_STATS};
use crate::events::{EventSink, SandboxEvent};

pub const EVENTS_BATCHER: &str = "sandbox-events";
pub const HOST_STATS_BATCHER: &str = "sandbox-host-stats";

pub struct Clickhouse {
    pub events: ClickhouseEvents,
    pub host_stats: ClickhouseHostStats,
}

pub struct ClickhouseEvents {
    batcher: Batcher<EventRow>,
}

pub struct ClickhouseHostStats {
    batcher: Batcher<HostStatRow>,
}

impl Clickhouse {
    pub fn open(primary: &str, additional: &[String]) -> Result<Self, String> {
        let endpoint = Endpoint::parse(primary).map_err(|e| format!("failed to create clickhouse driver: {e}"))?;
        let resolved = additional
            .iter()
            .filter(|dsn| match Endpoint::parse(dsn) {
                Ok(_) => true,
                Err(e) => {
                    tracing::error!(error = %e, "failed to parse clickhouse DSN, skipping endpoint");
                    false
                }
            })
            .count();
        if resolved > 0 {
            tracing::info!(count = resolved, "resolved additional ClickHouse delivery endpoints");
        }
        let writer = Writer::new(endpoint);
        let events_writer = writer.clone();
        let events = Batcher::start(EVENTS_BATCHER, BatcherOptions::default(), move |rows: Vec<EventRow>| {
            let writer = events_writer.clone();
            async move { writer.insert(INSERT_EVENTS, rows).await }
        });
        let host_stats = Batcher::start(
            HOST_STATS_BATCHER,
            BatcherOptions::default(),
            move |rows: Vec<HostStatRow>| {
                let writer = writer.clone();
                async move { writer.insert(INSERT_HOST_STATS, rows).await }
            },
        );
        Ok(Self {
            events: ClickhouseEvents { batcher: events },
            host_stats: ClickhouseHostStats { batcher: host_stats },
        })
    }
}

impl EventSink for ClickhouseEvents {
    fn publish(&self, event: &SandboxEvent) -> BoxFuture<'static, Result<(), String>> {
        let pushed = self.batcher.push(EventRow::from_event(event));
        async move { pushed }.boxed()
    }
}

impl ClickhouseEvents {
    pub async fn stop(&self) {
        self.batcher.stop().await;
    }
}

impl ClickhouseHostStats {
    pub fn push(&self, row: HostStatRow) -> Result<(), String> {
        self.batcher.push(row)
    }

    pub async fn stop(&self) {
        self.batcher.stop().await;
    }
}
