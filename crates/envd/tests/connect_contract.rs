mod common;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use common::spawn_server_with_state;
use serde_json::{Value, json};

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
async fn an_unparsable_timeout_is_rejected_like_connect_go() {
    let (addr, _) = spawn_server_with_state().await;
    let res = unary(addr, &[("connect-timeout-ms", "abc")]).await;

    assert_eq!(res.status(), 400);
    assert_eq!(res.headers()["accept-encoding"], "gzip");
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": "invalid_argument", "message": "parse timeout: strconv.ParseInt: parsing \"abc\": invalid syntax" })
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

    assert_eq!(body["message"], "parse timeout: \"12345678901\" has >10 digits");
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
        json!({ "error": { "code": "invalid_argument", "message": "parse timeout: strconv.ParseInt: parsing \"abc\": invalid syntax" } })
    );
}

#[tokio::test]
async fn a_unary_call_past_its_deadline_fails_with_the_go_text() {
    let (addr, _) = spawn_server_with_state().await;
    let res = reqwest::Client::new()
        .post(format!("http://{addr}/filesystem.Filesystem/ListDir"))
        .header("content-type", "application/json")
        .header("authorization", basic_auth())
        .header("connect-timeout-ms", "1")
        .json(&json!({ "path": "/usr", "depth": 30 }))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 504);
    let body: Value = res.json().await.unwrap();
    assert_eq!(
        body,
        json!({ "code": "deadline_exceeded", "message": "context deadline exceeded" })
    );
}

#[tokio::test]
async fn a_malformed_grpc_timeout_is_rejected_like_connect_go() {
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
        trailers.starts_with("grpc-status: 3\r\ngrpc-message: protocol error: timeout has invalid unit 'x'\r\n"),
        "{trailers}"
    );
}
