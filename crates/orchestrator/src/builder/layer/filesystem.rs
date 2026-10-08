use std::path::PathBuf;

use super::LayerExecutor;
use crate::builder::uploads::LayerUpload;
use crate::proto::LogLevel;
use crate::sandbox::snapshot::filesystem::FilesystemSnapshot;
use crate::template::metadata::TemplateMetadata;

impl LayerExecutor<'_> {
    pub fn persist_filesystem_layer(
        &self,
        layer: FilesystemSnapshot,
        dir: PathBuf,
        metadata: &TemplateMetadata,
        hash: &str,
        build_origin: &str,
    ) -> Result<(), String> {
        let node = &self.env.node;
        let build_id = &metadata.template.build_id;
        self.log
            .push(LogLevel::Debug, format!("Adding layer to cache: {build_id}"));
        node.cache_filesystem_layer(&layer, metadata, dir)
            .map_err(|e| format!("error adding snapshot to template cache: {e}"))?;
        self.log.push(LogLevel::Debug, format!("Uploading layer: {build_id}"));
        self.uploads.start(
            node.spawn_filesystem_upload(layer, self.object_metadata(build_origin)),
            LayerUpload {
                index: self.index.clone(),
                hash: hash.to_owned(),
                build_id: build_id.clone(),
                fields: self.log.fields.clone(),
            },
        );
        Ok(())
    }
}
