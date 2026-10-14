use std::fmt::Display;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub struct ErrorResponse {
    pub status: StatusCode,
    pub content_type: &'static str,
    pub body: String,
}

#[bon::bon]
impl ErrorResponse {
    #[builder]
    pub fn new(
        #[builder(default = StatusCode::INTERNAL_SERVER_ERROR)] status: StatusCode,
        #[builder(default, with = |message: impl Display| message.to_string())] message: String,
        #[builder(default, with = || true)] plain_text: bool,
    ) -> Self {
        match plain_text {
            true => ErrorResponse {
                status,
                content_type: "text/plain; charset=utf-8",
                body: format!("{message}\n"),
            },
            false => ErrorResponse {
                status,
                content_type: "application/json; charset=utf-8",
                body: format!(
                    "{}\n",
                    serde_json::json!({ "code": status.as_u16(), "message": message })
                ),
            },
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_like_go_json_error() {
        let error = ErrorResponse::builder()
            .status(StatusCode::NOT_FOUND)
            .message("path '/x' does not exist")
            .build();
        assert_eq!(error.content_type, "application/json; charset=utf-8");
        assert_eq!(error.body, "{\"code\":404,\"message\":\"path '/x' does not exist\"}\n");
    }

    #[test]
    fn plain_text_like_go_http_error() {
        let error = ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message("bad")
            .plain_text()
            .build();
        assert_eq!(error.content_type, "text/plain; charset=utf-8");
        assert_eq!(error.body, "bad\n");
    }
}
