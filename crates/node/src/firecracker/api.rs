use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Method, Request};
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::UnixStream;

#[derive(Debug, Clone)]
pub struct FirecrackerApi {
    socket: PathBuf,
}

impl FirecrackerApi {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    pub async fn get(&self, path: &str) -> Result<Bytes> {
        self.send(Method::GET, path, Bytes::new()).await
    }

    pub async fn put(&self, path: &str, body: Value) -> Result<()> {
        self.send(Method::PUT, path, Bytes::from(body.to_string()))
            .await
            .map(drop)
    }

    pub async fn patch(&self, path: &str, body: Value) -> Result<()> {
        self.send(Method::PATCH, path, Bytes::from(body.to_string()))
            .await
            .map(drop)
    }

    async fn send(&self, method: Method, path: &str, body: Bytes) -> Result<Bytes> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connect {}", self.socket.display()))?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(connection);
        let request = Request::builder()
            .method(method.clone())
            .uri(format!("http://localhost{path}"))
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
            .body(Full::new(body))?;
        let response = sender.send_request(request).await?;
        let status = response.status();
        let body = response.into_body().collect().await?.to_bytes();
        ensure!(
            status.is_success(),
            "firecracker {method} {path}: {status} {}",
            String::from_utf8_lossy(&body)
        );
        Ok(body)
    }
}
