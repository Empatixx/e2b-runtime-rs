use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const SERVICE: &str = "ecr";
const TARGET: &str = "AmazonEC2ContainerRegistry_V20150921.GetAuthorizationToken";
const CONTENT_TYPE: &str = "application/x-amz-json-1.1";
const BODY: &str = "{}";
const ALGORITHM: &str = "AWS4-HMAC-SHA256";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsCredentials {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: Option<String>,
    pub region: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BasicCredentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    Request(String),
    Missing,
    Decode(String),
    Format,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(rename = "authorizationData", default)]
    authorization_data: Vec<AuthorizationData>,
}

#[derive(Deserialize)]
struct AuthorizationData {
    #[serde(rename = "authorizationToken", default)]
    authorization_token: Option<String>,
}

#[derive(Deserialize)]
struct ApiError {
    #[serde(rename = "__type", default)]
    kind: String,
    #[serde(alias = "Message", default)]
    message: String,
}

struct SignedRequest {
    url: String,
    headers: Vec<(String, String)>,
}

impl AwsCredentials {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        Self {
            access_key_id: var("AWS_ACCESS_KEY_ID").unwrap_or_default(),
            secret_access_key: var("AWS_SECRET_ACCESS_KEY").unwrap_or_default(),
            session_token: var("AWS_SESSION_TOKEN"),
            region: var("AWS_REGION")
                .or_else(|| var("AWS_DEFAULT_REGION"))
                .unwrap_or_default(),
        }
    }

    pub async fn ecr_basic_auth(&self) -> Result<BasicCredentials, TokenError> {
        let token = self.authorization_token().await?;
        let decoded = STANDARD
            .decode(&token)
            .map_err(|e| TokenError::Decode(base64_error(&token, e)))?;
        let decoded = String::from_utf8_lossy(&decoded).into_owned();
        decoded
            .split_once(':')
            .map(|(username, password)| BasicCredentials {
                username: username.to_owned(),
                password: password.to_owned(),
            })
            .ok_or(TokenError::Format)
    }

    async fn authorization_token(&self) -> Result<String, TokenError> {
        if self.region.is_empty() {
            return Err(TokenError::Request(operation_error(
                "failed to resolve service endpoint, an AWS region is required, but was not found",
            )));
        }
        if self.access_key_id.is_empty() || self.secret_access_key.is_empty() {
            return Err(TokenError::Request(operation_error(
                "get identity: get credentials: failed to refresh cached credentials, static credentials are empty",
            )));
        }
        let signed = self.sign(Utc::now());
        let request = signed
            .headers
            .iter()
            .fold(reqwest::Client::new().post(&signed.url), |request, (name, value)| {
                request.header(name, value)
            })
            .body(BODY);
        let response = super::registry::send_with_retry(request)
            .await
            .map_err(|e| TokenError::Request(operation_error(&format!("https response error: {e}"))))?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-amzn-requestid")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response.bytes().await.unwrap_or_default();
        if !status.is_success() {
            return Err(TokenError::Request(api_failure(status.as_u16(), &request_id, &body)));
        }
        serde_json::from_slice::<TokenResponse>(&body)
            .map_err(|e| TokenError::Request(operation_error(&format!("deserialization failed, {e}"))))?
            .authorization_data
            .into_iter()
            .next()
            .and_then(|data| data.authorization_token)
            .ok_or(TokenError::Missing)
    }

    fn host(&self) -> String {
        format!("api.ecr.{}.amazonaws.com", self.region)
    }

    fn sign(&self, now: DateTime<Utc>) -> SignedRequest {
        let amz_date = now.format("%Y%m%dT%H%M%SZ").to_string();
        let day = now.format("%Y%m%d").to_string();
        let host = self.host();
        let mut headers = vec![
            ("content-type".to_owned(), CONTENT_TYPE.to_owned()),
            ("host".to_owned(), host.clone()),
            ("x-amz-date".to_owned(), amz_date.clone()),
        ];
        if let Some(token) = &self.session_token {
            headers.push(("x-amz-security-token".to_owned(), token.clone()));
        }
        headers.push(("x-amz-target".to_owned(), TARGET.to_owned()));
        let signed_headers = headers
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>()
            .join(";");
        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect();
        let canonical_request = format!("POST\n/\n\n{canonical_headers}\n{signed_headers}\n{}", sha256_hex(BODY));
        let scope = format!("{day}/{}/{SERVICE}/aws4_request", self.region);
        let string_to_sign = format!("{ALGORITHM}\n{amz_date}\n{scope}\n{}", sha256_hex(&canonical_request));
        let key = signing_key(&self.secret_access_key, &day, &self.region, SERVICE);
        let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
        let authorization = format!(
            "{ALGORITHM} Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key_id
        );
        headers.retain(|(name, _)| name != "host");
        headers.push(("authorization".to_owned(), authorization));
        SignedRequest {
            url: format!("https://{host}/"),
            headers,
        }
    }
}

fn signing_key(secret: &str, day: &str, region: &str, service: &str) -> Vec<u8> {
    [region, service, "aws4_request"]
        .iter()
        .fold(hmac(format!("AWS4{secret}").as_bytes(), day.as_bytes()), |key, part| {
            hmac(&key, part.as_bytes())
        })
}

pub fn base64_error(input: &str, error: base64::DecodeError) -> String {
    let offset = match error {
        base64::DecodeError::InvalidByte(offset, _) | base64::DecodeError::InvalidLastSymbol(offset, _) => offset,
        base64::DecodeError::InvalidLength(length) => length,
        base64::DecodeError::InvalidPadding => input.len(),
    };
    format!("illegal base64 data at input byte {offset}")
}

fn api_failure(status: u16, request_id: &str, body: &[u8]) -> String {
    let error = serde_json::from_slice::<ApiError>(body).unwrap_or(ApiError {
        kind: String::new(),
        message: String::from_utf8_lossy(body).into_owned(),
    });
    let kind = error.kind.rsplit('#').next().unwrap_or_default();
    operation_error(&format!(
        "https response error StatusCode: {status}, RequestID: {request_id}, api error {kind}: {}",
        error.message
    ))
}

fn operation_error(detail: &str) -> String {
    format!("operation error ECR: GetAuthorizationToken, {detail}")
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(text: &str) -> String {
    hex(&Sha256::digest(text.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn requests_are_signed_with_sigv4() {
        let credentials = AwsCredentials {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: None,
            region: "us-east-1".to_owned(),
        };

        let signed = credentials.sign(Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap());

        assert_eq!(signed.url, "https://api.ecr.us-east-1.amazonaws.com/");
        let authorization = &signed
            .headers
            .iter()
            .find(|(name, _)| name == "authorization")
            .unwrap()
            .1;
        assert!(authorization.starts_with(
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/ecr/aws4_request, SignedHeaders=content-type;host;x-amz-date;x-amz-target, Signature="
        ));
        assert_eq!(
            hex(&signing_key(
                "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
                "20120215",
                "us-east-1",
                "iam"
            )),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    #[test]
    fn api_errors_read_like_the_go_sdk() {
        let body = br#"{"__type":"com.amazonaws#UnrecognizedClientException","message":"The security token included in the request is invalid."}"#;

        assert_eq!(
            api_failure(400, "abc", body),
            "operation error ECR: GetAuthorizationToken, https response error StatusCode: 400, RequestID: abc, api error UnrecognizedClientException: The security token included in the request is invalid."
        );
    }
}
