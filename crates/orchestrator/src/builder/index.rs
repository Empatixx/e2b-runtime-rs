use bytes::Bytes;
use serde::{Deserialize, Serialize};

use crate::storage::backend::Storage;
use crate::storage::paths::BuildPaths;
use crate::template::metadata::{CURRENT_VERSION, TemplateMetadata};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexEntry {
    template: IndexTemplate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct IndexTemplate {
    build_id: String,
}

pub struct LayerIndex {
    storage: Storage,
    scope: String,
}

impl LayerIndex {
    pub fn new(storage: Storage, scope: &str) -> Self {
        Self {
            storage,
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
        self.storage.put(&self.object(hash), Bytes::from(json)).await
    }

    pub async fn cached(&self, hash: &str) -> Result<Option<(String, TemplateMetadata)>, String> {
        let Some(bytes) = self.storage.find(&self.object(hash)).await? else {
            return Ok(None);
        };
        let entry: IndexEntry =
            serde_json::from_slice(&bytes).map_err(|e| format!("error unmarshalling layer index: {e}"))?;
        let Ok(build_id) = uuid::Uuid::parse_str(&entry.template.build_id) else {
            return Ok(None);
        };
        let metadata = self
            .storage
            .find(&BuildPaths::new(build_id).metadata())
            .await?
            .map(|bytes| TemplateMetadata::from_json(&bytes))
            .transpose()?;
        Ok(metadata
            .filter(|metadata| metadata.version >= CURRENT_VERSION)
            .map(|metadata| (entry.template.build_id, metadata)))
    }

    fn object(&self, hash: &str) -> String {
        format!("{}/index/{hash}", self.scope)
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;

    #[tokio::test]
    async fn a_saved_layer_is_cached_only_with_v2_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        let index = LayerIndex::new(storage.clone(), "scope");
        let build_id = Uuid::new_v4();
        let metadata = TemplateMetadata {
            version: CURRENT_VERSION,
            ..TemplateMetadata::default()
        };

        index.save("h1", &build_id.to_string()).await.unwrap();
        assert_eq!(index.cached("h1").await.unwrap(), None);

        storage
            .put(
                &BuildPaths::new(build_id).metadata(),
                Bytes::from(TemplateMetadata::v1().to_json()),
            )
            .await
            .unwrap();
        assert_eq!(index.cached("h1").await.unwrap(), None);

        storage
            .put(&BuildPaths::new(build_id).metadata(), Bytes::from(metadata.to_json()))
            .await
            .unwrap();
        assert_eq!(
            index.cached("h1").await.unwrap(),
            Some((build_id.to_string(), metadata))
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("scope/index/h1")).unwrap(),
            format!(r#"{{"template":{{"build_id":"{build_id}"}}}}"#)
        );
        assert_eq!(index.cached("missing").await.unwrap(), None);
    }
}
