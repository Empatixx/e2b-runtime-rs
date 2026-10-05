//! Firecracker MMDS client (upstream `host/mmds.go`). MMDS = metadata the orchestrator puts next to the VM.

use std::time::Duration;

use futures::future::BoxFuture;
use serde::Deserialize;

use crate::utils::envvars::EnvVars;

const MMDS_ADDRESS: &str = "http://169.254.169.254";
pub const E2B_RUN_DIR: &str = "/run/e2b";

#[derive(Debug, Default, Deserialize)]
pub struct MmdsOpts {
    #[serde(rename = "instanceID", default)]
    pub sandbox_id: String,
    #[serde(rename = "envID", default)]
    pub template_id: String,
    #[serde(rename = "accessTokenHash", default)]
    pub access_token_hash: String,
}

/// Abstracted so `/init` token validation can be tested without a VM.
pub trait Mmds: Send + Sync {
    fn opts(&self) -> BoxFuture<'_, Result<MmdsOpts, String>>;
}

pub struct HttpMmds {
    client: reqwest::Client,
}

impl Default for HttpMmds {
    fn default() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(0)
            .build()
            .expect("reqwest client");
        Self { client }
    }
}

impl Mmds for HttpMmds {
    fn opts(&self) -> BoxFuture<'_, Result<MmdsOpts, String>> {
        Box::pin(async move {
            let token = self
                .client
                .put(format!("{MMDS_ADDRESS}/latest/api/token"))
                .header("X-metadata-token-ttl-seconds", "60")
                .send()
                .await
                .and_then(|r| r.error_for_status())
                .map_err(|e| format!("failed to get MMDS token: {e}"))?
                .text()
                .await
                .map_err(|e| format!("failed to get MMDS token: {e}"))?;
            if token.is_empty() {
                return Err("failed to get MMDS token: mmds token is an empty string".into());
            }
            self.client
                .get(MMDS_ADDRESS)
                .header("X-metadata-token", token)
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|e| format!("failed to get MMDS opts: {e}"))?
                .json::<MmdsOpts>()
                .await
                .map_err(|e| format!("failed to get MMDS opts: {e}"))
        })
    }
}

/// After `/init`: poll MMDS (every 50 ms, up to 60 s) and publish sandbox/template id as internal env vars.
pub async fn poll_for_opts(mmds: &dyn Mmds, env_vars: &EnvVars) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let mut last_err = None;
    while tokio::time::Instant::now() < deadline {
        tick.tick().await;
        match mmds.opts().await {
            Ok(opts) => {
                env_vars.store("E2B_SANDBOX_ID", &opts.sandbox_id);
                env_vars.store("E2B_TEMPLATE_ID", &opts.template_id);
                let dir = std::path::Path::new(E2B_RUN_DIR);
                if let Err(e) = std::fs::write(dir.join(".E2B_SANDBOX_ID"), &opts.sandbox_id) {
                    eprintln!("error writing sandbox ID file: {e}");
                }
                if let Err(e) = std::fs::write(dir.join(".E2B_TEMPLATE_ID"), &opts.template_id) {
                    eprintln!("error writing template ID file: {e}");
                }
                return;
            }
            Err(e) => last_err = Some(e),
        }
    }
    eprintln!("<4>gave up polling for mmds opts (last error: {last_err:?})");
}
