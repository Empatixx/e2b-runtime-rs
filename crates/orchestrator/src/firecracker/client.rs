use std::path::PathBuf;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use tokio::net::UnixStream;

use super::config::VmConfig;
use super::mmds::MmdsMetadata;
use crate::network::slot::{GUEST_MAC, TAP_NAME};

const API_TIMEOUT: Duration = Duration::from_secs(10);

pub struct FirecrackerClient {
    socket: PathBuf,
    timeout: Duration,
}

impl FirecrackerClient {
    pub fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            timeout: API_TIMEOUT,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub async fn boot(&self, config: &VmConfig) -> Result<(), String> {
        self.put(
            "/machine-config",
            json!({ "vcpu_count": config.vcpus, "mem_size_mib": config.memory_mib }),
        )
        .await?;
        self.put(
            "/boot-source",
            json!({ "kernel_image_path": config.kernel, "boot_args": config.boot_args() }),
        )
        .await?;
        self.put(
            "/drives/rootfs",
            json!({ "drive_id": "rootfs", "path_on_host": config.rootfs, "is_root_device": true, "is_read_only": false }),
        )
        .await?;
        self.put(
            "/network-interfaces/eth0",
            json!({ "iface_id": "eth0", "host_dev_name": TAP_NAME, "guest_mac": GUEST_MAC }),
        )
        .await?;
        self.put(
            "/mmds/config",
            json!({ "version": "V2", "network_interfaces": ["eth0"] }),
        )
        .await?;
        self.put("/actions", json!({ "action_type": "InstanceStart" })).await
    }

    pub async fn set_mmds(&self, metadata: &MmdsMetadata) -> Result<(), String> {
        self.put("/mmds", metadata.to_json()).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<(), String> {
        let call = format!("PUT {path}");
        tokio::time::timeout(self.timeout, self.send(&call, path, body))
            .await
            .unwrap_or_else(|_| Err(format!("{call}: timed out after {:?}", self.timeout)))
    }

    async fn send(&self, call: &str, path: &str, body: Value) -> Result<(), String> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("{call}: connect {}: {e}", self.socket.display()))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("{call}: {e}"))?;
        tokio::spawn(connection);

        let request = Request::put(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body.to_string())))
            .map_err(|e| format!("{call}: {e}"))?;
        let response = sender.send_request(request).await.map_err(|e| format!("{call}: {e}"))?;
        let status = response.status();
        if status.is_success() {
            return Ok(());
        }
        let body = response
            .into_body()
            .collect()
            .await
            .map(|collected| String::from_utf8_lossy(&collected.to_bytes()).into_owned())
            .unwrap_or_default();
        Err(format!("{call}: {status}: {body}"))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    use super::*;
    use crate::firecracker::config::VmConfig;
    use crate::firecracker::mmds::MmdsMetadata;

    #[derive(Debug, Clone, PartialEq)]
    struct Call {
        method: String,
        path: String,
        body: Value,
    }

    #[derive(Clone, Copy, Default)]
    struct Faults {
        fail: Option<&'static str>,
        hang: Option<&'static str>,
    }

    async fn fake_firecracker(socket: PathBuf, faults: Faults) -> Arc<Mutex<Vec<Call>>> {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let listener = UnixListener::bind(&socket).unwrap();
        let recorded = calls.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                reader.read_line(&mut request_line).await.unwrap();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).await.unwrap();
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).await.unwrap();
                let mut parts = request_line.split_whitespace();
                let call = Call {
                    method: parts.next().unwrap().to_owned(),
                    path: parts.next().unwrap().to_owned(),
                    body: serde_json::from_slice(&body).unwrap(),
                };
                let failing = faults.fail == Some(call.path.as_str());
                let hanging = faults.hang == Some(call.path.as_str());
                recorded.lock().push(call);
                if hanging {
                    tokio::spawn(async move {
                        let _held = reader;
                        std::future::pending::<()>().await
                    });
                    continue;
                }
                let response = if failing {
                    "HTTP/1.1 400 Bad Request\r\ncontent-length: 27\r\n\r\n{\"fault_message\":\"bad cfg\"}"
                } else {
                    "HTTP/1.1 204 No Content\r\n\r\n"
                };
                reader.get_mut().write_all(response.as_bytes()).await.unwrap();
            }
        });
        calls
    }

    fn config() -> VmConfig {
        VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/dev/nbd3")
            .memory_mib(512)
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn boot_sends_the_api_calls_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;

        FirecrackerClient::new(socket).boot(&config()).await.unwrap();

        let calls = calls.lock().clone();
        let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/machine-config",
                "/boot-source",
                "/drives/rootfs",
                "/network-interfaces/eth0",
                "/mmds/config",
                "/actions"
            ]
        );
        assert!(calls.iter().all(|call| call.method == "PUT"));
        assert_eq!(calls[0].body, json!({ "vcpu_count": 1, "mem_size_mib": 512 }));
        assert_eq!(
            calls[2].body,
            json!({ "drive_id": "rootfs", "path_on_host": "/dev/nbd3", "is_root_device": true, "is_read_only": false })
        );
        assert_eq!(
            calls[3].body,
            json!({ "iface_id": "eth0", "host_dev_name": "tap0", "guest_mac": "02:FC:00:00:00:05" })
        );
        assert_eq!(
            calls[4].body,
            json!({ "version": "V2", "network_interfaces": ["eth0"] })
        );
        assert_eq!(calls[5].body, json!({ "action_type": "InstanceStart" }));
    }

    #[tokio::test]
    async fn api_errors_name_the_call_status_and_body() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let faults = Faults {
            fail: Some("/boot-source"),
            ..Faults::default()
        };
        let _calls = fake_firecracker(socket.clone(), faults).await;

        let error = FirecrackerClient::new(socket).boot(&config()).await.unwrap_err();
        assert_eq!(
            error,
            "PUT /boot-source: 400 Bad Request: {\"fault_message\":\"bad cfg\"}"
        );
    }

    #[tokio::test]
    async fn set_mmds_puts_the_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;
        let metadata = MmdsMetadata {
            sandbox_id: "sbx".into(),
            template_id: "tpl".into(),
            logs_collector_address: String::new(),
            access_token_hash: None,
        };

        FirecrackerClient::new(socket).set_mmds(&metadata).await.unwrap();

        let calls = calls.lock().clone();
        assert_eq!(
            calls,
            vec![Call {
                method: "PUT".into(),
                path: "/mmds".into(),
                body: metadata.to_json(),
            }]
        );
    }

    #[tokio::test]
    async fn a_silent_api_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let faults = Faults {
            hang: Some("/boot-source"),
            ..Faults::default()
        };
        let _calls = fake_firecracker(socket.clone(), faults).await;

        let error = FirecrackerClient::new(socket)
            .timeout(Duration::from_millis(100))
            .put("/boot-source", json!({}))
            .await
            .unwrap_err();
        assert_eq!(error, "PUT /boot-source: timed out after 100ms");
    }
}
