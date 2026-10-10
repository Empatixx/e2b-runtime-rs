use std::collections::BTreeMap;
use std::net::SocketAddr;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::BytesMut;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::process::{Frame, envelope, next_frame, user_header};

const START_PATH: &str = "/process.Process/Start";
const SEND_INPUT_PATH: &str = "/process.Process/SendInput";
const UPDATE_PATH: &str = "/process.Process/Update";
const CONNECT_STREAM: &str = "application/connect+json";
const CONNECT_UNARY: &str = "application/json";
const SHELL: &str = "/bin/bash";
const EVENTS_BUFFER: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtyTarget {
    pub envd: SocketAddr,
    pub access_token: Option<String>,
    pub user: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtyEvent {
    Started(u32),
    Output(Vec<u8>),
    Ended(i32),
}

#[derive(Debug, Default, Deserialize)]
struct StartResponse {
    #[serde(default)]
    event: Option<Event>,
}

#[derive(Debug, Default, Deserialize)]
struct Event {
    #[serde(default)]
    start: Option<StartEvent>,
    #[serde(default)]
    data: Option<DataEvent>,
    #[serde(default)]
    end: Option<EndEvent>,
}

#[derive(Debug, Default, Deserialize)]
struct StartEvent {
    #[serde(default)]
    pid: u32,
}

#[derive(Debug, Default, Deserialize)]
struct DataEvent {
    #[serde(default)]
    pty: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct EndEvent {
    #[serde(rename = "exitCode", default)]
    exit_code: i32,
}

impl PtyTarget {
    pub async fn start_shell(
        &self,
        envs: &BTreeMap<String, String>,
        size: PtySize,
    ) -> Result<mpsc::Receiver<Result<PtyEvent, String>>, String> {
        let body = json!({
            "process": { "cmd": SHELL, "args": ["-l"], "envs": envs },
            "pty": { "size": { "cols": size.cols, "rows": size.rows } },
        });
        let response = self
            .request(START_PATH, CONNECT_STREAM)
            .header("Connect-Protocol-Version", "1")
            .body(envelope(&body))
            .send()
            .await
            .map_err(|e| format!("start shell: {e}"))?;
        if !response.status().is_success() {
            return Err(format!("start shell: HTTP status {}", response.status().as_u16()));
        }
        let (events, received) = mpsc::channel(EVENTS_BUFFER);
        tokio::spawn(forward_events(response, events));
        Ok(received)
    }

    pub async fn send_input(&self, pid: u32, input: &[u8]) -> Result<(), String> {
        self.unary(
            SEND_INPUT_PATH,
            json!({ "process": { "pid": pid }, "input": { "pty": STANDARD.encode(input) } }),
        )
        .await
    }

    pub async fn resize(&self, pid: u32, size: PtySize) -> Result<(), String> {
        self.unary(
            UPDATE_PATH,
            json!({ "process": { "pid": pid }, "pty": { "size": { "cols": size.cols, "rows": size.rows } } }),
        )
        .await
    }

    async fn unary(&self, path: &str, body: Value) -> Result<(), String> {
        let response = self
            .request(path, CONNECT_UNARY)
            .header("Connect-Protocol-Version", "1")
            .body(body.to_string())
            .send()
            .await
            .map_err(|e| e.to_string())?;
        match response.status().is_success() {
            true => Ok(()),
            false => Err(format!("HTTP status {}", response.status().as_u16())),
        }
    }

    fn request(&self, path: &str, content_type: &str) -> reqwest::RequestBuilder {
        let request = reqwest::Client::new()
            .post(format!("http://{}{path}", self.envd))
            .header("Content-Type", content_type)
            .header("Authorization", user_header(&self.user));
        match &self.access_token {
            Some(token) => request.header("X-Access-Token", token),
            None => request,
        }
    }
}

async fn forward_events(response: reqwest::Response, events: mpsc::Sender<Result<PtyEvent, String>>) {
    let mut stream = response.bytes_stream();
    let mut buffer = BytesMut::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => buffer.extend_from_slice(&chunk),
            Err(e) => {
                let _ = events.send(Err(format!("shell stream: {e}"))).await;
                return;
            }
        }
        while let Some(frame) = next_frame(&mut buffer) {
            let Frame::Message(payload) = frame else {
                return;
            };
            for event in decode_events(&payload) {
                if events.send(Ok(event)).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn decode_events(payload: &[u8]) -> Vec<PtyEvent> {
    let Some(event) = serde_json::from_slice::<StartResponse>(payload)
        .ok()
        .and_then(|response| response.event)
    else {
        return Vec::new();
    };
    let started = event.start.map(|start| PtyEvent::Started(start.pid));
    let output = event
        .data
        .and_then(|data| data.pty)
        .and_then(|encoded| STANDARD.decode(encoded).ok())
        .map(PtyEvent::Output);
    let ended = event.end.map(|end| PtyEvent::Ended(end.exit_code));
    [started, output, ended].into_iter().flatten().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_decode_from_connect_json() {
        assert_eq!(
            decode_events(br#"{"event":{"start":{"pid":42}}}"#),
            vec![PtyEvent::Started(42)]
        );
        assert_eq!(
            decode_events(br#"{"event":{"data":{"pty":"aGk="}}}"#),
            vec![PtyEvent::Output(b"hi".to_vec())]
        );
        assert_eq!(
            decode_events(br#"{"event":{"end":{"exitCode":3}}}"#),
            vec![PtyEvent::Ended(3)]
        );
        assert!(decode_events(b"{}").is_empty());
    }
}
