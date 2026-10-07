pub mod compose;
pub mod download;
mod encoding;
pub mod upload;

use std::collections::BTreeMap;
use std::path::Path;

use axum::http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::auth::validate_signing;
use crate::user::User;
use crate::util::error_response::ErrorResponse;
use crate::util::metadata::read_metadata;

#[derive(Deserialize)]
pub struct FilesQuery {
    pub path: Option<String>,
    pub username: Option<String>,
    pub signature: Option<String>,
    pub signature_expiration: Option<String>,
}

#[derive(Serialize)]
pub struct FileEntry {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
    pub name: String,
    pub path: String,
    pub r#type: &'static str,
}

pub fn file_entry(path: &Path, metadata: BTreeMap<String, String>) -> FileEntry {
    FileEntry {
        metadata,
        name: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        path: path.display().to_string(),
        r#type: "file",
    }
}

pub fn uploaded_entry(path: &Path) -> FileEntry {
    file_entry(path, read_metadata(path))
}

pub fn authorize(
    state: &AppState,
    query: &FilesQuery,
    headers: &HeaderMap,
    operation: &str,
) -> Result<User, ErrorResponse> {
    let expiration = query
        .signature_expiration
        .as_deref()
        .map(parse_expiration)
        .transpose()?;
    let token = state.access_token();
    let header_token = headers.get("x-access-token").and_then(|value| value.to_str().ok());

    validate_signing(
        token.as_deref(),
        header_token,
        query.signature.as_deref(),
        expiration,
        query.username.as_deref().unwrap_or_default(),
        query.path.as_deref().unwrap_or_default(),
        operation,
    )
    .map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::UNAUTHORIZED)
            .message(e)
            .build()
    })?;

    lookup_user(query.username.as_deref(), &state.defaults().user)
}

pub fn lookup_user(username: Option<&str>, default_user: &str) -> Result<User, ErrorResponse> {
    let name = match username {
        Some(name) => name,
        None if !default_user.is_empty() => default_user,
        None => {
            return Err(ErrorResponse::builder()
                .status(StatusCode::BAD_REQUEST)
                .message("username not provided")
                .build());
        }
    };
    User::lookup(name).ok_or_else(|| {
        ErrorResponse::builder()
            .status(StatusCode::UNAUTHORIZED)
            .message(format!("error looking up user '{name}': user: unknown user {name}"))
            .build()
    })
}

fn parse_expiration(value: &str) -> Result<i64, ErrorResponse> {
    value.parse::<i64>().map_err(|e| {
        let reason = match e.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => "value out of range",
            _ => "invalid syntax",
        };
        ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!(
                "Invalid format for parameter signature_expiration: error binding string parameter: strconv.ParseInt: parsing {value:?}: {reason}"
            ))
            .plain_text()
            .build()
    })
}
