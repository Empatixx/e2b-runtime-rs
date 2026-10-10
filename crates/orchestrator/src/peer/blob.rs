use std::path::Path;

use bytes::{Bytes, BytesMut};
use futures::stream::BoxStream;
use futures::{StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;

use super::availability::served_by_peer;
use super::handle::{PeerHandle, PeerReply};
use super::stream::{PeerChunks, status_text};
use crate::proto::{GetBuildBlobRequest, GetBuildFileExistsRequest};
use crate::storage::backend::Storage;

pub struct PeerBlob {
    pub handle: PeerHandle,
    pub base: Storage,
}

impl PeerBlob {
    pub fn base_path(&self) -> String {
        format!("{}/{}", self.handle.build_id, self.handle.name)
    }

    pub async fn bytes(&self) -> PeerReply<Bytes> {
        match self.chunks().await? {
            Some(chunks) => collect(chunks).await.map(Some).map_err(|e| self.stream_error(&e)),
            None => Ok(None),
        }
    }

    fn stream_error(&self, error: &str) -> String {
        format!(
            "failed to stream file {} from peer: {error}",
            crate::go_strconv::quote(&self.handle.name)
        )
    }

    pub async fn read(&self) -> Result<Bytes, String> {
        match self.bytes().await? {
            Some(bytes) => Ok(bytes),
            None => self.base.get_direct(&self.base_path()).await,
        }
    }

    pub async fn find(&self) -> Result<Option<Bytes>, String> {
        match self.bytes().await? {
            Some(bytes) => Ok(Some(bytes)),
            None => self.base.find_direct(&self.base_path()).await,
        }
    }

    pub async fn write_to(&self, destination: &Path) -> Result<(), String> {
        match self.chunks().await? {
            Some(chunks) => self.copy_to(chunks, destination).await,
            None => self.base.write_to_direct(&self.base_path(), destination).await,
        }
    }

    pub async fn exists(&self) -> Result<bool, String> {
        match self.peer_has_file().await? {
            Some(exists) => Ok(exists),
            None => self.base.exists_direct(&self.base_path()).await,
        }
    }

    async fn peer_has_file(&self) -> PeerReply<bool> {
        self.handle
            .try_peer("peer-blob-exists", || async {
                let request = GetBuildFileExistsRequest {
                    build_id: self.handle.build_id.clone(),
                    name: self.handle.name.clone(),
                };
                match self.handle.client.clone().get_build_file_exists(request).await {
                    Ok(response) => Ok(served_by_peer(
                        response.into_inner().availability.as_ref(),
                        &self.handle.uploaded,
                    )
                    .then_some(true)),
                    Err(status) => {
                        tracing::warn!(
                            build_id = %self.handle.build_id,
                            file_name = %self.handle.name,
                            error = %status_text(&status),
                            "failed to check build file exists from peer"
                        );
                        Ok(None)
                    }
                }
            })
            .await
    }

    async fn chunks(&self) -> PeerReply<BoxStream<'static, Result<Bytes, String>>> {
        self.handle
            .try_peer("peer-blob-write-to", || async {
                let request = GetBuildBlobRequest {
                    build_id: self.handle.build_id.clone(),
                    name: self.handle.name.clone(),
                };
                let opened = self.handle.client.clone().get_build_blob(request).await;
                match PeerChunks::open(opened, "blob", self.handle.uploaded.clone()).await {
                    Ok(chunks) => Ok(Some(chunks.into_stream())),
                    Err(error) => {
                        tracing::warn!(
                            build_id = %self.handle.build_id,
                            file_name = %self.handle.name,
                            error = %error,
                            "failed to open peer blob stream"
                        );
                        Ok(None)
                    }
                }
            })
            .await
    }

    async fn copy_to(
        &self,
        mut chunks: BoxStream<'static, Result<Bytes, String>>,
        destination: &Path,
    ) -> Result<(), String> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(destination)
            .await
            .map_err(|e| crate::go_os::path_error("open", destination, &e))?;
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| self.stream_error(&e))?;
            file.write_all(&chunk)
                .await
                .map_err(|e| crate::go_os::path_error("write", destination, &e))?;
        }
        file.flush()
            .await
            .map_err(|e| crate::go_os::path_error("write", destination, &e))
    }
}

async fn collect(chunks: BoxStream<'static, Result<Bytes, String>>) -> Result<Bytes, String> {
    chunks
        .try_fold(BytesMut::new(), |mut all, chunk| async move {
            all.extend_from_slice(&chunk);
            Ok(all)
        })
        .await
        .map(BytesMut::freeze)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tonic::Status;

    use super::*;
    use crate::peer::testing::{
        FakeChunks, blob_data, blob_data_then_use_storage, blob_not_available, flag, handle, local_storage, put,
    };
    use crate::proto::{GetBuildFileExistsResponse, PeerAvailability};

    async fn peer_blob(chunks: FakeChunks, base: Storage, uploaded: Arc<std::sync::atomic::AtomicBool>) -> PeerBlob {
        let address = chunks.serve().await;
        PeerBlob {
            handle: handle(address, "build-1", "snapfile", uploaded),
            base,
        }
    }

    async fn downloaded(blob: &PeerBlob) -> Result<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("snapfile");
        std::fs::write(&destination, b"").unwrap();
        blob.write_to(&destination).await?;
        Ok(std::fs::read_to_string(destination).unwrap())
    }

    #[tokio::test]
    async fn write_to_streams_the_peer_blob_when_the_peer_has_it() {
        let chunks = FakeChunks {
            blob: Arc::new(|request| {
                assert_eq!(
                    (request.build_id.as_str(), request.name.as_str()),
                    ("build-1", "snapfile")
                );
                Ok(vec![blob_data("hello "), blob_data("world")])
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();

        let blob = peer_blob(chunks, base, flag()).await;

        assert_eq!(downloaded(&blob).await.unwrap(), "hello world");
    }

    #[tokio::test]
    async fn write_to_falls_back_to_the_base_when_the_peer_is_not_available() {
        let chunks = FakeChunks {
            blob: Arc::new(|_| Ok(vec![blob_not_available()])),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        put(&base, "build-1/snapfile", b"from gcs").await;

        let blob = peer_blob(chunks, base, flag()).await;

        assert_eq!(downloaded(&blob).await.unwrap(), "from gcs");
    }

    #[tokio::test]
    async fn write_to_falls_back_to_the_base_when_the_peer_errors() {
        let chunks = FakeChunks {
            blob: Arc::new(|_| Err(Status::unavailable("connection refused"))),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        put(&base, "build-1/snapfile", b"from gcs").await;

        let blob = peer_blob(chunks, base, flag()).await;

        assert_eq!(downloaded(&blob).await.unwrap(), "from gcs");
    }

    #[tokio::test]
    async fn a_use_storage_signal_mid_stream_completes_from_the_peer_then_switches_to_the_base() {
        let opened = Arc::new(AtomicUsize::new(0));
        let counted = opened.clone();
        let chunks = FakeChunks {
            blob: Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(vec![
                    blob_data("aaa"),
                    blob_data_then_use_storage("bbb"),
                    blob_data("ccc"),
                ])
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        put(&base, "build-1/snapfile", b"from storage").await;
        let uploaded = flag();

        let blob = peer_blob(chunks, base, uploaded.clone()).await;

        assert_eq!(downloaded(&blob).await.unwrap(), "aaabbbccc");
        assert!(uploaded.load(Ordering::SeqCst));
        assert_eq!(downloaded(&blob).await.unwrap(), "from storage");
        assert_eq!(opened.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exists_is_true_when_the_peer_has_the_file() {
        let chunks = FakeChunks {
            exists: Arc::new(|request| {
                assert_eq!(
                    (request.build_id.as_str(), request.name.as_str()),
                    ("build-1", "snapfile")
                );
                Ok(GetBuildFileExistsResponse::default())
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();

        let blob = peer_blob(chunks, base, flag()).await;

        assert!(blob.exists().await.unwrap());
    }

    #[tokio::test]
    async fn exists_falls_back_to_the_base_when_the_peer_is_not_available() {
        let chunks = FakeChunks {
            exists: Arc::new(|_| {
                Ok(GetBuildFileExistsResponse {
                    availability: Some(PeerAvailability {
                        not_available: true,
                        use_storage: false,
                    }),
                })
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        put(&base, "build-1/snapfile", b"present").await;

        let blob = peer_blob(chunks, base, flag()).await;

        assert!(blob.exists().await.unwrap());
    }

    #[tokio::test]
    async fn exists_falls_back_to_the_base_and_marks_the_build_uploaded_on_use_storage() {
        let chunks = FakeChunks {
            exists: Arc::new(|_| {
                Ok(GetBuildFileExistsResponse {
                    availability: Some(PeerAvailability {
                        not_available: false,
                        use_storage: true,
                    }),
                })
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        put(&base, "build-1/snapfile", b"present").await;
        let uploaded = flag();

        let blob = peer_blob(chunks, base, uploaded.clone()).await;

        assert!(blob.exists().await.unwrap());
        assert!(uploaded.load(Ordering::SeqCst));
    }
}
