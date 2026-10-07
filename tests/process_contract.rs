#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::spawn_server_with_state;
use serde_json::{Value, json};

struct Stream {
    response: reqwest::Response,
    buffer: Vec<u8>,
}

struct Frame {
    end_stream: bool,
    body: Value,
}

fn envelope(body: &Value) -> Vec<u8> {
    let json = serde_json::to_vec(body).unwrap();
    let mut framed = vec![0];
    framed.extend_from_slice(&(json.len() as u32).to_be_bytes());
    framed.extend_from_slice(&json);
    framed
}

async fn open(addr: std::net::SocketAddr, method: &str, body: Value) -> Stream {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/process.Process/{method}"))
        .header("content-type", "application/connect+json")
        .header("keepalive-ping-interval", "60")
        .body(envelope(&body))
        .send()
        .await
        .unwrap();
    Stream {
        response,
        buffer: Vec::new(),
    }
}

async fn next_frame(stream: &mut Stream) -> Option<Frame> {
    loop {
        if stream.buffer.len() >= 5 {
            let len = u32::from_be_bytes(stream.buffer[1..5].try_into().unwrap()) as usize;
            if stream.buffer.len() >= 5 + len {
                let end_stream = stream.buffer[0] & 0x02 != 0;
                let message: Vec<u8> = stream.buffer.drain(..5 + len).skip(5).collect();
                return Some(Frame {
                    end_stream,
                    body: serde_json::from_slice(&message).unwrap(),
                });
            }
        }
        let chunk = tokio::time::timeout(Duration::from_secs(10), stream.response.chunk())
            .await
            .expect("frame in time")
            .unwrap()?;
        stream.buffer.extend_from_slice(&chunk);
    }
}

async fn collect(mut stream: Stream) -> (Vec<Value>, Value) {
    let mut events = Vec::new();
    while let Some(frame) = next_frame(&mut stream).await {
        if frame.end_stream {
            return (events, frame.body);
        }
        events.push(frame.body["event"].clone());
    }
    (events, Value::Null)
}

fn output(events: &[Value], stream: &str) -> String {
    let bytes: Vec<u8> = events
        .iter()
        .filter_map(|event| event["data"][stream].as_str())
        .flat_map(|chunk| STANDARD.decode(chunk).unwrap())
        .collect();
    String::from_utf8(bytes).unwrap()
}

fn start_body(cmd: &str, args: &[&str]) -> Value {
    json!({ "process": { "cmd": cmd, "args": args } })
}

#[tokio::test]
async fn start_streams_start_output_and_end() {
    let (addr, _) = spawn_server_with_state().await;
    let (events, trailer) =
        collect(open(addr, "Start", start_body("/bin/sh", &["-c", "echo hi; echo err >&2"])).await).await;

    assert_eq!(trailer, json!({}));
    assert!(events[0]["start"]["pid"].as_u64().unwrap() > 0, "{events:?}");
    assert_eq!(output(&events, "stdout"), "hi\n");
    assert_eq!(output(&events, "stderr"), "err\n");
    assert_eq!(
        events.last().unwrap(),
        &json!({ "end": { "exited": true, "status": "exit status 0" } })
    );
}

#[tokio::test]
async fn failing_and_killed_processes_report_go_status() {
    let (addr, _) = spawn_server_with_state().await;
    let (events, _) = collect(open(addr, "Start", start_body("/bin/sh", &["-c", "exit 3"])).await).await;
    assert_eq!(
        events.last().unwrap()["end"],
        json!({ "exitCode": 3, "exited": true, "status": "exit status 3", "error": "exit status 3" })
    );

    let (events, _) = collect(open(addr, "Start", start_body("/bin/sh", &["-c", "kill -9 $$"])).await).await;
    assert_eq!(
        events.last().unwrap()["end"],
        json!({ "exitCode": -1, "status": "signal: killed", "error": "signal: killed" })
    );
}

#[tokio::test]
async fn environment_user_and_cwd() {
    let (addr, _) = spawn_server_with_state().await;
    let body = json!({
        "process": {
            "cmd": "/bin/sh",
            "args": ["-c", "echo $HOME $USER $LOGNAME $FOO; pwd; cat /proc/self/oom_score_adj"],
            "envs": { "FOO": "bar" },
            "cwd": "/tmp"
        }
    });
    let (events, _) = collect(open(addr, "Start", body).await).await;
    assert_eq!(output(&events, "stdout"), "/root root root bar\n/tmp\n100\n");
}

#[tokio::test]
async fn missing_cwd_is_invalid_argument() {
    let (addr, _) = spawn_server_with_state().await;
    let body = json!({ "process": { "cmd": "/bin/true", "cwd": "/nope" } });
    let (events, trailer) = collect(open(addr, "Start", body).await).await;
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(trailer["error"]["code"], "invalid_argument");
    assert_eq!(trailer["error"]["message"], "cwd '/nope' does not exist");
}

#[tokio::test]
async fn pty_process_gets_terminal_with_size() {
    let (addr, _) = spawn_server_with_state().await;
    let body = json!({
        "process": { "cmd": "/bin/sh", "args": ["-c", "stty size"] },
        "pty": { "size": { "cols": 100, "rows": 30 } }
    });
    let (events, _) = collect(open(addr, "Start", body).await).await;
    assert_eq!(output(&events, "pty"), "30 100\r\n");
    assert_eq!(events.last().unwrap()["end"]["status"], "exit status 0");
}

#[tokio::test]
async fn connect_after_disconnect_and_after_exit() {
    let (addr, _) = spawn_server_with_state().await;
    let mut started = open(addr, "Start", start_body("/bin/sh", &["-c", "sleep 1; echo later"])).await;
    let first = next_frame(&mut started).await.unwrap();
    let pid = first.body["event"]["start"]["pid"].as_u64().unwrap();
    drop(started);

    let (events, _) = collect(open(addr, "Connect", json!({ "process": { "pid": pid } })).await).await;
    assert_eq!(events[0], json!({ "start": { "pid": pid } }));
    assert_eq!(output(&events, "stdout"), "later\n");
    assert_eq!(events.last().unwrap()["end"]["status"], "exit status 0");

    let (events, _) = collect(open(addr, "Connect", json!({ "process": { "pid": pid } })).await).await;
    assert_eq!(
        events,
        vec![
            json!({ "start": { "pid": pid } }),
            json!({ "end": { "exited": true, "status": "exit status 0" } })
        ]
    );
}

#[tokio::test]
async fn connect_to_unknown_process_is_not_found() {
    let (addr, _) = spawn_server_with_state().await;
    let (_, trailer) = collect(open(addr, "Connect", json!({ "process": { "pid": 999999 } })).await).await;
    assert_eq!(trailer["error"]["code"], "not_found");
    assert_eq!(trailer["error"]["message"], "process with pid 999999 not found");

    let (_, trailer) = collect(open(addr, "Connect", json!({ "process": { "tag": "nope" } })).await).await;
    assert_eq!(trailer["error"]["message"], "process with tag nope not found");
}

#[tokio::test]
async fn request_timeout_kills_the_process() {
    let (addr, _) = spawn_server_with_state().await;
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/process.Process/Start"))
        .header("content-type", "application/connect+json")
        .header("connect-timeout-ms", "500")
        .body(envelope(&start_body("/bin/sleep", &["30"])))
        .send()
        .await
        .unwrap();
    let mut stream = Stream {
        response,
        buffer: Vec::new(),
    };
    let pid = next_frame(&mut stream).await.unwrap().body["event"]["start"]["pid"]
        .as_u64()
        .unwrap();
    while next_frame(&mut stream).await.is_some_and(|frame| !frame.end_stream) {}

    tokio::time::sleep(Duration::from_millis(300)).await;
    let (events, _) = collect(open(addr, "Connect", json!({ "process": { "pid": pid } })).await).await;
    assert_eq!(events.last().unwrap()["end"]["status"], "signal: killed", "{events:?}");
}
