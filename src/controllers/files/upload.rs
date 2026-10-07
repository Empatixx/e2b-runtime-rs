use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, chown};
use std::path::Path;
use std::sync::Arc;

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use tokio_stream::{Stream, StreamExt};

use super::encoding::{self, Chunks};
use super::{FileEntry, FilesQuery, authorize, uploaded_entry};
use crate::app::AppState;
use crate::auth::WRITE;
use crate::user::User;
use crate::util::error_response::ErrorResponse;
use crate::util::fs::{dir, os_error};
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
        ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!("error decompressing request body: {e}"))
            .build()
    })?;

    let metadata = metadata_from_headers(&parts.headers);
    validate_metadata(&metadata).map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!("invalid metadata: {e}"))
            .build()
    })?;

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
    let upload = Upload {
        user,
        workdir: state.defaults().workdir,
        metadata,
        content_length,
    };

    let entries = if media_type == "application/octet-stream" {
        raw_upload(&upload, query.path.as_deref(), body).await?
    } else if media_type.starts_with("multipart/") {
        multipart_upload(&upload, query.path.as_deref(), content_type, body).await?
    } else {
        return Err(ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!(
                "unsupported content type: {content_type}, expected multipart/form-data or application/octet-stream"
            ))
            .build());
    };

    let json = serde_json::to_string(&entries).map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(format!("error marshaling response: {e}"))
            .build()
    })?;
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], json).into_response())
}

struct Upload {
    user: User,
    workdir: Option<String>,
    metadata: BTreeMap<String, String>,
    content_length: Option<u64>,
}

async fn raw_upload(upload: &Upload, path: Option<&str>, mut body: Chunks) -> Result<Vec<FileEntry>, ErrorResponse> {
    let Some(path) = path else {
        return Err(ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message("path query parameter is required for raw body upload")
            .build());
    };
    let path = expand_and_resolve(path, &upload.user, upload.workdir.as_deref()).map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!("error resolving path: {e}"))
            .build()
    })?;

    write_file(upload, &path, &mut body).await?;
    Ok(vec![uploaded_entry(&path)])
}

async fn multipart_upload(
    upload: &Upload,
    path: Option<&str>,
    content_type: &str,
    body: Chunks,
) -> Result<Vec<FileEntry>, ErrorResponse> {
    let form_error = |e: multer::Error| {
        ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(format!("error parsing multipart form: {e}"))
            .build()
    };
    let boundary = multer::parse_boundary(content_type).map_err(form_error)?;
    let mut multipart = multer::Multipart::new(body, boundary);
    let mut entries: Vec<FileEntry> = Vec::new();

    while let Some(field) = multipart.next_field().await.map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(format!("error reading form: {e}"))
            .build()
    })? {
        if field.name() != Some("file") {
            continue;
        }

        let requested = match path.or(field.file_name()) {
            Some(requested) => requested.to_owned(),
            None => {
                return Err(ErrorResponse::builder().status(StatusCode::BAD_REQUEST).message("error getting multipart custom part file name: filename not found in Content-Disposition header").build());
            }
        };
        let file_path = expand_and_resolve(&requested, &upload.user, upload.workdir.as_deref()).map_err(|e| {
            ErrorResponse::builder()
                .status(StatusCode::BAD_REQUEST)
                .message(format!("error resolving path: {e}"))
                .build()
        })?;
        let shown = file_path.display().to_string();
        if entries.iter().any(|entry| entry.path == shown) {
            return Err(ErrorResponse::builder()
                .status(StatusCode::BAD_REQUEST)
                .message(duplicate_path_message(&shown, &entries))
                .build());
        }

        let mut chunks = field.map(|chunk| chunk.map_err(io::Error::other));
        write_file(upload, &file_path, &mut chunks).await?;
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
    upload: &Upload,
    path: &Path,
    content: &mut (impl Stream<Item = io::Result<Bytes>> + Unpin),
) -> Result<(), ErrorResponse> {
    let shown = path.display();
    let parent = dir(path);
    ensure_dirs(&parent, upload.user.uid, upload.user.gid).map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(format!("error ensuring directories: {e}"))
            .build()
    })?;

    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {
            return Err(ErrorResponse::builder()
                .status(StatusCode::BAD_REQUEST)
                .message(format!("path is a directory: {shown}"))
                .build());
        }
        Err(e) if e.kind() != io::ErrorKind::NotFound => {
            let cause = os_error("stat", path, &e);
            return Err(ErrorResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .message(format!("error getting file info: {cause}"))
                .build());
        }
        _ => {}
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .open(path)
        .map_err(|e| {
            let cause = os_error("open", path, &e);
            match e.kind() {
                io::ErrorKind::StorageFull => ErrorResponse::builder()
                    .status(StatusCode::INSUFFICIENT_STORAGE)
                    .message(format!("not enough inodes available: {cause}"))
                    .build(),
                _ => ErrorResponse::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .message(format!("error opening file: {cause}"))
                    .build(),
            }
        })?;

    chown(path, Some(upload.user.uid), Some(upload.user.gid)).map_err(|e| {
        let cause = os_error("chown", path, &e);
        ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(format!("error changing file ownership: {cause}"))
            .build()
    })?;

    copy_to_file(content, &mut file)
        .await
        .map_err(|e| match (e.kind(), upload.content_length) {
            (io::ErrorKind::StorageFull, Some(length)) if length > 0 => ErrorResponse::builder()
                .status(StatusCode::INSUFFICIENT_STORAGE)
                .message(format!(
                    "attempted to write {length} bytes: not enough disk space available"
                ))
                .build(),
            (io::ErrorKind::StorageFull, _) => ErrorResponse::builder()
                .status(StatusCode::INSUFFICIENT_STORAGE)
                .message("not enough disk space available")
                .build(),
            _ => ErrorResponse::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .message(format!("error writing file: {e}"))
                .build(),
        })?;

    match write_metadata(&file, &upload.metadata) {
        Ok(()) => Ok(()),
        Err(e) if is_xattr_unsupported(&e) => {
            tracing::warn!(path = %shown, error = %e, "filesystem does not support xattrs; metadata not persisted");
            Ok(())
        }
        Err(e) if matches!(e.kind(), io::ErrorKind::StorageFull | io::ErrorKind::QuotaExceeded) => {
            Err(ErrorResponse::builder()
                .status(StatusCode::INSUFFICIENT_STORAGE)
                .message(format!("not enough space for file metadata: {e}"))
                .build())
        }
        Err(e) => Err(ErrorResponse::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .message(format!("error writing file metadata: {e}"))
            .build()),
    }
}

async fn copy_to_file(
    content: &mut (impl Stream<Item = io::Result<Bytes>> + Unpin),
    file: &mut std::fs::File,
) -> io::Result<()> {
    while let Some(chunk) = content.next().await {
        file.write_all(&chunk?)?;
    }
    Ok(())
}
