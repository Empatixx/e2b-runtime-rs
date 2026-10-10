use tonic::{Code, Status};

use crate::proto::{UserError, UserErrorCode};
use crate::server::user_error::wrapped_user_error_status;

const HTTP_BAD_REQUEST: u32 = 400;
const HTTP_NOT_FOUND: u32 = 404;
const HTTP_CONFLICT: u32 = 409;
const HTTP_INTERNAL_SERVER_ERROR: u32 = 500;

#[derive(Debug, Clone, PartialEq, bon::Builder)]
pub struct ApiError {
    code: Code,
    user_code: UserErrorCode,
    http_status: u32,
    #[builder(into)]
    message: String,
}

impl ApiError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::InvalidArgument)
            .user_code(UserErrorCode::InvalidRequest)
            .http_status(HTTP_BAD_REQUEST)
            .message(message)
            .build()
    }

    pub fn depth_out_of_range(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::InvalidArgument)
            .user_code(UserErrorCode::DepthOutOfRange)
            .http_status(HTTP_BAD_REQUEST)
            .message(message)
            .build()
    }

    pub fn cannot_delete_root(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::InvalidArgument)
            .user_code(UserErrorCode::CannotDeleteRoot)
            .http_status(HTTP_BAD_REQUEST)
            .message(message)
            .build()
    }

    pub fn path_not_found(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::NotFound)
            .user_code(UserErrorCode::PathNotFound)
            .http_status(HTTP_NOT_FOUND)
            .message(message)
            .build()
    }

    pub fn path_already_exists(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::AlreadyExists)
            .user_code(UserErrorCode::PathAlreadyExists)
            .http_status(HTTP_CONFLICT)
            .message(message)
            .build()
    }

    pub fn not_supported(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::Internal)
            .user_code(UserErrorCode::NotSupported)
            .http_status(HTTP_INTERNAL_SERVER_ERROR)
            .message(message)
            .build()
    }

    pub fn unknown(message: impl Into<String>) -> Self {
        Self::builder()
            .code(Code::Internal)
            .user_code(UserErrorCode::UnknownUserErrorCode)
            .http_status(HTTP_INTERNAL_SERVER_ERROR)
            .message(message)
            .build()
    }

    pub fn into_status(self) -> Status {
        self.wrapped("")
    }

    pub fn wrapped(self, prefix: &str) -> Status {
        let detail = UserError {
            code: self.user_code.into(),
            message: self.message.clone(),
            http_status: self.http_status,
        };
        wrapped_user_error_status(self.code, format!("{prefix}{}", self.message), detail)
    }
}

impl From<ApiError> for Status {
    fn from(error: ApiError) -> Self {
        error.into_status()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::user_error::user_error_of;

    #[test]
    fn an_api_error_carries_its_user_error_detail() {
        let status = Status::from(ApiError::path_not_found("failed to stat: \"/x\" not found."));

        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(status.message(), "failed to stat: \"/x\" not found.");
        assert_eq!(
            user_error_of(&status),
            Some(UserError {
                code: UserErrorCode::PathNotFound.into(),
                message: "failed to stat: \"/x\" not found.".to_owned(),
                http_status: 404,
            })
        );
    }

    #[test]
    fn wrapping_prefixes_the_status_message_but_not_the_detail() {
        let status = ApiError::invalid_request("invalid team ID \"x\"").wrapped("failed to build volume path: ");

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "failed to build volume path: invalid team ID \"x\"");
        assert_eq!(user_error_of(&status).unwrap().message, "invalid team ID \"x\"");
    }

    #[test]
    fn every_kind_has_the_http_status_and_code_of_upstream() {
        let cases = [
            (ApiError::invalid_request(""), Code::InvalidArgument, 400),
            (ApiError::depth_out_of_range(""), Code::InvalidArgument, 400),
            (ApiError::cannot_delete_root(""), Code::InvalidArgument, 400),
            (ApiError::path_not_found(""), Code::NotFound, 404),
            (ApiError::path_already_exists(""), Code::AlreadyExists, 409),
            (ApiError::not_supported(""), Code::Internal, 500),
            (ApiError::unknown(""), Code::Internal, 500),
        ];

        for (error, code, http_status) in cases {
            let status = Status::from(error);
            assert_eq!(status.code(), code);
            assert_eq!(user_error_of(&status).unwrap().http_status, http_status);
        }
    }
}
