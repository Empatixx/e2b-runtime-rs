use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::cgroup::CgroupStats;
use crate::clickhouse::sinks::ClickhouseHostStats;
use crate::hoststats::{HostStats, StatsIdentity};

const SAMPLING_INTERVAL: Duration = Duration::from_secs(1);

pub struct HostStatsCollector {
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl HostStatsCollector {
    pub fn start(reader: CgroupStats, identity: StatsIdentity, sink: Arc<ClickhouseHostStats>) -> Self {
        let (stop, stopped) = oneshot::channel();
        let task = tokio::spawn(collect(reader, identity, sink, stopped));
        Self { stop, task }
    }

    pub async fn stop(self) {
        let _ = self.stop.send(());
        let _ = self.task.await;
    }
}

async fn collect(
    mut reader: CgroupStats,
    identity: StatsIdentity,
    sink: Arc<ClickhouseHostStats>,
    mut stopped: oneshot::Receiver<()>,
) {
    let sandbox_id = identity.sandbox_id.clone();
    let (mut stats, baseline) = HostStats::start(identity, SystemTime::now());
    if let Err(e) = sink.push(baseline) {
        tracing::error!(sandbox_id = %sandbox_id, error = %e, "failed to push initial host stats baseline");
    }
    let mut ticks = tokio::time::interval(SAMPLING_INTERVAL);
    ticks.tick().await;
    loop {
        tokio::select! {
            _ = ticks.tick() => {
                if let Err(e) = sample(&mut reader, &mut stats, &sink) {
                    tracing::error!(sandbox_id = %sandbox_id, error = %e, "failed to collect host stats sample");
                }
            }
            _ = &mut stopped => {
                if let Err(e) = sample(&mut reader, &mut stats, &sink) {
                    tracing::error!(sandbox_id = %sandbox_id, error = %e, "failed to collect final host stats sample");
                }
                return;
            }
        }
    }
}

fn sample(reader: &mut CgroupStats, stats: &mut HostStats, sink: &ClickhouseHostStats) -> Result<(), String> {
    let now = SystemTime::now();
    let current = reader.sample().map_err(|e| e.to_string())?;
    sink.push(stats.sample(current, now))?;
    stats.advance(current, now);
    Ok(())
}
