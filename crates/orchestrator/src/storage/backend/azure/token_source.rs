use std::time::{Duration, SystemTime, UNIX_EPOCH};

const EARLY_REFRESH: Duration = Duration::from_secs(5 * 60);

#[derive(Clone, PartialEq, Eq)]
pub struct BearerToken {
    pub secret: String,
    pub expires_at: SystemTime,
    pub refresh_on: Option<SystemTime>,
}

impl BearerToken {
    pub fn without_refresh(secret: String, expires_at: SystemTime) -> Self {
        Self {
            secret,
            expires_at,
            refresh_on: None,
        }
    }

    pub fn refresh_instant(&self) -> SystemTime {
        self.refresh_on
            .unwrap_or_else(|| self.expires_at.checked_sub(EARLY_REFRESH).unwrap_or(UNIX_EPOCH))
    }
}

pub struct TokenRequest<'a> {
    pub scope: &'a str,
    pub tenant: &'a str,
    pub cae: bool,
}

impl<'a> TokenRequest<'a> {
    pub fn new(scope: &'a str) -> Self {
        Self {
            scope,
            tenant: "",
            cae: false,
        }
    }

    pub fn with_tenant(self, tenant: &'a str) -> Self {
        Self { tenant, ..self }
    }

    pub fn with_cae(self) -> Self {
        Self { cae: true, ..self }
    }
}

impl std::fmt::Debug for BearerToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BearerToken")
            .field("expires_at", &self.expires_at)
            .field("refresh_on", &self.refresh_on)
            .finish_non_exhaustive()
    }
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
    async fn token_for(&self, request: &TokenRequest<'_>) -> Result<BearerToken, SourceError>;

    #[cfg(test)]
    async fn token(&self, scope: &str) -> Result<BearerToken, SourceError> {
        self.token_for(&TokenRequest::new(scope)).await
    }
}

pub fn expiry(unix_seconds: i64) -> SystemTime {
    u64::try_from(unix_seconds).map_or(UNIX_EPOCH, |seconds| UNIX_EPOCH + Duration::from_secs(seconds))
}
