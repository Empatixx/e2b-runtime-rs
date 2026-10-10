use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::{Buf, BytesMut};
use futures::StreamExt;
use reqwest::Response;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::oneshot;

const START_PATH: &str = "/process.Process/Start";
const CONNECT_JSON: &str = "application/connect+json";
const END_STREAM: u8 = 0x02;
const FRAME_HEADER: usize = 5;
const SHELL: &str = "/bin/bash";
const PATH_SUFFIX: &str = ":/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const COMMAND_HARD_TIMEOUT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandContext {
    pub user: String,
    pub workdir: Option<String>,
    pub env_vars: BTreeMap<String, String>,
    pub access_token: Option<String>,
    pub timeout: Option<Duration>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct StartResponse {
    #[serde(default)]
    event: Option<ProcessEvent>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct ProcessEvent {
    #[serde(default)]
    data: Option<DataEvent>,
    #[serde(default)]
    end: Option<EndEvent>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct DataEvent {
    #[serde(default)]
    stdout: Option<String>,
    #[serde(default)]
    stderr: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct EndEvent {
    #[serde(rename = "exitCode", default)]
    exit_code: i32,
    #[serde(default)]
    status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct EndStream {
    #[serde(default)]
    error: Option<ConnectError>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
struct ConnectError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Frame {
    Message(Vec<u8>),
    End(Vec<u8>),
}

impl ConnectError {
    fn text(&self) -> String {
        match self.message.is_empty() {
            true => self.code.clone(),
            false => format!("{}: {}", self.code, self.message),
        }
    }
}

pub async fn run_command(
    envd: SocketAddr,
    command: &str,
    context: &CommandContext,
    on_output: impl FnMut(&str, &str),
) -> Result<(), String> {
    stream_output(start_process(envd, command, context).await?, on_output).await
}

pub async fn run_command_confirmed(
    envd: SocketAddr,
    command: &str,
    context: &CommandContext,
    started: oneshot::Sender<()>,
    on_output: impl FnMut(&str, &str),
) -> Result<(), String> {
    let response = start_process(envd, command, context).await;
    drop(started);
    stream_output(response?, on_output).await
}

async fn start_process(envd: SocketAddr, command: &str, context: &CommandContext) -> Result<Response, String> {
    let request = reqwest::Client::builder()
        .timeout(COMMAND_HARD_TIMEOUT)
        .build()
        .map_err(|e| format!("error starting process: {e}"))?
        .post(format!("http://{envd}{START_PATH}"))
        .header("Content-Type", CONNECT_JSON)
        .header("Connect-Protocol-Version", "1")
        .header("Authorization", user_header(&context.user));
    let request = match &context.access_token {
        Some(token) => request.header("X-Access-Token", token),
        None => request,
    };
    let request = match context.timeout {
        Some(timeout) => request.header("Connect-Timeout-Ms", timeout.as_millis().to_string()),
        None => request,
    };
    let response = request
        .body(envelope(&start_request(command, context)))
        .send()
        .await
        .map_err(|e| format!("error starting process: unavailable: {e}"))?;
    match response.status() {
        reqwest::StatusCode::OK => Ok(response),
        status => Err(format!(
            "command failed: {}: HTTP status {} {}",
            http_to_code(status.as_u16()),
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        )),
    }
}

async fn stream_output(response: Response, mut on_output: impl FnMut(&str, &str)) -> Result<(), String> {
    let mut stream = response.bytes_stream();
    let mut buffer = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.map_err(|e| format!("command failed: unavailable: {e}"))?);
        while let Some(frame) = next_frame(&mut buffer) {
            match frame {
                Frame::End(payload) => return end_of_stream(&payload),
                Frame::Message(payload) => {
                    if let Some(outcome) = handle_message(&payload, &mut on_output) {
                        return outcome;
                    }
                }
            }
        }
    }
    Ok(())
}

fn start_request(command: &str, context: &CommandContext) -> serde_json::Value {
    let mut envs = context.env_vars.clone();
    if let Some(path) = envs.get_mut("PATH") {
        path.push_str(PATH_SUFFIX);
    }
    let mut process = json!({ "cmd": SHELL, "args": ["-l", "-c", command], "envs": envs });
    if let Some(workdir) = &context.workdir {
        process["cwd"] = json!(workdir);
    }
    json!({ "process": process })
}

pub(super) fn user_header(user: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{user}:")))
}

pub(super) fn envelope(message: &serde_json::Value) -> Vec<u8> {
    let payload = message.to_string().into_bytes();
    let mut framed = Vec::with_capacity(FRAME_HEADER + payload.len());
    framed.push(0);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(&payload);
    framed
}

pub(super) fn next_frame(buffer: &mut BytesMut) -> Option<Frame> {
    if buffer.len() < FRAME_HEADER {
        return None;
    }
    let flags = buffer[0];
    let length = u32::from_be_bytes([buffer[1], buffer[2], buffer[3], buffer[4]]) as usize;
    if buffer.len() < FRAME_HEADER + length {
        return None;
    }
    buffer.advance(FRAME_HEADER);
    let payload = buffer.split_to(length).to_vec();
    Some(match flags & END_STREAM {
        0 => Frame::Message(payload),
        _ => Frame::End(payload),
    })
}

fn handle_message(payload: &[u8], on_output: &mut impl FnMut(&str, &str)) -> Option<Result<(), String>> {
    let event = match serde_json::from_slice::<StartResponse>(payload) {
        Ok(response) => response.event,
        Err(e) => {
            return Some(Err(format!(
                "command failed: invalid_argument: unmarshal into *process.StartResponse: {e}"
            )));
        }
    };
    let Some(event) = event else {
        tracing::error!("received nil command event");
        return Some(Ok(()));
    };
    if let Some(data) = event.data {
        on_output(&decode(data.stdout.as_deref()), &decode(data.stderr.as_deref()));
    }
    match event.end {
        Some(end) if end.exit_code != 0 => Some(Err(end.status)),
        _ => None,
    }
}

fn http_to_code(status: u16) -> &'static str {
    match status {
        400 => "internal",
        401 => "unauthenticated",
        403 => "permission_denied",
        404 => "unimplemented",
        429 | 502 | 503 | 504 => "unavailable",
        _ => "unknown",
    }
}

fn end_of_stream(payload: &[u8]) -> Result<(), String> {
    match serde_json::from_slice::<EndStream>(payload)
        .ok()
        .and_then(|end| end.error)
    {
        Some(error) => Err(format!("command failed: {}", error.text())),
        None => Ok(()),
    }
}

fn decode(field: Option<&str>) -> String {
    field
        .and_then(|encoded| STANDARD.decode(encoded).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Bytes;
    use axum::http::HeaderMap;
    use axum::routing::post;

    use super::*;

    fn frame(flags: u8, message: serde_json::Value) -> Vec<u8> {
        let mut framed = envelope(&message);
        framed[0] = flags;
        framed
    }

    async fn fake_envd(frames: Vec<Vec<u8>>) -> (SocketAddr, tokio::sync::oneshot::Receiver<(HeaderMap, Bytes)>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sender = std::sync::Arc::new(parking_lot::Mutex::new(Some(sender)));
        let body = frames.concat();
        let router = Router::new().route(
            START_PATH,
            post(move |headers: HeaderMap, request: Bytes| {
                if let Some(sender) = sender.lock().take() {
                    let _ = sender.send((headers, request));
                }
                let body = body.clone();
                async move { body }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        (address, receiver)
    }

    #[tokio::test]
    async fn the_confirmed_run_signals_once_the_process_started() {
        let (address, _) = fake_envd(vec![
            frame(0, json!({"event": {"start": {"pid": 7}}})),
            frame(
                0,
                json!({"event": {"end": {"exitCode": 3, "exited": true, "status": "exit status 3"}}}),
            ),
            frame(END_STREAM, json!({})),
        ])
        .await;
        let (started, confirmed) = oneshot::channel();

        let ran = run_command_confirmed(address, "exit 3", &CommandContext::default(), started, |_, _| {}).await;

        assert_eq!(ran, Err("exit status 3".to_owned()));
        assert!(confirmed.await.is_err());
        let (unreachable, confirmed) = oneshot::channel();
        let failed = run_command_confirmed(
            "127.0.0.1:1".parse().unwrap(),
            "x",
            &CommandContext::default(),
            unreachable,
            |_, _| {},
        )
        .await;
        assert!(failed.unwrap_err().starts_with("error starting process: unavailable: "));
        assert!(confirmed.await.is_err());
    }

    #[tokio::test]
    async fn runs_the_command_as_the_user_and_streams_output() {
        let (address, request) = fake_envd(vec![
            frame(0, json!({"event": {"start": {"pid": 7}}})),
            frame(0, json!({"event": {"data": {"stdout": STANDARD.encode("hello\n")}}})),
            frame(
                0,
                json!({"event": {"end": {"exited": true, "status": "exit status 0"}}}),
            ),
            frame(END_STREAM, json!({})),
        ])
        .await;
        let context = CommandContext {
            user: "root".to_owned(),
            workdir: None,
            env_vars: BTreeMap::from([("PATH".to_owned(), "/opt/bin".to_owned())]),
            ..CommandContext::default()
        };
        let mut output = String::new();

        run_command(address, "/usr/bin/busybox sync", &context, |stdout, _| {
            output.push_str(stdout)
        })
        .await
        .unwrap();

        assert_eq!(output, "hello\n");
        let (headers, body) = request.await.unwrap();
        assert_eq!(headers["authorization"], "Basic cm9vdDo=");
        let sent: serde_json::Value = serde_json::from_slice(&body[FRAME_HEADER..]).unwrap();
        assert_eq!(sent["process"]["cmd"], "/bin/bash");
        assert_eq!(sent["process"]["args"], json!(["-l", "-c", "/usr/bin/busybox sync"]));
        assert_eq!(
            sent["process"]["envs"]["PATH"],
            "/opt/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
        );
    }

    #[tokio::test]
    async fn a_non_zero_exit_returns_the_status_and_stream_errors_are_wrapped() {
        let (failing, _) = fake_envd(vec![frame(
            0,
            json!({"event": {"end": {"exitCode": 2, "exited": true, "status": "exit status 2"}}}),
        )])
        .await;
        let (broken, _) = fake_envd(vec![frame(
            END_STREAM,
            json!({"error": {"code": "internal", "message": "boom"}}),
        )])
        .await;
        let context = CommandContext::default();

        assert_eq!(
            run_command(failing, "false", &context, |_, _| {}).await,
            Err("exit status 2".to_owned())
        );
        assert_eq!(
            run_command(broken, "x", &context, |_, _| {}).await,
            Err("command failed: internal: boom".to_owned())
        );
    }

    #[tokio::test]
    async fn http_failures_map_to_connect_codes() {
        let router = Router::new().route(
            START_PATH,
            post(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });

        assert_eq!(
            run_command(address, "x", &CommandContext::default(), |_, _| {}).await,
            Err("command failed: unavailable: HTTP status 503 Service Unavailable".to_owned())
        );
    }
}
