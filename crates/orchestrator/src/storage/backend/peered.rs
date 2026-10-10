use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use bytes::{Bytes, BytesMut};
use futures::TryStreamExt;
use futures::stream::BoxStream;
use tracing::Instrument;

use super::Storage;
use crate::peer::blob::PeerBlob;
use crate::peer::handle::PeerHandle;
use crate::peer::resolver::{PeerRoute, Resolution};
use crate::peer::seekable::{PeerSeekable, SeekableError};
use crate::peer::switch::PeerSwitch;

const BLOB_FILES: [&str; 4] = ["snapfile", "metadata.json", "memfile.header", "rootfs.ext4.header"];
const SEEKABLE_FILES: [&str; 2] = ["memfile", "rootfs.ext4"];

impl Storage {
    pub fn with_peers(self, peers: Arc<PeerSwitch>) -> Self {
        Self {
            peers: Some(peers),
            ..self
        }
    }

    pub fn peers(&self) -> Option<&Arc<PeerSwitch>> {
        self.peers.as_ref()
    }

    pub fn for_template(&self) -> Self {
        Self {
            peer_routed: self.peers.as_ref().is_some_and(|peers| peers.enabled()),
            ..self.clone()
        }
    }

    pub async fn get(&self, path: &str) -> Result<Bytes, String> {
        match self.peer_blob(path).await {
            Some(blob) => blob.read().await,
            None => self.get_direct(path).await,
        }
    }

    pub async fn find(&self, path: &str) -> Result<Option<Bytes>, String> {
        match self.peer_blob(path).await {
            Some(blob) => blob.find().await,
            None => self.find_direct(path).await,
        }
    }

    pub async fn write_to(&self, path: &str, destination: &Path) -> Result<(), String> {
        match self.peer_blob(path).await {
            Some(blob) => blob.write_to(destination).await,
            None => self.write_to_direct(path, destination).await,
        }
    }

    pub async fn exists(&self, path: &str) -> Result<bool, String> {
        match self.peer_blob(path).await {
            Some(blob) => blob.exists().await,
            None => self.exists_direct(path).await,
        }
    }

    pub async fn size(&self, path: &str) -> Result<u64, String> {
        match self.peer_seekable(path).await {
            Some(seekable) => match seekable.size().await {
                Ok(size) => Ok(size),
                Err(SeekableError::Transitioned) => self.size_direct(path).await,
                Err(SeekableError::Failed(e)) => Err(e),
            },
            None => self.size_direct(path).await,
        }
    }

    pub async fn get_stream(
        &self,
        path: &str,
        range: Range<u64>,
    ) -> Result<BoxStream<'static, Result<Bytes, String>>, String> {
        match self.peer_seekable(path).await {
            Some(seekable) => match seekable.open_range_reader(range.clone()).await {
                Ok(chunks) => Ok(chunks),
                Err(SeekableError::Transitioned) => self.get_stream_direct(path, range).await,
                Err(SeekableError::Failed(e)) => Err(e),
            },
            None => self.get_stream_direct(path, range).await,
        }
    }

    pub async fn get_range(&self, path: &str, range: Range<u64>) -> Result<Bytes, String> {
        match self.peer_seekable(path).await {
            Some(_) => self.get_range_through_peer(path, range).await,
            None => self.get_range_direct(path, range).await,
        }
    }

    async fn get_range_through_peer(&self, path: &str, range: Range<u64>) -> Result<Bytes, String> {
        let expected = range.end - range.start;
        let bytes = self
            .get_stream(path, range.clone())
            .await?
            .try_fold(BytesMut::new(), |mut all, chunk| async move {
                all.extend_from_slice(&chunk);
                Ok(all)
            })
            .await
            .map(BytesMut::freeze)?;
        match bytes.len() as u64 == expected {
            true => Ok(bytes),
            false => Err(format!("get {path} {range:?}: got {} of {expected} bytes", bytes.len())),
        }
    }

    async fn peer_blob(&self, path: &str) -> Option<PeerBlob> {
        let (route, build_id, name) = self.peer_route(path, &BLOB_FILES).await?;
        Some(PeerBlob {
            handle: handle(route, build_id, name),
            base: self.clone(),
        })
    }

    async fn peer_seekable(&self, path: &str) -> Option<PeerSeekable> {
        let (route, build_id, name) = self.peer_route(path, &SEEKABLE_FILES).await?;
        Some(PeerSeekable {
            handle: handle(route, build_id, name),
            base: self.clone(),
        })
    }

    async fn peer_route<'a>(&self, path: &'a str, files: &[&str]) -> Option<(PeerRoute, &'a str, &'a str)> {
        let peers = self.peers.as_ref().filter(|_| self.peer_routed)?;
        let (build_id, name) = path.split_once('/').filter(|(_, name)| files.contains(name))?;
        let span = tracing::info_span!("resolve peer-provider", build_id, peer_resolve = tracing::field::Empty);
        let resolution = peers.resolve(build_id).instrument(span.clone()).await;
        match resolution {
            Resolution::Peer(route) => {
                span.record("peer_resolve", "peer");
                Some((*route, build_id, name))
            }
            Resolution::Base(reason) => {
                span.record("peer_resolve", reason.label());
                None
            }
        }
    }
}

fn handle(route: PeerRoute, build_id: &str, name: &str) -> PeerHandle {
    PeerHandle::builder()
        .client(route.client)
        .build_id(build_id)
        .name(name)
        .uploaded(route.uploaded)
        .build()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::featureflags::FeatureFlags;
    use crate::peer::availability::not_available;
    use crate::peer::testing::{FakeChunks, FixedRegistry, blob_data, local_storage, put, seekable_data};
    use crate::proto::{GetBuildFileExistsResponse, GetBuildFileSizeResponse};

    async fn routed(chunks: FakeChunks, flag_on: bool) -> (tempfile::TempDir, Storage) {
        let address = chunks.serve().await;
        let flags = FeatureFlags::builder()
            .override_flag("peer-to-peer-chunk-transfer", flag_on)
            .offline();
        let peers = PeerSwitch::new(flags);
        peers.install(
            Arc::new(FixedRegistry {
                lookup: Ok(Some(address.to_string())),
            }),
            "10.0.0.1:5008".to_owned(),
        );
        let (dir, storage) = local_storage();
        (dir, storage.with_peers(peers).for_template())
    }

    fn size_of(total_size: i64) -> FakeChunks {
        FakeChunks {
            size: Arc::new(move |request| {
                assert_eq!(
                    (request.build_id.as_str(), request.name.as_str()),
                    ("build-1", "memfile")
                );
                Ok(GetBuildFileSizeResponse {
                    total_size,
                    availability: None,
                })
            }),
            ..FakeChunks::default()
        }
    }

    #[tokio::test]
    async fn a_blob_path_is_split_into_build_id_and_file_name_for_the_peer() {
        let chunks = FakeChunks {
            blob: Arc::new(|request| {
                assert_eq!(
                    (request.build_id.as_str(), request.name.as_str()),
                    ("build-1", "snapfile")
                );
                Ok(vec![blob_data("data")])
            }),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;

        assert_eq!(storage.get("build-1/snapfile").await.unwrap(), "data");
    }

    #[tokio::test]
    async fn a_seekable_path_is_split_into_build_id_and_file_name_for_the_peer() {
        let (_dir, storage) = routed(size_of(512), true).await;

        assert_eq!(storage.size("build-1/memfile").await.unwrap(), 512);
    }

    #[tokio::test]
    async fn find_returns_the_peer_blob() {
        let chunks = FakeChunks {
            blob: Arc::new(|_| Ok(vec![blob_data("meta")])),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;

        assert_eq!(storage.find("build-1/metadata.json").await.unwrap().unwrap(), "meta");
    }

    #[tokio::test]
    async fn without_the_flag_every_read_goes_to_the_base() {
        let chunks = FakeChunks {
            blob: Arc::new(|_| panic!("the peer must not be asked")),
            size: Arc::new(|_| panic!("the peer must not be asked")),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, false).await;
        put(&storage, "build-1/snapfile", b"base").await;
        put(&storage, "build-1/memfile", b"base memfile").await;

        assert_eq!(storage.get("build-1/snapfile").await.unwrap(), "base");
        assert_eq!(storage.size("build-1/memfile").await.unwrap(), 12);
    }

    #[tokio::test]
    async fn a_storage_that_was_not_made_for_a_template_does_not_route() {
        let chunks = FakeChunks {
            blob: Arc::new(|_| panic!("the peer must not be asked")),
            ..FakeChunks::default()
        };
        let (_dir, routed_storage) = routed(chunks, true).await;
        let plain = Storage {
            peer_routed: false,
            ..routed_storage
        };
        put(&plain, "build-1/snapfile", b"base").await;

        assert_eq!(plain.get("build-1/snapfile").await.unwrap(), "base");
    }

    #[tokio::test]
    async fn files_the_peer_does_not_serve_never_ask_the_peer() {
        let chunks = FakeChunks {
            blob: Arc::new(|_| panic!("the peer must not be asked")),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;
        put(&storage, "build-1/layer.index", b"index").await;

        assert_eq!(storage.get("build-1/layer.index").await.unwrap(), "index");
        assert!(storage.get("no-slash").await.unwrap_err().contains("no-slash"));
    }

    #[tokio::test]
    async fn a_size_miss_on_the_peer_is_answered_by_the_base() {
        let chunks = FakeChunks {
            size: Arc::new(|_| {
                Ok(GetBuildFileSizeResponse {
                    total_size: 0,
                    availability: Some(not_available()),
                })
            }),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;
        put(&storage, "build-1/memfile", b"base memfile").await;

        assert_eq!(storage.size("build-1/memfile").await.unwrap(), 12);
    }

    #[tokio::test]
    async fn a_range_is_streamed_from_the_peer_and_checked_for_length() {
        let chunks = FakeChunks {
            seekable: Arc::new(|request| {
                assert_eq!((request.offset, request.length), (4, 6));
                Ok(vec![seekable_data(b"abc"), seekable_data(b"def")])
            }),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;

        assert_eq!(storage.get_range("build-1/memfile", 4..10).await.unwrap(), "abcdef");
    }

    #[tokio::test]
    async fn a_short_peer_range_is_an_error() {
        let chunks = FakeChunks {
            seekable: Arc::new(|_| Ok(vec![seekable_data(b"abc")])),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;

        assert_eq!(
            storage.get_range("build-1/memfile", 4..10).await.unwrap_err(),
            "get build-1/memfile 4..10: got 3 of 6 bytes"
        );
    }

    #[tokio::test]
    async fn the_range_that_sees_use_storage_is_the_last_one_served_by_the_peer() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counted = asked.clone();
        let chunks = FakeChunks {
            seekable: Arc::new(move |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(vec![
                    seekable_data(b"abc"),
                    crate::peer::testing::seekable_use_storage(),
                ])
            }),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;
        put(&storage, "build-1/memfile", b"XYZdef").await;

        assert_eq!(storage.get_range("build-1/memfile", 0..3).await.unwrap(), "abc");
        assert_eq!(storage.get_range("build-1/memfile", 0..3).await.unwrap(), "XYZ");
        assert_eq!(asked.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn exists_asks_the_peer_for_blobs() {
        let chunks = FakeChunks {
            exists: Arc::new(|_| Ok(GetBuildFileExistsResponse::default())),
            ..FakeChunks::default()
        };
        let (_dir, storage) = routed(chunks, true).await;

        assert!(storage.exists("build-1/snapfile").await.unwrap());
    }
}
