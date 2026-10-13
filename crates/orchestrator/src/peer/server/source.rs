use std::fmt;

use bytes::Bytes;
use futures::stream::BoxStream;
use tonic::Status;

pub type Chunks = BoxStream<'static, Result<Bytes, SourceError>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    NotAvailable,
    NotSupported,
    UnknownFile(String),
    Failed(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceError::NotAvailable => f.write_str("not available in local peer cache"),
            SourceError::NotSupported => f.write_str("operation not supported"),
            SourceError::UnknownFile(name) => write!(f, "unknown file: {name:?}"),
            SourceError::Failed(message) => f.write_str(message),
        }
    }
}

impl From<SourceError> for Status {
    fn from(error: SourceError) -> Self {
        match error {
            SourceError::UnknownFile(_) | SourceError::NotSupported => Status::invalid_argument(error.to_string()),
            _ => Status::internal(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use tonic::Code;

    use super::*;

    #[test]
    fn unknown_files_and_unsupported_operations_are_invalid_arguments() {
        let unknown = Status::from(SourceError::UnknownFile("unknown.file".to_owned()));
        let unsupported = Status::from(SourceError::NotSupported);

        assert_eq!(unknown.code(), Code::InvalidArgument);
        assert_eq!(unknown.message(), "unknown file: \"unknown.file\"");
        assert_eq!(unsupported.code(), Code::InvalidArgument);
        assert_eq!(unsupported.message(), "operation not supported");
    }

    #[test]
    fn every_other_failure_is_internal() {
        let status = Status::from(SourceError::Failed("get file: boom".to_owned()));

        assert_eq!(status.code(), Code::Internal);
        assert_eq!(status.message(), "get file: boom");
        assert_eq!(
            Status::from(SourceError::NotAvailable).message(),
            "not available in local peer cache"
        );
    }
}
