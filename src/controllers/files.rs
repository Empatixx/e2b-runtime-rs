pub mod compose;
pub mod download;
mod encoding;
pub mod upload;

use std::collections::BTreeMap;
use std::fmt::Display;
use std::path::Path;

use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::auth::validate_signing;
use crate::user::User;
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

pub struct ErrorResponse {
    pub status: StatusCode,
    pub content_type: &'static str,
    pub body: String,
}

impl IntoResponse for ErrorResponse {
    fn into_response(self) -> Response {
        (
            self.status,
            [
                (header::CONTENT_TYPE, self.content_type),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            self.body,
        )
            .into_response()
    }
}

pub fn json_error(status: StatusCode, message: impl Display) -> ErrorResponse {
    let body = serde_json::json!({ "code": status.as_u16(), "message": message.to_string() });
    ErrorResponse {
        status,
        content_type: "application/json; charset=utf-8",
        body: format!("{body}\n"),
    }
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
    .map_err(|message| json_error(StatusCode::UNAUTHORIZED, message))?;

    lookup_user(query.username.as_deref(), &state.defaults().user)
}

pub fn lookup_user(username: Option<&str>, default_user: &str) -> Result<User, ErrorResponse> {
    let name = match username {
        Some(name) => name,
        None if !default_user.is_empty() => default_user,
        None => return Err(json_error(StatusCode::BAD_REQUEST, "username not provided")),
    };
    User::lookup(name).ok_or_else(|| {
        json_error(
            StatusCode::UNAUTHORIZED,
            format!("error looking up user '{name}': user: unknown user {name}"),
        )
    })
}

fn parse_expiration(value: &str) -> Result<i64, ErrorResponse> {
    value.parse::<i64>().map_err(|e| {
        let reason = match e.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => "value out of range",
            _ => "invalid syntax",
        };
        ErrorResponse {
            status: StatusCode::BAD_REQUEST,
            content_type: "text/plain; charset=utf-8",
            body: format!(
                "Invalid format for parameter signature_expiration: error binding string parameter: strconv.ParseInt: parsing {value:?}: {reason}\n"
            ),
        }
    })
}
