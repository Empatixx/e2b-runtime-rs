#![cfg(target_os = "linux")]

mod common;

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::{spawn_server_with_flags, spawn_server_with_state};
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
    open_as(addr, method, body, "root").await
}

async fn open_as(addr: std::net::SocketAddr, method: &str, body: Value, user: &str) -> Stream {
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/process.Process/{method}"))
        .header(
            "authorization",
            format!("Basic {}", STANDARD.encode(format!("{user}:"))),
        )
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

async fn call(addr: std::net::SocketAddr, method: &str, body: Value) -> (u16, Value) {
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/process.Process/{method}"))
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .unwrap();
    (res.status().as_u16(), res.json().await.unwrap())
}

async fn started(addr: std::net::SocketAddr, body: Value) -> (Stream, u64) {
    let mut stream = open(addr, "Start", body).await;
    let pid = next_frame(&mut stream).await.unwrap().body["event"]["start"]["pid"]
        .as_u64()
        .unwrap();
    (stream, pid)
}

async fn read_until(stream: &mut Stream, kind: &str, expected: &str) -> Vec<Value> {
    let mut events = Vec::new();
    while output(&events, kind) != expected {
        let frame = next_frame(stream).await.expect("more output");
        events.push(frame.body["event"].clone());
    }
    events
}

#[tokio::test]
async fn list_shows_running_processes_with_tags() {
    let (addr, _) = spawn_server_with_state().await;
    let mut body = start_body("/bin/sleep", &["2"]);
    body["tag"] = json!("sleeper");
    let (_stream, pid) = started(addr, body).await;

    let (status, body) = call(addr, "List", json!({})).await;
    assert_eq!(status, 200, "{body}");
    let listed = body["processes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|process| process["pid"] == pid)
        .unwrap();
    assert_eq!(listed["tag"], "sleeper");
    assert_eq!(listed["config"], json!({ "cmd": "/bin/sleep", "args": ["2"] }));
}

#[tokio::test]
async fn send_input_and_close_stdin() {
    let (addr, _) = spawn_server_with_state().await;
    let (mut stream, pid) = started(addr, start_body("/bin/cat", &[])).await;

    let input = json!({ "process": { "pid": pid }, "input": { "stdin": STANDARD.encode("hello\n") } });
    assert_eq!(call(addr, "SendInput", input).await, (200, json!({})));
    read_until(&mut stream, "stdout", "hello\n").await;

    assert_eq!(
        call(addr, "CloseStdin", json!({ "process": { "pid": pid } })).await,
        (200, json!({}))
    );
    let (events, _) = collect(stream).await;
    assert_eq!(events.last().unwrap()["end"]["status"], "exit status 0");
}

#[tokio::test]
async fn stream_input_writes_in_order() {
    let (addr, _) = spawn_server_with_state().await;
    let (mut stream, pid) = started(addr, start_body("/bin/cat", &[])).await;

    let frames = [
        json!({ "start": { "process": { "pid": pid } } }),
        json!({ "data": { "input": { "stdin": STANDARD.encode("one ") } } }),
        json!({ "keepalive": {} }),
        json!({ "data": { "input": { "stdin": STANDARD.encode("two\n") } } }),
    ];
    let body: Vec<u8> = frames.iter().flat_map(envelope).collect();
    let response = reqwest::Client::new()
        .post(format!("http://{addr}/process.Process/StreamInput"))
        .header("content-type", "application/connect+json")
        .body(body)
        .send()
        .await
        .unwrap();
    let (_, trailer) = collect(Stream {
        response,
        buffer: Vec::new(),
    })
    .await;
    assert_eq!(trailer, json!({}));
    read_until(&mut stream, "stdout", "one two\n").await;
}

#[tokio::test]
async fn sigterm_ends_process_with_signal_status() {
    let (addr, _) = spawn_server_with_state().await;
    let (stream, pid) = started(addr, start_body("/bin/sleep", &["30"])).await;

    let signal = json!({ "process": { "pid": pid }, "signal": "SIGNAL_SIGTERM" });
    assert_eq!(call(addr, "SendSignal", signal).await, (200, json!({})));
    let (events, _) = collect(stream).await;
    assert_eq!(
        events.last().unwrap()["end"],
        json!({ "exitCode": -1, "status": "signal: terminated", "error": "signal: terminated" })
    );
}

#[tokio::test]
async fn update_resizes_pty() {
    let (addr, _) = spawn_server_with_state().await;
    let body = json!({
        "process": { "cmd": "/bin/sh", "args": ["-c", "sleep 0.5; stty size"] },
        "pty": { "size": { "cols": 80, "rows": 24 } }
    });
    let (mut stream, pid) = started(addr, body).await;

    let resize = json!({ "process": { "pid": pid }, "pty": { "size": { "cols": 120, "rows": 50 } } });
    assert_eq!(call(addr, "Update", resize).await, (200, json!({})));
    read_until(&mut stream, "pty", "50 120\r\n").await;
}

#[tokio::test]
async fn control_errors_match_go() {
    let (addr, _) = spawn_server_with_state().await;
    let (status, body) = call(addr, "SendInput", json!({ "process": { "pid": 999999 } })).await;
    assert_eq!(
        (status, body["message"].as_str()),
        (404, Some("process with pid 999999 not found"))
    );

    let pty =
        json!({ "process": { "cmd": "/bin/sleep", "args": ["5"] }, "pty": { "size": { "cols": 80, "rows": 24 } } });
    let (_pty_stream, pty_pid) = started(addr, pty).await;
    let (_pipe_stream, pipe_pid) = started(addr, start_body("/bin/sleep", &["5"])).await;

    let stdin = json!({ "process": { "pid": pty_pid }, "input": { "stdin": "eA==" } });
    let (status, body) = call(addr, "SendInput", stdin).await;
    assert_eq!(status, 500);
    assert_eq!(
        body["message"],
        "error writing to stdin: tty assigned to process — input should be written to the pty, not the stdin"
    );

    let tty = json!({ "process": { "pid": pipe_pid }, "input": { "pty": "eA==" } });
    let (_, body) = call(addr, "SendInput", tty).await;
    assert_eq!(
        body["message"],
        "error writing to tty: tty not assigned to process — input should be written to the stdin, not the tty"
    );

    let (status, body) = call(addr, "CloseStdin", json!({ "process": { "pid": pty_pid } })).await;
    assert_eq!(status, 500);
    assert_eq!(body["code"], "unknown");
    assert_eq!(
        body["message"],
        "error closing stdin: cannot close stdin for PTY process — send Ctrl+D (0x04) instead"
    );

    let (status, body) = call(addr, "SendSignal", json!({ "process": { "pid": pipe_pid } })).await;
    assert_eq!(
        (status, body["message"].as_str()),
        (501, Some("invalid signal: SIGNAL_UNSPECIFIED"))
    );

    let resize = json!({ "process": { "pid": pipe_pid }, "pty": { "size": { "cols": 1, "rows": 1 } } });
    let (status, body) = call(addr, "Update", resize).await;
    assert_eq!(status, 500);
    assert_eq!(body["message"], "error resizing tty: tty not assigned to process");
}

#[tokio::test]
async fn disconnected_subscriber_does_not_stop_others() {
    let (addr, _) = spawn_server_with_state().await;
    let script = "for i in $(seq 1 200); do echo $i; sleep 0.005; done";
    let (started, pid) = started(addr, start_body("/bin/sh", &["-c", script])).await;
    let mut leaving = open(addr, "Connect", json!({ "process": { "pid": pid } })).await;
    let staying = open(addr, "Connect", json!({ "process": { "pid": pid } })).await;

    for _ in 0..5 {
        next_frame(&mut leaving).await.unwrap();
    }
    drop(leaving);
    drop(started);

    let (events, _) = collect(staying).await;
    let expected: String = (1..=200).map(|i| format!("{i}\n")).collect();
    let received = output(&events, "stdout");
    assert!(expected.ends_with(&received), "missing tail of output");
    assert!(received.len() > expected.len() / 2, "{received}");
    assert_eq!(events.last().unwrap()["end"]["status"], "exit status 0");
}

#[tokio::test]
async fn processes_start_in_their_cgroup() {
    let root = std::path::PathBuf::from(format!("/sys/fs/cgroup/envd-contract-{}", std::process::id()));
    let cgroup2 =
        nix::sys::statfs::statfs("/sys/fs/cgroup").is_ok_and(|stats| stats.filesystem_type().0 == 0x6367_7270);
    if !nix::unistd::geteuid().is_root() || !cgroup2 {
        eprintln!("skipping: needs root and cgroup2");
        return;
    }
    std::fs::create_dir_all(&root).unwrap();
    let (addr, state) = spawn_server_with_flags(&["-cgroup-root", root.to_str().unwrap()]).await;
    let pipes = start_body("/bin/cat", &["/proc/self/cgroup"]);
    let pty = json!({
        "process": { "cmd": "/bin/cat", "args": ["/proc/self/cgroup"] },
        "pty": { "size": { "cols": 80, "rows": 24 } }
    });
    let system = json!({ "process": { "cmd": "/bin/cat", "args": ["/proc/self/cgroup"] }, "tag": "_system" });

    let (piped, _) = collect(open(addr, "Start", pipes).await).await;
    let (terminal, _) = collect(open(addr, "Start", pty).await).await;
    let (untouched, _) = collect(open(addr, "Start", system).await).await;
    let unprivileged = json!({ "process": { "cmd": "/bin/cat", "args": ["/proc/self/cgroup"], "cwd": "/" } });
    let (as_nobody, nobody_trailer) = collect(open_as(addr, "Start", unprivileged, "nobody").await).await;
    let own = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    drop(state);
    ["user", "ptys", "socats"]
        .iter()
        .for_each(|name| drop(std::fs::remove_dir(root.join(name))));
    let _ = std::fs::remove_dir(&root);

    let name = root.file_name().unwrap().to_str().unwrap();
    assert_eq!(output(&piped, "stdout"), format!("0::/{name}/user\n"));
    assert_eq!(output(&terminal, "pty"), format!("0::/{name}/ptys\r\n"));
    assert_eq!(output(&untouched, "stdout"), own);
    assert_eq!(
        output(&as_nobody, "stdout"),
        format!("0::/{name}/user\n"),
        "{nobody_trailer:?}"
    );
}

#[tokio::test]
async fn freeze_stops_the_workload_and_unfreeze_resumes_it() {
    let root = std::path::PathBuf::from(format!("/sys/fs/cgroup/envd-freeze-{}", std::process::id()));
    let cgroup2 =
        nix::sys::statfs::statfs("/sys/fs/cgroup").is_ok_and(|stats| stats.filesystem_type().0 == 0x6367_7270);
    if !nix::unistd::geteuid().is_root() || !cgroup2 {
        eprintln!("skipping: needs root and cgroup2");
        return;
    }
    std::fs::create_dir_all(&root).unwrap();
    let (addr, state) = spawn_server_with_flags(&["-cgroup-root", root.to_str().unwrap()]).await;
    let mut sleeper = open(addr, "Start", start_body("/bin/sleep", &["30"])).await;
    let pid = next_frame(&mut sleeper).await.unwrap().body["event"]["start"]["pid"]
        .as_u64()
        .unwrap();
    let client = reqwest::Client::new();

    let frozen: Value = client
        .post(format!("http://{addr}/freeze?maxWaitMs=2000&mode=hierarchy"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let state_while_frozen = std::fs::read_to_string(root.join("user/cgroup.events")).unwrap();
    let thawed = client
        .post(format!("http://{addr}/unfreeze"))
        .send()
        .await
        .unwrap()
        .status();
    let state_after = std::fs::read_to_string(root.join("user/cgroup.events")).unwrap();
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    );
    drop(sleeper);
    drop(state);
    tokio::time::sleep(Duration::from_millis(200)).await;
    ["user", "ptys", "socats"]
        .iter()
        .for_each(|name| drop(std::fs::remove_dir(root.join(name))));
    let _ = std::fs::remove_dir(&root);

    assert_eq!(frozen["mode"], "hierarchy", "{frozen}");
    assert_eq!(
        (frozen["requested"].as_i64(), frozen["notFrozen"].as_i64()),
        (Some(2), Some(0)),
        "{frozen}"
    );
    assert_eq!(frozen["allowlisted"], 1, "{frozen}");
    assert!(state_while_frozen.contains("frozen 1"), "{state_while_frozen}");
    assert_eq!(thawed, 204);
    assert!(state_after.contains("frozen 0"), "{state_after}");
}

fn no_wait() -> envd::cgroups::freeze::FreezeOptions {
    envd::cgroups::freeze::FreezeOptions {
        max_wait: Duration::ZERO,
        mode: envd::cgroups::freeze::FreezeMode::Legacy,
        max_cgroups: 0,
    }
}

#[tokio::test]
async fn a_start_into_a_frozen_workload_is_refused_as_unavailable() {
    let (addr, state) = spawn_server_with_state().await;
    state.freezer.freeze(no_wait()).await;

    let (events, trailer) = collect(open(addr, "Start", start_body("echo", &["hello"])).await).await;

    assert!(
        events.is_empty(),
        "no process may be started into a frozen cgroup: {events:?}"
    );
    assert_eq!(trailer["error"]["code"], "unavailable");
    assert_eq!(
        trailer["error"]["message"],
        "error starting process 'echo hello': the target cgroup is frozen"
    );
}

#[tokio::test]
async fn a_pty_start_into_a_frozen_workload_is_refused_as_unavailable() {
    let (addr, state) = spawn_server_with_state().await;
    state.freezer.freeze(no_wait()).await;
    let body = json!({
        "process": { "cmd": "echo", "args": ["hello"], "cwd": "/tmp" },
        "pty": { "size": { "cols": 80, "rows": 24 } }
    });

    let (events, trailer) = collect(open(addr, "Start", body).await).await;

    assert!(events.is_empty(), "{events:?}");
    assert_eq!(trailer["error"]["code"], "unavailable");
    assert_eq!(
        trailer["error"]["message"],
        "error starting pty with command 'echo hello' in dir '/tmp' with '80' cols and '24' rows: the target cgroup is frozen"
    );
}

#[tokio::test]
async fn the_system_tag_survives_the_frozen_window() {
    let (addr, state) = spawn_server_with_state().await;
    state.freezer.freeze(no_wait()).await;
    let body = json!({ "process": { "cmd": "echo", "args": ["hello"] }, "tag": "_system" });

    let (events, trailer) = collect(open(addr, "Start", body).await).await;

    assert_eq!(
        trailer,
        json!({}),
        "a system-tagged start was refused by the frozen window"
    );
    assert_eq!(output(&events, "stdout"), "hello\n");
}

#[tokio::test]
async fn every_start_outcome_gives_its_admission_back() {
    let (addr, state) = spawn_server_with_state().await;
    let missing_cwd = json!({ "process": { "cmd": "echo", "cwd": "/nonexistent/cwd" } });
    for body in [missing_cwd, start_body("echo", &["hello"])] {
        collect(open(addr, "Start", body).await).await;
    }

    let (_, errors) = state.freezer.freeze(no_wait()).await;

    assert!(errors.is_empty(), "a start leaked its admission: {errors:?}");
}
