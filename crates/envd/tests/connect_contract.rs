mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::spawn_server_with_state;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

fn basic_auth() -> String {
    let user = nix::unistd::User::from_uid(nix::unistd::getuid())
        .unwrap()
        .unwrap()
        .name;
    format!("Basic {}", STANDARD.encode(format!("{user}:")))
}

async fn unary(addr: std::net::SocketAddr, headers: &[(&str, &str)]) -> reqwest::Response {
    headers
        .iter()
        .fold(
            reqwest::Client::new()
                .post(format!("http://{addr}/filesystem.Filesystem/Stat"))
                .header("content-type", "application/json")
                .header("authorization", basic_auth()),
            |request, (name, value)| request.header(*name, *value),
        )
        .json(&json!({ "path": "/" }))
        .send()
        .await
        .unwrap()
}

fn envelope(message: &Value) -> Vec<u8> {
    let payload = serde_json::to_vec(message).unwrap();
    [vec![0], (payload.len() as u32).to_be_bytes().to_vec(), payload].concat()
}

#[tokio::test]
async fn an_unparsable_timeout_is_rejected_as_invalid_argument() {
    let (addr, _) = spawn_server_with_state().await;
    let res = unary(addr, &[("connect-timeout-ms", "abc")]).await;

    assert_eq!(res.status(), 400);
    assert_eq!(res.headers()["accept-encoding"], "gzip");
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": "invalid_argument", "message": "invalid timeout \"abc\"" })
    );
}

#[tokio::test]
async fn a_timeout_with_more_than_ten_digits_is_rejected() {
    let (addr, _) = spawn_server_with_state().await;
    let body: Value = unary(addr, &[("connect-timeout-ms", "12345678901")])
        .await
        .json()
        .await
        .unwrap();

    assert_eq!(body["message"], "timeout \"12345678901\" has more than 10 digits");
}

#[tokio::test]
async fn an_expired_timeout_fails_before_the_handler() {
    let (addr, _) = spawn_server_with_state().await;
    for timeout in ["0", "-1"] {
        let res = unary(addr, &[("connect-timeout-ms", timeout)]).await;
        assert_eq!(res.status(), 504);
        let body: Value = res.json().await.unwrap();
        assert_eq!(
            body,
            json!({ "code": "deadline_exceeded", "message": "context deadline exceeded" })
        );
    }
}

#[tokio::test]
async fn small_responses_are_gzipped_when_accepted() {
    let (addr, _) = spawn_server_with_state().await;
    let res = unary(addr, &[("accept-encoding", "gzip")]).await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-encoding"], "gzip");
    assert_eq!(res.headers()["accept-encoding"], "gzip");
}

#[tokio::test]
async fn a_stream_with_an_unparsable_timeout_ends_with_the_error() {
    let (addr, _) = spawn_server_with_state().await;
    let dir = tempfile::tempdir().unwrap();
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/filesystem.Filesystem/WatchDir"))
        .header("content-type", "application/connect+json")
        .header("authorization", basic_auth())
        .header("connect-timeout-ms", "abc")
        .body(envelope(&json!({ "path": dir.path() })))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["connect-accept-encoding"], "gzip");
    let body = res.bytes().await.unwrap();
    assert_eq!(body[0], 2);
    let end: Value = serde_json::from_slice(&body[5..]).unwrap();
    assert_eq!(
        end,
        json!({ "error": { "code": "invalid_argument", "message": "invalid timeout \"abc\"" } })
    );
}

async fn list_dir_sent_slowly(addr: std::net::SocketAddr, timeout_ms: u64, delay: Duration) -> String {
    let body = json!({ "path": "/" }).to_string();
    let head = format!(
        "POST /filesystem.Filesystem/ListDir HTTP/1.1\r\nHost: x\r\nContent-Type: application/json\r\nAuthorization: {}\r\nConnect-Timeout-Ms: {timeout_ms}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        basic_auth(),
        body.len()
    );
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    tokio::time::sleep(delay).await;
    stream.write_all(body.as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn a_unary_call_whose_deadline_passes_before_the_handler_fails() {
    let (addr, _) = spawn_server_with_state().await;
    let response = list_dir_sent_slowly(addr, 50, Duration::from_millis(300)).await;

    assert!(response.starts_with("HTTP/1.1 504"), "{response}");
    assert!(response.contains(r#"{"code":"deadline_exceeded","message":"context deadline exceeded"}"#));
}

#[tokio::test]
async fn a_unary_handler_that_runs_past_its_deadline_still_answers() {
    let (addr, _) = spawn_server_with_state().await;
    let dir = tempfile::tempdir().unwrap();
    (0..20_000).for_each(|i| std::fs::write(dir.path().join(format!("f{i}")), "").unwrap());
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/filesystem.Filesystem/ListDir"))
        .header("content-type", "application/json")
        .header("authorization", basic_auth())
        .header("connect-timeout-ms", "20")
        .json(&json!({ "path": dir.path() }))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert_eq!(body["entries"].as_array().unwrap().len(), 20_000);
}

#[tokio::test]
async fn a_malformed_grpc_timeout_is_rejected_as_invalid_argument() {
    let (addr, _) = spawn_server_with_state().await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/filesystem.Filesystem/Stat"))
        .header("content-type", "application/grpc-web+json")
        .header("authorization", basic_auth())
        .header("grpc-timeout", "5x")
        .body(envelope(&json!({ "path": "/" })))
        .send()
        .await
        .unwrap();

    let body = res.bytes().await.unwrap();
    let trailers = String::from_utf8_lossy(&body[5..]);
    assert!(
        trailers.starts_with("grpc-status: 3\r\ngrpc-message: timeout \"5x\" has an invalid unit\r\n"),
        "{trailers}"
    );
}
