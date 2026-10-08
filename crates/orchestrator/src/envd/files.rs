use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use reqwest::multipart::{Form, Part};
use tokio_util::io::ReaderStream;

use crate::go_os::path_error;
use crate::go_url::escape::{Mode, escape};

const FILE_COPY_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const FILE_FIELD: &str = "file";

pub async fn copy_file(envd: SocketAddr, user: &str, source: &Path, target: &str) -> Result<(), String> {
    let file = tokio::fs::File::open(source)
        .await
        .map_err(|e| format!("failed to open source file: {}", path_error("open", source, &e)))?;
    let name = source
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let part = Part::stream(reqwest::Body::wrap_stream(ReaderStream::new(file))).file_name(name);
    let url = format!(
        "http://{envd}/files?path={}&username={}",
        escape(target, Mode::QueryComponent),
        escape(user, Mode::QueryComponent)
    );
    let response = reqwest::Client::builder()
        .timeout(FILE_COPY_TIMEOUT)
        .build()
        .map_err(|e| format!("failed to create request: {e}"))?
        .post(url)
        .multipart(Form::new().part(FILE_FIELD, part))
        .send()
        .await
        .map_err(|e| format!("failed to send request: {e}"))?;
    let status = response.status();
    match status == reqwest::StatusCode::OK {
        true => Ok(()),
        false => Err(format!(
            "failed to upload file ({}): {}",
            status.as_u16(),
            response.text().await.unwrap_or_default()
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::extract::{Multipart, RawQuery};
    use axum::http::StatusCode;
    use axum::routing::post;
    use parking_lot::Mutex;

    use super::*;

    #[tokio::test]
    async fn files_are_posted_as_multipart_with_the_target_and_user() {
        let received = Arc::new(Mutex::new(None));
        let seen = received.clone();
        let router = Router::new().route(
            "/files",
            post(move |RawQuery(query): RawQuery, mut multipart: Multipart| {
                let seen = seen.clone();
                async move {
                    let field = multipart.next_field().await.unwrap().unwrap();
                    let name = field.name().map(str::to_owned);
                    let file_name = field.file_name().map(str::to_owned);
                    let body = field.bytes().await.unwrap();
                    *seen.lock() = Some((query, name, file_name, body.to_vec()));
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await });
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("envd");
        std::fs::write(&source, b"binary").unwrap();

        copy_file(address, "root", &source, "/tmp/envd updated").await.unwrap();

        let (query, name, file_name, body) = received.lock().clone().unwrap();
        assert_eq!(query.as_deref(), Some("path=%2Ftmp%2Fenvd+updated&username=root"));
        assert_eq!((name.as_deref(), file_name.as_deref()), (Some("file"), Some("envd")));
        assert_eq!(body, b"binary");
        let missing = copy_file(address, "root", &dir.path().join("missing"), "/x").await;
        assert!(missing.unwrap_err().starts_with("failed to open source file: open "));
    }
}
