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
    build_id: String,
}

const MINIMAL_CACHED_VERSION: u64 = 2;

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
        self.index_storage.put(&self.object(hash), Bytes::from(json)).await
    }

    pub async fn cached(&self, hash: &str) -> Option<CachedLayer> {
        let build_id = self.layer_build_id(hash).await?;
        let bytes = self
            .template_storage
            .find(&format!("{build_id}/metadata.json"))
            .await
            .ok()
            .flatten()?;
        TemplateMetadata::from_json(&bytes)
            .ok()
            .filter(|metadata| metadata.version >= MINIMAL_CACHED_VERSION)
            .map(|metadata| CachedLayer { build_id, metadata })
    }

    async fn layer_build_id(&self, hash: &str) -> Option<String> {
        let bytes = self.index_storage.find(&self.object(hash)).await.ok().flatten()?;
        serde_json::from_slice::<IndexEntry>(&bytes)
            .ok()
            .map(|entry| entry.template.build_id)
            .filter(|build_id| !build_id.is_empty())
    }

    fn object(&self, hash: &str) -> String {
        join_path(&[&self.scope, "index", hash])
    }
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
        assert_eq!(index.cached("h1").await, None);

        storage
            .put(
                &BuildPaths::new(build_id).metadata(),
                Bytes::from(TemplateMetadata::v1().to_json()),
            )
            .await
            .unwrap();
        assert_eq!(index.cached("h1").await, None);

        storage
            .put(&BuildPaths::new(build_id).metadata(), Bytes::from(metadata.to_json()))
            .await
            .unwrap();
        assert_eq!(
            index.cached("h1").await,
            Some(CachedLayer {
                build_id: build_id.to_string(),
                metadata
            })
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("scope/index/h1")).unwrap(),
            format!(r#"{{"template":{{"build_id":"{build_id}"}}}}"#)
        );
        assert_eq!(index.cached("missing").await, None);
        storage
            .put("scope/index/broken", Bytes::from_static(b"{"))
            .await
            .unwrap();
        assert_eq!(index.cached("broken").await, None);
    }

    #[test]
    fn paths_join_like_go_path_join() {
        assert_eq!(join_path(&["scope", "index", "h"]), "scope/index/h");
        assert_eq!(join_path(&["scope", "files", "../x.tar"]), "scope/x.tar");
        assert_eq!(join_path(&["", "files", "h.tar"]), "files/h.tar");
        assert_eq!(join_path(&["a", "..", ".."]), "..");
    }
}
