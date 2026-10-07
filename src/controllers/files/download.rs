use std::io;
use std::path::Path;
use std::sync::Arc;

use async_compression::tokio::bufread::GzipEncoder;
use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use tokio::io::BufReader;
use tokio_util::io::ReaderStream;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use super::encoding::{self, GZIP};
use super::{ErrorResponse, FilesQuery, authorize};
use crate::app::AppState;
use crate::auth::READ;
use crate::util::fs::os_error;
use crate::util::paths::expand_and_resolve;

const CONDITIONAL_HEADERS: [&str; 4] = ["range", "if-modified-since", "if-none-match", "if-range"];

pub async fn get_files(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FilesQuery>,
    request: Request,
) -> Result<Response, ErrorResponse> {
    let user = authorize(&state, &query, request.headers(), READ)?;
    let requested = query.path.unwrap_or_default();
    let path = expand_and_resolve(&requested, &user, state.defaults().workdir.as_deref())
        .map_err(|e| ErrorResponse::bad_request(format!("error expanding and resolving path '{requested}': {e}")))?;

    let shown = path.display();
    let meta = std::fs::metadata(&path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => ErrorResponse::not_found(format!("path '{shown}' does not exist")),
        _ => ErrorResponse::internal(format!(
            "error checking if path exists '{shown}': {}",
            os_error("stat", &path, &e)
        )),
    })?;
    if meta.is_dir() {
        return Err(ErrorResponse::bad_request(format!("path '{shown}' is a directory")));
    }

    let headers = request.headers();
    let mut chosen = encoding::accept_encoding(headers)
        .map_err(|e| ErrorResponse::not_acceptable(format!("error parsing Accept-Encoding: {e}")))?;
    if CONDITIONAL_HEADERS.iter().any(|name| headers.contains_key(*name)) {
        if !encoding::identity_acceptable(headers) {
            return Err(ErrorResponse::not_acceptable(
                "identity encoding not acceptable for Range or conditional request",
            ));
        }
        chosen = encoding::IDENTITY;
    }

    let file = tokio::fs::File::open(&path).await.map_err(|e| {
        ErrorResponse::internal(format!("error opening file '{shown}': {}", os_error("open", &path, &e)))
    })?;

    let mut response = if chosen == GZIP {
        let content_type = mime_guess::from_path(&requested)
            .first_raw()
            .unwrap_or("application/octet-stream");
        let body = Body::from_stream(ReaderStream::new(GzipEncoder::new(BufReader::new(file))));
        (
            [(header::CONTENT_ENCODING, GZIP), (header::CONTENT_TYPE, content_type)],
            body,
        )
            .into_response()
    } else {
        let served = ServeFile::new(&path).oneshot(request).await;
        served.map(Body::new).into_response()
    };

    let headers = response.headers_mut();
    headers.insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    if let Ok(disposition) = HeaderValue::from_str(&inline_disposition(&path)) {
        headers.insert(header::CONTENT_DISPOSITION, disposition);
    }
    Ok(response)
}

fn inline_disposition(path: &Path) -> String {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();

    if name.bytes().any(|byte| (byte < 0x20 && byte != b'\t') || byte >= 0x7F) {
        let encoded: String = name
            .bytes()
            .map(|byte| match byte {
                b'*' | b'\'' | b'%' => format!("%{byte:02X}"),
                _ if is_token_byte(byte) => (byte as char).to_string(),
                _ => format!("%{byte:02X}"),
            })
            .collect();
        return format!("inline; filename*=utf-8''{encoded}");
    }

    if !name.is_empty() && name.bytes().all(is_token_byte) {
        return format!("inline; filename={name}");
    }

    let quoted = name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("inline; filename=\"{quoted}\"")
}

fn is_token_byte(byte: u8) -> bool {
    byte > 0x20 && byte < 0x7F && !b"()<>@,;:\\\"/[]?=".contains(&byte)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_disposition_like_go_mime() {
        assert_eq!(inline_disposition(Path::new("/a/c.txt")), "inline; filename=c.txt");
        assert_eq!(
            inline_disposition(Path::new("/a/my file.txt")),
            "inline; filename=\"my file.txt\""
        );
        assert_eq!(
            inline_disposition(Path::new("/a/q\"x.txt")),
            "inline; filename=\"q\\\"x.txt\""
        );
        assert_eq!(
            inline_disposition(Path::new("/a/žluť.txt")),
            "inline; filename*=utf-8''%C5%BElu%C5%A5.txt"
        );
    }
}
