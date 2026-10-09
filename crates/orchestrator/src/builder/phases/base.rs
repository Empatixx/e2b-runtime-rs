mod build;

use std::collections::BTreeMap;
use std::sync::Arc;

use futures::FutureExt;
use futures::future::BoxFuture;
use uuid::Uuid;

use super::BuildEnv;
use crate::builder::config::BuildSettings;
use crate::builder::hash::{BaseKey, template_source};
use crate::builder::index::{CacheMiss, LayerIndex, MetadataError};
use crate::builder::logs::ScopedLog;
use crate::builder::phase::{BuildError, LayerResult, Phase, PhaseMeta};
use crate::builder::template_files::hash::provision_version;
use crate::builder::uploads::LayerUploads;
use crate::proto::FromImageRegistry;
use crate::template::metadata::{CURRENT_VERSION, Context, TemplateMetadata, TemplateVersions};

const PREFIX: &str = "base";
const STEP_TYPE: &str = "base";
const ROOT_USER: &str = "root";
const MISSING_TEMPLATE: &str = "error getting base template, you may need to rebuild it first";

#[derive(Debug, Clone, PartialEq)]
pub enum BaseSource {
    Image {
        image: String,
        registry: Option<FromImageRegistry>,
    },
    Template {
        alias: String,
        build_id: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct BaseRequest {
    pub template_id: String,
    pub team_id: String,
    pub source: BaseSource,
    pub memory_mb: i64,
    pub vcpus: i64,
    pub disk_size_mb: i64,
    pub force: bool,
    pub envd_version: String,
}

pub struct BasePhase {
    pub env: Arc<BuildEnv>,
    pub request: BaseRequest,
    pub settings: Arc<BuildSettings>,
    pub index: Arc<LayerIndex>,
    pub uploads: Arc<LayerUploads>,
}

impl BasePhase {
    fn source_key(&self) -> String {
        match &self.request.source {
            BaseSource::Image { image, .. } => image.clone(),
            BaseSource::Template { build_id, .. } => template_source(build_id),
        }
    }
}

pub struct BaseLayers<'a> {
    pub index: &'a LayerIndex,
    pub request: &'a BaseRequest,
    pub kernel_version: &'a str,
    pub firecracker_version: &'a str,
}

impl BaseLayers<'_> {
    pub async fn resolve(&self, hash: String) -> Result<LayerResult, BuildError> {
        match &self.request.source {
            BaseSource::Template { alias, build_id } => self.template_layer(alias, build_id, hash).await,
            BaseSource::Image { image, .. } => Ok(self.image_layer(image, hash).await),
        }
    }

    fn fresh_metadata(&self, image: &str) -> TemplateMetadata {
        TemplateMetadata {
            version: CURRENT_VERSION,
            template: TemplateVersions {
                build_id: Uuid::new_v4().to_string(),
                kernel_version: self.kernel_version.to_owned(),
                firecracker_version: self.firecracker_version.to_owned(),
            },
            context: Context {
                user: Some(ROOT_USER.to_owned()),
                workdir: None,
                env_vars: Some(BTreeMap::new()),
            },
            from_image: Some(image.to_owned()),
            ..TemplateMetadata::default()
        }
    }

    async fn template_layer(&self, alias: &str, build_id: &str, hash: String) -> Result<LayerResult, BuildError> {
        let metadata = self.index.metadata_of(build_id).await.map_err(|e| match e {
            MetadataError::Missing(_) => BuildError::user(&base_meta(), MISSING_TEMPLATE),
            MetadataError::Failed(e) => BuildError::Internal(format!("error getting base template: {e}")),
        })?;
        Ok(LayerResult {
            metadata: metadata.based_on(alias, build_id),
            cached: true,
            hash,
        })
    }

    async fn image_layer(&self, image: &str, hash: String) -> LayerResult {
        let fresh = LayerResult {
            metadata: self.fresh_metadata(image),
            cached: false,
            hash: hash.clone(),
        };
        if self.request.force {
            return fresh;
        }
        match self.index.cached(&hash).await {
            Ok(layer) => LayerResult {
                metadata: layer.metadata,
                cached: true,
                hash,
            },
            Err(CacheMiss::NoLayer(e)) => {
                tracing::info!(error = %e, hash = %hash, "base layer not found in cache, building new base layer");
                fresh
            }
            Err(CacheMiss::NoMetadata(e)) => {
                tracing::info!(error = %e, hash = %hash, "base layer metadata not found in cache, building new base layer");
                fresh
            }
        }
    }
}

fn base_hash(settings: &BuildSettings, request: &BaseRequest, source_key: &str) -> String {
    let provision_version = settings
        .flags
        .provision_version()
        .map_or_else(provision_version, |version| version.to_string());
    BaseKey {
        provision_version: &provision_version,
        disk_size_mb: request.disk_size_mb,
        source: source_key,
        envd_memory_protection: matches!(request.source, BaseSource::Image { .. }) && settings.envd_memory_protection,
    }
    .hash()
}

fn base_meta() -> PhaseMeta {
    PhaseMeta {
        phase: PREFIX,
        step_type: STEP_TYPE.to_owned(),
        step_number: None,
    }
}

impl Phase for BasePhase {
    fn prefix(&self) -> String {
        PREFIX.to_owned()
    }

    fn describe(&self) -> String {
        match &self.request.source {
            BaseSource::Image { image, .. } => format!("FROM {image}"),
            BaseSource::Template { alias, .. } => format!("FROM TEMPLATE {alias}"),
        }
    }

    fn meta(&self) -> PhaseMeta {
        base_meta()
    }

    fn hash<'a>(&'a self, _source: &'a LayerResult) -> BoxFuture<'a, Result<String, BuildError>> {
        let key = base_hash(&self.settings, &self.request, &self.source_key());
        async move { Ok(key) }.boxed()
    }

    fn layer<'a>(&'a self, _source: &'a LayerResult, hash: String) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move {
            BaseLayers {
                index: &self.index,
                request: &self.request,
                kernel_version: &self.settings.kernel_version,
                firecracker_version: &self.settings.firecracker_version,
            }
            .resolve(hash)
            .await
        }
        .boxed()
    }

    fn build<'a>(
        &'a self,
        log: ScopedLog<'a>,
        _source: &'a LayerResult,
        current: LayerResult,
    ) -> BoxFuture<'a, Result<LayerResult, BuildError>> {
        async move {
            let metadata = build::BaseBuild { phase: self, log: &log }
                .run(current.metadata, &current.hash)
                .await?;
            Ok(LayerResult {
                metadata,
                cached: false,
                hash: current.hash,
            })
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::storage::backend::Storage;
    use crate::template::metadata::FromTemplate;

    fn request(source: BaseSource, force: bool) -> BaseRequest {
        BaseRequest {
            template_id: "t".to_owned(),
            team_id: "team".to_owned(),
            source,
            memory_mb: 1024,
            vcpus: 2,
            disk_size_mb: 1024,
            force,
            envd_version: "0.9.0".to_owned(),
        }
    }

    fn image() -> BaseSource {
        BaseSource::Image {
            image: "debian:12".to_owned(),
            registry: None,
        }
    }

    async fn settings_with(overrides: &[(&str, serde_json::Value)]) -> BuildSettings {
        let flags = overrides
            .iter()
            .fold(crate::featureflags::FeatureFlags::builder(), |builder, (key, value)| {
                builder.override_flag(key, value.clone())
            })
            .offline();
        let config = crate::builder::config::BuilderConfig {
            feature_flags: flags,
            ..crate::builder::config::BuilderConfig::from_vars(&Default::default())
        };
        BuildSettings::resolve(&config, "template", "team").await.unwrap()
    }

    fn template_source() -> BaseSource {
        BaseSource::Template {
            alias: "base".to_owned(),
            build_id: "b1".to_owned(),
        }
    }

    #[tokio::test]
    async fn an_unset_provision_version_keeps_the_files_hash_key() {
        let request = request(image(), false);
        let unset = base_hash(&settings_with(&[]).await, &request, "debian:12");
        let zero = base_hash(
            &settings_with(&[("build-provision-version", 0.into())]).await,
            &request,
            "debian:12",
        );
        let set = base_hash(
            &settings_with(&[("build-provision-version", 7.into())]).await,
            &request,
            "debian:12",
        );

        assert_eq!(unset, zero);
        assert_ne!(unset, set);
        assert_eq!(
            set,
            BaseKey {
                provision_version: "7",
                disk_size_mb: 1024,
                source: "debian:12",
                envd_memory_protection: false,
            }
            .hash()
        );
    }

    #[tokio::test]
    async fn memory_protection_enters_the_key_only_for_a_build_from_an_image() {
        let on = settings_with(&[("build-envd-memory-protection", true.into())]).await;
        let off = settings_with(&[]).await;

        assert_ne!(
            base_hash(&on, &request(image(), false), "debian:12"),
            base_hash(&off, &request(image(), false), "debian:12")
        );
        assert_eq!(
            base_hash(&on, &request(template_source(), false), "template:b1"),
            base_hash(&off, &request(template_source(), false), "template:b1")
        );
    }

    async fn resolve(storage: &Storage, request: &BaseRequest, hash: &str) -> Result<LayerResult, BuildError> {
        let index = LayerIndex::new(storage.clone(), storage.clone(), "scope");
        BaseLayers {
            index: &index,
            request,
            kernel_version: "vmlinux-6.1.158",
            firecracker_version: "v1.14-0.2.0",
        }
        .resolve(hash.to_owned())
        .await
    }

    async fn store_metadata(storage: &Storage, build_id: &str, version: u64) {
        let metadata = TemplateMetadata {
            version,
            template: TemplateVersions {
                build_id: build_id.to_owned(),
                kernel_version: "k".to_owned(),
                firecracker_version: "f".to_owned(),
            },
            ..TemplateMetadata::default()
        };
        storage
            .put(&format!("{build_id}/metadata.json"), Bytes::from(metadata.to_json()))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_image_layer_is_fresh_until_the_index_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();

        let fresh = resolve(&storage, &request(image(), false), "h1").await.unwrap();
        assert!(!fresh.cached);
        assert_eq!(fresh.metadata.from_image.as_deref(), Some("debian:12"));
        assert_eq!(fresh.metadata.context.user.as_deref(), Some("root"));
        assert_eq!(fresh.metadata.template.kernel_version, "vmlinux-6.1.158");

        store_metadata(&storage, "b1", 2).await;
        LayerIndex::new(storage.clone(), storage.clone(), "scope")
            .save("h1", "b1")
            .await
            .unwrap();
        let cached = resolve(&storage, &request(image(), false), "h1").await.unwrap();
        let forced = resolve(&storage, &request(image(), true), "h1").await.unwrap();

        assert!(cached.cached);
        assert_eq!(cached.metadata.template.build_id, "b1");
        assert!(!forced.cached);
    }

    #[tokio::test]
    async fn a_template_base_needs_its_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        let from = |build_id: &str| BaseSource::Template {
            alias: "base".to_owned(),
            build_id: build_id.to_owned(),
        };
        store_metadata(&storage, "old", 1).await;
        store_metadata(&storage, "good", 2).await;

        let missing = resolve(&storage, &request(from("nope"), false), "h").await.unwrap_err();
        let outdated = resolve(&storage, &request(from("old"), false), "h").await.unwrap_err();
        let good = resolve(&storage, &request(from("good"), false), "h").await.unwrap();

        assert_eq!(
            missing.reason().message,
            "error getting base template, you may need to rebuild it first"
        );
        assert_eq!(missing.reason().step.as_deref(), Some("base"));
        assert_eq!(
            outdated,
            BuildError::Internal(
                "error getting base template: outdated template metadata: expected 2, got 1".to_owned()
            )
        );
        assert!(good.cached);
        assert_eq!(
            good.metadata.from_template,
            Some(FromTemplate {
                alias: "base".to_owned(),
                build_id: "good".to_owned()
            })
        );
    }
}
