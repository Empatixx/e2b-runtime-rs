use prost::Message;
use tonic::{Code, Status};

use crate::proto::{UserError, UserErrorCode};

const USER_ERROR_TYPE: &str = "type.googleapis.com/UserError";
const HTTP_BAD_REQUEST: u32 = 400;

#[derive(Clone, PartialEq, Message)]
struct RpcStatus {
    #[prost(int32, tag = "1")]
    code: i32,
    #[prost(string, tag = "2")]
    message: String,
    #[prost(message, repeated, tag = "3")]
    details: Vec<prost_types::Any>,
}

pub fn filesystem_only_checkpoint_disabled(sandbox_id: &str) -> Status {
    user_error_status(
        Code::FailedPrecondition,
        format!("filesystem-only checkpoint of sandbox '{sandbox_id}' is disabled"),
        UserErrorCode::FilesystemOnlyCheckpointDisabled,
        HTTP_BAD_REQUEST,
    )
}

fn user_error_status(code: Code, message: String, user_code: UserErrorCode, http_status: u32) -> Status {
    let detail = UserError {
        code: user_code.into(),
        message: message.clone(),
        http_status,
    };
    wrapped_user_error_status(code, message, detail)
}

pub fn wrapped_user_error_status(code: Code, status_message: String, detail: UserError) -> Status {
    let details = RpcStatus {
        code: code.into(),
        message: status_message.clone(),
        details: vec![prost_types::Any {
            type_url: USER_ERROR_TYPE.to_owned(),
            value: detail.encode_to_vec(),
        }],
    };
    Status::with_details(code, status_message, details.encode_to_vec().into())
}

pub fn user_error_of(status: &Status) -> Option<UserError> {
    RpcStatus::decode(status.details())
        .ok()
        .and_then(|details| {
            details
                .details
                .into_iter()
                .find(|detail| detail.type_url == USER_ERROR_TYPE)
        })
        .and_then(|detail| UserError::decode(detail.value.as_slice()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_filesystem_only_checkpoint_carries_a_user_error_for_the_api() {
        let status = filesystem_only_checkpoint_disabled("sbx");

        let details = RpcStatus::decode(status.details()).unwrap();
        let detail = UserError::decode(details.details[0].value.as_slice()).unwrap();
        assert_eq!(status.code(), Code::FailedPrecondition);
        assert_eq!(
            status.message(),
            "filesystem-only checkpoint of sandbox 'sbx' is disabled"
        );
        assert_eq!(details.code, Code::FailedPrecondition as i32);
        assert_eq!(details.details[0].type_url, "type.googleapis.com/UserError");
        assert_eq!(
            detail,
            UserError {
                code: UserErrorCode::FilesystemOnlyCheckpointDisabled.into(),
                message: "filesystem-only checkpoint of sandbox 'sbx' is disabled".to_owned(),
                http_status: 400,
            }
        );
    }
}
