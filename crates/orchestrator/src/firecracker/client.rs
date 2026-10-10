use std::path::{Path, PathBuf};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::client::conn::http1;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::net::UnixStream;

use super::config::VmConfig;
use super::memory_info::{DirtyMemory, GuestRegion, MemoryInfo, MemoryMappings};
use super::mmds::MmdsMetadata;
use crate::network::slot::{GUEST_MAC, TAP_NAME};
use roaring::RoaringBitmap;

const ENTROPY_BYTES_SIZE: u64 = 1024;
const ENTROPY_REFILL_TIME: u64 = 100;
const ENTROPY_ONE_TIME_BURST: u64 = 0;
const HUGE_PAGES_2M: &str = "2M";

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

    pub async fn boot(&self, config: &VmConfig, metadata: Option<&MmdsMetadata>) -> Result<(), String> {
        self.put(
            "/boot-source",
            json!({ "kernel_image_path": config.kernel, "boot_args": config.boot_args }),
        )
        .await?;
        self.put("/drives/rootfs", root_drive(config)).await?;
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
        self.put("/machine-config", machine_config(config)).await?;
        self.set_cpu_config(config).await?;
        self.put("/entropy", entropy_device()).await?;
        self.install_balloon(config.balloon).await?;
        if let Some(metadata) = metadata {
            self.set_mmds(metadata).await?;
        }
        self.put("/actions", json!({ "action_type": "InstanceStart" })).await
    }

    async fn set_cpu_config(&self, config: &VmConfig) -> Result<(), String> {
        match config.cpu_template.as_ref().filter(|template| !template.is_empty()) {
            Some(template) => self.put("/cpu-config", json!(template)).await.map_err(|e| {
                format!(
                    "error setting fc cpu config: error setting fc cpu config (template {}): {e}",
                    template.digest()
                )
            }),
            None => Ok(()),
        }
    }

    async fn install_balloon(&self, balloon: BalloonCaps) -> Result<(), String> {
        match balloon.reporting || balloon.hinting {
            true => self
                .put("/balloon", balloon_device(balloon))
                .await
                .map_err(|e| format!("error installing balloon device: error installing balloon device: {e}")),
            false => Ok(()),
        }
    }

    pub async fn set_rate_limits(&self) -> Result<(), String> {
        self.patch(
            "/network-interfaces/eth0",
            json!({ "iface_id": "eth0", "tx_rate_limiter": {} }),
        )
        .await?;
        self.patch("/drives/rootfs", json!({ "drive_id": "rootfs", "rate_limiter": {} }))
            .await
    }

    pub async fn set_mmds(&self, metadata: &MmdsMetadata) -> Result<(), String> {
        self.put("/mmds", metadata.to_json()).await
    }

    pub async fn pause(&self) -> Result<(), String> {
        self.patch("/vm", json!({ "state": "Paused" })).await
    }

    pub async fn resume(&self) -> Result<(), String> {
        self.patch("/vm", json!({ "state": "Resumed" })).await
    }

    pub async fn create_snapshot(&self, snapfile: &Path) -> Result<(), String> {
        self.put(
            "/snapshot/create",
            json!({ "snapshot_type": "Full", "snapshot_path": snapfile }),
        )
        .await
    }

    pub async fn load_snapshot(
        &self,
        snapfile: &Path,
        uffd_socket: &Path,
        use_memfd: bool,
        use_sync_wp: bool,
    ) -> Result<(), String> {
        let mut backend = json!({ "backend_type": "Uffd", "backend_path": uffd_socket });
        if use_memfd {
            backend["use_memfd"] = json!(true);
        }
        if use_sync_wp {
            backend["use_sync_wp"] = json!(true);
        }
        self.put(
            "/snapshot/load",
            json!({
                "snapshot_path": snapfile,
                "enable_diff_snapshots": false,
                "resume_vm": false,
                "mem_backend": backend,
            }),
        )
        .await
    }

    pub async fn memory_mappings(&self) -> Result<Vec<GuestRegion>, String> {
        self.get::<MemoryMappings>("/memory/mappings")
            .await
            .map(|response| response.mappings)
    }

    pub async fn memory_dirty(&self) -> Result<RoaringBitmap, String> {
        self.get::<DirtyMemory>("/memory/dirty")
            .await
            .map(|dirty| dirty.pages())
    }

    pub async fn memory_info(&self) -> Result<MemoryInfo, String> {
        self.get("/memory").await
    }

    pub async fn balloon_caps(&self) -> Result<BalloonCaps, String> {
        let call = "GET /balloon";
        let (status, body) = self.exchange(Method::GET, "/balloon", None).await?;
        match status {
            StatusCode::BAD_REQUEST => Ok(BalloonCaps::default()),
            status if status.is_success() => serde_json::from_slice::<BalloonDevice>(&body)
                .map(|device| BalloonCaps {
                    reporting: device.free_page_reporting,
                    hinting: device.free_page_hinting,
                })
                .map_err(|e| format!("{call}: {e}")),
            status => Err(format!("{call}: {status}: {}", String::from_utf8_lossy(&body))),
        }
    }

    pub async fn pause_free_page_reporting(&self) -> Result<(), String> {
        self.request(Method::PATCH, "/balloon/reporting/pause", None)
            .await
            .map(|_| ())
    }

    pub async fn resume_free_page_reporting(&self) -> Result<(), String> {
        self.request(Method::PATCH, "/balloon/reporting/resume", None)
            .await
            .map(|_| ())
    }

    pub async fn free_page_reporting_paused(&self) -> Result<bool, String> {
        let status: ReportingStatus = self.get("/balloon/reporting/status").await?;
        status
            .paused
            .ok_or_else(|| "balloon free-page reporting status carried no paused field".to_owned())
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<(), String> {
        self.request(Method::PUT, path, Some(body)).await.map(|_| ())
    }

    async fn patch(&self, path: &str, body: Value) -> Result<(), String> {
        self.request(Method::PATCH, path, Some(body)).await.map(|_| ())
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T, String> {
        let body = self.request(Method::GET, path, None).await?;
        serde_json::from_slice(&body).map_err(|e| format!("GET {path}: {e}"))
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Bytes, String> {
        let call = format!("{method} {path}");
        let (status, body) = self.exchange(method, path, body).await?;
        status
            .is_success()
            .then_some(body.clone())
            .ok_or_else(|| format!("{call}: {status}: {}", String::from_utf8_lossy(&body)))
    }

    async fn exchange(&self, method: Method, path: &str, body: Option<Value>) -> Result<(StatusCode, Bytes), String> {
        let call = format!("{method} {path}");
        tokio::time::timeout(self.timeout, self.send(&call, method, path, body))
            .await
            .unwrap_or_else(|_| Err(format!("{call}: timed out after {:?}", self.timeout)))
    }

    async fn send(
        &self,
        call: &str,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Bytes), String> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("{call}: connect {}: {e}", self.socket.display()))?;
        let (mut sender, connection) = http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| format!("{call}: {e}"))?;
        tokio::spawn(connection);

        let request = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .body(Full::new(
                body.map_or_else(Bytes::new, |body| Bytes::from(body.to_string())),
            ))
            .map_err(|e| format!("{call}: {e}"))?;
        let response = sender.send_request(request).await.map_err(|e| format!("{call}: {e}"))?;
        let status = response.status();
        let body = response
            .into_body()
            .collect()
            .await
            .map(|collected| collected.to_bytes())
            .map_err(|e| format!("{call}: {e}"))?;
        Ok((status, body))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BalloonCaps {
    pub reporting: bool,
    pub hinting: bool,
}

#[derive(serde::Deserialize)]
struct BalloonDevice {
    #[serde(default)]
    free_page_reporting: bool,
    #[serde(default)]
    free_page_hinting: bool,
}

#[derive(serde::Deserialize)]
struct ReportingStatus {
    paused: Option<bool>,
}

fn machine_config(config: &VmConfig) -> serde_json::Value {
    let mut machine = json!({
        "vcpu_count": config.vcpus,
        "mem_size_mib": config.memory_mib,
        "smt": std::env::consts::ARCH != "aarch64",
        "track_dirty_pages": false
    });
    if config.huge_pages {
        machine["huge_pages"] = json!(HUGE_PAGES_2M);
    }
    machine
}

fn entropy_device() -> serde_json::Value {
    json!({
        "rate_limiter": {
            "bandwidth": {
                "one_time_burst": ENTROPY_ONE_TIME_BURST,
                "size": ENTROPY_BYTES_SIZE,
                "refill_time": ENTROPY_REFILL_TIME
            }
        }
    })
}

fn balloon_device(balloon: BalloonCaps) -> serde_json::Value {
    json!({
        "amount_mib": 0,
        "deflate_on_oom": false,
        "free_page_reporting": balloon.reporting,
        "free_page_hinting": balloon.hinting
    })
}

fn root_drive(config: &VmConfig) -> serde_json::Value {
    let mut drive =
        json!({ "drive_id": "rootfs", "path_on_host": config.rootfs, "is_root_device": true, "is_read_only": false });
    if let Some(io_engine) = &config.io_engine {
        drive["io_engine"] = json!(io_engine);
    }
    drive
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
    use crate::firecracker::memory_info::GuestRegion;
    use crate::firecracker::mmds::MmdsMetadata;
    use crate::template::metadata::CpuTemplate;

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
        answers: &'static [(&'static str, &'static str)],
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
                    body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                };
                let failing = faults.fail == Some(call.path.as_str());
                let answer = faults
                    .answers
                    .iter()
                    .find(|(path, _)| *path == call.path)
                    .map(|(_, body)| *body);
                let hanging = faults.hang == Some(call.path.as_str());
                recorded.lock().push(call);
                if hanging {
                    tokio::spawn(async move {
                        let _held = reader;
                        std::future::pending::<()>().await
                    });
                    continue;
                }
                let response = match (failing, answer) {
                    (true, _) => {
                        "HTTP/1.1 400 Bad Request\r\ncontent-length: 27\r\n\r\n{\"fault_message\":\"bad cfg\"}"
                            .to_owned()
                    }
                    (false, Some(body)) => format!("HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}", body.len()),
                    (false, None) => "HTTP/1.1 204 No Content\r\n\r\n".to_owned(),
                };
                reader.get_mut().write_all(response.as_bytes()).await.unwrap();
            }
        });
        calls
    }

    fn metadata() -> MmdsMetadata {
        MmdsMetadata {
            sandbox_id: "sbx".into(),
            template_id: "tpl".into(),
            logs_collector_address: String::new(),
            access_token_hash: None,
        }
    }

    fn config() -> VmConfig {
        VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/dev/nbd3")
            .memory_mib(512)
            .build()
    }

    #[tokio::test]
    async fn boot_sends_the_api_calls_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;

        FirecrackerClient::new(socket)
            .boot(&config(), Some(&metadata()))
            .await
            .unwrap();

        let calls = calls.lock().clone();
        let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/boot-source",
                "/drives/rootfs",
                "/network-interfaces/eth0",
                "/mmds/config",
                "/machine-config",
                "/entropy",
                "/mmds",
                "/actions"
            ]
        );
        assert!(calls.iter().all(|call| call.method == "PUT"));
        assert_eq!(
            calls[1].body,
            json!({ "drive_id": "rootfs", "path_on_host": "/dev/nbd3", "is_root_device": true, "is_read_only": false })
        );
        assert_eq!(
            calls[2].body,
            json!({ "iface_id": "eth0", "host_dev_name": "tap0", "guest_mac": "02:FC:00:00:00:05" })
        );
        assert_eq!(
            calls[3].body,
            json!({ "version": "V2", "network_interfaces": ["eth0"] })
        );
        assert_eq!(
            calls[4].body,
            json!({ "vcpu_count": 1, "mem_size_mib": 512, "smt": std::env::consts::ARCH != "aarch64", "track_dirty_pages": false })
        );
        assert_eq!(
            calls[5].body,
            json!({ "rate_limiter": { "bandwidth": { "one_time_burst": 0, "size": 1024, "refill_time": 100 } } })
        );
        assert_eq!(calls[6].body, metadata().to_json());
        assert_eq!(calls[7].body, json!({ "action_type": "InstanceStart" }));
    }

    #[tokio::test]
    async fn a_build_boot_skips_only_the_mmds_data_and_sets_the_io_engine() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/fc-vm/rootfs.ext4")
            .io_engine("Sync")
            .build();

        FirecrackerClient::new(socket).boot(&config, None).await.unwrap();

        let calls = calls.lock().clone();
        let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "/boot-source",
                "/drives/rootfs",
                "/network-interfaces/eth0",
                "/mmds/config",
                "/machine-config",
                "/entropy",
                "/actions"
            ]
        );
        assert_eq!(calls[1].body["io_engine"], json!("Sync"));
    }

    #[tokio::test]
    async fn a_cpu_template_is_sent_between_the_machine_config_and_the_entropy_device() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;
        let template = CpuTemplate {
            kvm_capabilities: vec!["!121".to_owned()],
            ..CpuTemplate::default()
        };
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/fc-vm/rootfs.ext4")
            .cpu_template(template)
            .build();

        FirecrackerClient::new(socket).boot(&config, None).await.unwrap();

        let calls = calls.lock().clone();
        let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
        assert_eq!(
            paths[4..7],
            ["/machine-config", "/cpu-config", "/entropy"],
            "Firecracker accepts the template only before boot, with the machine config omitting cpu_template"
        );
        assert_eq!(calls[5].body, json!({ "kvm_capabilities": ["!121"] }));
    }

    #[tokio::test]
    async fn an_empty_cpu_template_sends_no_request() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/fc-vm/rootfs.ext4")
            .cpu_template(CpuTemplate::default())
            .build();

        FirecrackerClient::new(socket).boot(&config, None).await.unwrap();

        assert!(calls.lock().iter().all(|call| call.path != "/cpu-config"));
    }

    #[tokio::test]
    async fn a_balloon_is_installed_after_the_entropy_device_like_go() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/fc-vm/rootfs.ext4")
            .balloon(BalloonCaps {
                reporting: true,
                hinting: false,
            })
            .build();

        FirecrackerClient::new(socket).boot(&config, None).await.unwrap();

        let calls = calls.lock().clone();
        let paths: Vec<&str> = calls.iter().map(|call| call.path.as_str()).collect();
        assert_eq!(paths[5..8], ["/entropy", "/balloon", "/actions"]);
        assert_eq!(
            calls[6].body,
            json!({ "amount_mib": 0, "deflate_on_oom": false, "free_page_reporting": true, "free_page_hinting": false })
        );
    }

    #[tokio::test]
    async fn no_balloon_feature_means_no_balloon_device() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;

        FirecrackerClient::new(socket).boot(&config(), None).await.unwrap();

        assert!(calls.lock().iter().all(|call| call.path != "/balloon"));
    }

    #[tokio::test]
    async fn a_refused_balloon_names_the_device() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let faults = Faults {
            fail: Some("/balloon"),
            ..Faults::default()
        };
        let _calls = fake_firecracker(socket.clone(), faults).await;
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/fc-vm/rootfs.ext4")
            .balloon(BalloonCaps {
                reporting: false,
                hinting: true,
            })
            .build();

        let error = FirecrackerClient::new(socket).boot(&config, None).await.unwrap_err();

        assert_eq!(
            error,
            "error installing balloon device: error installing balloon device: PUT /balloon: 400 Bad Request: {\"fault_message\":\"bad cfg\"}"
        );
    }

    #[tokio::test]
    async fn a_huge_pages_boot_asks_for_2m_pages() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), Faults::default()).await;
        let config = VmConfig::builder()
            .kernel("/opt/vmlinux")
            .rootfs("/fc-vm/rootfs.ext4")
            .memory_mib(512)
            .huge_pages(true)
            .build();

        FirecrackerClient::new(socket).boot(&config, None).await.unwrap();

        let calls = calls.lock().clone();
        let machine = calls.iter().find(|call| call.path == "/machine-config").unwrap();
        assert_eq!(
            machine.body,
            json!({ "vcpu_count": 1, "mem_size_mib": 512, "smt": std::env::consts::ARCH != "aarch64", "track_dirty_pages": false, "huge_pages": "2M" })
        );
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

        let error = FirecrackerClient::new(socket)
            .boot(&config(), Some(&metadata()))
            .await
            .unwrap_err();
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

    async fn calls_after(faults: Faults, call: impl AsyncFnOnce(FirecrackerClient)) -> Vec<Call> {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("fc.sock");
        let calls = fake_firecracker(socket.clone(), faults).await;
        call(FirecrackerClient::new(socket)).await;
        calls.lock().clone()
    }

    fn call(method: &str, path: &str, body: Value) -> Call {
        Call {
            method: method.into(),
            path: path.into(),
            body,
        }
    }

    #[tokio::test]
    async fn pause_and_resume_patch_the_vm_state() {
        let calls = calls_after(Faults::default(), async |client| {
            client.pause().await.unwrap();
            client.resume().await.unwrap();
        })
        .await;

        assert_eq!(
            calls,
            vec![
                call("PATCH", "/vm", json!({ "state": "Paused" })),
                call("PATCH", "/vm", json!({ "state": "Resumed" })),
            ]
        );
    }

    #[tokio::test]
    async fn rate_limits_are_reset_on_the_interface_and_the_drive() {
        let calls = calls_after(Faults::default(), async |client| {
            client.set_rate_limits().await.unwrap()
        })
        .await;

        assert_eq!(
            calls,
            vec![
                call(
                    "PATCH",
                    "/network-interfaces/eth0",
                    json!({ "iface_id": "eth0", "tx_rate_limiter": {} })
                ),
                call(
                    "PATCH",
                    "/drives/rootfs",
                    json!({ "drive_id": "rootfs", "rate_limiter": {} })
                ),
            ]
        );
    }

    #[tokio::test]
    async fn create_snapshot_sends_a_full_snapshot_without_a_memory_file() {
        let calls = calls_after(Faults::default(), async |client| {
            client.create_snapshot(Path::new("/run/sbx/snapfile")).await.unwrap();
        })
        .await;

        assert_eq!(
            calls,
            vec![call(
                "PUT",
                "/snapshot/create",
                json!({ "snapshot_type": "Full", "snapshot_path": "/run/sbx/snapfile" })
            )]
        );
    }

    #[tokio::test]
    async fn load_snapshot_uses_the_uffd_backend() {
        let calls = calls_after(Faults::default(), async |client| {
            client
                .load_snapshot(
                    Path::new("/run/sbx/snapfile"),
                    Path::new("/run/sbx/uffd.sock"),
                    false,
                    false,
                )
                .await
                .unwrap();
            client
                .load_snapshot(
                    Path::new("/run/sbx/snapfile"),
                    Path::new("/run/sbx/uffd.sock"),
                    true,
                    false,
                )
                .await
                .unwrap();
        })
        .await;

        assert_eq!(
            calls,
            vec![
                call(
                    "PUT",
                    "/snapshot/load",
                    json!({
                        "snapshot_path": "/run/sbx/snapfile",
                        "enable_diff_snapshots": false,
                        "resume_vm": false,
                        "mem_backend": { "backend_type": "Uffd", "backend_path": "/run/sbx/uffd.sock" }
                    })
                ),
                call(
                    "PUT",
                    "/snapshot/load",
                    json!({
                        "snapshot_path": "/run/sbx/snapfile",
                        "enable_diff_snapshots": false,
                        "resume_vm": false,
                        "mem_backend": { "backend_type": "Uffd", "backend_path": "/run/sbx/uffd.sock", "use_memfd": true }
                    })
                )
            ]
        );
    }

    #[tokio::test]
    async fn memory_mappings_are_parsed() {
        let faults = Faults {
            answers: &[(
                "/memory/mappings",
                r#"{"mappings":[{"base_host_virt_addr":4096,"size":8192,"offset":0,"page_size":4096}]}"#,
            )],
            ..Faults::default()
        };
        let mut mappings = Vec::new();
        let calls = calls_after(faults, async |client| {
            mappings = client.memory_mappings().await.unwrap();
        })
        .await;

        assert_eq!(calls, vec![call("GET", "/memory/mappings", Value::Null)]);
        assert_eq!(
            mappings,
            vec![GuestRegion {
                base_host_virt_addr: 4096,
                size: 8192,
                offset: 0,
                page_size: 4096
            }]
        );
    }

    #[tokio::test]
    async fn memory_dirty_is_parsed() {
        let faults = Faults {
            answers: &[("/memory/dirty", r#"{"bitmap":[3,0,1]}"#)],
            ..Faults::default()
        };
        let mut dirty = None;
        calls_after(faults, async |client| {
            dirty = Some(client.memory_dirty().await.unwrap());
        })
        .await;

        assert_eq!(dirty.unwrap().iter().collect::<Vec<_>>(), vec![0, 1, 128]);
    }

    #[tokio::test]
    async fn memory_info_turns_word_bitmaps_into_page_sets() {
        let faults = Faults {
            answers: &[("/memory", r#"{"resident":[5,1],"empty":[4]}"#)],
            ..Faults::default()
        };
        let mut info = None;
        calls_after(faults, async |client| {
            info = Some(client.memory_info().await.unwrap());
        })
        .await;

        let info = info.unwrap();
        assert_eq!(info.resident.iter().collect::<Vec<_>>(), vec![0, 2, 64]);
        assert_eq!(info.empty.iter().collect::<Vec<_>>(), vec![2]);
    }

    #[tokio::test]
    async fn load_snapshot_asks_for_synchronous_write_protect_when_told_to() {
        let calls = calls_after(Faults::default(), async |client| {
            client
                .load_snapshot(Path::new("/s"), Path::new("/u.sock"), false, true)
                .await
                .unwrap();
        })
        .await;

        assert_eq!(
            calls[0].body["mem_backend"],
            json!({ "backend_type": "Uffd", "backend_path": "/u.sock", "use_sync_wp": true })
        );
    }

    #[tokio::test]
    async fn the_balloon_device_decides_the_caps() {
        let faults = Faults {
            answers: &[(
                "/balloon",
                r#"{"amount_mib":0,"free_page_reporting":true,"free_page_hinting":false}"#,
            )],
            ..Faults::default()
        };
        let caps = std::sync::Arc::new(Mutex::new(None));
        let seen = caps.clone();
        calls_after(faults, async move |client| {
            *seen.lock() = Some(client.balloon_caps().await.unwrap());
        })
        .await;

        assert_eq!(
            *caps.lock(),
            Some(BalloonCaps {
                reporting: true,
                hinting: false
            })
        );
    }

    #[tokio::test]
    async fn no_balloon_device_means_no_caps() {
        let faults = Faults {
            fail: Some("/balloon"),
            ..Faults::default()
        };
        let caps = std::sync::Arc::new(Mutex::new(None));
        let seen = caps.clone();
        calls_after(faults, async move |client| {
            *seen.lock() = Some(client.balloon_caps().await.unwrap());
        })
        .await;

        assert_eq!(*caps.lock(), Some(BalloonCaps::default()));
    }

    #[tokio::test]
    async fn free_page_reporting_is_paused_resumed_and_confirmed_through_the_reporting_endpoints() {
        let faults = Faults {
            answers: &[("/balloon/reporting/status", r#"{"paused":true}"#)],
            ..Faults::default()
        };
        let paused = std::sync::Arc::new(Mutex::new(false));
        let seen = paused.clone();
        let calls = calls_after(faults, async move |client| {
            client.pause_free_page_reporting().await.unwrap();
            *seen.lock() = client.free_page_reporting_paused().await.unwrap();
            client.resume_free_page_reporting().await.unwrap();
        })
        .await;

        assert!(*paused.lock());
        assert_eq!(
            calls
                .iter()
                .map(|call| (call.method.as_str(), call.path.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("PATCH", "/balloon/reporting/pause"),
                ("GET", "/balloon/reporting/status"),
                ("PATCH", "/balloon/reporting/resume")
            ]
        );
    }

    #[tokio::test]
    async fn a_reporting_status_without_the_paused_field_is_an_error() {
        let faults = Faults {
            answers: &[("/balloon/reporting/status", "{}")],
            ..Faults::default()
        };
        let error = std::sync::Arc::new(Mutex::new(String::new()));
        let seen = error.clone();
        calls_after(faults, async move |client| {
            *seen.lock() = client.free_page_reporting_paused().await.unwrap_err();
        })
        .await;

        assert_eq!(
            *error.lock(),
            "balloon free-page reporting status carried no paused field"
        );
    }
}
