use std::sync::Arc;

use uuid::Uuid;

use super::blob::{BlobSource, HeaderFile};
use super::seekable::SeekableSource;
use super::source::SourceError;
use crate::block::build_file::{Artifact, DiffStore};
use crate::storage::header::PAGE_SIZE;
use crate::template::cache::TemplateCache;

const MEMFILE: &str = "memfile";
const ROOTFS: &str = "rootfs.ext4";
const SNAPFILE: &str = "snapfile";
const METADATA: &str = "metadata.json";
const MEMFILE_HEADER: &str = "memfile.header";
const ROOTFS_HEADER: &str = "rootfs.ext4.header";

#[derive(Clone)]
pub struct PeerCache {
    pub templates: Arc<TemplateCache>,
    pub diffs: Arc<DiffStore>,
}

impl PeerCache {
    pub async fn resolve_seekable(&self, build_id: &str, file_name: &str) -> Result<SeekableSource, SourceError> {
        let artifact = match file_name {
            MEMFILE => Artifact::Memfile,
            ROOTFS => Artifact::Rootfs,
            unknown => return Err(SourceError::UnknownFile(unknown.to_owned())),
        };
        let build_id = Uuid::parse_str(build_id).map_err(|_| SourceError::NotAvailable)?;
        self.diffs
            .cached(build_id, artifact)
            .await
            .map(|chunker| SeekableSource {
                chunker,
                block_size: self.block_size(build_id, artifact),
            })
            .ok_or(SourceError::NotAvailable)
    }

    fn block_size(&self, build_id: Uuid, artifact: Artifact) -> usize {
        self.templates
            .lookup(build_id)
            .and_then(|template| template.loaded_build())
            .map(|build| match artifact {
                Artifact::Memfile => build.memfile.metadata.block_size,
                Artifact::Rootfs => build.rootfs.metadata.block_size,
            })
            .unwrap_or(PAGE_SIZE)
            .max(1) as usize
    }

    pub fn resolve_blob(&self, build_id: &str, file_name: &str) -> Result<BlobSource, SourceError> {
        let template = Uuid::parse_str(build_id)
            .ok()
            .and_then(|build_id| self.templates.lookup(build_id))
            .ok_or(SourceError::NotAvailable)?;
        match file_name {
            SNAPFILE => Ok(BlobSource::Snapfile(template)),
            METADATA => Ok(BlobSource::Metadata(template)),
            MEMFILE_HEADER => Ok(header(template, HeaderFile::Memfile)),
            ROOTFS_HEADER => Ok(header(template, HeaderFile::Rootfs)),
            unknown => Err(SourceError::UnknownFile(unknown.to_owned())),
        }
    }
}

fn header(template: Arc<crate::template::Template>, file: HeaderFile) -> BlobSource {
    BlobSource::Header { template, file }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::testing::{cache_fixture, header};

    const BLOB_FILES: [&str; 4] = [SNAPFILE, METADATA, MEMFILE_HEADER, ROOTFS_HEADER];

    #[tokio::test]
    async fn resolve_seekable_is_not_available_when_the_build_is_not_in_the_cache() {
        let fixture = cache_fixture();

        for file_name in [MEMFILE, ROOTFS] {
            let error = fixture
                .cache
                .resolve_seekable(&Uuid::new_v4().to_string(), file_name)
                .await;
            assert_eq!(error.err(), Some(SourceError::NotAvailable), "{file_name}");
        }
    }

    #[tokio::test]
    async fn resolve_seekable_rejects_an_unknown_file() {
        let fixture = cache_fixture();

        let error = fixture.cache.resolve_seekable("build-1", "unknown.file").await;

        assert_eq!(error.err(), Some(SourceError::UnknownFile("unknown.file".to_owned())));
    }

    #[tokio::test]
    async fn resolve_seekable_treats_a_build_id_that_is_not_a_uuid_as_not_cached() {
        let fixture = cache_fixture();

        let error = fixture.cache.resolve_seekable("build-1", MEMFILE).await;

        assert_eq!(error.err(), Some(SourceError::NotAvailable));
    }

    #[test]
    fn resolve_blob_is_not_available_when_the_build_is_not_in_the_cache() {
        let fixture = cache_fixture();

        for file_name in BLOB_FILES {
            let error = fixture.cache.resolve_blob(&Uuid::new_v4().to_string(), file_name);
            assert_eq!(error.err(), Some(SourceError::NotAvailable), "{file_name}");
        }
    }

    #[test]
    fn resolve_blob_rejects_an_unknown_file_of_a_cached_build() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("snapfile"));

        let error = fixture.cache.resolve_blob(&build_id.to_string(), "unknown.file");

        assert_eq!(error.err(), Some(SourceError::UnknownFile("unknown.file".to_owned())));
    }

    #[test]
    fn a_resolved_blob_holds_the_template_until_it_is_dropped() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        let template = fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("snapfile"));
        drop(template);

        for file_name in BLOB_FILES {
            let source = fixture.cache.resolve_blob(&build_id.to_string(), file_name).unwrap();
            let held = fixture
                .cache
                .templates
                .lookup(build_id)
                .map(|template| Arc::strong_count(&template));
            assert_eq!(
                held,
                Some(3),
                "{file_name}: the cache, the source and this lookup hold it"
            );

            drop(source);
            let released = fixture
                .cache
                .templates
                .lookup(build_id)
                .map(|template| Arc::strong_count(&template));
            assert_eq!(released, Some(2), "{file_name}: only the cache and this lookup hold it");
        }
    }

    #[test]
    fn an_unknown_file_gives_the_template_back_before_resolve_blob_returns() {
        let fixture = cache_fixture();
        let build_id = Uuid::new_v4();
        fixture.add_template(build_id, header(build_id, 4096), &fixture.dir.path().join("snapfile"));

        let error = fixture.cache.resolve_blob(&build_id.to_string(), "unknown.file");

        assert!(matches!(error, Err(SourceError::UnknownFile(_))));
        let held = fixture
            .cache
            .templates
            .lookup(build_id)
            .map(|template| Arc::strong_count(&template));
        assert_eq!(held, Some(2));
    }
}
