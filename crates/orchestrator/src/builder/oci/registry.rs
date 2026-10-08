use std::collections::BTreeMap;

use futures::TryStreamExt;
use futures::stream::BoxStream;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;

use super::ecr::BasicCredentials;
use super::reference::ImageReference;

const MANIFEST_ACCEPT: &str = "application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.v2+json";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    Transport {
        status: u16,
        codes: Vec<String>,
        message: String,
    },
    Other(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RegistryError::Transport { message, .. } | RegistryError::Other(message) => f.write_str(message),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType", default)]
    pub media_type: String,
    pub digest: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub platform: Option<Platform>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Platform {
    #[serde(default)]
    pub os: String,
    #[serde(default)]
    pub architecture: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
struct Manifest {
    #[serde(rename = "mediaType", default)]
    media_type: String,
    #[serde(default)]
    manifests: Vec<Descriptor>,
    #[serde(default)]
    config: Option<Descriptor>,
    #[serde(default)]
    layers: Vec<Descriptor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageManifest {
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    #[serde(default)]
    errors: Vec<Diagnostic>,
}

#[derive(Deserialize)]
struct Diagnostic {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: String,
    #[serde(default)]
    access_token: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Authorization {
    Anonymous,
    Basic(BasicCredentials),
    Bearer(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Challenge {
    scheme: String,
    params: BTreeMap<String, String>,
}

pub struct RegistrySession {
    client: Client,
    reference: ImageReference,
    authorization: Authorization,
}

impl RegistrySession {
    pub async fn open(
        reference: &ImageReference,
        credentials: Option<BasicCredentials>,
    ) -> Result<Self, RegistryError> {
        let client = Client::new();
        let base = format!("{}://{}/v2/", reference.scheme(), reference.registry);
        let ping = client.get(&base).send().await.map_err(request_failure)?;
        let authorization = match ping.status() {
            StatusCode::OK => Authorization::Anonymous,
            StatusCode::UNAUTHORIZED => {
                let challenge = challenge(&ping);
                authorize(&client, reference, challenge, credentials).await?
            }
            _ => return Err(transport_error("GET", &base, ping).await),
        };
        Ok(Self {
            client,
            reference: reference.clone(),
            authorization,
        })
    }

    pub async fn image(&self, platform: &Platform) -> Result<ImageManifest, RegistryError> {
        let top = self.manifest(self.reference.identifier()).await?;
        let manifest = match top.media_type.as_str() {
            OCI_INDEX | DOCKER_LIST => {
                let child = top
                    .manifests
                    .iter()
                    .find(|entry| {
                        entry
                            .platform
                            .as_ref()
                            .is_some_and(|candidate| matches(candidate, platform))
                    })
                    .ok_or_else(|| {
                        RegistryError::Other(format!(
                            "no child with platform {}/{} in index {}",
                            platform.os,
                            platform.architecture,
                            self.reference.name()
                        ))
                    })?;
                self.manifest(&child.digest).await?
            }
            _ => top,
        };
        let config = manifest
            .config
            .ok_or_else(|| RegistryError::Other("manifest has no config".to_owned()))?;
        Ok(ImageManifest {
            config,
            layers: manifest.layers,
        })
    }

    pub async fn blob_bytes(&self, digest: &str) -> Result<Vec<u8>, RegistryError> {
        let response = self.blob(digest).await?;
        response
            .bytes()
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(request_failure)
    }

    pub async fn blob_stream(
        &self,
        digest: &str,
    ) -> Result<BoxStream<'static, std::io::Result<bytes::Bytes>>, RegistryError> {
        let response = self.blob(digest).await?;
        Ok(Box::pin(response.bytes_stream().map_err(std::io::Error::other)))
    }

    async fn blob(&self, digest: &str) -> Result<Response, RegistryError> {
        let url = self.url("blobs", digest);
        let response = self
            .authorized(self.client.get(&url))
            .send()
            .await
            .map_err(request_failure)?;
        match response.status().is_success() {
            true => Ok(response),
            false => Err(transport_error("GET", &url, response).await),
        }
    }

    async fn manifest(&self, identifier: &str) -> Result<Manifest, RegistryError> {
        let url = self.url("manifests", identifier);
        let response = self
            .authorized(self.client.get(&url).header(ACCEPT, MANIFEST_ACCEPT))
            .send()
            .await
            .map_err(request_failure)?;
        if response.status() != StatusCode::OK {
            return Err(transport_error("GET", &url, response).await);
        }
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        let body = response.bytes().await.map_err(request_failure)?;
        let manifest: Manifest =
            serde_json::from_slice(&body).map_err(|e| RegistryError::Other(format!("parsing manifest: {e}")))?;
        Ok(match manifest.media_type.is_empty() {
            true => Manifest {
                media_type: content_type,
                ..manifest
            },
            false => manifest,
        })
    }

    fn url(&self, kind: &str, identifier: &str) -> String {
        format!(
            "{}://{}/v2/{}/{kind}/{identifier}",
            self.reference.scheme(),
            self.reference.registry,
            self.reference.repository_str()
        )
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &self.authorization {
            Authorization::Anonymous => request,
            Authorization::Basic(basic) => request.basic_auth(&basic.username, Some(&basic.password)),
            Authorization::Bearer(token) => request.header(AUTHORIZATION, format!("Bearer {token}")),
        }
    }
}

async fn authorize(
    client: &Client,
    reference: &ImageReference,
    challenge: Option<Challenge>,
    credentials: Option<BasicCredentials>,
) -> Result<Authorization, RegistryError> {
    match challenge {
        Some(challenge) if challenge.scheme.eq_ignore_ascii_case("bearer") => {
            bearer_token(client, reference, &challenge.params, credentials.as_ref())
                .await
                .map(Authorization::Bearer)
        }
        _ => Ok(credentials.map_or(Authorization::Anonymous, Authorization::Basic)),
    }
}

async fn bearer_token(
    client: &Client,
    reference: &ImageReference,
    params: &BTreeMap<String, String>,
    credentials: Option<&BasicCredentials>,
) -> Result<String, RegistryError> {
    let realm = params
        .get("realm")
        .ok_or_else(|| RegistryError::Other("malformed www-authenticate, missing realm".to_owned()))?;
    let scope = format!("repository:{}:pull", reference.repository_str());
    let query: Vec<(&str, &str)> = [("scope", scope.as_str())]
        .into_iter()
        .chain(params.get("service").map(|service| ("service", service.as_str())))
        .collect();
    let request = client.get(realm).query(&query);
    let request = match credentials {
        Some(basic) => request.basic_auth(&basic.username, Some(&basic.password)),
        None => request,
    };
    let response = request.send().await.map_err(request_failure)?;
    if response.status() != StatusCode::OK {
        return Err(transport_error("GET", realm, response).await);
    }
    let token: TokenResponse = response
        .json()
        .await
        .map_err(|e| RegistryError::Other(format!("decoding token response: {e}")))?;
    Ok(match token.token.is_empty() {
        true => token.access_token,
        false => token.token,
    })
}

fn challenge(response: &Response) -> Option<Challenge> {
    let header = response.headers().get(WWW_AUTHENTICATE)?.to_str().ok()?;
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    Some(Challenge {
        scheme: scheme.to_owned(),
        params: parse_params(rest),
    })
}

fn parse_params(text: &str) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    let mut rest = text.trim();
    while let Some((key, after)) = rest.split_once('=') {
        let key = key.trim().trim_start_matches(',').trim().to_ascii_lowercase();
        let (value, remaining) = match after.strip_prefix('"') {
            Some(quoted) => quoted.split_once('"').unwrap_or((quoted, "")),
            None => after.split_once(',').unwrap_or((after, "")),
        };
        params.insert(key, value.to_owned());
        rest = remaining.trim_start_matches([',', ' ']);
    }
    params
}

fn matches(candidate: &Platform, wanted: &Platform) -> bool {
    candidate.os == wanted.os && candidate.architecture == wanted.architecture
}

async fn transport_error(method: &str, url: &str, response: Response) -> RegistryError {
    let status = response.status();
    let body = response.bytes().await.unwrap_or_default();
    let diagnostics = serde_json::from_slice::<ErrorEnvelope>(&body)
        .map(|envelope| envelope.errors)
        .unwrap_or_default();
    let detail = match diagnostics.as_slice() {
        [] => format!(
            "unexpected status code {} {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or_default()
        ),
        [one] => format!("{}: {}", one.code, one.message),
        many => format!(
            "multiple errors returned: {}",
            many.iter()
                .map(|d| format!("{}: {}", d.code, d.message))
                .collect::<Vec<_>>()
                .join("; ")
        ),
    };
    RegistryError::Transport {
        status: status.as_u16(),
        codes: diagnostics.into_iter().map(|diagnostic| diagnostic.code).collect(),
        message: format!("{method} {url}: {detail}"),
    }
}

fn request_failure(error: reqwest::Error) -> RegistryError {
    RegistryError::Other(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_parse_quoted_params() {
        let params = parse_params(
            r#"realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/ubuntu:pull""#,
        );

        assert_eq!(params["realm"], "https://auth.docker.io/token");
        assert_eq!(params["service"], "registry.docker.io");
        assert_eq!(params["scope"], "repository:library/ubuntu:pull");
    }
}
