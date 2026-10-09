use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use azure_core::credentials::TokenCredential;
use azure_core::error::ErrorKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerToken {
    pub secret: String,
    pub expires_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceError {
    Unavailable(String),
    Failed(String),
}

impl SourceError {
    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message) | Self::Failed(message) => message,
        }
    }
}

#[async_trait::async_trait]
pub trait TokenSource: Send + Sync {
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError>;
}

pub struct SdkSource {
    credential: Arc<dyn TokenCredential>,
}

impl SdkSource {
    pub fn new(credential: Arc<dyn TokenCredential>) -> Self {
        Self { credential }
    }
}

#[async_trait::async_trait]
impl TokenSource for SdkSource {
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
        self.credential
            .get_token(&[scope], None)
            .await
            .map(|token| BearerToken {
                secret: token.token.secret().to_owned(),
                expires_at: expiry(token.expires_on.unix_timestamp()),
            })
            .map_err(|e| match e.kind() {
                ErrorKind::HttpResponse { .. } => SourceError::Failed(e.to_string()),
                _ => SourceError::Unavailable(e.to_string()),
            })
    }
}

pub fn expiry(unix_seconds: i64) -> SystemTime {
    u64::try_from(unix_seconds).map_or(UNIX_EPOCH, |seconds| UNIX_EPOCH + Duration::from_secs(seconds))
}
