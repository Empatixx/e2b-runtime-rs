use axum::http::StatusCode;

use super::error_response::ErrorResponse;

pub fn parse_int(name: &str, value: &str) -> Result<i64, ErrorResponse> {
    value.parse::<i64>().map_err(|e| {
        ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!(
                "Invalid format for parameter {name}: {value:?} is not an integer: {e}"
            ))
            .plain_text()
            .build()
    })
}
