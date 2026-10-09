use std::time::{Duration, SystemTime};

use reqwest::Response;
use serde::Deserialize;

use crate::go_url::query::Query;

pub struct OAuthToken {
    pub access_token: String,
    pub expiry: Option<SystemTime>,
    pub refresh_token: String,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct TokenJson {
    access_token: String,
    expires_in: i64,
    refresh_token: String,
    error: String,
    error_description: String,
    error_uri: String,
}

struct OAuthError {
    code: String,
    description: String,
    uri: String,
    status: u16,
    body: String,
}

impl OAuthError {
    fn text(&self) -> String {
        match self.code.is_empty() {
            false => {
                let quote = crate::go_strconv::quote;
                let mut text = format!("auth: {}", quote(&self.code));
                if !self.description.is_empty() {
                    text.push_str(&format!(" {}", quote(&self.description)));
                }
                if !self.uri.is_empty() {
                    text.push_str(&format!(" {}", quote(&self.uri)));
                }
                text
            }
            true => cannot_fetch(self.status, &self.body),
        }
    }
}

pub fn cannot_fetch(status: u16, body: &str) -> String {
    format!("auth: cannot fetch token: {status}\nResponse: {body}")
}

fn expiry_in(seconds: i64) -> Option<SystemTime> {
    u64::try_from(seconds)
        .ok()
        .filter(|seconds| *seconds > 0)
        .map(|seconds| SystemTime::now() + Duration::from_secs(seconds))
}

pub async fn read_two_legged(response: Response) -> Result<OAuthToken, String> {
    let status = response.status().as_u16();
    let body = response
        .text()
        .await
        .map_err(|e| format!("auth: cannot fetch token: {e}"))?;
    if !(200..300).contains(&status) {
        return Err(cannot_fetch(status, &body));
    }
    let parsed: TokenJson = serde_json::from_str(&body).map_err(|e| format!("auth: cannot fetch token: {e}"))?;
    Ok(OAuthToken {
        access_token: parsed.access_token,
        expiry: expiry_in(parsed.expires_in),
        refresh_token: String::new(),
    })
}

pub async fn read_three_legged(response: Response) -> Result<OAuthToken, String> {
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.split(';').next().unwrap_or_default().trim().to_owned())
        .unwrap_or_default();
    let body = response.text().await.map_err(|e| e.to_string())?;
    let failure = !(200..300).contains(&status);
    let mut error = OAuthError {
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
        _ => match serde_json::from_str::<TokenJson>(&body) {
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
            Err(e) => return Err(format!("auth: cannot parse json: {e}")),
        },
    };
    if failure || !error.code.is_empty() {
        return Err(error.text());
    }
    match token.access_token.is_empty() {
        true => Err("auth: server response missing access_token".to_owned()),
        false => Ok(token),
    }
}
