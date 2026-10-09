use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use uuid::Uuid;

use super::{HarvestTemplates, HarvestUpload, MetadataUploader};
use crate::server::uploads::{Outcome, Uploads};
use crate::storage::backend::{ObjectMetadata, Storage};
use crate::storage::paths::BuildPaths;
use crate::template::Template;
use crate::template::cache::TemplateCache;
use crate::template::metadata::TemplateMetadata;

const METADATA_FILE: &str = "metadata.json";

#[async_trait]
impl HarvestTemplates for TemplateCache {
    async fn pin(&self, build_id: Uuid, max_sandbox_length_hours: i64) -> Result<Arc<Template>, String> {
        Ok(self.get_for(build_id, max_sandbox_length_hours))
    }

    async fn update_metadata(&self, build_id: Uuid, metadata: &TemplateMetadata) -> Result<(), String> {
        let template = self.get(build_id);
        let file = template.dir().join(METADATA_FILE);
        tokio::fs::write(&file, metadata.to_json())
            .await
            .map_err(|e| format!("write {}: {e}", file.display()))?;
        template.update_metadata(metadata.clone());
        Ok(())
    }
}

pub struct SnapshotUpload {
    pub uploads: Arc<Uploads>,
    pub build_id: Uuid,
}

#[async_trait]
impl HarvestUpload for SnapshotUpload {
    async fn wait(&self) -> Result<(), String> {
        match self.uploads.wait(self.build_id).await {
            Outcome::Failed => Err(format!("snapshot upload {} failed", self.build_id)),
            Outcome::Pending | Outcome::Landed => Ok(()),
        }
    }
}

pub struct StorageMetadataUploader {
    pub storage: Storage,
}

#[async_trait]
impl MetadataUploader for StorageMetadataUploader {
    async fn upload_metadata(
        &self,
        build_id: Uuid,
        metadata: &TemplateMetadata,
        object_metadata: &ObjectMetadata,
    ) -> Result<(), String> {
        self.storage
            .put_with(
                &BuildPaths::new(build_id).metadata(),
                Bytes::from(metadata.to_json()),
                object_metadata,
            )
            .await
    }
}
