use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{OpenOptionsExt, chown};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use super::{ErrorResponse, file_entry, lookup_user};
use crate::app::AppState;
use crate::util::fs::os_error;
use crate::util::paths::{ensure_dirs, expand_and_resolve};

#[derive(Deserialize)]
pub struct ComposeRequest {
    #[serde(default)]
    pub source_paths: Vec<String>,
    #[serde(default)]
    pub destination: String,
    pub username: Option<String>,
}

pub async fn post_files_compose(State(state): State<Arc<AppState>>, body: Bytes) -> Result<Response, ErrorResponse> {
    let request: ComposeRequest =
        serde_json::from_slice(&body).map_err(|e| ErrorResponse::bad_request(format!("invalid request body: {e}")))?;
    if request.source_paths.is_empty() {
        return Err(ErrorResponse::bad_request("source_paths must not be empty".to_owned()));
    }
    if request.destination.is_empty() {
        return Err(ErrorResponse::bad_request("destination is required".to_owned()));
    }

    let defaults = state.defaults();
    let user = lookup_user(request.username.as_deref(), &defaults.user)?;
    let workdir = defaults.workdir.as_deref();
    let destination = expand_and_resolve(&request.destination, &user, workdir)
        .map_err(|e| ErrorResponse::bad_request(format!("error resolving destination path: {e}")))?;

    let sources = request
        .source_paths
        .iter()
        .map(|source| {
            let resolved = expand_and_resolve(source, &user, workdir)
                .map_err(|e| ErrorResponse::bad_request(format!("error resolving source path {source:?}: {e}")))?;
            if resolved == destination {
                return Err(ErrorResponse::bad_request(format!(
                    "source path {source:?} cannot be the same as destination"
                )));
            }
            match std::fs::metadata(&resolved) {
                Err(_) => Err(ErrorResponse::not_found(format!("source file not found: {source}"))),
                Ok(meta) if !meta.is_file() => Err(ErrorResponse::bad_request(format!(
                    "source path is not a regular file: {source}"
                ))),
                Ok(_) => Ok(resolved),
            }
        })
        .collect::<Result<Vec<PathBuf>, ErrorResponse>>()?;

    let parent = destination.parent().unwrap_or(Path::new("/"));
    ensure_dirs(parent, user.uid, user.gid)
        .map_err(|e| ErrorResponse::internal(format!("error ensuring directories: {e}")))?;

    let target = destination.clone();
    tokio::task::spawn_blocking(move || compose(&sources, &target, user.uid, user.gid))
        .await
        .map_err(ErrorResponse::internal)??;

    let body = serde_json::to_string(&file_entry(&destination, Default::default())).unwrap_or_default();
    Ok(([(header::CONTENT_TYPE, "application/json")], format!("{body}\n")).into_response())
}

fn compose(sources: &[PathBuf], destination: &Path, uid: u32, gid: u32) -> Result<(), ErrorResponse> {
    let temporary = PathBuf::from(format!(
        "{}.e2b-compose.{:032x}.tmp",
        destination.display(),
        rand::random::<u128>()
    ));

    let composed = concatenate(sources, &temporary, uid, gid).and_then(|()| {
        std::fs::rename(&temporary, destination).map_err(|e| {
            ErrorResponse::internal(format!(
                "error finalizing compose: {}",
                rename_error(&temporary, destination, &e)
            ))
        })
    });
    if composed.is_err() {
        let _ = std::fs::remove_file(&temporary);
        return composed;
    }

    for source in sources {
        let _ = std::fs::remove_file(source);
    }
    Ok(())
}

fn concatenate(sources: &[PathBuf], temporary: &Path, uid: u32, gid: u32) -> Result<(), ErrorResponse> {
    let mut output = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o666)
        .open(temporary)
        .map_err(|e| match e.kind() {
            io::ErrorKind::StorageFull => ErrorResponse::insufficient_storage("not enough disk space available"),
            _ => ErrorResponse::internal(format!(
                "error creating destination file: {}",
                os_error("open", temporary, &e)
            )),
        })?;

    chown(temporary, Some(uid), Some(gid)).map_err(|e| {
        ErrorResponse::internal(format!(
            "error changing file ownership: {}",
            os_error("chown", temporary, &e)
        ))
    })?;

    for source in sources {
        let mut input = File::open(source).map_err(|e| {
            ErrorResponse::internal(format!(
                "error opening source file {}: {}",
                source.display(),
                os_error("open", source, &e)
            ))
        })?;
        io::copy(&mut input, &mut output).map_err(|e| match e.kind() {
            io::ErrorKind::StorageFull => ErrorResponse::insufficient_storage("not enough disk space available"),
            _ => ErrorResponse::internal(format!("error composing source {}: {e}", source.display())),
        })?;
    }
    Ok(())
}

fn rename_error(from: &Path, to: &Path, err: &io::Error) -> String {
    format!(
        "rename {} {}: {}",
        from.display(),
        to.display(),
        crate::util::fs::reason(err)
    )
}
