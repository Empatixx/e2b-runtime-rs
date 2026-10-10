use std::fmt;
use std::ops::Range;

use bytes::Bytes;
use futures::stream::BoxStream;

use super::availability::served_by_peer;
use super::handle::{PeerHandle, PeerReply};
use super::stream::{PeerChunks, status_text};
use crate::proto::{GetBuildFileSizeRequest, ReadAtBuildSeekableRequest};
use crate::storage::backend::Storage;

const TRANSITIONED: &str = "peer upload completed, reload header from storage";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeekableError {
    Transitioned,
    Failed(String),
}

impl fmt::Display for SeekableError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SeekableError::Transitioned => f.write_str(TRANSITIONED),
            SeekableError::Failed(message) => f.write_str(message),
        }
    }
}

pub struct PeerSeekable {
    pub handle: PeerHandle,
    pub base: Storage,
}

impl PeerSeekable {
    pub fn base_path(&self) -> String {
        format!("{}/{}", self.handle.build_id, self.handle.name)
    }

    pub async fn size(&self) -> Result<u64, SeekableError> {
        let size = self
            .handle
            .try_peer("size peer-seekable", || async {
                let request = GetBuildFileSizeRequest {
                    build_id: self.handle.build_id.clone(),
                    name: self.handle.name.clone(),
                };
                match self.handle.client.clone().get_build_file_size(request).await {
                    Ok(response) => {
                        let response = response.into_inner();
                        Ok(served_by_peer(response.availability.as_ref(), &self.handle.uploaded)
                            .then_some(response.total_size.max(0) as u64))
                    }
                    Err(status) => {
                        tracing::warn!(
                            build_id = %self.handle.build_id,
                            error = %status_text(&status),
                            "failed to get build file size from peer"
                        );
                        Ok(None)
                    }
                }
            })
            .await
            .map_err(SeekableError::Failed)?;
        size.ok_or(SeekableError::Transitioned)
    }

    pub async fn open_range_reader(
        &self,
        range: Range<u64>,
    ) -> Result<BoxStream<'static, Result<Bytes, String>>, SeekableError> {
        match self.peer_range(&range).await.map_err(SeekableError::Failed)? {
            Some(chunks) => Ok(chunks),
            None if self.handle.switched_to_storage() => Err(SeekableError::Transitioned),
            None => self
                .base
                .get_stream_direct(&self.base_path(), range)
                .await
                .map_err(SeekableError::Failed),
        }
    }

    async fn peer_range(&self, range: &Range<u64>) -> PeerReply<BoxStream<'static, Result<Bytes, String>>> {
        self.handle
            .try_peer("peer-seekable-open-range-reader", || async {
                let request = ReadAtBuildSeekableRequest {
                    build_id: self.handle.build_id.clone(),
                    name: self.handle.name.clone(),
                    offset: range.start as i64,
                    length: (range.end - range.start) as i64,
                };
                let opened = self.handle.client.clone().read_at_build_seekable(request).await;
                match PeerChunks::open(opened, "seekable", self.handle.uploaded.clone()).await {
                    Ok(chunks) => Ok(Some(chunks.into_stream())),
                    Err(error) => {
                        tracing::warn!(
                            build_id = %self.handle.build_id,
                            offset = range.start,
                            length = range.end - range.start,
                            error = %error,
                            "failed to open range reader from peer"
                        );
                        Ok(None)
                    }
                }
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use futures::TryStreamExt;
    use tonic::Status;

    use super::*;
    use crate::peer::availability::not_available;
    use crate::peer::testing::{FakeChunks, flag, handle, local_storage, put, seekable_data, seekable_use_storage};
    use crate::proto::GetBuildFileSizeResponse;

    async fn peer_seekable(chunks: FakeChunks, base: Storage, uploaded: Arc<AtomicBool>) -> PeerSeekable {
        let address = chunks.serve().await;
        PeerSeekable {
            handle: handle(address, "build-1", "memfile", uploaded),
            base,
        }
    }

    async fn read(seekable: &PeerSeekable, range: Range<u64>) -> Result<Vec<u8>, SeekableError> {
        let chunks = seekable.open_range_reader(range).await?;
        chunks
            .try_fold(Vec::new(), |mut all, chunk| async move {
                all.extend_from_slice(&chunk);
                Ok(all)
            })
            .await
            .map_err(SeekableError::Failed)
    }

    #[tokio::test]
    async fn size_comes_from_the_peer() {
        let chunks = FakeChunks {
            size: Arc::new(|request| {
                assert_eq!(
                    (request.build_id.as_str(), request.name.as_str()),
                    ("build-1", "memfile")
                );
                Ok(GetBuildFileSizeResponse {
                    total_size: 4096,
                    availability: None,
                })
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();

        let seekable = peer_seekable(chunks, base, flag()).await;

        assert_eq!(seekable.size().await, Ok(4096));
    }

    #[tokio::test]
    async fn size_on_a_peer_miss_asks_the_caller_to_reload_from_storage() {
        let chunks = FakeChunks {
            size: Arc::new(|_| {
                Ok(GetBuildFileSizeResponse {
                    total_size: 0,
                    availability: Some(not_available()),
                })
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();

        let seekable = peer_seekable(chunks, base, flag()).await;

        assert_eq!(seekable.size().await, Err(SeekableError::Transitioned));
    }

    #[tokio::test]
    async fn a_transitioned_error_reads_like_upstream() {
        assert_eq!(
            SeekableError::Transitioned.to_string(),
            "peer upload completed, reload header from storage"
        );
    }

    #[tokio::test]
    async fn open_range_reader_streams_the_peer_range() {
        let data = b"range data";
        let chunks = FakeChunks {
            seekable: Arc::new(move |request| {
                assert_eq!((request.offset, request.length), (10, data.len() as i64));
                Ok(vec![seekable_data(data)])
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();

        let seekable = peer_seekable(chunks, base, flag()).await;

        assert_eq!(read(&seekable, 10..20).await.unwrap(), data);
    }

    #[tokio::test]
    async fn open_range_reader_falls_back_to_the_base_when_the_peer_errors() {
        let chunks = FakeChunks {
            seekable: Arc::new(|_| Err(Status::unavailable("peer unavailable"))),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        put(&base, "build-1/memfile", b"base range").await;

        let seekable = peer_seekable(chunks, base, flag()).await;

        assert_eq!(read(&seekable, 0..10).await.unwrap(), b"base range");
    }

    #[tokio::test]
    async fn open_range_reader_after_the_upload_asks_the_caller_to_reload_without_touching_peer_or_base() {
        let chunks = FakeChunks {
            seekable: Arc::new(|_| panic!("the peer must not be asked")),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        let uploaded = flag();
        uploaded.store(true, Ordering::SeqCst);

        let seekable = peer_seekable(chunks, base, uploaded).await;

        assert_eq!(read(&seekable, 0..100).await.unwrap_err(), SeekableError::Transitioned);
    }

    #[tokio::test]
    async fn the_read_that_sees_use_storage_finishes_and_the_next_one_asks_for_a_reload() {
        let chunks = FakeChunks {
            seekable: Arc::new(|request| {
                assert_eq!(
                    (request.build_id.as_str(), request.name.as_str()),
                    ("build-1", "memfile")
                );
                Ok(vec![
                    seekable_data(b"pre-transition peer payload"),
                    seekable_use_storage(),
                ])
            }),
            ..FakeChunks::default()
        };
        let (_dir, base) = local_storage();
        let uploaded = flag();

        let seekable = peer_seekable(chunks, base, uploaded.clone()).await;

        assert_eq!(read(&seekable, 0..27).await.unwrap(), b"pre-transition peer payload");
        assert!(uploaded.load(Ordering::SeqCst));
        assert_eq!(read(&seekable, 0..1).await.unwrap_err(), SeekableError::Transitioned);
    }
}
