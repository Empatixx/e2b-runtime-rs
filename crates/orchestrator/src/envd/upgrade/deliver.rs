use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use futures::TryStreamExt;
use http_body_util::{BodyDataStream, StreamBody};
use hyper::body::Frame;
use hyper::client::conn::http1;
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;
use tokio_util::io::ReaderStream;

use crate::envd::body::error_body;
use crate::fs::path_error;

const TOKEN_HEADER: &str = "x-access-token";
const GUEST_PATH_HEADER: &str = "x-envd-upgrade-bin";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeliveryFailure {
    pub message: String,
    pub source_missing: bool,
}

impl DeliveryFailure {
    fn failed(message: String) -> Self {
        Self {
            message,
            source_missing: false,
        }
    }
}

impl std::fmt::Display for DeliveryFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

pub async fn call_envd_upgrade(
    address: SocketAddr,
    access_token: Option<&str>,
    source: &Path,
    guest_path: &str,
    timeout: Duration,
) -> Result<bool, DeliveryFailure> {
    let file = tokio::fs::File::open(source).await.map_err(|e| DeliveryFailure {
        message: format!(
            "open envd source {}: {}",
            source.display(),
            path_error("open", source, &e)
        ),
        source_missing: e.kind() == std::io::ErrorKind::NotFound,
    })?;
    let length = file
        .metadata()
        .await
        .map_err(|e| DeliveryFailure::failed(path_error("stat", source, &e)))?
        .len();
    let exchange = exchange(address, access_token, file, length, guest_path);
    tokio::time::timeout(timeout, exchange).await.unwrap_or(Ok(false))
}

async fn exchange(
    address: SocketAddr,
    access_token: Option<&str>,
    file: tokio::fs::File,
    length: u64,
    guest_path: &str,
) -> Result<bool, DeliveryFailure> {
    let stream = TcpStream::connect(address)
        .await
        .map_err(|e| DeliveryFailure::failed(format!("deliver upgrade to envd at {address}: {e}")))?;
    let Ok((mut sender, connection)) = http1::handshake(TokioIo::new(stream)).await else {
        return Ok(true);
    };
    tokio::spawn(connection);
    let body = StreamBody::new(ReaderStream::new(file).map_ok(Frame::data));
    let request = access_token
        .iter()
        .fold(Request::post("/upgrade"), |request, token| {
            request.header(TOKEN_HEADER, *token)
        })
        .header("host", address.to_string())
        .header("content-length", length)
        .header(GUEST_PATH_HEADER, guest_path)
        .body(body)
        .map_err(|e| DeliveryFailure::failed(format!("build upgrade request: {e}")))?;
    match sender.send_request(request).await {
        Err(_) => Ok(true),
        Ok(response) => answered(response).await,
    }
}

async fn answered(response: hyper::Response<hyper::body::Incoming>) -> Result<bool, DeliveryFailure> {
    let status = response.status();
    match status {
        StatusCode::OK | StatusCode::NO_CONTENT => Ok(false),
        status => Err(DeliveryFailure::failed(format!(
            "upgrade returned {}: {}",
            status.as_u16(),
            error_body(BodyDataStream::new(response.into_body())).await
        ))),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    const WAIT: Duration = Duration::from_secs(5);

    async fn envd_that<F, Fut>(behaviour: F) -> SocketAddr
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            behaviour(stream).await;
        });
        address
    }

    async fn read_request(stream: &mut TcpStream) -> String {
        let mut received = Vec::new();
        let mut buffer = [0u8; 4096];
        while !String::from_utf8_lossy(&received).contains("payload") {
            match stream.read(&mut buffer).await.unwrap() {
                0 => break,
                read => received.extend_from_slice(&buffer[..read]),
            }
        }
        String::from_utf8_lossy(&received).into_owned()
    }

    fn payload(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let path = dir.path().join("envd");
        std::fs::write(&path, "payload").unwrap();
        path
    }

    #[tokio::test]
    async fn a_connection_refused_is_a_delivery_failure() {
        let dir = tempfile::tempdir().unwrap();
        let free = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = free.local_addr().unwrap();
        drop(free);

        let failure = call_envd_upgrade(address, None, &payload(&dir), "/usr/bin/envd.next", WAIT)
            .await
            .unwrap_err();

        assert!(failure.message.starts_with("deliver upgrade to envd at "), "{failure}");
        assert!(!failure.source_missing);
    }

    #[tokio::test]
    async fn a_reset_after_the_body_is_the_exec_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let address = envd_that(|mut stream| async move {
            read_request(&mut stream).await;
        })
        .await;

        let confirmed = call_envd_upgrade(address, None, &payload(&dir), "/usr/bin/envd.next", WAIT).await;

        assert_eq!(confirmed, Ok(true));
    }

    #[tokio::test]
    async fn the_request_carries_the_token_the_guest_path_and_the_binary() {
        let dir = tempfile::tempdir().unwrap();
        let (seen, request) = tokio::sync::oneshot::channel();
        let address = envd_that(|mut stream| async move {
            let _ = seen.send(read_request(&mut stream).await);
        })
        .await;

        call_envd_upgrade(address, Some("secret"), &payload(&dir), "/usr/bin/envd.next", WAIT)
            .await
            .unwrap();

        let request = request.await.unwrap().to_lowercase();
        assert!(request.starts_with("post /upgrade http/1.1"), "{request}");
        assert!(request.contains("x-access-token: secret"), "{request}");
        assert!(request.contains("x-envd-upgrade-bin: /usr/bin/envd.next"), "{request}");
        assert!(request.contains("content-length: 7"), "{request}");
        assert!(request.ends_with("payload"), "{request}");
    }

    #[tokio::test]
    async fn a_deadline_is_not_a_confirmed_exec_and_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let address = envd_that(|mut stream| async move {
            read_request(&mut stream).await;
            tokio::time::sleep(Duration::from_secs(30)).await;
        })
        .await;

        let confirmed = call_envd_upgrade(
            address,
            None,
            &payload(&dir),
            "/usr/bin/envd.next",
            Duration::from_millis(200),
        )
        .await;

        assert_eq!(confirmed, Ok(false));
    }

    #[tokio::test]
    async fn an_answer_means_envd_did_not_exec() {
        let dir = tempfile::tempdir().unwrap();
        let address = envd_that(|mut stream| async move {
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\ncontent-length: 0\r\n\r\n")
                .await
                .unwrap();
        })
        .await;

        let confirmed = call_envd_upgrade(address, None, &payload(&dir), "/usr/bin/envd.next", WAIT).await;

        assert_eq!(confirmed, Ok(false));
    }

    #[tokio::test]
    async fn a_refusal_carries_the_status_and_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let address = envd_that(|mut stream| async move {
            read_request(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 4\r\nconnection: close\r\n\r\nboom")
                .await
                .unwrap();
        })
        .await;

        let failure = call_envd_upgrade(address, None, &payload(&dir), "/usr/bin/envd.next", WAIT)
            .await
            .unwrap_err();

        assert_eq!(failure.message, "upgrade returned 500: boom");
    }

    #[tokio::test]
    async fn a_missing_source_is_reported_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:9".parse().unwrap();

        let failure = call_envd_upgrade(address, None, &dir.path().join("gone"), "/usr/bin/envd.next", WAIT)
            .await
            .unwrap_err();

        assert!(failure.source_missing);
        assert!(failure.message.starts_with("open envd source "), "{failure}");
    }
}
