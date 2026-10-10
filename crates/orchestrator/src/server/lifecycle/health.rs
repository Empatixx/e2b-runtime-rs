use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use reqwest::StatusCode;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::sandbox_logs::{Healthcheck, SandboxMeta, external};

const INTERVAL: Duration = Duration::from_secs(20);
const TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Clone)]
pub struct HealthCheck {
    address: SocketAddr,
    meta: SandboxMeta,
    envd_version: String,
    healthy: Arc<AtomicBool>,
    client: reqwest::Client,
}

impl HealthCheck {
    pub fn new(address: SocketAddr, meta: SandboxMeta, envd_version: String) -> Self {
        Self {
            address,
            meta,
            envd_version,
            healthy: Arc::new(AtomicBool::new(true)),
            client: reqwest::Client::new(),
        }
    }

    pub async fn check(&self, always_report: bool) {
        let probed = self.probe().await;
        let log = external(&self.meta);
        match probed {
            Err(e) if self.transition(true, false) => {
                log.healthcheck(Healthcheck::Fail);
                tracing::warn!(sandbox.id = %self.meta.sandbox_id, error = %e, envd.version = %self.envd_version, "healthcheck failed");
            }
            Ok(()) if self.transition(false, true) => log.healthcheck(Healthcheck::Success),
            Ok(()) if always_report => log.healthcheck(Healthcheck::ReportSuccess),
            Err(e) if always_report => {
                log.healthcheck(Healthcheck::ReportFail);
                tracing::error!(sandbox.id = %self.meta.sandbox_id, error = %e, envd.version = %self.envd_version, "control healthcheck failed");
            }
            _ => {}
        }
    }

    fn transition(&self, from: bool, to: bool) -> bool {
        self.healthy
            .compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    async fn probe(&self) -> Result<(), String> {
        let response = self
            .client
            .get(format!("http://{}/health", self.address))
            .timeout(TIMEOUT)
            .send()
            .await
            .map_err(|e| match e.is_timeout() {
                true => "health check timed out".to_owned(),
                false => e.to_string(),
            })?;
        let status = response.status();
        let _ = response.bytes().await;
        match status {
            StatusCode::NO_CONTENT => Ok(()),
            status => Err(format!("unexpected status code: {}", status.as_u16())),
        }
    }
}

pub struct HealthLoop {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl HealthLoop {
    pub fn start(check: HealthCheck) -> Self {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(check, cancel.clone()));
        Self { cancel, task }
    }

    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

async fn run(check: HealthCheck, cancel: CancellationToken) {
    let mut ticks = tokio::time::interval(INTERVAL);
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = ticks.tick() => {}
        }
        tokio::select! {
            () = cancel.cancelled() => return,
            () = check.check(false) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::routing::get;
    use tokio::net::TcpListener;

    use super::*;

    async fn envd(status: StatusCode) -> SocketAddr {
        let router = Router::new().route("/health", get(move || async move { status }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        address
    }

    fn check(address: SocketAddr) -> HealthCheck {
        HealthCheck::new(address, SandboxMeta::default(), "0.9.0".to_owned())
    }

    #[tokio::test]
    async fn only_204_is_healthy() {
        assert_eq!(check(envd(StatusCode::NO_CONTENT).await).probe().await, Ok(()));
        assert_eq!(
            check(envd(StatusCode::OK).await).probe().await,
            Err("unexpected status code: 200".to_owned())
        );
    }

    #[tokio::test]
    async fn a_failure_flips_the_state_once_and_a_success_flips_it_back() {
        let failing = check(envd(StatusCode::OK).await);

        failing.check(false).await;
        assert!(!failing.healthy.load(Ordering::SeqCst));
        failing.check(false).await;
        assert!(!failing.healthy.load(Ordering::SeqCst));

        let recovered = HealthCheck {
            address: envd(StatusCode::NO_CONTENT).await,
            ..failing.clone()
        };
        recovered.check(false).await;
        assert!(
            failing.healthy.load(Ordering::SeqCst),
            "the state is shared by every clone"
        );
    }
}
