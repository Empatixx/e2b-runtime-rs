use axum::http::StatusCode;

use super::error_response::ErrorResponse;

pub fn parse_int(name: &str, value: &str) -> Result<i64, ErrorResponse> {
    value.parse::<i64>().map_err(|e| {
        let reason = match e.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => "value out of range",
            _ => "invalid syntax",
        };
        ErrorResponse::builder()
            .status(StatusCode::BAD_REQUEST)
            .message(format!(
                "Invalid format for parameter {name}: error binding string parameter: strconv.ParseInt: parsing {value:?}: {reason}"
            ))
            .plain_text()
            .build()
    })
}
