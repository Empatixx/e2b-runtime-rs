use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::storage::backend::Storage;
use crate::template::metadata::TemplateMetadata;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexEntry {
    template: IndexTemplate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexTemplate {
    #[serde(default)]
    build_id: String,
}

const MINIMAL_CACHED_VERSION: u64 = 2;

const NOT_EXIST: &str = "object does not exist";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheMiss {
    NoLayer(String),
    NoMetadata(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataError {
    Missing(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CachedLayer {
    pub build_id: String,
    pub metadata: TemplateMetadata,
}

pub struct LayerIndex {
    index_storage: Storage,
    template_storage: Storage,
    scope: String,
}

impl LayerIndex {
    pub fn new(index_storage: Storage, template_storage: Storage, scope: &str) -> Self {
        Self {
            index_storage,
            template_storage,
            scope: scope.to_owned(),
        }
    }

    pub async fn save(&self, hash: &str, build_id: &str) -> Result<(), String> {
        let entry = IndexEntry {
            template: IndexTemplate {
                build_id: build_id.to_owned(),
            },
        };
        let json = serde_json::to_vec(&entry).map_err(|e| format!("error marshalling layer metadata: {e}"))?;
        self.index_storage
            .put(&self.object(hash), Bytes::from(json))
            .await
            .map_err(|e| format!("error writing layer metadata to object: {e}"))
    }

    pub async fn cached(&self, hash: &str) -> Result<CachedLayer, CacheMiss> {
        let build_id = self.layer_build_id(hash).await.map_err(CacheMiss::NoLayer)?;
        self.metadata_of(&build_id)
            .await
            .map(|metadata| CachedLayer { build_id, metadata })
            .map_err(|e| match e {
                MetadataError::Missing(e) | MetadataError::Failed(e) => CacheMiss::NoMetadata(e),
            })
    }

    pub async fn metadata_of(&self, build_id: &str) -> Result<TemplateMetadata, MetadataError> {
        let path = format!("{build_id}/metadata.json");
        let bytes = self
            .template_storage
            .find(&path)
            .await
            .map_err(|e| {
                MetadataError::Failed(format!(
                    "error reading template metadata: error reading template metadata from object: {e}"
                ))
            })?
            .ok_or_else(|| {
                MetadataError::Missing(format!(
                    "error reading template metadata: error reading template metadata from object: {NOT_EXIST}"
                ))
            })?;
        let metadata = TemplateMetadata::from_json(&bytes).map_err(|e| {
            MetadataError::Failed(format!(
                "error reading template metadata: error unmarshaling template version: {e}"
            ))
        })?;
        match metadata.version >= MINIMAL_CACHED_VERSION {
            true => Ok(metadata),
            false => Err(MetadataError::Failed(format!(
                "outdated template metadata: expected {}, got {}",
                crate::template::metadata::CURRENT_VERSION,
                metadata.version
            ))),
        }
    }

    pub async fn layer_build_id(&self, hash: &str) -> Result<String, String> {
        let bytes = self
            .index_storage
            .find(&self.object(hash))
            .await
            .map_err(|e| format!("error reading layer metadata from object: {e}"))?
            .ok_or_else(|| format!("error reading layer metadata from object: {NOT_EXIST}"))?;
        let entry = serde_json::from_slice::<IndexEntry>(&bytes)
            .map_err(|e| format!("error unmarshaling layer metadata: {e}"))?;
        match entry.template.build_id.is_empty() {
            true => Err("layer metadata is missing required fields: {{}}".to_owned()),
            false => Ok(entry.template.build_id),
        }
    }

    fn object(&self, hash: &str) -> String {
        join_path(&[&self.scope, "index", hash])
    }
}

pub fn layer_files_path(scope: &str, hash: &str) -> String {
    join_path(&[scope, "files", &format!("{hash}.tar")])
}

pub fn join_path(parts: &[&str]) -> String {
    let joined = parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    let absolute = joined.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in joined.split('/') {
        match segment {
            "" | "." => {}
            ".." if segments.last().is_some_and(|last| *last != "..") => {
                segments.pop();
            }
            ".." if absolute => {}
            segment => segments.push(segment),
        }
    }
    match (absolute, segments.is_empty()) {
        (true, _) => format!("/{}", segments.join("/")),
        (false, true) => String::new(),
        (false, false) => segments.join("/"),
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::storage::paths::BuildPaths;

    #[tokio::test]
    async fn a_saved_layer_is_cached_only_with_v2_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        let index = LayerIndex::new(storage.clone(), storage.clone(), "scope");
        let build_id = Uuid::new_v4();
        let metadata = TemplateMetadata {
            version: 2,
            ..TemplateMetadata::default()
        };

        index.save("h1", &build_id.to_string()).await.unwrap();
        assert_eq!(
            index.cached("h1").await,
            Err(CacheMiss::NoMetadata(
                "error reading template metadata: error reading template metadata from object: object does not exist"
                    .to_owned()
            ))
        );

        storage
            .put(
                &BuildPaths::new(build_id).metadata(),
                Bytes::from(TemplateMetadata::v1().to_json()),
            )
            .await
            .unwrap();
        assert_eq!(
            index.cached("h1").await,
            Err(CacheMiss::NoMetadata(
                "outdated template metadata: expected 2, got 1".to_owned()
            ))
        );

        storage
            .put(&BuildPaths::new(build_id).metadata(), Bytes::from(metadata.to_json()))
            .await
            .unwrap();
        assert_eq!(
            index.cached("h1").await,
            Ok(CachedLayer {
                build_id: build_id.to_string(),
                metadata
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("scope/index/h1")).unwrap(),
            format!(r#"{{"template":{{"build_id":"{build_id}"}}}}"#)
        );
        assert_eq!(
            index.cached("missing").await,
            Err(CacheMiss::NoLayer(
                "error reading layer metadata from object: object does not exist".to_owned()
            ))
        );
        storage
            .put("scope/index/broken", Bytes::from_static(b"{"))
            .await
            .unwrap();
        assert!(matches!(
            index.cached("broken").await,
            Err(CacheMiss::NoLayer(e)) if e.starts_with("error unmarshaling layer metadata: ")
        ));
        storage
            .put("scope/index/empty", Bytes::from_static(br#"{"template":{}}"#))
            .await
            .unwrap();
        assert_eq!(
            index.cached("empty").await,
            Err(CacheMiss::NoLayer(
                "layer metadata is missing required fields: {{}}".to_owned()
            ))
        );
    }

    #[test]
    fn paths_join_like_go_path_join() {
        assert_eq!(join_path(&["scope", "index", "h"]), "scope/index/h");
        assert_eq!(join_path(&["scope", "files", "../x.tar"]), "scope/x.tar");
        assert_eq!(join_path(&["", "files", "h.tar"]), "files/h.tar");
        assert_eq!(join_path(&["a", "..", ".."]), "..");
    }
}
