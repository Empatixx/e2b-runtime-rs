use aws_config::SdkConfig;
use aws_credential_types::Credentials;
use aws_sdk_ecr::error::{DisplayErrorContext, ProvideErrorMetadata, SdkError};
use aws_sdk_ecr::operation::get_authorization_token::GetAuthorizationTokenError;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::sync::OnceCell;

use crate::storage::backend::aws::AwsDefaults;

const MISSING_REGION: &str = "failed to resolve service endpoint, an AWS region is required, but was not found";
const EMPTY_STATIC_CREDENTIALS: &str =
    "get identity: get credentials: failed to refresh cached credentials, static credentials are empty";
const REDACTED: &str = "<redacted>";

#[derive(Clone, PartialEq, Eq)]
pub struct BasicCredentials {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for BasicCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicCredentials")
            .field("username", &self.username)
            .field("password", &REDACTED)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Request(String),
    Missing,
    Decode(String),
    Format,
}

#[derive(Debug, Clone)]
pub struct EcrClient {
    client: aws_sdk_ecr::Client,
    has_region: bool,
    credentials_empty: bool,
}

#[derive(Debug, Clone, Default)]
pub struct DefaultChainEcr {
    client: std::sync::Arc<OnceCell<EcrClient>>,
}

impl PartialEq for DefaultChainEcr {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for DefaultChainEcr {}

impl DefaultChainEcr {
    pub async fn basic_auth(&self) -> Result<BasicCredentials, TokenError> {
        self.client
            .get_or_init(|| async { EcrClient::from_config(AwsDefaults::load().await.sdk_config()) })
            .await
            .basic_auth()
            .await
    }
}

impl EcrClient {
    pub async fn static_credentials(access_key_id: &str, secret_access_key: &str, region: &str) -> Self {
        let credentials = Credentials::new(access_key_id, secret_access_key, None, None, "static");
        let config = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(aws_config::Region::new(region.to_owned()))
            .credentials_provider(credentials)
            .load()
            .await;
        Self {
            credentials_empty: access_key_id.is_empty() || secret_access_key.is_empty(),
            ..Self::from_config(&config)
        }
    }

    pub fn from_config(config: &SdkConfig) -> Self {
        Self {
            client: aws_sdk_ecr::Client::new(config),
            has_region: config.region().is_some_and(|region| !region.as_ref().is_empty()),
            credentials_empty: false,
        }
    }

    pub async fn basic_auth(&self) -> Result<BasicCredentials, TokenError> {
        let token = self.authorization_token().await?;
        let decoded = STANDARD
            .decode(&token)
            .map_err(|e| TokenError::Decode(base64_error(&token, e)))?;
        String::from_utf8_lossy(&decoded)
            .split_once(':')
            .map(|(username, password)| BasicCredentials {
                username: username.to_owned(),
                password: password.to_owned(),
            })
            .ok_or(TokenError::Format)
    }

    async fn authorization_token(&self) -> Result<String, TokenError> {
        if !self.has_region {
            return Err(TokenError::Request(operation_error(MISSING_REGION)));
        }
        if self.credentials_empty {
            return Err(TokenError::Request(operation_error(EMPTY_STATIC_CREDENTIALS)));
        }
        self.client
            .get_authorization_token()
            .send()
            .await
            .map_err(|e| TokenError::Request(sdk_failure(&e)))?
            .authorization_data()
            .first()
            .and_then(|data| data.authorization_token())
            .map(str::to_owned)
            .ok_or(TokenError::Missing)
    }
}

pub fn base64_error(input: &str, error: base64::DecodeError) -> String {
    let offset = match error {
        base64::DecodeError::InvalidByte(offset, _) | base64::DecodeError::InvalidLastSymbol(offset, _) => offset,
        base64::DecodeError::InvalidLength(length) => length,
        base64::DecodeError::InvalidPadding => input.len(),
    };
    format!("illegal base64 data at input byte {offset}")
}

fn sdk_failure(error: &SdkError<GetAuthorizationTokenError>) -> String {
    match error {
        SdkError::ServiceError(service) => api_failure(
            service.raw().status().as_u16(),
            service.raw().headers().get("x-amzn-requestid").unwrap_or_default(),
            error.code().unwrap_or_default(),
            error.message().unwrap_or_default(),
        ),
        other => operation_error(&format!("https response error: {}", DisplayErrorContext(other))),
    }
}

fn api_failure(status: u16, request_id: &str, kind: &str, message: &str) -> String {
    operation_error(&format!(
        "https response error StatusCode: {status}, RequestID: {request_id}, api error {kind}: {message}"
    ))
}

fn operation_error(detail: &str) -> String {
    format!("operation error ECR: GetAuthorizationToken, {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_errors_read_like_the_go_sdk() {
        assert_eq!(
            api_failure(
                400,
                "abc",
                "UnrecognizedClientException",
                "The security token included in the request is invalid."
            ),
            "operation error ECR: GetAuthorizationToken, https response error StatusCode: 400, RequestID: abc, api error UnrecognizedClientException: The security token included in the request is invalid."
        );
    }
}
