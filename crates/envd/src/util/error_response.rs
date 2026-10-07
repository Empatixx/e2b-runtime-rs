use std::fmt::Display;

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};

pub struct ErrorResponse {
    pub status: StatusCode,
    pub content_type: &'static str,
    pub body: String,
}

pub struct ErrorResponseBuilder {
    status: StatusCode,
    message: String,
    plain_text: bool,
}

impl ErrorResponse {
    pub fn builder() -> ErrorResponseBuilder {
        ErrorResponseBuilder {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: String::new(),
            plain_text: false,
        }
    }
}

impl ErrorResponseBuilder {
    pub fn status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }

    pub fn message(mut self, message: impl Display) -> Self {
        self.message = message.to_string();
        self
    }

    pub fn plain_text(mut self) -> Self {
        self.plain_text = true;
        self
    }

    pub fn build(self) -> ErrorResponse {
        if self.plain_text {
            return ErrorResponse {
                status: self.status,
                content_type: "text/plain; charset=utf-8",
                body: format!("{}\n", self.message),
            };
        }
        let body = serde_json::json!({ "code": self.status.as_u16(), "message": self.message });
        ErrorResponse {
            status: self.status,
            content_type: "application/json; charset=utf-8",
            body: format!("{body}\n"),
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
