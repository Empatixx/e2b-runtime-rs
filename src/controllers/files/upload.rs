use std::collections::BTreeMap;
use std::io;
use std::os::unix::fs::chown;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio_stream::StreamExt;
use tokio_util::io::{ReaderStream, StreamReader};

use super::{ErrorResponse, FileEntry, FilesQuery, authorize, encoding, json_error, uploaded_entry};
use crate::app::AppState;
use crate::auth::WRITE;
use crate::user::User;
use crate::util::fs::os_error;
use crate::util::metadata::{is_xattr_unsupported, metadata_from_headers, validate_metadata, write_metadata};
use crate::util::paths::{ensure_dirs, expand_and_resolve};

pub async fn post_files(
    State(state): State<Arc<AppState>>,
    Query(query): Query<FilesQuery>,
    request: Request,
) -> Result<Response, ErrorResponse> {
    let (parts, body) = request.into_parts();
    let user = authorize(&state, &query, &parts.headers, WRITE)?;
    let body = encoding::decoded_body(&parts.headers, body).map_err(|e| {
        json_error(
            StatusCode::BAD_REQUEST,
            format!("error decompressing request body: {e}"),
        )
    })?;

    let metadata = metadata_from_headers(&parts.headers);
    validate_metadata(&metadata).map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("invalid metadata: {e}")))?;

    let content_type = parts
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    let media_type = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let content_length = parts
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse::<u64>().ok());
    let workdir = state.defaults().workdir;
    let upload = Upload {
        user: &user,
        workdir: workdir.as_deref(),
        metadata: &metadata,
        content_length,
    };

    let entries = if media_type == "application/octet-stream" {
        raw_upload(&upload, query.path.as_deref(), body).await?
    } else if media_type.starts_with("multipart/") {
        multipart_upload(&upload, query.path.as_deref(), content_type, body).await?
    } else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            format!(
                "unsupported content type: {content_type}, expected multipart/form-data or application/octet-stream"
            ),
        ));
    };

    let json = serde_json::to_string(&entries).map_err(|e| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error marshaling response: {e}"),
        )
    })?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], json).into_response())
}

struct Upload<'a> {
    user: &'a User,
    workdir: Option<&'a str>,
    metadata: &'a BTreeMap<String, String>,
    content_length: Option<u64>,
}

async fn raw_upload(
    upload: &Upload<'_>,
    path: Option<&str>,
    mut body: Pin<Box<dyn AsyncRead + Send>>,
) -> Result<Vec<FileEntry>, ErrorResponse> {
    let Some(path) = path else {
        return Err(json_error(
            StatusCode::BAD_REQUEST,
            "path query parameter is required for raw body upload",
        ));
    };
    let path = expand_and_resolve(path, upload.user, upload.workdir)
        .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("error resolving path: {e}")))?;

    write_file(upload, &path, &mut body).await?;
    Ok(vec![uploaded_entry(&path)])
}

async fn multipart_upload(
    upload: &Upload<'_>,
    path: Option<&str>,
    content_type: &str,
    body: Pin<Box<dyn AsyncRead + Send>>,
) -> Result<Vec<FileEntry>, ErrorResponse> {
    let form_error = |e: multer::Error| {
        json_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("error parsing multipart form: {e}"),
        )
    };
    let boundary = multer::parse_boundary(content_type).map_err(form_error)?;
    let mut multipart = multer::Multipart::new(ReaderStream::new(body), boundary);
    let mut entries: Vec<FileEntry> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("error reading form: {e}")))?
    {
        if field.name() != Some("file") {
            continue;
        }

        let requested = match path.or(field.file_name()) {
            Some(requested) => requested.to_owned(),
            None => {
                return Err(json_error(
                    StatusCode::BAD_REQUEST,
                    "error getting multipart custom part file name: filename not found in Content-Disposition header",
                ));
            }
        };
        let file_path = expand_and_resolve(&requested, upload.user, upload.workdir)
            .map_err(|e| json_error(StatusCode::BAD_REQUEST, format!("error resolving path: {e}")))?;
        let shown = file_path.display().to_string();
        if entries.iter().any(|entry| entry.path == shown) {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                duplicate_path_message(&shown, &entries),
            ));
        }

        let mut reader = StreamReader::new(field.map(|chunk| chunk.map_err(io::Error::other)));
        write_file(upload, &file_path, &mut reader).await?;
        entries.push(uploaded_entry(&file_path));
    }
    Ok(entries)
}

fn duplicate_path_message(path: &str, entries: &[FileEntry]) -> String {
    let message = format!(
        "you cannot upload multiple files to the same path '{path}' in one upload request, only the first specified file was uploaded"
    );
    let others: Vec<&str> = entries
        .iter()
        .map(|entry| entry.path.as_str())
        .filter(|other| *other != path)
        .collect();
    if others.len() > 1 {
        return format!(
            "{message}, also the following files were uploaded: {}",
            others.join(", ")
        );
    }
    message
}

async fn write_file(
    upload: &Upload<'_>,
    path: &Path,
    content: &mut (impl AsyncRead + Unpin),
) -> Result<(), ErrorResponse> {
    let internal = |message: String| json_error(StatusCode::INTERNAL_SERVER_ERROR, message);
    let (uid, gid) = (upload.user.uid, upload.user.gid);

    let parent = path.parent().unwrap_or(Path::new("/"));
    ensure_dirs(parent, uid, gid).map_err(|e| internal(format!("error ensuring directories: {e}")))?;

    let chowned = match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(json_error(
                StatusCode::BAD_REQUEST,
                format!("path is a directory: {}", path.display()),
            ));
        }
        Ok(_) => match chown(path, Some(uid), Some(gid)) {
            Ok(()) => true,
            Err(e) if e.kind() == io::ErrorKind::NotFound => false,
            Err(e) => {
                return Err(internal(format!(
                    "error changing file ownership: {}",
                    os_error("chown", path, &e)
                )));
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(internal(format!(
                "error getting file info: {}",
                os_error("stat", path, &e)
            )));
        }
    };

    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .open(path)
        .await
        .map_err(|e| match e.kind() {
            io::ErrorKind::StorageFull => json_error(
                StatusCode::INSUFFICIENT_STORAGE,
                format!("not enough inodes available: {}", os_error("open", path, &e)),
            ),
            _ => internal(format!("error opening file: {}", os_error("open", path, &e))),
        })?;

    if !chowned {
        chown(path, Some(uid), Some(gid)).map_err(|e| {
            internal(format!(
                "error changing file ownership: {}",
                os_error("chown", path, &e)
            ))
        })?;
    }

    let written = match tokio::io::copy(content, &mut file).await {
        Ok(_) => file.flush().await,
        Err(e) => Err(e),
    };
    written.map_err(|e| match e.kind() {
        io::ErrorKind::StorageFull => {
            let message = match upload.content_length.filter(|length| *length > 0) {
                Some(length) => format!("attempted to write {length} bytes: not enough disk space available"),
                None => "not enough disk space available".to_owned(),
            };
            json_error(StatusCode::INSUFFICIENT_STORAGE, message)
        }
        _ => internal(format!("error writing file: {e}")),
    })?;

    let file = file.into_std().await;
    match write_metadata(&file, upload.metadata) {
        Ok(()) => Ok(()),
        Err(e) if is_xattr_unsupported(&e) => {
            tracing::warn!(path = %path.display(), error = %e, "filesystem does not support xattrs; metadata not persisted");
            Ok(())
        }
        Err(e) if matches!(e.kind(), io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded) => Err(json_error(
            StatusCode::INSUFFICIENT_STORAGE,
            format!("not enough space for file metadata: {e}"),
        )),
        Err(e) => Err(internal(format!("error writing file metadata: {e}"))),
    }
}
