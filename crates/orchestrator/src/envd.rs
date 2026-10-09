pub mod files;
pub mod process;
pub mod version;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use crate::go_time::rfc3339_nano;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use http_body_util::Full;
use hyper::Request;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::{Digest, Sha512};
use tokio::net::TcpStream;
use tokio::time::Instant;

pub const ENVD_PORT: u16 = 49983;
const RETRY_DELAY: Duration = Duration::from_millis(5);
const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_millis(50);

#[derive(Debug, Clone, Default)]
pub struct EnvdDefaults {
    pub user: Option<String>,
    pub workdir: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvdInit {
    pub access_token: Option<String>,
    pub default_user: Option<String>,
    pub default_workdir: Option<String>,
    pub env_vars: BTreeMap<String, String>,
    pub lifecycle_id: Option<String>,
    pub hyperloop_ip: Option<String>,
    pub volume_mounts: Vec<VolumeMount>,
    pub request_timeout: Option<Duration>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct VolumeMount {
    pub nfs_target: String,
    pub path: String,
}

impl EnvdInit {
    pub fn body(&self, timestamp: DateTime<Utc>) -> Value {
        let mut body = json!({
            "timestamp": rfc3339_nano(timestamp.into()),
        });
        if !self.env_vars.is_empty() {
            body["envVars"] = json!(self.env_vars);
        }
        [
            ("accessToken", &self.access_token),
            ("defaultUser", &self.default_user),
            ("defaultWorkdir", &self.default_workdir),
            ("lifecycleID", &self.lifecycle_id),
            ("hyperloopIP", &self.hyperloop_ip),
        ]
        .into_iter()
        .filter_map(|(key, value)| {
            value
                .as_ref()
                .filter(|value| !value.is_empty())
                .map(|value| (key, value))
        })
        .for_each(|(key, value)| body[key] = json!(value));
        if !self.volume_mounts.is_empty() {
            body["volumeMounts"] = json!(self.volume_mounts);
        }
        body
    }
}

pub fn hash_access_token(token: &str) -> String {
    format!("{:x}", Sha512::digest(token.as_bytes()))
}

pub async fn init(address: SocketAddr, settings: &EnvdInit, deadline: Instant) -> Result<u32, String> {
    let mut attempts = 0;
    loop {
        attempts += 1;
        match attempt(address, settings, deadline).await {
            Ok(204) => return Ok(attempts),
            Ok(status) => return Err(format!("unexpected status code: {status}")),
            Err(error) if Instant::now() + RETRY_DELAY >= deadline => {
                return Err(format!("failed to init envd: {error}"));
            }
            Err(_) => tokio::time::sleep(RETRY_DELAY).await,
        }
    }
}

async fn attempt(address: SocketAddr, settings: &EnvdInit, deadline: Instant) -> Result<u16, String> {
    let budget = settings
        .request_timeout
        .unwrap_or(DEFAULT_ATTEMPT_TIMEOUT)
        .min(deadline.saturating_duration_since(Instant::now()));
    tokio::time::timeout(budget, post_init(address, settings))
        .await
        .unwrap_or_else(|_| Err(format!("attempt timed out after {budget:?}")))
}

async fn post_init(address: SocketAddr, settings: &EnvdInit) -> Result<u16, String> {
    let stream = TcpStream::connect(address)
        .await
        .map_err(|e| format!("connect {address}: {e}"))?;
    let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(connection);
    let response = sender
        .send_request(init_request(address, settings)?)
        .await
        .map_err(|e| e.to_string())?;
    Ok(response.status().as_u16())
}

fn init_request(address: SocketAddr, settings: &EnvdInit) -> Result<Request<Full<Bytes>>, String> {
    let body = Full::new(Bytes::from(settings.body(Utc::now()).to_string()));
    settings
        .access_token
        .iter()
        .fold(Request::post("/init"), |request, token| {
            request.header("x-access-token", token)
        })
        .header("host", address.to_string())
        .header("content-type", "application/json")
        .body(body)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use chrono::TimeZone;
    use parking_lot::Mutex;
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Debug, Clone, Default)]
    struct Received {
        request_line: String,
        headers: Vec<String>,
        body: Vec<u8>,
    }

    async fn fake_envd(listener: TcpListener, status: &'static str) -> Arc<Mutex<Vec<Received>>> {
        let received = Arc::new(Mutex::new(Vec::new()));
        let recorded = received.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut request = Received::default();
                reader.read_line(&mut request.request_line).await.unwrap();
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    request.headers.push(line.trim().to_ascii_lowercase());
                }
                let length = request
                    .headers
                    .iter()
                    .find_map(|header| header.strip_prefix("content-length:"))
                    .map_or(0, |value| value.trim().parse().unwrap());
                request.body = vec![0; length];
                reader.read_exact(&mut request.body).await.unwrap();
                recorded.lock().push(request);
                let response = format!("HTTP/1.1 {status}\r\ncontent-length: 0\r\n\r\n");
                reader.get_mut().write_all(response.as_bytes()).await.unwrap();
            }
        });
        received
    }

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    fn free_address() -> SocketAddr {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }

    fn settings() -> EnvdInit {
        EnvdInit {
            access_token: Some("t".into()),
            default_user: Some("user".into()),
            default_workdir: Some("/home/user".into()),
            env_vars: BTreeMap::from([("A".into(), "1".into())]),
            ..EnvdInit::default()
        }
    }

    fn soon(duration: Duration) -> Instant {
        Instant::now() + duration
    }

    #[test]
    fn body_uses_upstream_keys() {
        let timestamp = Utc.timestamp_opt(1_791_540_000, 1).unwrap();
        assert_eq!(
            settings().body(timestamp),
            json!({
                "envVars": {"A": "1"},
                "accessToken": "t",
                "timestamp": "2026-10-09T10:00:00.000000001Z",
                "defaultUser": "user",
                "defaultWorkdir": "/home/user"
            })
        );
        assert_eq!(
            EnvdInit::default().body(timestamp),
            json!({
                "timestamp": "2026-10-09T10:00:00.000000001Z"
            })
        );
    }

    #[test]
    fn lifecycle_hyperloop_and_volume_mounts_are_sent_when_set() {
        let timestamp = Utc.timestamp_opt(1_791_540_000, 1).unwrap();
        let init = EnvdInit {
            lifecycle_id: Some("l1".to_owned()),
            hyperloop_ip: Some("192.0.2.1".to_owned()),
            volume_mounts: vec![VolumeMount {
                nfs_target: "192.0.2.1:/data".to_owned(),
                path: "/mnt/data".to_owned(),
            }],
            ..EnvdInit::default()
        };

        let body = init.body(timestamp);

        assert_eq!(body["lifecycleID"], "l1");
        assert_eq!(body["hyperloopIP"], "192.0.2.1");
        assert_eq!(
            body["volumeMounts"],
            json!([{"nfs_target": "192.0.2.1:/data", "path": "/mnt/data"}])
        );
    }

    #[test]
    fn hash_matches_envd() {
        assert_eq!(
            hash_access_token("abc"),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[tokio::test]
    async fn init_sends_token_header_and_accepts_204() {
        let (listener, address) = listener().await;
        let received = fake_envd(listener, "204 No Content").await;

        init(address, &settings(), soon(Duration::from_secs(5))).await.unwrap();

        let request = received.lock()[0].clone();
        assert_eq!(request.request_line.trim(), "POST /init HTTP/1.1");
        assert!(
            request.headers.contains(&"x-access-token: t".to_owned()),
            "{:?}",
            request.headers
        );
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["defaultUser"], "user");
    }

    #[tokio::test]
    async fn init_retries_until_envd_listens() {
        let address = free_address();
        let late_envd = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            fake_envd(TcpListener::bind(address).await.unwrap(), "204 No Content").await
        };

        let settings = settings();
        let (initialised, _received) = tokio::join!(init(address, &settings, soon(Duration::from_secs(5))), late_envd);

        assert!(initialised.unwrap() > 1, "the first attempts ran before envd listened");
    }

    #[tokio::test]
    async fn init_rejects_non_204() {
        let (listener, address) = listener().await;
        let _received = fake_envd(listener, "401 Unauthorized").await;

        let error = init(address, &settings(), soon(Duration::from_secs(5)))
            .await
            .unwrap_err();

        assert_eq!(error, "unexpected status code: 401");
    }

    #[tokio::test]
    async fn init_gives_up_at_the_deadline() {
        let address = free_address();
        let started = Instant::now();

        let error = init(address, &settings(), soon(Duration::from_millis(300)))
            .await
            .unwrap_err();

        assert!(error.starts_with("failed to init envd: "), "{error}");
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn init_never_overshoots_the_deadline() {
        let (listener, address) = listener().await;
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        let started = Instant::now();

        let error = init(address, &settings(), soon(Duration::from_millis(250)))
            .await
            .unwrap_err();

        assert!(error.starts_with("failed to init envd: "), "{error}");
        assert!(
            started.elapsed() < Duration::from_millis(290),
            "{:?}",
            started.elapsed()
        );
    }
}
