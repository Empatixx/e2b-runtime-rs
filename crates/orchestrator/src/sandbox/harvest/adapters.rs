use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use uuid::Uuid;

use super::{HarvestTemplates, HarvestUpload, MetadataUploader, MetadataWriteError};
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

    async fn update_metadata(&self, build_id: Uuid, metadata: &TemplateMetadata) -> Result<(), MetadataWriteError> {
        let template = self.get(build_id);
        let written = replace_atomically(&template.dir().join(METADATA_FILE), metadata.to_json()).await;
        if written.is_ok() {
            template.update_metadata(metadata.clone());
        }
        written.and_then(|committed| committed.map_or(Ok(()), Err))
    }
}

async fn replace_atomically(file: &Path, content: Vec<u8>) -> Result<Option<MetadataWriteError>, MetadataWriteError> {
    let file = file.to_owned();
    tokio::task::spawn_blocking(move || write_then_rename(&file, &content))
        .await
        .map_err(|e| MetadataWriteError::failed(e.to_string()))?
}

fn write_then_rename(file: &Path, content: &[u8]) -> Result<Option<MetadataWriteError>, MetadataWriteError> {
    let staged = file.with_extension("json.tmp");
    let failed = |e: std::io::Error| MetadataWriteError::failed(format!("{}: {e}", file.display()));
    std::fs::write(&staged, content).map_err(failed)?;
    std::fs::File::open(&staged)
        .and_then(|staged| staged.sync_all())
        .map_err(failed)?;
    std::fs::rename(&staged, file).map_err(failed)?;
    Ok(file
        .parent()
        .map(|parent| std::fs::File::open(parent).and_then(|dir| dir.sync_all()))
        .and_then(Result::err)
        .map(|e| MetadataWriteError::committed(format!("sync {}: {e}", file.display()))))
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
