use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use futures::StreamExt;
use futures::stream::FuturesUnordered;
use parking_lot::Mutex;
use tokio::task::JoinHandle;

use super::builds::BuildInfo;
use super::index::LayerIndex;
use crate::proto::LogLevel;

pub struct LayerUpload {
    pub index: Arc<LayerIndex>,
    pub hash: String,
    pub build_id: String,
    pub fields: BTreeMap<String, String>,
}

pub struct LayerUploads {
    info: Arc<BuildInfo>,
    tasks: Mutex<Vec<JoinHandle<Result<(), String>>>>,
}

impl LayerUploads {
    pub fn new(info: Arc<BuildInfo>) -> Self {
        Self {
            info,
            tasks: Mutex::new(Vec::new()),
        }
    }

    pub fn start(&self, upload: impl Future<Output = Result<(), String>> + Send + 'static, layer: LayerUpload) {
        let info = self.info.clone();
        let task = tokio::spawn(async move {
            let landed = land(upload, &layer, &info).await;
            if let Err(e) = &landed {
                tracing::error!(layer_build_id = %layer.build_id, layer_hash = %layer.hash, error = %e, "template layer snapshot upload failed");
            }
            landed
        });
        self.tasks.lock().push(task);
    }

    pub async fn wait(&self) -> Result<(), String> {
        let tasks: FuturesUnordered<_> = std::mem::take(&mut *self.tasks.lock()).into_iter().collect();
        tasks
            .map(|joined| joined.map_err(|e| e.to_string()).and_then(|landed| landed))
            .fold(Ok(()), |first, landed| async move { first.and(landed) })
            .await
    }
}

async fn land(
    upload: impl Future<Output = Result<(), String>>,
    layer: &LayerUpload,
    info: &BuildInfo,
) -> Result<(), String> {
    upload.await.map_err(|e| format!("error uploading snapshot: {e}"))?;
    layer
        .index
        .save(&layer.hash, &layer.build_id)
        .await
        .map_err(|e| format!("error saving UUID to hash mapping: {e}"))?;
    info.log.push_with(
        LogLevel::Debug,
        format!("Saved: {}", layer.build_id),
        layer.fields.clone(),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::storage::backend::Storage;

    fn layer(storage: &Storage, hash: &str) -> LayerUpload {
        LayerUpload {
            index: Arc::new(LayerIndex::new(storage.clone(), storage.clone(), "scope")),
            hash: hash.to_owned(),
            build_id: format!("build-{hash}"),
            fields: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn landed_uploads_are_indexed_and_the_first_failure_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        let info = crate::builder::builds::BuildCache::default().register("b").unwrap();
        let uploads = LayerUploads::new(info.clone());

        uploads.start(async { Ok(()) }, layer(&storage, "a"));
        uploads.start(
            async {
                tokio::time::sleep(Duration::from_millis(20)).await;
                Err("storage down".to_owned())
            },
            layer(&storage, "b"),
        );
        let waited = uploads.wait().await;

        assert_eq!(waited, Err("error uploading snapshot: storage down".to_owned()));
        assert!(dir.path().join("scope/index/a").is_file());
        assert!(!dir.path().join("scope/index/b").exists());
        let lines: Vec<String> = info
            .log
            .query(&Default::default())
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        assert_eq!(lines, vec!["Saved: build-a"]);
        assert_eq!(uploads.wait().await, Ok(()));
    }
}
