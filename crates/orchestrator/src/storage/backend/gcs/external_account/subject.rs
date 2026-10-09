use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;

use super::super::{credsfile, go_json};
use super::aws::AwsSubject;
use super::config::{CredentialSource, ExternalAccountFile, Format};
use super::executable::ExecutableSubject;
use super::x509::X509Subject;

const UNABLE_TO_PARSE: &str = "credentials: unable to parse credential source";

pub enum Subject {
    File {
        path: String,
        format: Option<Format>,
    },
    Url {
        url: String,
        headers: HashMap<String, String>,
        format: Option<Format>,
        client: Client,
    },
    Executable(ExecutableSubject),
    Aws(AwsSubject),
    X509(X509Subject),
}

impl Subject {
    pub fn from_file(account: &ExternalAccountFile, client: &Client) -> Result<Self, String> {
        let source = account.credential_source.clone().unwrap_or_default();
        match () {
            () if is_aws(&source) => AwsSubject::new(account, &source, client).map(Self::Aws),
            () if source.environment_id.len() > 3 && source.environment_id.starts_with("aws") => {
                Err(UNABLE_TO_PARSE.to_owned())
            }
            () if !source.file.is_empty() => Ok(Self::File {
                path: source.file,
                format: source.format,
            }),
            () if !source.url.is_empty() => Ok(Self::Url {
                url: source.url,
                headers: source.headers,
                format: source.format,
                client: client.clone(),
            }),
            () if source.executable.is_some() => ExecutableSubject::new(account, &source).map(Self::Executable),
            () if source.certificate.is_some() => X509Subject::new(&source).map(Self::X509),
            () => Err(UNABLE_TO_PARSE.to_owned()),
        }
    }

    pub fn provider_type(&self) -> &'static str {
        match self {
            Self::File { .. } => "file",
            Self::Url { .. } => "url",
            Self::Executable(_) => "executable",
            Self::Aws(_) => "aws",
            Self::X509(_) => "x509",
        }
    }

    pub async fn token(&self) -> Result<String, String> {
        match self {
            Self::File { path, format } => file_token(path, format.as_ref()),
            Self::Url {
                url,
                headers,
                format,
                client,
            } => url_token(client, url, headers, format.as_ref()).await,
            Self::Executable(executable) => executable.token().await,
            Self::Aws(aws) => aws.token().await,
            Self::X509(x509) => x509.token(),
        }
    }
}

fn is_aws(source: &CredentialSource) -> bool {
    source.environment_id.len() > 3
        && source.environment_id.starts_with("aws")
        && source.environment_id[3..].parse::<i64>().is_ok()
}

fn file_token(path: &str, format: Option<&Format>) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| {
        format!(
            "credentials: failed to open credential file {}: {}",
            crate::go_strconv::quote(path),
            crate::go_os::path_error("open", std::path::Path::new(path), &e)
        )
    })?;
    let text = String::from_utf8_lossy(&bytes).trim().to_owned();
    formatted(&text, format)
}

async fn url_token(
    client: &Client,
    url: &str,
    headers: &HashMap<String, String>,
    format: Option<&Format>,
) -> Result<String, String> {
    let request = headers
        .iter()
        .fold(client.get(url), |request, (name, value)| request.header(name, value));
    let response = request
        .timeout(Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("credentials: invalid response when retrieving subject token: {e}"))?;
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|e| format!("credentials: invalid response when retrieving subject token: {e}"))?;
    match (200..300).contains(&status.as_u16()) {
        true => formatted(&body, format),
        false => Err(format!("credentials: status code {}: {body}", status.as_u16())),
    }
}

fn formatted(text: &str, format: Option<&Format>) -> Result<String, String> {
    let Some(format) = format else {
        return Ok(text.to_owned());
    };
    match format.r#type.as_str() {
        "json" => json_field(text, &format.subject_token_field_name),
        "text" => Ok(text.to_owned()),
        other => Err(format!(
            "credentials: invalid credential_source file format type: {other}"
        )),
    }
}

fn json_field(text: &str, field: &str) -> Result<String, String> {
    let object: HashMap<String, Value> = go_json::unmarshal(text.as_bytes(), credsfile::ANY_OBJECT)
        .map_err(|e| format!("credentials: failed to unmarshal subject token file: {e}"))?;
    match object.get(field) {
        None => Err("credentials: provided subject_token_field_name not found in credentials".to_owned()),
        Some(Value::String(token)) => Ok(token.clone()),
        Some(_) => Err("credentials: improperly formatted subject token".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_format(field: &str) -> Format {
        Format {
            r#type: "json".to_owned(),
            subject_token_field_name: field.to_owned(),
        }
    }

    #[test]
    fn formats_pick_a_field_or_the_whole_text() {
        assert_eq!(formatted("tok", None).unwrap(), "tok");
        assert_eq!(formatted(r#"{"a":"v"}"#, Some(&json_format("a"))).unwrap(), "v");
        assert_eq!(
            formatted(r#"{"a":"v"}"#, Some(&json_format("b"))).unwrap_err(),
            "credentials: provided subject_token_field_name not found in credentials"
        );
        assert_eq!(
            formatted(r#"{"a":1}"#, Some(&json_format("a"))).unwrap_err(),
            "credentials: improperly formatted subject token"
        );
        assert_eq!(
            formatted(
                "x",
                Some(&Format {
                    r#type: "yaml".to_owned(),
                    subject_token_field_name: String::new()
                })
            )
            .unwrap_err(),
            "credentials: invalid credential_source file format type: yaml"
        );
    }

    #[test]
    fn file_sources_trim_and_report_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        std::fs::write(&path, "  jwt-token\n").unwrap();

        assert_eq!(file_token(path.to_str().unwrap(), None).unwrap(), "jwt-token");
        assert!(file_token("/nonexistent/token", None).unwrap_err().starts_with(
            "credentials: failed to open credential file \"/nonexistent/token\": open /nonexistent/token: no such file or directory"
        ));
    }

    #[test]
    fn the_source_kind_follows_the_go_precedence() {
        let client = Client::new();
        let account = |source: &str| {
            ExternalAccountFile::parse(
                format!(r#"{{"audience":"a","subject_token_type":"t","credential_source":{source}}}"#).as_bytes(),
            )
            .unwrap()
        };

        assert_eq!(
            Subject::from_file(&account(r#"{"file":"/f","url":"http://u"}"#), &client)
                .unwrap()
                .provider_type(),
            "file"
        );
        assert_eq!(
            Subject::from_file(&account(r#"{"url":"http://u"}"#), &client)
                .unwrap()
                .provider_type(),
            "url"
        );
        assert_eq!(
            Subject::from_file(&account(r#"{"environment_id":"awsx","file":"/f"}"#), &client)
                .err()
                .unwrap(),
            "credentials: unable to parse credential source"
        );
        assert_eq!(
            Subject::from_file(&account("{}"), &client).err().unwrap(),
            "credentials: unable to parse credential source"
        );
    }
}
