use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use serde::Deserialize;

use super::acr::AcrAuthenticator;
use super::auth::{auth_option, basic};
use super::ecr::{BasicCredentials, DefaultChainEcr, TokenError};
use super::reference::{ImageReference, Repository};
use super::registry::{Descriptor, ImageManifest, Platform, RegistryError, RegistrySession};
use crate::builder::config::BuilderConfig;
use crate::proto::FromImageRegistry;
use crate::storage::backend::azure::default_credential::DefaultAzureCredential;

const GCP_PROXY_USER: &str = "_json_key_base64";
const LINUX: &str = "linux";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DockerHubProxy {
    Noop,
    Gcp {
        url: String,
        credentials: BasicCredentials,
    },
    Aws {
        url: String,
        ecr: DefaultChainEcr,
    },
    Azure {
        url: String,
        authenticator: AcrAuthenticator,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ImageConfig {
    #[serde(default, deserialize_with = "nullable")]
    pub architecture: String,
    #[serde(default, deserialize_with = "nullable")]
    pub os: String,
    #[serde(default, deserialize_with = "nullable")]
    pub config: ContainerConfig,
}

fn nullable<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de> + Default,
{
    Option::<T>::deserialize(deserializer).map(Option::unwrap_or_default)
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ContainerConfig {
    #[serde(rename = "Env", default)]
    pub env: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct AuthConfig {
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

pub struct PulledImage {
    pub session: Arc<RegistrySession>,
    pub manifest: ImageManifest,
    pub config: ImageConfig,
}

pub struct Puller {
    pub proxy: DockerHubProxy,
    pub platform: Platform,
}

impl DockerHubProxy {
    pub fn from_config(config: &BuilderConfig) -> Result<Self, String> {
        if config.dockerhub_url.is_empty() {
            return Ok(DockerHubProxy::Noop);
        }
        let url = config.dockerhub_url.clone();
        match config.dockerhub_provider.as_str() {
            "GCP_REMOTE_REPOSITORY" => gcp_credentials()
                .map(|credentials| DockerHubProxy::Gcp { url, credentials })
                .map_err(|e| format!("error getting auth token: {e}")),
            "AWS_ECR" => Ok(DockerHubProxy::Aws {
                url,
                ecr: DefaultChainEcr::default(),
            }),
            "AZURE_ACR" => azure_proxy(url),
            "Local" => Ok(DockerHubProxy::Noop),
            other => Err(format!("unknown dockerhub remote repository provider: {other}")),
        }
    }
}

fn azure_proxy(url: String) -> Result<DockerHubProxy, String> {
    if url.contains("://") {
        return Err(format!(
            "invalid Azure remote repository URL {url:?}: want registry.host/repository without a scheme"
        ));
    }
    let repository = Repository::parse_strict(&url)
        .map_err(|e| format!("invalid Azure remote repository URL {url:?} (want registry.host/repository): {e}"))?;
    let credential = DefaultAzureCredential::new()
        .map(Arc::new)
        .map_err(|e| format!("failed to create Azure default credential: {e}"))?;
    Ok(DockerHubProxy::Azure {
        url,
        authenticator: AcrAuthenticator::new(&repository.registry, credential),
    })
}

fn gcp_credentials() -> Result<BasicCredentials, String> {
    let Some(encoded) = std::env::var("DOCKER_AUTH_BASE64")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return Ok(basic(
            GCP_PROXY_USER,
            &std::env::var("GOOGLE_SERVICE_ACCOUNT_BASE64").unwrap_or_default(),
        ));
    };
    let decoded = URL_SAFE.decode(&encoded).map_err(|e| {
        format!(
            "failed to decode auth config: {}",
            super::ecr::base64_error(&encoded, e)
        )
    })?;
    serde_json::from_slice::<AuthConfig>(&decoded)
        .map(|config| basic(&config.username, &config.password))
        .map_err(|e| format!("invalid JSON auth config: {e}"))
}

impl Puller {
    pub fn new(config: &BuilderConfig) -> Result<Self, String> {
        Ok(Self {
            proxy: DockerHubProxy::from_config(config)?,
            platform: Platform {
                os: LINUX.to_owned(),
                architecture: config.target_arch.clone(),
            },
        })
    }

    pub async fn get_public_image(
        &self,
        tag: &str,
        registry: Option<&FromImageRegistry>,
    ) -> Result<PulledImage, String> {
        let reference = ImageReference::parse(tag).map_err(|e| format!("invalid image reference '{tag}': {e}"))?;
        let has_auth = registry.is_some_and(|registry| registry.r#type.is_some());
        let (session, manifest) = match (has_auth, reference.is_docker_hub()) {
            (false, true) => self
                .through_proxy(&reference)
                .await
                .map_err(|e| wrap_pull_error(tag, &e))?,
            _ => {
                let credentials = auth_option(registry).await?;
                self.pull(&reference, credentials)
                    .await
                    .map_err(|e| wrap_pull_error(tag, &e))?
            }
        };
        let config = self.verified_config(tag, &session, &manifest).await?;
        Ok(PulledImage {
            session: Arc::new(session),
            manifest,
            config,
        })
    }

    async fn through_proxy(
        &self,
        reference: &ImageReference,
    ) -> Result<(RegistrySession, ImageManifest), RegistryError> {
        let (url, credentials) = match &self.proxy {
            DockerHubProxy::Noop => return self.pull(reference, None).await,
            DockerHubProxy::Azure { url, authenticator } => {
                return self.through_acr(reference, url, authenticator).await;
            }
            DockerHubProxy::Gcp { url, credentials } => (url, credentials.clone()),
            DockerHubProxy::Aws { url, ecr } => (
                url,
                ecr.basic_auth()
                    .await
                    .map_err(|e| RegistryError::Other(format!("error getting auth token: {}", proxy_token_error(e))))?,
            ),
        };
        let proxied = ImageReference::parse(&format!("{url}/{}", reference.without_registry()))
            .map_err(|e| RegistryError::Other(format!("invalid image reference: {e}")))?;
        self.pull(&proxied, Some(credentials)).await
    }

    async fn through_acr(
        &self,
        reference: &ImageReference,
        url: &str,
        authenticator: &AcrAuthenticator,
    ) -> Result<(RegistrySession, ImageManifest), RegistryError> {
        let proxied = ImageReference::parse(&format!("{url}/{}", reference.without_registry()))
            .map_err(|e| RegistryError::Other(format!("invalid image reference: {e}")))?;
        let first = self.pull_with_acr(&proxied, authenticator).await;
        match first {
            Err(RegistryError::Transport { status: 401 | 403, .. }) => {
                authenticator.invalidate();
                self.pull_with_acr(&proxied, authenticator).await
            }
            other => other,
        }
    }

    async fn pull_with_acr(
        &self,
        reference: &ImageReference,
        authenticator: &AcrAuthenticator,
    ) -> Result<(RegistrySession, ImageManifest), RegistryError> {
        let credentials = authenticator
            .authorization()
            .await
            .map_err(|e| RegistryError::Other(format!("error pulling image: {e}")))?;
        self.pull(reference, Some(credentials)).await
    }

    async fn pull(
        &self,
        reference: &ImageReference,
        credentials: Option<BasicCredentials>,
    ) -> Result<(RegistrySession, ImageManifest), RegistryError> {
        let session = RegistrySession::open(reference, credentials).await?;
        let manifest = session.image(&self.platform).await?;
        Ok((session, manifest))
    }

    async fn verified_config(
        &self,
        tag: &str,
        session: &RegistrySession,
        manifest: &ImageManifest,
    ) -> Result<ImageConfig, String> {
        let config = session
            .blob_bytes(&manifest.config)
            .await
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice::<ImageConfig>(&bytes).map_err(|e| e.to_string()))
            .map_err(|e| {
                tracing::warn!(image_ref = tag, error = %e, "failed to get image config file");
                format!("failed to inspect image '{tag}': unable to retrieve image metadata from registry")
            })?;
        match config.architecture == self.platform.architecture {
            true => Ok(config),
            false => Err(format!(
                "image architecture {:?} does not match expected {:?}",
                config.architecture, self.platform.architecture
            )),
        }
    }
}

fn proxy_token_error(error: TokenError) -> String {
    match error {
        TokenError::Request(e) => format!("failed to get aws ecr auth token: {e}"),
        TokenError::Missing => "no aws ecr auth token found".to_owned(),
        TokenError::Decode(e) => format!("failed to decode aws ecr auth token: {e}"),
        TokenError::Format => "invalid aws ecr auth token".to_owned(),
    }
}

pub fn wrap_pull_error(tag: &str, error: &RegistryError) -> String {
    tracing::warn!(image_ref = tag, error = %error, "failed to pull image");
    let RegistryError::Transport { status, codes, .. } = error else {
        return format!("failed to pull image '{tag}': unable to retrieve image from registry");
    };
    codes
        .iter()
        .find_map(|code| match code.as_str() {
            "MANIFEST_UNKNOWN" => Some(format!(
                "image '{tag}' not found: the image or tag does not exist in the registry"
            )),
            "NAME_UNKNOWN" => Some(format!(
                "repository '{tag}' not found: verify the image name is correct"
            )),
            "UNAUTHORIZED" => Some(format!(
                "access denied to '{tag}': authentication required or insufficient permissions"
            )),
            "DENIED" => Some(format!(
                "access denied to '{tag}': you don't have permission to pull this image"
            )),
            _ => None,
        })
        .unwrap_or_else(|| match status {
            0 => format!("failed to pull image '{tag}': unable to retrieve image from registry"),
            status => format!("failed to pull image '{tag}': registry returned status code {status}"),
        })
}

impl PulledImage {
    pub fn size(&self) -> u64 {
        self.manifest.layers.iter().map(|layer| layer.size).sum()
    }

    pub fn layers(&self) -> &[Descriptor] {
        &self.manifest.layers
    }

    pub fn envs(&self) -> BTreeMap<String, String> {
        parse_envs(self.config.config.env.as_deref().unwrap_or_default())
    }
}

pub fn parse_envs(entries: &[String]) -> BTreeMap<String, String> {
    entries
        .iter()
        .filter(|entry| !entry.trim().is_empty())
        .filter_map(|entry| entry.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .filter(|(key, value)| !key.is_empty() && !value.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transport(status: u16, codes: &[&str]) -> RegistryError {
        RegistryError::Transport {
            status,
            codes: codes.iter().map(|code| code.to_string()).collect(),
            message: String::new(),
        }
    }

    #[test]
    fn pull_errors_are_sanitised_like_upstream() {
        let tag = "registry.example.com/test/image:latest";

        assert_eq!(
            wrap_pull_error(tag, &transport(418, &["UNKNOWN"])),
            "failed to pull image 'registry.example.com/test/image:latest': registry returned status code 418"
        );
        assert_eq!(
            wrap_pull_error(tag, &transport(401, &["UNAUTHORIZED"])),
            "access denied to 'registry.example.com/test/image:latest': authentication required or insufficient permissions"
        );
        assert_eq!(
            wrap_pull_error(tag, &transport(404, &["BLOB_UNKNOWN", "MANIFEST_UNKNOWN"])),
            "image 'registry.example.com/test/image:latest' not found: the image or tag does not exist in the registry"
        );
        assert_eq!(
            wrap_pull_error(tag, &transport(401, &[])),
            "failed to pull image 'registry.example.com/test/image:latest': registry returned status code 401"
        );
        assert_eq!(
            wrap_pull_error(tag, &RegistryError::Other("dns".to_owned())),
            "failed to pull image 'registry.example.com/test/image:latest': unable to retrieve image from registry"
        );
    }

    #[test]
    fn envs_drop_blank_and_empty_entries() {
        let entries = ["PATH=/usr/bin", " ", "EMPTY=", "=x", "SPACED = v ", "NOEQ", "A=b=c"].map(str::to_owned);

        assert_eq!(
            parse_envs(&entries),
            BTreeMap::from([
                ("A".to_owned(), "b=c".to_owned()),
                ("PATH".to_owned(), "/usr/bin".to_owned()),
                ("SPACED".to_owned(), "v".to_owned()),
            ])
        );
    }

    #[test]
    fn proxy_provider_follows_upstream() {
        let config = |provider: &str, url: &str| BuilderConfig {
            dockerhub_provider: provider.to_owned(),
            dockerhub_url: url.to_owned(),
            ..BuilderConfig::from_vars(&Default::default())
        };

        assert_eq!(
            DockerHubProxy::from_config(&config("GCP_REMOTE_REPOSITORY", "")),
            Ok(DockerHubProxy::Noop)
        );
        assert_eq!(
            DockerHubProxy::from_config(&config("Local", "x")),
            Ok(DockerHubProxy::Noop)
        );
        assert_eq!(
            DockerHubProxy::from_config(&config("AWS_ECR", "1.dkr.ecr")),
            Ok(DockerHubProxy::Aws {
                url: "1.dkr.ecr".to_owned(),
                ecr: DefaultChainEcr::default(),
            })
        );
        assert!(matches!(
            DockerHubProxy::from_config(&config("AZURE_ACR", "reg.azurecr.io/team")),
            Ok(DockerHubProxy::Azure { url, .. }) if url == "reg.azurecr.io/team"
        ));
        assert_eq!(
            DockerHubProxy::from_config(&config("AZURE_ACR", "https://reg.azurecr.io/team")),
            Err("invalid Azure remote repository URL \"https://reg.azurecr.io/team\": want registry.host/repository without a scheme".to_owned())
        );
        assert_eq!(
            DockerHubProxy::from_config(&config("AZURE_ACR", "team/app")),
            Err("invalid Azure remote repository URL \"team/app\" (want registry.host/repository): strict validation requires the registry to be explicitly defined".to_owned())
        );
        assert_eq!(
            DockerHubProxy::from_config(&config("Nope", "x")),
            Err("unknown dockerhub remote repository provider: Nope".to_owned())
        );
    }

    mod fake_registry {
        use axum::Router;
        use axum::extract::Path;
        use axum::http::{HeaderMap, StatusCode, header};
        use axum::response::{IntoResponse, Response};
        use axum::routing::get;

        use crate::builder::oci::registry::digest_of;

        pub const INDEX_TAG: &str = "multi";
        const CREDENTIALS: &str = "Basic dGVzdHVzZXI6dGVzdHBhc3M=";
        const LAYER: [u8; 1024] = [0; 1024];

        #[derive(Clone)]
        struct Image {
            config: String,
            manifest: String,
        }

        fn image(arch: &str) -> Image {
            let config = format!(r#"{{"architecture":"{arch}","os":"linux","config":{{"Env":["A=1","B="]}}}}"#);
            let manifest = format!(
                r#"{{"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{{"mediaType":"application/vnd.oci.image.config.v1+json","digest":"{}","size":{}}},"layers":[{{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":"{}","size":1024}}]}}"#,
                digest_of(config.as_bytes()),
                config.len(),
                digest_of(&LAYER)
            );
            Image { config, manifest }
        }

        pub async fn serve(arch: &'static str) -> String {
            serve_accepting(arch, CREDENTIALS.to_owned()).await
        }

        pub fn basic_header(username: &str, password: &str) -> String {
            use base64::Engine;
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
            )
        }

        pub async fn serve_accepting(arch: &'static str, accepted: String) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let served = image(arch);
            let blobs = served.clone();
            let (for_ping, for_manifest, for_blob) = (accepted.clone(), accepted.clone(), accepted);
            let router = Router::new()
                .route("/v2/", get(move |headers| ping(headers, for_ping.clone())))
                .route(
                    "/v2/test/image/manifests/{reference}",
                    get(move |headers, path| manifest(headers, path, served.clone(), for_manifest.clone())),
                )
                .route(
                    "/v2/test/image/blobs/{digest}",
                    get(move |headers, path| blob(headers, path, blobs.clone(), for_blob.clone())),
                );
            tokio::spawn(async move { axum::serve(listener, router).await });
            address.to_string()
        }

        fn authorized(headers: &HeaderMap, accepted: &str) -> bool {
            headers
                .get(header::AUTHORIZATION)
                .is_some_and(|value| value == accepted)
        }

        fn unauthorized() -> Response {
            (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Basic realm=\"test\"")],
                r#"{"errors":[{"code":"UNAUTHORIZED","message":"authentication required"}]}"#,
            )
                .into_response()
        }

        async fn ping(headers: HeaderMap, accepted: String) -> Response {
            match authorized(&headers, &accepted) {
                true => StatusCode::OK.into_response(),
                false => unauthorized(),
            }
        }

        async fn manifest(
            headers: HeaderMap,
            Path(reference): Path<String>,
            image: Image,
            accepted: String,
        ) -> Response {
            if !authorized(&headers, &accepted) {
                return unauthorized();
            }
            let index = format!(
                r#"{{"schemaVersion":2,"manifests":[{{"digest":"sha256:other","size":1,"platform":{{"os":"linux","architecture":"s390x"}}}},{{"digest":"{}","size":{}}}]}}"#,
                digest_of(image.manifest.as_bytes()),
                image.manifest.len()
            );
            match reference.as_str() {
                "teapot" => (
                    StatusCode::IM_A_TEAPOT,
                    r#"{"errors":[{"code":"UNKNOWN","message":"teapot"}]}"#,
                )
                    .into_response(),
                INDEX_TAG => (
                    [(header::CONTENT_TYPE, "application/vnd.oci.image.index.v1+json")],
                    index,
                )
                    .into_response(),
                _ => (
                    [(header::CONTENT_TYPE, "application/vnd.oci.image.manifest.v1+json")],
                    image.manifest,
                )
                    .into_response(),
            }
        }

        async fn blob(headers: HeaderMap, Path(digest): Path<String>, image: Image, accepted: String) -> Response {
            match (
                authorized(&headers, &accepted),
                digest == digest_of(image.config.as_bytes()),
            ) {
                (false, _) => unauthorized(),
                (true, true) => image.config.into_response(),
                (true, false) => LAYER.to_vec().into_response(),
            }
        }
    }

    fn puller(arch: &str) -> Puller {
        Puller {
            proxy: DockerHubProxy::Noop,
            platform: Platform {
                os: LINUX.to_owned(),
                architecture: arch.to_owned(),
            },
        }
    }

    fn general(username: &str, password: &str) -> FromImageRegistry {
        FromImageRegistry {
            r#type: Some(crate::proto::from_image_registry::Type::General(
                crate::proto::GeneralRegistry {
                    username: username.to_owned(),
                    password: password.to_owned(),
                },
            )),
        }
    }

    #[tokio::test]
    async fn pulls_with_general_auth_from_a_registry() {
        let address = fake_registry::serve("amd64").await;
        let tag = format!("{address}/test/image:latest");

        let image = puller("amd64")
            .get_public_image(&tag, Some(&general("testuser", "testpass")))
            .await
            .unwrap();

        assert_eq!(image.layers().len(), 1);
        assert_eq!(image.size(), 1024);
        assert_eq!(image.envs(), BTreeMap::from([("A".to_owned(), "1".to_owned())]));
        let wrong = puller("amd64")
            .get_public_image(&tag, Some(&general("testuser", "nope")))
            .await;
        assert_eq!(
            wrong.err().unwrap(),
            format!("access denied to '{tag}': authentication required or insufficient permissions")
        );
        let teapot = puller("amd64")
            .get_public_image(
                &format!("{address}/test/image:teapot"),
                Some(&general("testuser", "testpass")),
            )
            .await;
        assert_eq!(
            teapot.err().unwrap(),
            format!("failed to pull image '{address}/test/image:teapot': registry returned status code 418")
        );
    }

    async fn exchange_server(tokens: [&'static str; 2]) -> String {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let served = std::sync::Arc::new(AtomicUsize::new(0));
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            loop {
                let (mut connection, _) = listener.accept().await.unwrap();
                crate::storage::backend::azure::testing::read_request(&mut connection).await;
                let token = tokens[served.fetch_add(1, Ordering::SeqCst).min(1)];
                let body = format!(r#"{{"refresh_token":"{token}"}}"#);
                let reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                connection.write_all(reply.as_bytes()).await.unwrap();
            }
        });
        format!("http://{address}")
    }

    #[tokio::test]
    async fn acr_proxy_retries_once_with_a_fresh_token_after_an_unauthorized_pull() {
        let registry = fake_registry::serve_accepting(
            "amd64",
            fake_registry::basic_header("00000000-0000-0000-0000-000000000000", "fresh"),
        )
        .await;
        let exchange = exchange_server(["dead", "fresh"]).await;
        let authenticator = AcrAuthenticator::with_base_url(
            "reg.azurecr.io",
            exchange,
            std::sync::Arc::new(DefaultAzureCredential::with_static_token("aad")),
        );
        let mut puller = puller("amd64");
        puller.proxy = DockerHubProxy::Azure {
            url: registry,
            authenticator,
        };

        let image = puller.get_public_image("test/image:latest", None).await.unwrap();

        assert_eq!(image.layers().len(), 1);
    }

    #[tokio::test]
    async fn the_platform_is_picked_from_an_index_and_the_architecture_is_checked() {
        let address = fake_registry::serve("arm64").await;
        let credentials = general("testuser", "testpass");

        let mismatch = puller("amd64")
            .get_public_image(&format!("{address}/test/image:latest"), Some(&credentials))
            .await;
        let index = RegistrySession::open(
            &ImageReference::parse(&format!("{address}/test/image:{}", fake_registry::INDEX_TAG)).unwrap(),
            Some(basic("testuser", "testpass")),
        )
        .await
        .unwrap();
        let missing = index
            .image(&Platform {
                os: LINUX.to_owned(),
                architecture: "riscv64".to_owned(),
            })
            .await;
        let defaulted = index
            .image(&Platform {
                os: LINUX.to_owned(),
                architecture: "amd64".to_owned(),
            })
            .await
            .unwrap();
        let layer = index.blob_bytes(&defaulted.layers[0]).await.unwrap();

        assert_eq!(
            mismatch.err().unwrap(),
            "image architecture \"arm64\" does not match expected \"amd64\""
        );
        assert!(
            matches!(missing, Err(RegistryError::Other(text)) if text.starts_with("no child with platform {Architecture:riscv64 OS:linux"))
        );
        assert_eq!(layer.len(), 1024);
    }
}
