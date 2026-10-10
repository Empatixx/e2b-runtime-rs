use std::path::Path;
use std::sync::Arc;

use bytes::Bytes;
use futures::TryStreamExt;
use tokio_util::io::ReaderStream;

use super::chunk::{SEND_CHUNK_SIZE, chunked, rechunked};
use super::source::{Chunks, SourceError};
use crate::template::Template;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderFile {
    Memfile,
    Rootfs,
}

pub enum BlobSource {
    Snapfile(Arc<Template>),
    Metadata(Arc<Template>),
    Header { template: Arc<Template>, file: HeaderFile },
}

impl BlobSource {
    pub async fn exists(&self) -> Result<bool, SourceError> {
        match self {
            BlobSource::Snapfile(template) => snapfile_exists(template).await,
            BlobSource::Metadata(_) | BlobSource::Header { .. } => Err(SourceError::NotSupported),
        }
    }

    pub async fn stream(self) -> Result<Chunks, SourceError> {
        match self {
            BlobSource::Snapfile(template) => stream_snapfile(&template).await,
            BlobSource::Metadata(template) => stream_metadata(&template).await,
            BlobSource::Header { template, file } => stream_header(&template, file).await,
        }
    }
}

async fn snapfile_exists(template: &Template) -> Result<bool, SourceError> {
    let path = template
        .snapfile()
        .await
        .map_err(|e| SourceError::Failed(e.to_string()))?;
    match tokio::fs::metadata(&path).await {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(SourceError::Failed(crate::go_os::path_error("stat", &path, &e))),
    }
}

async fn stream_snapfile(template: &Template) -> Result<Chunks, SourceError> {
    let path = template
        .snapfile()
        .await
        .map_err(|e| SourceError::Failed(format!("get file: {e}")))?;
    let file = tokio::fs::File::open(&path).await.map_err(|e| open_error(&path, &e))?;
    let pieces = ReaderStream::with_capacity(file, SEND_CHUNK_SIZE)
        .map_err(|e| SourceError::Failed(format!("stream file: {e}")));
    Ok(rechunked(pieces))
}

fn open_error(path: &Path, error: &std::io::Error) -> SourceError {
    match error.kind() {
        std::io::ErrorKind::NotFound => SourceError::NotAvailable,
        _ => SourceError::Failed(format!(
            "open file {}: {}",
            crate::go_strconv::quote(&path.display().to_string()),
            crate::go_os::path_error("open", path, error)
        )),
    }
}

async fn stream_metadata(template: &Template) -> Result<Chunks, SourceError> {
    let metadata = template
        .metadata()
        .await
        .map_err(|e| SourceError::Failed(format!("get metadata: {e}")))?;
    Ok(chunked(Bytes::from(metadata.to_json())))
}

async fn stream_header(template: &Template, file: HeaderFile) -> Result<Chunks, SourceError> {
    let build = template
        .build()
        .await
        .map_err(|e| SourceError::Failed(format!("get device: {e}")))?;
    let header = match file {
        HeaderFile::Memfile => build.memfile,
        HeaderFile::Rootfs => build.rootfs,
    };
    if header.metadata.size == 0 {
        return Err(SourceError::NotAvailable);
    }
    let serialized = header
        .serialize()
        .map_err(|e| SourceError::Failed(format!("serialize header: {e}")))?;
    Ok(chunked(Bytes::from(serialized)))
}

#[cfg(test)]
mod tests {
    use futures::TryStreamExt;
    use uuid::Uuid;

    use super::*;
    use crate::peer::server::chunk::SEND_CHUNK_SIZE;
    use crate::peer::testing::{CacheFixture, cache_fixture, header};
    use crate::storage::header::{BuildMapping, Header, Metadata};

    fn source(fixture: &CacheFixture, build_id: Uuid, file_name: &str) -> BlobSource {
        fixture.cache.resolve_blob(&build_id.to_string(), file_name).unwrap()
    }

    async fn collected(chunks: Chunks) -> Vec<Bytes> {
        chunks.try_collect().await.unwrap()
    }

    fn joined(chunks: &[Bytes]) -> Vec<u8> {
        chunks.iter().flat_map(|chunk| chunk.to_vec()).collect()
    }

    fn with_snapfile(content: &[u8]) -> (CacheFixture, Uuid) {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let snapfile = fixture.write_file("snapfile", content);
        fixture.add_template(build_id, header(build_id, 4096), &snapfile);
        (fixture, build_id)
    }

    fn without_snapfile() -> (CacheFixture, Uuid) {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let missing = fixture.dir.path().join("missing");
        fixture.add_template(build_id, header(build_id, 4096), &missing);
        (fixture, build_id)
    }

    #[tokio::test]
    async fn the_snapfile_exists_when_it_is_on_disk() {
        let (fixture, build_id) = with_snapfile(b"blob content");

        assert_eq!(source(&fixture, build_id, "snapfile").exists().await, Ok(true));
    }

    #[tokio::test]
    async fn the_snapfile_does_not_exist_when_it_is_not_on_disk() {
        let (fixture, build_id) = without_snapfile();

        assert_eq!(source(&fixture, build_id, "snapfile").exists().await, Ok(false));
    }

    #[tokio::test]
    async fn the_snapfile_streams_its_content() {
        let (fixture, build_id) = with_snapfile(b"snapfile content");

        let chunks = source(&fixture, build_id, "snapfile").stream().await.unwrap();

        assert_eq!(joined(&collected(chunks).await), b"snapfile content");
    }

    #[tokio::test]
    async fn a_snapfile_larger_than_the_bound_reaches_the_peer_as_several_bounded_messages() {
        let content = vec![b's'; 2 * SEND_CHUNK_SIZE + 7];
        let (fixture, build_id) = with_snapfile(&content);

        let chunks = collected(source(&fixture, build_id, "snapfile").stream().await.unwrap()).await;

        let sizes: Vec<usize> = chunks.iter().map(Bytes::len).collect();
        assert_eq!(sizes, vec![SEND_CHUNK_SIZE, SEND_CHUNK_SIZE, 7]);
        assert_eq!(joined(&chunks), content);
    }

    #[tokio::test]
    async fn a_snapfile_that_is_not_on_disk_is_not_available() {
        let (fixture, build_id) = without_snapfile();

        let error = source(&fixture, build_id, "snapfile").stream().await;

        assert_eq!(error.err(), Some(SourceError::NotAvailable));
    }

    #[tokio::test]
    async fn metadata_and_headers_do_not_support_exists() {
        let (fixture, build_id) = with_snapfile(b"x");

        for file_name in ["metadata.json", "memfile.header", "rootfs.ext4.header"] {
            assert_eq!(
                source(&fixture, build_id, file_name).exists().await,
                Err(SourceError::NotSupported),
                "{file_name}"
            );
        }
    }

    #[tokio::test]
    async fn the_metadata_streams_as_json() {
        let (fixture, build_id) = with_snapfile(b"x");

        let chunks = source(&fixture, build_id, "metadata.json").stream().await.unwrap();

        let json = String::from_utf8(joined(&collected(chunks).await)).unwrap();
        assert!(json.contains(&build_id.to_string()));
    }

    #[tokio::test]
    async fn a_prefetch_mapping_larger_than_the_bound_reaches_the_peer_as_bounded_messages() {
        let (fixture, build_id) = with_snapfile(b"x");
        let entries = SEND_CHUNK_SIZE / 2;
        let mut metadata = fixture
            .cache
            .templates
            .lookup(build_id)
            .unwrap()
            .metadata()
            .await
            .unwrap();
        metadata.prefetch = Some(crate::template::metadata::Prefetch {
            memory: Some(crate::template::metadata::MemoryPrefetch {
                indices: (0..entries as u64).collect(),
                access_types: vec!["read".to_owned(); entries],
                block_size: 4096,
            }),
            ..crate::template::metadata::Prefetch::default()
        });
        fixture
            .cache
            .templates
            .lookup(build_id)
            .unwrap()
            .update_metadata(metadata);

        let chunks = collected(source(&fixture, build_id, "metadata.json").stream().await.unwrap()).await;

        assert!(
            joined(&chunks).len() > SEND_CHUNK_SIZE,
            "fixture is too small to exercise chunking"
        );
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= SEND_CHUNK_SIZE));
        let parsed = crate::template::metadata::TemplateMetadata::from_json(&joined(&chunks)).unwrap();
        assert_eq!(parsed.template.build_id, build_id.to_string());
        assert_eq!(parsed.prefetch.unwrap().memory.unwrap().indices.len(), entries);
    }

    #[tokio::test]
    async fn the_memfile_header_streams_serialized() {
        let (fixture, build_id) = with_snapfile(b"x");

        let chunks = source(&fixture, build_id, "memfile.header").stream().await.unwrap();

        let decoded = Header::decode(&joined(&collected(chunks).await)).unwrap();
        assert_eq!(decoded, header(build_id, 4096));
    }

    #[tokio::test]
    async fn the_rootfs_header_streams_serialized() {
        let (fixture, build_id) = with_snapfile(b"x");

        let chunks = source(&fixture, build_id, "rootfs.ext4.header").stream().await.unwrap();

        let decoded = Header::decode(&joined(&collected(chunks).await)).unwrap();
        assert_eq!(decoded, header(build_id, 4096));
    }

    #[tokio::test]
    async fn a_header_without_content_is_not_available() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let snapfile = fixture.write_file("snapfile", b"x");
        fixture.add_template(build_id, header(build_id, 0), &snapfile);

        let error = source(&fixture, build_id, "memfile.header").stream().await;

        assert_eq!(error.err(), Some(SourceError::NotAvailable));
    }

    #[tokio::test]
    async fn a_header_larger_than_the_bound_reaches_the_peer_as_bounded_messages_that_reassemble() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let entries = 40_000u64;
        let mappings = (0..entries)
            .map(|index| BuildMapping {
                offset: index * 4096,
                length: 4096,
                build_id: Uuid::new_v4(),
                storage_offset: index * 4096,
            })
            .collect();
        let big = Header::new(Metadata::template(build_id, 4096, entries * 4096), mappings).unwrap();
        let snapfile = fixture.write_file("snapfile", b"x");
        fixture.add_template(build_id, big.clone(), &snapfile);

        let chunks = collected(source(&fixture, build_id, "memfile.header").stream().await.unwrap()).await;

        assert!(
            joined(&chunks).len() > SEND_CHUNK_SIZE,
            "fixture is too small to exercise chunking"
        );
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= SEND_CHUNK_SIZE));
        assert_eq!(Header::decode(&joined(&chunks)).unwrap(), big);
    }
}
