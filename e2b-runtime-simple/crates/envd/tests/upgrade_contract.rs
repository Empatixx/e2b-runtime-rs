#![cfg(target_os = "linux")]

use std::process::{Child, Command};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

struct Envd {
    child: Child,
    base: String,
}

impl Drop for Envd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn envd(port: u16) -> Envd {
    let child = Command::new(env!("CARGO_BIN_EXE_envd"))
        .args(["-isnotfc", "-no-cgroups", "-port", &port.to_string()])
        .spawn()
        .unwrap();
    Envd {
        child,
        base: format!("http://127.0.0.1:{port}"),
    }
}

async fn healthy(base: &str) {
    for _ in 0..100 {
        if reqwest::get(format!("{base}/health"))
            .await
            .is_ok_and(|res| res.status() == 204)
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("envd did not come up at {base}");
}

async fn unary(base: &str, method: &str, body: Value) -> Value {
    reqwest::Client::new()
        .post(format!("{base}/process.Process/{method}"))
        .header("content-type", "application/json")
        .header("authorization", format!("Basic {}", STANDARD.encode("root:")))
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn envelope(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).unwrap();
    [vec![0], (json.len() as u32).to_be_bytes().to_vec(), json].concat()
}

async fn stream(base: &str, method: &str, body: Value) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{base}/process.Process/{method}"))
        .header("content-type", "application/connect+json")
        .header("authorization", format!("Basic {}", STANDARD.encode("root:")))
        .body(envelope(&body))
        .send()
        .await
        .unwrap()
}

async fn read_until(response: &mut reqwest::Response, needle: &str) -> String {
    let mut seen = String::new();
    while !seen.contains(needle) {
        let chunk = tokio::time::timeout(Duration::from_secs(10), response.chunk())
            .await
            .expect("output in time")
            .unwrap()
            .expect("stream open");
        seen.push_str(&String::from_utf8_lossy(&chunk));
    }
    seen
}

#[tokio::test]
async fn a_live_upgrade_keeps_running_processes_and_their_pipes() {
    if !nix::unistd::geteuid().is_root() {
        eprintln!("skipping: the handover blob lives in /run/e2b and needs root");
        return;
    }
    let server = envd(49_871);
    healthy(&server.base).await;
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .post(format!("{}/init", server.base))
            .send()
            .await
            .unwrap()
            .status(),
        204
    );
    let mut started = stream(
        &server.base,
        "Start",
        json!({ "process": { "cmd": "/bin/cat" }, "tag": "kept", "stdin": true }),
    )
    .await;
    let start = read_until(&mut started, "pid").await;
    let pid: u64 = start
        .split("\"pid\":")
        .nth(1)
        .and_then(|rest| rest.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|digits| digits.parse().ok())
        .unwrap();
    drop(started);

    let upgraded = client.post(format!("{}/upgrade", server.base)).send().await;
    assert!(upgraded.is_err() || upgraded.is_ok_and(|res| res.status() != 500));
    healthy(&server.base).await;
    let init = client.post(format!("{}/init", server.base)).send().await.unwrap();
    let report: Value = serde_json::from_str(init.headers()["x-envd-handover"].to_str().unwrap()).unwrap();
    let listed = unary(&server.base, "List", json!({})).await;
    let mut connected = stream(&server.base, "Connect", json!({ "process": { "pid": pid } })).await;
    unary(
        &server.base,
        "SendInput",
        json!({ "process": { "pid": pid }, "input": { "stdin": STANDARD.encode("still here\n") } }),
    )
    .await;
    let echoed = read_until(&mut connected, &STANDARD.encode("still here\n")).await;

    assert_eq!(
        report,
        json!({ "failed": false, "procs": 1, "procs_failed": 0, "retained": 0, "retained_failed": 0, "watchers": 0, "watchers_failed": 0 })
    );
    assert_eq!(listed["processes"][0]["pid"], pid);
    assert_eq!(listed["processes"][0]["tag"], "kept");
    assert!(echoed.contains(&STANDARD.encode("still here\n")));
    unary(
        &server.base,
        "SendSignal",
        json!({ "process": { "pid": pid }, "signal": "SIGNAL_SIGKILL" }),
    )
    .await;
}
