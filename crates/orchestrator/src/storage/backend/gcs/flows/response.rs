use std::time::{Duration, SystemTime};

use reqwest::{Response, StatusCode};
use serde::{Deserialize, Deserializer};

use super::super::credsfile;
use super::super::json;
use crate::go_url::query::Query;

fn seconds<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Lifetime {
        Number(i64),
        Text(String),
    }
    match Option::<Lifetime>::deserialize(deserializer)? {
        None => Ok(0),
        Some(Lifetime::Number(seconds)) => Ok(seconds),
        Some(Lifetime::Text(text)) => text.trim().parse().map_err(serde::de::Error::custom),
    }
}

pub struct OAuthToken {
    pub access_token: String,
    pub expiry: Option<SystemTime>,
    pub refresh_token: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenJson {
    access_token: String,
    #[serde(deserialize_with = "seconds")]
    expires_in: i64,
    refresh_token: String,
    error: String,
    error_description: String,
    error_uri: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    Auth,
    Oauth2,
}

impl Dialect {
    pub fn prefix(self) -> &'static str {
        match self {
            Self::Auth => "auth",
            Self::Oauth2 => "oauth2",
        }
    }

    fn token_target(self) -> json::Target {
        match self {
            Self::Auth => credsfile::AUTH_TOKEN_3LO,
            Self::Oauth2 => credsfile::OAUTH2_TOKEN,
        }
    }

    pub fn cannot_fetch(self, status: StatusCode, body: &str) -> String {
        match self {
            Self::Auth => format!("auth: cannot fetch token: {}\nResponse: {body}", status.as_u16()),
            Self::Oauth2 => format!(
                "oauth2: cannot fetch token: {} {}\nResponse: {body}",
                status.as_u16(),
                status.canonical_reason().unwrap_or_default()
            ),
        }
    }
}

struct OAuthError {
    dialect: Dialect,
    code: String,
    description: String,
    uri: String,
    status: StatusCode,
    body: String,
}

impl OAuthError {
    fn text(&self) -> String {
        match self.code.is_empty() {
            false => {
                let mut text = format!("{}: {:?}", self.dialect.prefix(), self.code);
                if !self.description.is_empty() {
                    text.push_str(&format!(" {:?}", self.description));
                }
                if !self.uri.is_empty() {
                    text.push_str(&format!(" {:?}", self.uri));
                }
                text
            }
            true => self.dialect.cannot_fetch(self.status, &self.body),
        }
    }
}

fn expiry_in(seconds: i64) -> Option<SystemTime> {
    u64::try_from(seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(|seconds| SystemTime::now() + Duration::from_secs(seconds))
}

pub async fn read_two_legged(response: Response) -> Result<OAuthToken, String> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("auth: cannot fetch token: {e}"))?;
    if !status.is_success() {
        return Err(Dialect::Auth.cannot_fetch(status, &body));
    }
    let parsed: TokenJson = json::unmarshal(body.as_bytes(), credsfile::AUTH_TOKEN_2LO)
        .map_err(|e| format!("auth: cannot fetch token: {e}"))?;
    Ok(OAuthToken {
        access_token: parsed.access_token,
        expiry: expiry_in(parsed.expires_in),
        refresh_token: String::new(),
    })
}

pub async fn read_three_legged(response: Response, dialect: Dialect) -> Result<OAuthToken, String> {
    let status = response.status();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim().to_owned())
        .unwrap_or_default();
    let body = response.text().await.map_err(|e| e.to_string())?;
    let failure = !status.is_success();
    let mut error = OAuthError {
        dialect,
        code: String::new(),
        description: String::new(),
        uri: String::new(),
        status,
        body: body.clone(),
    };
    let token = match content_type.as_str() {
        "application/x-www-form-urlencoded" | "text/plain" => {
            let values = Query::parse(&body);
            let field = |name: &str| {
                values
                    .values
                    .get(name)
                    .and_then(|v| v.first())
                    .cloned()
                    .unwrap_or_default()
            };
            error.code = field("error");
            error.description = field("error_description");
            error.uri = field("error_uri");
            OAuthToken {
                access_token: field("access_token"),
                expiry: expiry_in(field("expires_in").parse().unwrap_or(0)),
                refresh_token: field("refresh_token"),
            }
        }
        _ => match json::unmarshal::<TokenJson>(body.as_bytes(), dialect.token_target()) {
            Ok(parsed) => {
                error.code = parsed.error;
                error.description = parsed.error_description;
                error.uri = parsed.error_uri;
                OAuthToken {
                    access_token: parsed.access_token,
                    expiry: expiry_in(parsed.expires_in),
                    refresh_token: parsed.refresh_token,
                }
            }
            Err(_) if failure => return Err(error.text()),
            Err(e) => return Err(format!("{}: cannot parse json: {e}", dialect.prefix())),
        },
    };
    if failure || !error.code.is_empty() {
        return Err(error.text());
    }
    match token.access_token.is_empty() {
        true => Err(format!("{}: server response missing access_token", dialect.prefix())),
        false => Ok(token),
    }
}
