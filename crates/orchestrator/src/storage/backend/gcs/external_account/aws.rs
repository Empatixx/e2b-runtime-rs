use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::config::{CredentialSource, ExternalAccountFile};
use crate::go_url::GoUrl;
use crate::go_url::escape::{Mode, escape};
use crate::go_url::query::Query;

const ALGORITHM: &str = "AWS4-HMAC-SHA256";
const REQUEST_TYPE: &str = "aws4_request";
const DEFAULT_VERIFICATION_URL: &str = "https://sts.{region}.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15";
const SESSION_TOKEN_HEADER: &str = "X-aws-ec2-metadata-token";
const SESSION_TTL_HEADER: &str = "X-aws-ec2-metadata-token-ttl-seconds";
const SESSION_TTL: &str = "300";
const SUPPORTED_VERSION: i64 = 1;

pub struct AwsSubject {
    region_url: String,
    verification_url: String,
    metadata_credentials_url: String,
    session_token_url: String,
    target_resource: String,
    client: Client,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsKeys {
    pub access_key_id: String,
    pub secret_access_key: String,
    pub session_token: String,
}

#[derive(Serialize)]
struct SignedRequest {
    url: String,
    method: &'static str,
    headers: Vec<SignedHeader>,
}

#[derive(Serialize)]
struct SignedHeader {
    key: String,
    value: String,
}

impl AwsSubject {
    pub fn new(account: &ExternalAccountFile, source: &CredentialSource, client: &Client) -> Result<Self, String> {
        let version: i64 = source.environment_id[3..].parse().unwrap_or(0);
        if version != SUPPORTED_VERSION {
            return Err(format!(
                "credentials: aws version '{version}' is not supported in the current build"
            ));
        }
        Ok(Self {
            region_url: source.region_url.clone(),
            verification_url: match source.regional_cred_verification_url.is_empty() {
                true => DEFAULT_VERIFICATION_URL.to_owned(),
                false => source.regional_cred_verification_url.clone(),
            },
            metadata_credentials_url: source.url.clone(),
            session_token_url: source.imdsv2_session_token_url.clone(),
            target_resource: account.audience.clone(),
            client: client.clone(),
        })
    }

    pub async fn token(&self) -> Result<String, String> {
        let session_token = self.session_token().await?;
        let keys = self.security_credentials(session_token.as_deref()).await?;
        let region = self.region(session_token.as_deref()).await?;
        let url = self.verification_url.replacen("{region}", &region, 1);
        signed_subject_token(&url, &self.target_resource, &region, &keys, Utc::now())
    }

    fn region_from_env(&self) -> Option<String> {
        environment("AWS_REGION").or_else(|| environment("AWS_DEFAULT_REGION"))
    }

    fn keys_from_env(&self) -> Option<AwsKeys> {
        Some(AwsKeys {
            access_key_id: environment("AWS_ACCESS_KEY_ID")?,
            secret_access_key: environment("AWS_SECRET_ACCESS_KEY")?,
            session_token: environment("AWS_SESSION_TOKEN").unwrap_or_default(),
        })
    }

    async fn session_token(&self) -> Result<Option<String>, String> {
        let uses_metadata = self.region_from_env().is_none() || self.keys_from_env().is_none();
        if !uses_metadata || self.session_token_url.is_empty() {
            return Ok(None);
        }
        let response = self
            .client
            .put(&self.session_token_url)
            .header(SESSION_TTL_HEADER, SESSION_TTL)
            .send()
            .await
            .map_err(|e| e.to_string())?;
        let status = response.status();
        let body = response.text().await.map_err(|e| e.to_string())?;
        match (status.as_u16(), body.is_empty()) {
            (200, true) => Ok(None),
            (200, false) => Ok(Some(body)),
            _ => Err(format!("credentials: unable to retrieve AWS session token: {body}")),
        }
    }

    async fn get(&self, url: &str, session_token: Option<&str>) -> Result<(u16, String), String> {
        let request = session_token.iter().fold(self.client.get(url), |request, token| {
            request.header(SESSION_TOKEN_HEADER, *token)
        });
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| e.to_string())?;
        Ok((status, body))
    }

    async fn region(&self, session_token: Option<&str>) -> Result<String, String> {
        if let Some(region) = self.region_from_env() {
            return Ok(region);
        }
        if self.region_url.is_empty() {
            return Err("credentials: unable to determine AWS region".to_owned());
        }
        match self.get(&self.region_url, session_token).await? {
            (200, body) => {
                let mut zone = body;
                zone.pop();
                Ok(zone)
            }
            (_, body) => Err(format!("credentials: unable to retrieve AWS region - {body}")),
        }
    }

    async fn security_credentials(&self, session_token: Option<&str>) -> Result<AwsKeys, String> {
        if let Some(keys) = self.keys_from_env() {
            return Ok(keys);
        }
        if self.metadata_credentials_url.is_empty() {
            return Err(
                "credentials: unable to determine the AWS metadata server security credentials endpoint".to_owned(),
            );
        }
        let role = match self.get(&self.metadata_credentials_url, session_token).await? {
            (200, body) => body,
            (_, body) => return Err(format!("credentials: unable to retrieve AWS role name - {body}")),
        };
        let url = format!("{}/{role}", self.metadata_credentials_url);
        let body = match self.get(&url, session_token).await? {
            (200, body) => body,
            (_, body) => {
                return Err(format!(
                    "credentials: unable to retrieve AWS security credentials - {body}"
                ));
            }
        };
        let keys = parse_keys(&body)?;
        match (keys.access_key_id.is_empty(), keys.secret_access_key.is_empty()) {
            (true, _) => Err("credentials: missing AccessKeyId credential".to_owned()),
            (_, true) => Err("credentials: missing SecretAccessKey credential".to_owned()),
            _ => Ok(keys),
        }
    }
}

fn environment(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

fn parse_keys(body: &str) -> Result<AwsKeys, String> {
    let fields: BTreeMap<String, String> = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(body)
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter_map(|(key, value)| Some((key.to_ascii_lowercase(), value.as_str()?.to_owned())))
        .collect();
    let field = |name: &str| fields.get(name).cloned().unwrap_or_default();
    Ok(AwsKeys {
        access_key_id: field("accesskeyid"),
        secret_access_key: field("secretaccesskey"),
        session_token: field("token"),
    })
}

pub struct UnsignedRequest<'a> {
    pub method: &'a str,
    pub url: &'a GoUrl,
    pub headers: &'a [(String, String)],
}

pub fn authorization(request: &UnsignedRequest, keys: &AwsKeys, region: &str, now: DateTime<Utc>) -> String {
    let (columns, data) = canonical_headers(request.headers);
    let date_stamp = now.format("%Y%m%d").to_string();
    let service = request.url.host.split('.').next().unwrap_or_default();
    let scope = [date_stamp.as_str(), region, service, REQUEST_TYPE].join("/");
    let canonical = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        request.method,
        canonical_path(request.url),
        canonical_query(request.url),
        data,
        columns,
        sha256_hex(b"")
    );
    let to_sign = [
        ALGORITHM,
        &now.format("%Y%m%dT%H%M%SZ").to_string(),
        &scope,
        &sha256_hex(canonical.as_bytes()),
    ]
    .join("\n");
    let key = [date_stamp.as_str(), region, service, REQUEST_TYPE, to_sign.as_str()]
        .iter()
        .fold(format!("AWS4{}", keys.secret_access_key).into_bytes(), |key, input| {
            hmac(&key, input.as_bytes())
        });
    format!(
        "{ALGORITHM} Credential={}/{scope}, SignedHeaders={columns}, Signature={}",
        keys.access_key_id,
        hex(&key)
    )
}

fn signed_subject_token(
    url: &str,
    target_resource: &str,
    region: &str,
    keys: &AwsKeys,
    now: DateTime<Utc>,
) -> Result<String, String> {
    let parsed = GoUrl::parse(url).map_err(|e| e.to_string())?;
    let mut headers = vec![("host".to_owned(), parsed.host.clone())];
    if !keys.session_token.is_empty() {
        headers.push(("x-amz-security-token".to_owned(), keys.session_token.clone()));
    }
    headers.push(("x-amz-date".to_owned(), now.format("%Y%m%dT%H%M%SZ").to_string()));
    if !target_resource.is_empty() {
        headers.push(("x-goog-cloud-target-resource".to_owned(), target_resource.to_owned()));
    }
    let signature = authorization(
        &UnsignedRequest {
            method: "POST",
            url: &parsed,
            headers: &headers,
        },
        keys,
        region,
        now,
    );
    headers.push(("authorization".to_owned(), signature));
    let mut signed: Vec<SignedHeader> = headers
        .into_iter()
        .map(|(key, value)| SignedHeader {
            key: canonical_header_key(&key),
            value,
        })
        .collect();
    signed.sort_by(|a, b| (a.key.as_str(), a.value.as_str()).cmp(&(b.key.as_str(), b.value.as_str())));
    let json = serde_json::to_string(&SignedRequest {
        url: parsed.to_string(),
        method: "POST",
        headers: signed,
    })
    .map_err(|e| e.to_string())?;
    Ok(escape(&json, Mode::QueryComponent))
}

fn canonical_header_key(key: &str) -> String {
    key.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(|first| first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn canonical_headers(headers: &[(String, String)]) -> (String, String) {
    let mut merged: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for (key, value) in headers {
        merged.entry(key.to_ascii_lowercase()).or_default().push(value);
    }
    let columns = merged.keys().cloned().collect::<Vec<_>>().join(";");
    let data = merged
        .iter()
        .map(|(key, values)| format!("{key}:{}\n", values.join(",")))
        .collect();
    (columns, data)
}

fn canonical_path(url: &GoUrl) -> String {
    match url.escaped_path().as_str() {
        "" => "/".to_owned(),
        path => clean_path(path),
    }
}

fn clean_path(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|last| *last != "..") => {
                parts.pop();
            }
            ".." if rooted => {}
            other => parts.push(other),
        }
    }
    match (rooted, parts.is_empty()) {
        (true, _) => format!("/{}", parts.join("/")),
        (false, true) => ".".to_owned(),
        (false, false) => parts.join("/"),
    }
}

fn canonical_query(url: &GoUrl) -> String {
    let mut query = Query::parse(&url.raw_query);
    query.values.values_mut().for_each(|values| values.sort());
    query.encode()
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn keys() -> AwsKeys {
        AwsKeys {
            access_key_id: "AKIDEXAMPLE".to_owned(),
            secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".to_owned(),
            session_token: String::new(),
        }
    }

    #[test]
    fn the_signature_matches_the_aws_get_vanilla_vector() {
        let url = GoUrl::parse("https://host.foo.com").unwrap();
        let headers = [
            ("host".to_owned(), "host.foo.com".to_owned()),
            ("date".to_owned(), "Mon, 09 Sep 2011 23:36:00 GMT".to_owned()),
        ];

        let signature = authorization(
            &UnsignedRequest {
                method: "GET",
                url: &url,
                headers: &headers,
            },
            &keys(),
            "us-east-1",
            Utc.with_ymd_and_hms(2011, 9, 9, 23, 36, 0).unwrap(),
        );

        assert_eq!(
            signature,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20110909/us-east-1/host/aws4_request, SignedHeaders=date;host, Signature=b27ccfbfa7df52a200ff74193ca6e32d4b48b8856fab7ebf1c595d0670a7e470"
        );
    }

    #[test]
    fn paths_are_cleaned_like_go() {
        assert_eq!(clean_path("/foo/bar/../.."), "/");
        assert_eq!(clean_path("/./"), "/");
        assert_eq!(clean_path("/a//b/./c"), "/a/b/c");
    }

    #[test]
    fn the_subject_token_is_the_url_escaped_signed_caller_identity_request() {
        let now = Utc.with_ymd_and_hms(2011, 9, 9, 23, 36, 0).unwrap();

        let token = signed_subject_token(
            "https://sts.us-east-1.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15",
            "//iam.googleapis.com/projects/1/locations/global/workloadIdentityPools/p/providers/x",
            "us-east-1",
            &AwsKeys {
                session_token: "tok".to_owned(),
                ..keys()
            },
            now,
        )
        .unwrap();

        let decoded = crate::go_url::escape::unescape(&token, Mode::QueryComponent).unwrap();
        let value: serde_json::Value = serde_json::from_str(&decoded).unwrap();
        assert_eq!(
            value["url"],
            "https://sts.us-east-1.amazonaws.com?Action=GetCallerIdentity&Version=2011-06-15"
        );
        assert_eq!(value["method"], "POST");
        let names: Vec<&str> = value["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|header| header["key"].as_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "Authorization",
                "Host",
                "X-Amz-Date",
                "X-Amz-Security-Token",
                "X-Goog-Cloud-Target-Resource"
            ]
        );
        assert!(
            value["headers"][0]["value"]
                .as_str()
                .unwrap()
                .contains("Credential=AKIDEXAMPLE/20110909/us-east-1/sts/aws4_request, SignedHeaders=host;x-amz-date;x-amz-security-token;x-goog-cloud-target-resource")
        );
    }

    #[test]
    fn metadata_credentials_match_json_keys_case_insensitively() {
        let keys = parse_keys(r#"{"AccessKeyId":"a","SecretAccessKey":"s","Token":"t","Expiration":"x"}"#).unwrap();

        assert_eq!(
            keys,
            AwsKeys {
                access_key_id: "a".to_owned(),
                secret_access_key: "s".to_owned(),
                session_token: "t".to_owned()
            }
        );
    }
}
