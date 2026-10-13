mod challenge;
mod retry;
mod verify;

pub use retry::send as send_with_retry;
pub use verify::digest_of;

use std::time::Duration;

use futures::TryStreamExt;
use futures::stream::BoxStream;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use serde::Deserialize;

use super::ecr::BasicCredentials;
use super::reference::{Identifier, ImageReference};
use challenge::{TokenRequest, all, bearer_token, pick, sendable};
use parking_lot::{Mutex, RwLock};
use verify::Verified;

const MANIFEST_ACCEPT: &str = "application/vnd.docker.distribution.manifest.v2+json,application/vnd.oci.image.manifest.v1+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.index.v1+json,application/vnd.docker.distribution.manifest.v1+json,application/vnd.docker.distribution.manifest.v1+prettyjws";
const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const SCHEMA1: [&str; 2] = [
    "application/vnd.docker.distribution.manifest.v1+json",
    "application/vnd.docker.distribution.manifest.v1+prettyjws",
];
const DEFAULT_OS: &str = "linux";
const DEFAULT_ARCHITECTURE: &str = "amd64";
const HTTP_FALLBACK_DELAY: Duration = Duration::from_millis(300);

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

#[derive(Clone, PartialEq, Eq)]
enum Authorization {
    Anonymous,
    Basic(BasicCredentials),
    Bearer(String),
}

struct Ping {
    scheme: &'static str,
    response: Response,
}

struct Bearer {
    realm: String,
    service: String,
    scopes: Mutex<Vec<String>>,
    credentials: Option<BasicCredentials>,
    insecure: bool,
}

pub struct RegistrySession {
    client: Client,
    reference: ImageReference,
    scheme: &'static str,
    authorization: RwLock<Authorization>,
    bearer: Option<Bearer>,
}

impl RegistrySession {
    pub async fn open(
        reference: &ImageReference,
        credentials: Option<BasicCredentials>,
    ) -> Result<Self, RegistryError> {
        let client = Client::new();
        let ping = ping(&client, reference).await?;
        let insecure = ping.scheme == "http";
        let credentials = credentials.filter(sendable);
        let bearer = match ping.response.status() {
            StatusCode::OK => None,
            StatusCode::UNAUTHORIZED => bearer_of(reference, &ping.response, credentials.clone(), insecure)?,
            _ => {
                let url = format!("{}://{}/v2/", ping.scheme, reference.registry);
                return Err(transport_error("GET", &url, ping.response).await);
            }
        };
        let session = Self {
            client,
            reference: reference.clone(),
            scheme: ping.scheme,
            authorization: RwLock::new(credentials.map_or(Authorization::Anonymous, Authorization::Basic)),
            bearer,
        };
        session.refresh().await?;
        Ok(session)
    }

    pub async fn image(&self, platform: &Platform) -> Result<ImageManifest, RegistryError> {
        let top = self
            .manifest(self.reference.identifier(), self.requested_digest())
            .await?;
        let manifest = match top.media_type.as_str() {
            OCI_INDEX | DOCKER_LIST => {
                let child = top
                    .manifests
                    .iter()
                    .find(|entry| matches(&entry.platform.clone().unwrap_or_else(default_platform), platform))
                    .ok_or_else(|| {
                        RegistryError::Other(format!(
                            "no child with platform {{Architecture:{} OS:{} OSVersion: OSFeatures:[] Variant: Features:[]}} in index {}",
                            platform.architecture,
                            platform.os,
                            self.reference.name()
                        ))
                    })?;
                self.manifest(&child.digest, Some(&child.digest)).await?
            }
            schema1 if SCHEMA1.contains(&schema1) => {
                return Err(RegistryError::Other(format!("unsupported MediaType: {schema1:?}")));
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

    pub async fn blob_bytes(&self, descriptor: &Descriptor) -> Result<Vec<u8>, RegistryError> {
        self.blob_stream(descriptor)
            .await?
            .try_fold(Vec::new(), |mut all, chunk| {
                all.extend_from_slice(&chunk);
                async move { Ok(all) }
            })
            .await
            .map_err(|e| RegistryError::Other(e.to_string()))
    }

    pub async fn blob_stream(
        &self,
        descriptor: &Descriptor,
    ) -> Result<BoxStream<'static, std::io::Result<bytes::Bytes>>, RegistryError> {
        let url = self.url("blobs", &descriptor.digest);
        let response = self.send(|| self.client.get(&url)).await?;
        if response.status() != StatusCode::OK {
            return Err(transport_error("GET", &url, response).await);
        }
        if let Some(length) = content_length(&response).filter(|length| *length != descriptor.size) {
            return Err(RegistryError::Other(format!(
                "GET {url}: Content-Length header {length} does not match expected size {}",
                descriptor.size
            )));
        }
        let body = Box::pin(response.bytes_stream().map_err(std::io::Error::other));
        Ok(Box::pin(Verified::new(body, &descriptor.digest, descriptor.size)))
    }

    fn requested_digest(&self) -> Option<&str> {
        match &self.reference.identifier {
            Identifier::Digest(digest) => Some(digest),
            Identifier::Tag(_) => None,
        }
    }

    async fn manifest(&self, identifier: &str, digest: Option<&str>) -> Result<Manifest, RegistryError> {
        let url = self.url("manifests", identifier);
        let response = self
            .send(|| self.client.get(&url).header(ACCEPT, MANIFEST_ACCEPT))
            .await?;
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
        if let Some(wanted) = digest.filter(|wanted| digest_of(&body) != *wanted) {
            return Err(RegistryError::Other(format!(
                "manifest digest: {:?} does not match requested digest: {wanted:?} for {:?}",
                digest_of(&body),
                self.reference.name()
            )));
        }
        let manifest: Manifest =
            serde_json::from_slice(&body).map_err(|e| RegistryError::Other(format!("parsing manifest: {e}")))?;
        Ok(Manifest {
            media_type: match content_type.is_empty() {
                true => manifest.media_type.clone(),
                false => content_type,
            },
            ..manifest
        })
    }

    fn url(&self, kind: &str, identifier: &str) -> String {
        format!(
            "{}://{}/v2/{}/{kind}/{identifier}",
            self.scheme,
            self.reference.registry,
            self.reference.repository_str()
        )
    }

    async fn send(&self, request: impl Fn() -> RequestBuilder) -> Result<Response, RegistryError> {
        let response = retry::send(self.authorized(request())).await.map_err(request_failure)?;
        let challenges = all(response.headers());
        if self.bearer.is_none() || challenges.is_empty() {
            return Ok(response);
        }
        self.add_scopes(challenges.iter().filter_map(|challenge| challenge.params.get("scope")));
        self.refresh().await?;
        retry::send(self.authorized(request())).await.map_err(request_failure)
    }

    fn add_scopes<'a>(&self, wanted: impl Iterator<Item = &'a String>) {
        if let Some(bearer) = &self.bearer {
            let mut scopes = bearer.scopes.lock();
            let mut added: Vec<String> = wanted.filter(|scope| !scopes.contains(scope)).cloned().collect();
            added.append(&mut scopes);
            *scopes = added;
        }
    }

    async fn refresh(&self) -> Result<(), RegistryError> {
        let Some(bearer) = &self.bearer else {
            return Ok(());
        };
        let scopes = bearer.scopes.lock().clone();
        let token = bearer_token(TokenRequest {
            realm: &bearer.realm,
            service: &bearer.service,
            scopes: &scopes,
            registry: &self.reference.registry,
            insecure: bearer.insecure,
            credentials: bearer.credentials.as_ref(),
        })
        .await?;
        *self.authorization.write() = Authorization::Bearer(token);
        Ok(())
    }

    fn authorized(&self, request: RequestBuilder) -> RequestBuilder {
        match &*self.authorization.read() {
            Authorization::Anonymous => request,
            Authorization::Basic(basic) => request.basic_auth(&basic.username, Some(&basic.password)),
            Authorization::Bearer(token) => request.header(AUTHORIZATION, format!("Bearer {token}")),
        }
    }
}

async fn ping(client: &Client, reference: &ImageReference) -> Result<Ping, RegistryError> {
    let secure = ping_scheme(client, reference, "https");
    if reference.scheme() == "https" {
        return secure.await;
    }
    let insecure = async {
        tokio::time::sleep(HTTP_FALLBACK_DELAY).await;
        ping_scheme(client, reference, "http").await
    };
    tokio::pin!(secure, insecure);
    let (first, other) = tokio::select! {
        result = &mut secure => (result, Either::Insecure),
        result = &mut insecure => (result, Either::Secure),
    };
    match (first, other) {
        (Ok(ping), _) => Ok(ping),
        (Err(first), Either::Insecure) => insecure.await.map_err(|second| combine(first, second)),
        (Err(first), Either::Secure) => secure.await.map_err(|second| combine(second, first)),
    }
}

enum Either {
    Secure,
    Insecure,
}

fn combine(primary: RegistryError, fallback: RegistryError) -> RegistryError {
    match primary {
        RegistryError::Transport { .. } => primary,
        _ => match fallback {
            RegistryError::Transport { .. } => fallback,
            _ => RegistryError::Other(format!("{primary}; {fallback}")),
        },
    }
}

async fn ping_scheme(client: &Client, reference: &ImageReference, scheme: &'static str) -> Result<Ping, RegistryError> {
    let url = format!("{scheme}://{}/v2/", reference.registry);
    let response = retry::send(client.get(&url)).await.map_err(request_failure)?;
    match response.status() {
        StatusCode::OK | StatusCode::UNAUTHORIZED => Ok(Ping { scheme, response }),
        _ => Err(transport_error("GET", &url, response).await),
    }
}

fn bearer_of(
    reference: &ImageReference,
    ping: &Response,
    credentials: Option<BasicCredentials>,
    insecure: bool,
) -> Result<Option<Bearer>, RegistryError> {
    let Some(picked) = pick(ping.headers()).filter(|picked| picked.scheme.eq_ignore_ascii_case("bearer")) else {
        return Ok(None);
    };
    let realm = picked.params.get("realm").ok_or_else(|| {
        RegistryError::Other(format!(
            "malformed www-authenticate, missing realm: {:?}",
            picked.params
        ))
    })?;
    Ok(Some(Bearer {
        realm: realm.clone(),
        service: picked.params.get("service").cloned().unwrap_or_default(),
        scopes: Mutex::new(vec![format!("repository:{}:pull", reference.repository_str())]),
        credentials,
        insecure,
    }))
}

fn default_platform() -> Platform {
    Platform {
        os: DEFAULT_OS.to_owned(),
        architecture: DEFAULT_ARCHITECTURE.to_owned(),
    }
}

fn matches(candidate: &Platform, wanted: &Platform) -> bool {
    candidate.os == wanted.os && candidate.architecture == wanted.architecture
}

fn content_length(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

pub(super) async fn transport_error(method: &str, url: &str, response: Response) -> RegistryError {
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
