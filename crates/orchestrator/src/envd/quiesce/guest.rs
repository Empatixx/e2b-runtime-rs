use std::time::Duration;

use bytes::BytesMut;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;

use crate::envd::process::{Frame, envelope, next_frame, user_header};

const CLIENT_TIMEOUT: Duration = Duration::from_secs(10);
const SHELL_SLACK: Duration = Duration::from_millis(500);
const SYSTEM_TAG: &str = "_system";
const SYSTEM_USER: &str = "root";
const ERROR_BODY_RUNES: usize = 100;
const ELLIPSES: &str = "...";
const START_PATH: &str = "/process.Process/Start";
const NO_EXIT_CODE: i32 = -1;

#[derive(Debug, Clone)]
pub struct GuestEnvd {
    pub base_url: String,
    pub access_token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct StartResponse {
    #[serde(default)]
    event: Option<ProcessEvent>,
}

#[derive(Debug, Default, Deserialize)]
struct ProcessEvent {
    #[serde(default)]
    end: Option<EndEvent>,
}

#[derive(Debug, Default, Deserialize)]
struct EndEvent {
    #[serde(rename = "exitCode", default)]
    exit_code: i32,
}

#[derive(Debug, Default, Deserialize)]
struct EndStream {
    #[serde(default)]
    error: Option<StreamError>,
}

#[derive(Debug, Default, Deserialize)]
struct StreamError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

impl GuestEnvd {
    pub async fn post(&self, path: &str, timeout: Duration) -> Result<(), String> {
        let response = self
            .request(reqwest::Client::new().post(format!("{}/{path}", self.base_url)))
            .timeout(timeout.min(CLIENT_TIMEOUT))
            .send()
            .await
            .map_err(|e| format!("{path} request: {e}"))?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => Ok(()),
            status => Err(format!(
                "{path} returned {}: {}",
                status.as_u16(),
                error_body(&response.bytes().await.unwrap_or_default())
            )),
        }
    }

    pub async fn shell(&self, script: &str, timeout: Duration) -> Result<i32, String> {
        let body = json!({"process": {"cmd": "/bin/sh", "args": ["-c", script]}, "tag": SYSTEM_TAG});
        let request = reqwest::Client::new()
            .post(format!("{}{START_PATH}", self.base_url))
            .header("Content-Type", "application/connect+json")
            .header("Connect-Protocol-Version", "1")
            .header("Connect-Timeout-Ms", timeout.as_millis().to_string())
            .header("Authorization", user_header(SYSTEM_USER))
            .body(envelope(&body))
            .timeout(timeout + SHELL_SLACK);
        let response = self
            .request(request)
            .send()
            .await
            .map_err(|e| format!("start guest command: {e}"))?;
        match response.status() {
            reqwest::StatusCode::OK => exit_code(response).await,
            status => Err(format!("start guest command: HTTP status {}", status.as_u16())),
        }
    }

    fn request(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.access_token {
            Some(token) => request.header("X-Access-Token", token),
            None => request,
        }
    }
}

async fn exit_code(response: reqwest::Response) -> Result<i32, String> {
    let mut stream = response.bytes_stream();
    let mut buffer = BytesMut::new();
    let mut exit_code = NO_EXIT_CODE;
    while let Some(chunk) = stream.next().await {
        buffer.extend_from_slice(&chunk.map_err(|e| format!("guest command stream: {e}"))?);
        while let Some(frame) = next_frame(&mut buffer) {
            match frame {
                Frame::End(payload) => return end_of_stream(&payload).map(|()| exit_code),
                Frame::Message(payload) => {
                    exit_code = serde_json::from_slice::<StartResponse>(&payload)
                        .ok()
                        .and_then(|response| response.event)
                        .and_then(|event| event.end)
                        .map_or(exit_code, |end| end.exit_code);
                }
            }
        }
    }
    Ok(exit_code)
}

fn end_of_stream(payload: &[u8]) -> Result<(), String> {
    match serde_json::from_slice::<EndStream>(payload)
        .ok()
        .and_then(|end| end.error)
    {
        Some(error) => Err(format!("guest command stream: {}: {}", error.code, error.message)),
        None => Ok(()),
    }
}

fn error_body(body: &[u8]) -> String {
    let limit = ERROR_BODY_RUNES * 4 + 1;
    let text = String::from_utf8_lossy(&body[..body.len().min(limit)]).into_owned();
    truncate(&text, ERROR_BODY_RUNES)
}

fn truncate(text: &str, max_runes: usize) -> String {
    let runes: Vec<char> = text.chars().collect();
    match runes.len() <= max_runes {
        true => text.to_owned(),
        false => format!(
            "{}{ELLIPSES}",
            runes[..max_runes - ELLIPSES.len()].iter().collect::<String>()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_error_body_is_cut_to_a_hundred_runes_with_ellipses() {
        let body = "é".repeat(300);

        let cut = error_body(body.as_bytes());

        assert_eq!(cut.chars().count(), 100);
        assert!(cut.ends_with("é..."));
        assert_eq!(error_body(b"short"), "short");
    }
}
