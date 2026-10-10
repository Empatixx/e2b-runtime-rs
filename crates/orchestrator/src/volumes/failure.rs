use rustix::io::Errno;
use tonic::Status;

use super::api_error::ApiError;
use super::chrooted::error::PathError;

pub fn process_error(context: &str, error: &PathError) -> Status {
    match error {
        error if error.is_exist() => ApiError::path_already_exists(format!("{context}: {error}")).into_status(),
        error if error.is_not_found() => ApiError::path_not_found(format!("{context}: {error}")).into_status(),
        error if error.has(Errno::NOTDIR) => ApiError::invalid_request(format!(
            "{context}: a component of the path is a file, but must be a directory"
        ))
        .into_status(),
        error if error.has(Errno::INVAL) => ApiError::invalid_request(format!("{context}: invalid path")).into_status(),
        error => internal(format!("{context}: {error}")),
    }
}

pub fn internal(message: impl Into<String>) -> Status {
    Status::unknown(message)
}

pub fn received(prefix: &str, status: Option<Status>) -> Status {
    match status {
        None => internal(format!("{prefix}: EOF")),
        Some(status) => Status::new(
            status.code(),
            format!(
                "{prefix}: rpc error: code = {} desc = {}",
                go_code_name(status.code()),
                status.message()
            ),
        ),
    }
}

fn go_code_name(code: tonic::Code) -> String {
    match code {
        tonic::Code::Cancelled => "Canceled".to_owned(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;
    use crate::proto::UserErrorCode;
    use crate::server::user_error::user_error_of;

    fn error(errno: Errno) -> PathError {
        PathError::builder().operation("op").path("/p").errno(errno).build()
    }

    fn user_code(status: &Status) -> i32 {
        user_error_of(status).unwrap().code
    }

    #[test]
    fn an_existing_path_is_already_exists() {
        let status = process_error("op", &error(Errno::EXIST));

        assert_eq!(status.code(), Code::AlreadyExists);
        assert_eq!(user_code(&status), UserErrorCode::PathAlreadyExists as i32);
        assert_eq!(status.message(), "op: op /p: file exists");
    }

    #[test]
    fn a_missing_path_is_not_found() {
        let status = process_error("op", &error(Errno::NOENT));

        assert_eq!(status.code(), Code::NotFound);
        assert_eq!(user_code(&status), UserErrorCode::PathNotFound as i32);
    }

    #[test]
    fn a_file_in_the_middle_of_the_path_is_an_invalid_request() {
        let status = process_error("op", &error(Errno::NOTDIR));

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(user_code(&status), UserErrorCode::InvalidRequest as i32);
        assert_eq!(
            status.message(),
            "op: a component of the path is a file, but must be a directory"
        );
    }

    #[test]
    fn a_refused_name_is_an_invalid_path() {
        let status = process_error("op", &error(Errno::INVAL));

        assert_eq!(status.code(), Code::InvalidArgument);
        assert_eq!(status.message(), "op: invalid path");
    }

    #[test]
    fn any_other_error_is_passed_through_unmapped() {
        let status = process_error("op", &error(Errno::IO));

        assert_eq!(status.code(), Code::Unknown);
        assert_eq!(status.message(), "op: op /p: input/output error");
        assert_eq!(user_error_of(&status), None);
    }

    #[test]
    fn a_stream_that_ended_reads_like_go_eof() {
        assert_eq!(
            received("failed to receive start message", None).message(),
            "failed to receive start message: EOF"
        );
    }

    #[test]
    fn a_stream_status_keeps_its_code_and_reads_like_a_go_rpc_error() {
        let status = received("failed to receive chunk", Some(Status::cancelled("context canceled")));

        assert_eq!(status.code(), Code::Cancelled);
        assert_eq!(
            status.message(),
            "failed to receive chunk: rpc error: code = Canceled desc = context canceled"
        );
    }
}
