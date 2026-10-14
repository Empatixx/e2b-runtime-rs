use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

use crate::envvars::EnvVars;

const MMDS_ADDRESS: &str = "http://169.254.169.254";
const E2B_RUN_DIR: &str = "/run/e2b";
const MMDS_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Default, Deserialize)]
pub struct MmdsOpts {
    #[serde(rename = "instanceID", default)]
    pub sandbox_id: String,
    #[serde(rename = "envID", default)]
    pub template_id: String,
    #[serde(rename = "address", default)]
    pub logs_collector_address: String,
    #[serde(rename = "accessTokenHash", default)]
    pub access_token_hash: String,
}

pub async fn get_opts() -> Result<MmdsOpts, String> {
    let client = mmds_client()?;
    let token = fetch_token(&client)
        .await
        .map_err(|e| format!("failed to get MMDS token: {e}"))?;
    fetch_opts(&client, &token)
        .await
        .map_err(|e| format!("failed to get MMDS opts: {e}"))
}

pub async fn poll_for_opts(env_vars: &EnvVars, limit: Option<Duration>) -> Option<MmdsOpts> {
    let deadline = limit.map(|limit| tokio::time::Instant::now() + limit);
    let client = mmds_client().ok()?;
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    tick.tick().await;
    let mut last_error = None;
    loop {
        if deadline.is_some_and(|deadline| tokio::time::Instant::now() >= deadline) {
            match &last_error {
                Some(last) => {
                    eprintln!("<4>gave up polling for mmds opts: context deadline exceeded (last error: {last})")
                }
                None => eprintln!("<4>gave up polling for mmds opts: context deadline exceeded"),
            }
            return None;
        }
        tick.tick().await;
        match poll_once(&client).await {
            Ok(opts) => {
                record(env_vars, &opts);
                return Some(opts);
            }
            Err(e) => last_error = Some(e),
        }
    }
}

async fn poll_once(client: &reqwest::Client) -> Result<MmdsOpts, String> {
    let token = fetch_token(client).await.map_err(|e| format!("get mmds token: {e}"))?;
    fetch_opts(client, &token)
        .await
        .map_err(|e| format!("get mmds opts: {e}"))
}

fn record(env_vars: &EnvVars, opts: &MmdsOpts) {
    env_vars.store("E2B_SANDBOX_ID", &opts.sandbox_id);
    env_vars.store("E2B_TEMPLATE_ID", &opts.template_id);
    let run_dir = Path::new(E2B_RUN_DIR);
    if let Err(e) = std::fs::write(run_dir.join(".E2B_SANDBOX_ID"), &opts.sandbox_id) {
        eprintln!("error writing sandbox ID file: {e}");
    }
    if let Err(e) = std::fs::write(run_dir.join(".E2B_TEMPLATE_ID"), &opts.template_id) {
        eprintln!("error writing template ID file: {e}");
    }
}

fn mmds_client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(MMDS_REQUEST_TIMEOUT)
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| e.to_string())
}

async fn fetch_token(client: &reqwest::Client) -> Result<String, String> {
    let token = client
        .put(format!("{MMDS_ADDRESS}/latest/api/token"))
        .header("X-metadata-token-ttl-seconds", "60")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    match token.is_empty() {
        true => Err("mmds token is an empty string".to_owned()),
        false => Ok(token),
    }
}

async fn fetch_opts(client: &reqwest::Client, token: &str) -> Result<MmdsOpts, String> {
    let body = client
        .get(MMDS_ADDRESS)
        .header("X-metadata-token", token)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&body).map_err(|e| e.to_string())
}

pub fn mark_sandbox(env_vars: &EnvVars, is_not_fc: bool) {
    if let Err(e) = std::fs::create_dir_all(E2B_RUN_DIR) {
        eprintln!("error creating E2B run directory: {e}");
    }
    let is_firecracker = (!is_not_fc).to_string();
    env_vars.store("E2B_SANDBOX", &is_firecracker);
    let marker = Path::new(E2B_RUN_DIR).join(".E2B_SANDBOX");
    let written = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o444)
        .open(&marker)
        .and_then(|mut file| std::io::Write::write_all(&mut file, is_firecracker.as_bytes()));
    if let Err(e) = written {
        eprintln!("error writing sandbox file: {e}");
    }
}
