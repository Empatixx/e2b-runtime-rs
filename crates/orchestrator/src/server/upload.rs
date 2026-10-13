use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use futures::TryStreamExt;
use tokio::io::AsyncWriteExt;

use crate::go_url::query::Query;
use crate::storage::local_upload::UploadSigner;

const UPLOAD_ROUTE: &str = "/upload";

#[derive(Clone)]
pub struct UploadHandler {
    pub base_path: PathBuf,
    pub signer: UploadSigner,
}

impl UploadHandler {
    pub fn route(self, router: Router) -> Router {
        router.route(UPLOAD_ROUTE, any(serve).with_state(Arc::new(self)))
    }

    fn authorize(&self, query: Option<&str>) -> Result<String, UploadFailure> {
        let query = Query::parse(query.unwrap_or_default());
        let get = |key: &str| {
            query
                .values
                .get(key)
                .and_then(|values| values.first())
                .cloned()
                .unwrap_or_default()
        };
        let (path, expires, token) = (get("path"), get("expires"), get("token"));
        if path.is_empty() || expires.is_empty() || token.is_empty() {
            return Err(failure(
                StatusCode::BAD_REQUEST,
                "missing required query parameters: path, expires, token",
            ));
        }
        let expires = expires
            .parse()
            .map_err(|_| failure(StatusCode::BAD_REQUEST, "invalid expires value"))?;
        if !self.signer.validate(&path, expires, &token) {
            return Err(failure(StatusCode::FORBIDDEN, "invalid or expired token"));
        }
        match is_local(&path) {
            true => Ok(path),
            false => Err(failure(StatusCode::BAD_REQUEST, "invalid path")),
        }
    }

    async fn store(&self, path: &str, body: Body) -> Result<(), UploadFailure> {
        let full_path = self.base_path.join(path);
        let dir = crate::fs::parent(&full_path);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|_| internal("failed to create directory"))?;
        TempUpload::create(&dir)
            .await?
            .fill(body)
            .await?
            .finish(&full_path)
            .await
    }
}

async fn serve(State(handler): State<Arc<UploadHandler>>, request: Request) -> Response {
    if request.method() != Method::PUT {
        return failure(StatusCode::METHOD_NOT_ALLOWED, "method not allowed").into_response();
    }
    let path = match handler.authorize(request.uri().query()) {
        Ok(path) => path,
        Err(failure) => return failure.into_response(),
    };
    match handler.store(&path, request.into_body()).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(failure) => failure.into_response(),
    }
}

struct TempUpload {
    path: PathBuf,
    file: tokio::fs::File,
}

impl TempUpload {
    async fn create(dir: &Path) -> Result<Self, UploadFailure> {
        let path = dir.join(format!(".upload-{}", rand::random::<u32>()));
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map(|file| Self { path, file })
            .map_err(|_| internal("failed to create temporary file"))
    }

    async fn fill(mut self, body: Body) -> Result<FilledUpload, UploadFailure> {
        let mut stream = body.into_data_stream();
        let written: Result<(), String> = async {
            while let Some(chunk) = stream.try_next().await.map_err(|e| e.to_string())? {
                self.file.write_all(&chunk).await.map_err(|e| e.to_string())?;
            }
            Ok(())
        }
        .await;
        if written.is_err() {
            return Err(self.discard(internal("failed to write file")).await);
        }
        match self.file.flush().await {
            Ok(()) => Ok(FilledUpload { path: self.path }),
            Err(_) => Err(self.discard(internal("failed to finalize file")).await),
        }
    }

    async fn discard(self, failure: UploadFailure) -> UploadFailure {
        drop(self.file);
        let _ = tokio::fs::remove_file(&self.path).await;
        failure
    }
}

struct FilledUpload {
    path: PathBuf,
}

impl FilledUpload {
    async fn finish(self, destination: &Path) -> Result<(), UploadFailure> {
        use std::os::unix::fs::PermissionsExt;
        let outcome = match tokio::fs::set_permissions(&self.path, std::fs::Permissions::from_mode(0o644)).await {
            Ok(()) => tokio::fs::rename(&self.path, destination)
                .await
                .map_err(|_| internal("failed to store file")),
            Err(_) => Err(internal("failed to set file permissions")),
        };
        if outcome.is_err() {
            let _ = tokio::fs::remove_file(&self.path).await;
        }
        outcome
    }
}

fn is_local(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .try_fold(0usize, |depth, segment| match segment {
                ".." => depth.checked_sub(1),
                "" | "." => Some(depth),
                _ => Some(depth + 1),
            })
            .is_some()
}

struct UploadFailure {
    status: StatusCode,
    message: &'static str,
}

impl IntoResponse for UploadFailure {
    fn into_response(self) -> Response {
        (
            self.status,
            [
                (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            format!("{}\n", self.message),
        )
            .into_response()
    }
}

fn internal(message: &'static str) -> UploadFailure {
    failure(StatusCode::INTERNAL_SERVER_ERROR, message)
}

fn failure(status: StatusCode, message: &'static str) -> UploadFailure {
    UploadFailure { status, message }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tower::ServiceExt;

    use super::*;
    use crate::go_url::GoUrl;

    struct Fixture {
        router: Router,
        signer: UploadSigner,
        dir: tempfile::TempDir,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let signer = UploadSigner::with_key("http://localhost:5008", vec![3; 32]);
        let handler = UploadHandler {
            base_path: dir.path().to_path_buf(),
            signer: signer.clone(),
        };
        Fixture {
            router: handler.route(Router::new()),
            signer,
            dir,
        }
    }

    async fn send(router: &Router, method: Method, uri: &str, body: &'static str) -> (StatusCode, String) {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    fn relative(url: &str) -> String {
        let url = GoUrl::parse(url).unwrap();
        format!("{}?{}", url.path, url.raw_query)
    }

    #[tokio::test]
    async fn a_signed_put_stores_the_file() {
        let fixture = fixture();
        let uri = relative(&fixture.signer.signed_url("scope/files/h.tar", Duration::from_secs(60)));

        let response = send(&fixture.router, Method::PUT, &uri, "tar bytes").await;

        assert_eq!(response, (StatusCode::OK, String::new()));
        let stored = fixture.dir.path().join("scope/files/h.tar");
        assert_eq!(std::fs::read_to_string(&stored).unwrap(), "tar bytes");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&stored).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[tokio::test]
    async fn bad_requests_get_the_upstream_texts() {
        let fixture = fixture();
        let signed = relative(&fixture.signer.signed_url("../escape", Duration::from_secs(60)));
        let cases = [
            (
                Method::GET,
                "/upload".to_owned(),
                StatusCode::METHOD_NOT_ALLOWED,
                "method not allowed\n",
            ),
            (
                Method::PUT,
                "/upload?path=a".to_owned(),
                StatusCode::BAD_REQUEST,
                "missing required query parameters: path, expires, token\n",
            ),
            (
                Method::PUT,
                "/upload?path=a&expires=x&token=t".to_owned(),
                StatusCode::BAD_REQUEST,
                "invalid expires value\n",
            ),
            (
                Method::PUT,
                "/upload?path=a&expires=99999999999&token=t".to_owned(),
                StatusCode::FORBIDDEN,
                "invalid or expired token\n",
            ),
            (Method::PUT, signed, StatusCode::BAD_REQUEST, "invalid path\n"),
        ];

        for (method, uri, status, body) in cases {
            assert_eq!(
                send(&fixture.router, method, &uri, "").await,
                (status, body.to_owned()),
                "{uri}"
            );
        }
    }

    #[test]
    fn local_paths_never_climb_out() {
        assert!(is_local("a/b"));
        assert!(is_local("a/../b"));
        assert!(is_local("."));
        assert!(!is_local(""));
        assert!(!is_local("/a"));
        assert!(!is_local(".."));
        assert!(!is_local("a/../../b"));
    }
}
