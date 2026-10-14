use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Result, bail};
use base64::Engine;
use serde::Serialize;
use sha2::{Digest, Sha512};
use tokio::time::{Instant, sleep};

pub const ENVD_PORT: u16 = 49983;
const HEALTH_TIMEOUT: Duration = Duration::from_secs(20);
const HEALTH_POLL: Duration = Duration::from_millis(5);
const LIFECYCLE_ID: &str = "e2b-simple";
const GUEST_USER: &str = "user";

pub struct Envd {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitRequest<'a> {
    env_vars: &'a HashMap<String, String>,
    access_token: &'a str,
    timestamp: String,
    default_user: &'a str,
    default_workdir: String,
    volume_mounts: Vec<VolumeMount>,
    #[serde(rename = "lifecycleID")]
    lifecycle_id: &'a str,
}

#[derive(Serialize)]
pub struct VolumeMount {
    pub nfs_target: String,
    pub path: String,
}

pub fn hash_token(token: &str) -> String {
    hex::encode(Sha512::digest(token.as_bytes()))
}

impl Envd {
    pub fn new(host: Ipv4Addr) -> Self {
        Self {
            base_url: format!("http://{host}:{ENVD_PORT}"),
            http: reqwest::Client::builder().no_proxy().build().expect("http client"),
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn wait_ready(&self) -> Result<()> {
        let deadline = Instant::now() + HEALTH_TIMEOUT;
        while Instant::now() < deadline {
            if self.healthy().await {
                return Ok(());
            }
            sleep(HEALTH_POLL).await;
        }
        bail!("envd at {} did not become healthy", self.base_url)
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn init(&self, token: &str, env_vars: &HashMap<String, String>, mounts: Vec<VolumeMount>) -> Result<()> {
        let request = InitRequest {
            env_vars,
            access_token: token,
            timestamp: chrono::Utc::now().to_rfc3339(),
            default_user: GUEST_USER,
            default_workdir: format!("/home/{GUEST_USER}"),
            volume_mounts: mounts,
            lifecycle_id: LIFECYCLE_ID,
        };
        let response = self
            .http
            .post(format!("{}/init", self.base_url))
            .header("X-Access-Token", token)
            .json(&request)
            .send()
            .await?;
        match response.status().is_success() {
            true => Ok(()),
            false => bail!(
                "envd init: {} {}",
                response.status(),
                response.text().await.unwrap_or_default()
            ),
        }
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn sync_disks(&self, token: &str) -> Result<()> {
        let response = self
            .http
            .post(format!("{}/process.Process/Start", self.base_url))
            .header("Content-Type", "application/connect+json")
            .header("Connect-Protocol-Version", "1")
            .header("Authorization", basic_auth("root"))
            .header("X-Access-Token", token)
            .body(connect_envelope(br#"{"process":{"cmd":"/bin/sync"}}"#))
            .timeout(Duration::from_secs(30))
            .send()
            .await?;
        let status = response.status();
        let body = response.bytes().await?;
        match status.is_success() {
            true => Ok(()),
            false => bail!("envd sync: {status} {}", String::from_utf8_lossy(&body)),
        }
    }

    async fn healthy(&self) -> bool {
        self.http
            .get(format!("{}/health", self.base_url))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }
}

fn basic_auth(user: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{user}:"))
    )
}

fn connect_envelope(message: &[u8]) -> Vec<u8> {
    let length = (message.len() as u32).to_be_bytes();
    [&[0u8][..], &length, message].concat()
}
