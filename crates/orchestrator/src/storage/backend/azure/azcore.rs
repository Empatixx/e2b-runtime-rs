use std::time::Duration;

use crate::go_url::escape::{Mode, unescape};
use reqwest::{Client, RequestBuilder, Response, StatusCode};

use super::token_source::SourceError;

const SEPARATOR: &str = "--------------------------------------------------------------------------------";
const RETRIES: u32 = 3;
const DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const MSAL_TIMEOUT: Duration = Duration::from_secs(30);
const RETRY_DELAY: Duration = Duration::from_millis(800);
const RETRY_STATUSES: [u16; 6] = [408, 429, 500, 502, 503, 504];
const TROUBLESHOOT: &str = "To troubleshoot, visit https://aka.ms/azsdk/go/identity/troubleshoot";
const NO_BODY: &str = "Response contained no body";
const NO_ERROR_CODE: &str = "ERROR CODE UNAVAILABLE";

pub fn pipeline_client() -> Client {
    Client::builder()
        .connect_timeout(DIAL_TIMEOUT)
        .build()
        .unwrap_or_default()
}

pub fn msal_client() -> Client {
    Client::builder().timeout(MSAL_TIMEOUT).build().unwrap_or_default()
}

pub async fn send(request: RequestBuilder) -> Result<Response, String> {
    let mut delay = RETRY_DELAY;
    for _ in 0..RETRIES {
        let Some(attempt) = request.try_clone() else {
            break;
        };
        match attempt.send().await {
            Ok(response) if !RETRY_STATUSES.contains(&response.status().as_u16()) => return Ok(response),
            _ => {}
        }
        tokio::time::sleep(delay).await;
        delay *= 2;
    }
    request.send().await.map_err(|e| e.to_string())
}

pub struct ResponseError<'a> {
    pub method: &'a str,
    pub url: &'a str,
    pub status: StatusCode,
    pub error_code: Option<String>,
    pub body: &'a str,
}

impl ResponseError<'_> {
    pub fn text(&self) -> String {
        let code = self
            .error_code
            .clone()
            .or_else(|| error_code_in(self.body))
            .map_or_else(|| NO_ERROR_CODE.to_owned(), |code| format!("ERROR CODE: {code}"));
        format!(
            "{} {}\n{SEPARATOR}\nRESPONSE {}: {}\n{code}\n{SEPARATOR}\n{}\n{SEPARATOR}\n",
            self.method,
            self.url.split_once('?').map_or(self.url, |(base, _)| base),
            self.status.as_u16(),
            self.status,
            shown_body(self.body)
        )
    }
}

pub struct AuthenticationFailure<'a> {
    pub credential: &'a str,
    pub message: &'a str,
    pub method: &'a str,
    pub url: &'a str,
    pub status: StatusCode,
    pub body: &'a str,
}

impl AuthenticationFailure<'_> {
    pub fn text(&self) -> String {
        format!(
            "{} authentication failed. {}\n{} {}\n{SEPARATOR}\nRESPONSE {}: {}\n{SEPARATOR}\n{}\n{SEPARATOR}\n{}",
            self.credential,
            self.message,
            self.method,
            printed_url(self.url),
            self.status.as_u16(),
            self.status,
            shown_body(self.body),
            self.advice()
        )
    }

    fn advice(&self) -> String {
        match troubleshooting_anchor(self.credential) {
            "" => String::new(),
            anchor => format!("{TROUBLESHOOT}#{anchor}"),
        }
    }
}

fn troubleshooting_anchor(credential: &str) -> &'static str {
    match credential {
        "AzureCLICredential" => "azure-cli",
        "AzureDeveloperCLICredential" => "azd",
        "AzurePipelinesCredential" => "apc",
        "ClientCertificateCredential" => "client-cert",
        "AzurePowerShellCredential" => "azure-pwsh",
        "ClientSecretCredential" => "client-secret",
        "ManagedIdentityCredential" => "managed-id",
        "WorkloadIdentityCredential" => "workload",
        _ => "",
    }
}

fn printed_url(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    match without_query.split_once("://") {
        Some((scheme, rest)) => {
            let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
            format!(
                "{scheme}://{host}{}",
                unescape(path, Mode::Path).unwrap_or_else(|_| path.to_owned())
            )
        }
        None => without_query.to_owned(),
    }
}

pub struct HttpFailure {
    pub method: String,
    pub url: String,
    pub status: StatusCode,
    pub body: String,
}

pub enum CallError {
    Http(HttpFailure),
    Message(String),
}

impl CallError {
    pub fn message(&self) -> String {
        match self {
            Self::Message(message) => message.clone(),
            Self::Http(failure) => failure.message(),
        }
    }

    pub fn failed(self, credential: &str, describe: impl FnOnce(String) -> String) -> SourceError {
        match self {
            Self::Message(message) => SourceError::Failed(format!("{credential}: {}", describe(message))),
            Self::Http(failure) => SourceError::Failed(
                AuthenticationFailure {
                    credential,
                    message: "",
                    method: &failure.method,
                    url: &failure.url,
                    status: failure.status,
                    body: &failure.body,
                }
                .text(),
            ),
        }
    }
}

impl HttpFailure {
    pub fn message(&self) -> String {
        let status = self.status.as_u16();
        match self.body.trim() {
            "" => format!(
                "http call({})({}) error: reply status code was {status}",
                self.url, self.method
            ),
            body => format!(
                "http call({})({}) error: reply status code was {status}:\n{body}",
                self.url, self.method
            ),
        }
    }
}

fn shown_body(body: &str) -> String {
    match (body.is_empty(), serde_json::from_str::<serde_json::Value>(body)) {
        (true, _) => NO_BODY.to_owned(),
        (false, Ok(_)) => indent_json(body),
        (false, Err(_)) => body.to_owned(),
    }
}

fn error_code_in(body: &str) -> Option<String> {
    json_error_code(body).or_else(|| xml_error_code(body))
}

fn json_error_code(body: &str) -> Option<String> {
    let object = serde_json::from_str::<serde_json::Value>(body).ok()?;
    let object = object.as_object()?;
    let wrapped = object.get("error").or_else(|| object.get("odata.error"));
    let object = match wrapped {
        Some(inner) => inner.as_object()?,
        None => object,
    };
    object.get("code")?.as_str().map(str::to_owned)
}

fn xml_error_code(body: &str) -> Option<String> {
    let start = body.find("<Code>").or_else(|| body.find("<code>"))? + "<Code>".len();
    let end = body[start..].find('<')? + start;
    let code = body[start..end].trim();
    (!code.is_empty() && code.chars().all(|c| c.is_alphanumeric() || c == '_')).then(|| code.to_owned())
}

pub fn indent_json(text: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    let mut chars = text.trim().chars().peekable();
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '{' | '[' => {
                out.push(c);
                let closes = matches!((c, chars.peek()), ('{', Some('}')) | ('[', Some(']')));
                if closes {
                    out.extend(chars.next());
                } else {
                    depth += 1;
                    newline(&mut out, depth);
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                newline(&mut out, depth);
                out.push(c);
            }
            ',' => {
                out.push(c);
                newline(&mut out, depth);
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    out.push_str(&"  ".repeat(depth));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_bodies_are_indented_like_go() {
        assert_eq!(
            indent_json(r#"{"errors":[{"code":"DENIED","detail":{}}],"n":1}"#),
            "{\n  \"errors\": [\n    {\n      \"code\": \"DENIED\",\n      \"detail\": {}\n    }\n  ],\n  \"n\": 1\n}"
        );
    }

    #[test]
    fn error_codes_come_from_the_header_json_or_xml() {
        assert_eq!(json_error_code(r#"{"error":{"code":"Nope"}}"#).as_deref(), Some("Nope"));
        assert_eq!(json_error_code(r#"{"errors":[{"code":"DENIED"}]}"#), None);
        assert_eq!(
            xml_error_code("<Error><Code>BlobNotFound</Code></Error>").as_deref(),
            Some("BlobNotFound")
        );
    }

    #[test]
    fn response_errors_read_like_azcore() {
        let error = ResponseError {
            method: "POST",
            url: "https://r.azurecr.io/oauth2/exchange?api-version=2021-07-01",
            status: StatusCode::FORBIDDEN,
            error_code: None,
            body: r#"{"errors":[{"code":"DENIED"}]}"#,
        };

        assert_eq!(
            error.text(),
            format!(
                "POST https://r.azurecr.io/oauth2/exchange\n{SEPARATOR}\nRESPONSE 403: 403 Forbidden\nERROR CODE UNAVAILABLE\n{SEPARATOR}\n{{\n  \"errors\": [\n    {{\n      \"code\": \"DENIED\"\n    }}\n  ]\n}}\n{SEPARATOR}\n"
            )
        );
    }

    #[tokio::test]
    async fn transient_statuses_are_retried_and_the_last_response_is_returned() {
        let (base, handle) = super::super::testing::serve(vec![
            (503, "text/plain", String::new()),
            (200, "text/plain", "ok".to_owned()),
        ])
        .await;

        let response = send(Client::new().get(&base)).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(handle.await.unwrap().len(), 2);
    }

    #[test]
    fn printed_urls_drop_the_query_and_decode_the_path() {
        assert_eq!(
            printed_url("https://login.example:8443/common/UserRealm/a%20b%40c?api-version=1.0"),
            "https://login.example:8443/common/UserRealm/a b@c"
        );
        assert_eq!(printed_url("http://127.0.0.1:80"), "http://127.0.0.1:80");
    }
}
