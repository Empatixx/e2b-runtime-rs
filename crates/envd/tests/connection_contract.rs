mod common;

use common::spawn_server_with_state;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

const GO_ERROR_HEADERS: &str = "\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\n";

async fn raw(request: String) -> String {
    let (addr, _) = spawn_server_with_state().await;
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap_or_default();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap_or_default();
    String::from_utf8_lossy(&response).into_owned()
}

async fn health_with(headers: String) -> String {
    raw(format!(
        "GET /health HTTP/1.1\r\nHost: x\r\n{headers}Connection: close\r\n\r\n"
    ))
    .await
}

fn go_page(status: &str) -> String {
    format!("HTTP/1.1 {status}{GO_ERROR_HEADERS}{status}")
}

#[tokio::test]
async fn headers_up_to_a_mebibyte_are_accepted_like_go() {
    let response = health_with(format!("X-Big: {}\r\n", "a".repeat(1 << 20))).await;
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");
}

#[tokio::test]
async fn thousands_of_headers_are_accepted_like_go() {
    let response = health_with((0..5000).map(|i| format!("X-H{i}: v\r\n")).collect()).await;
    assert!(response.starts_with("HTTP/1.1 204"), "{response}");
}

#[tokio::test]
async fn headers_over_the_go_limit_get_the_go_page() {
    let response = health_with(format!("X-Big: {}\r\n", "a".repeat(2 << 20))).await;
    assert_eq!(response, go_page("431 Request Header Fields Too Large"));
}

#[tokio::test]
async fn a_request_without_host_gets_the_go_page() {
    let response = raw("GET /health HTTP/1.1\r\nConnection: close\r\n\r\n".to_owned()).await;
    assert_eq!(response, go_page("400 Bad Request: missing required Host header"));
}

#[tokio::test]
async fn a_malformed_host_gets_the_go_page() {
    let response = raw("GET /health HTTP/1.1\r\nHost: a b\r\n\r\n".to_owned()).await;
    assert_eq!(response, go_page("400 Bad Request: malformed Host header"));
}

#[tokio::test]
async fn an_http_1_0_request_without_host_is_served() {
    let response = raw("GET /health HTTP/1.0\r\n\r\n".to_owned()).await;
    assert!(response.starts_with("HTTP/1.0 204"), "{response}");
}

#[tokio::test]
async fn a_malformed_request_gets_the_go_page() {
    let response = health_with("Bad Header: v\r\n".to_owned()).await;
    assert_eq!(response, go_page("400 Bad Request"));
}
