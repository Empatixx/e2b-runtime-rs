pub mod atomic_write;
pub mod cache;
pub mod metadata;
pub mod optimize;

use std::fmt;
use std::path::{Path, PathBuf};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::OnceCell;
use uuid::Uuid;

use self::metadata::TemplateMetadata;
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::storage::header::Header;
use crate::storage::paths::BuildPaths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateError {
    NotFound(String),
    Failed(String),
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TemplateError::NotFound(object) => write!(f, "{object} not found"),
            TemplateError::Failed(error) => f.write_str(error),
        }
    }
}

pub struct Template {
    build_id: Uuid,
    dir: PathBuf,
    storage: Storage,
    snapfile: OnceCell<Result<PathBuf, TemplateError>>,
    build: OnceCell<Result<Build, TemplateError>>,
    metadata: OnceCell<Result<TemplateMetadata, TemplateError>>,
    updated_metadata: Mutex<Option<TemplateMetadata>>,
}

impl fmt::Debug for Template {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Template").field("build_id", &self.build_id).finish()
    }
}

impl Template {
    pub fn new(storage: Storage, build_id: Uuid, dir: PathBuf) -> Self {
        Self {
            build_id,
            dir,
            storage,
            snapfile: OnceCell::new(),
            build: OnceCell::new(),
            metadata: OnceCell::new(),
            updated_metadata: Mutex::new(None),
        }
    }

    pub fn cached(storage: Storage, build: Build, snapfile: PathBuf, metadata: TemplateMetadata, dir: PathBuf) -> Self {
        Self {
            build_id: build.build_id,
            dir,
            storage,
            snapfile: OnceCell::new_with(Some(Ok(snapfile))),
            build: OnceCell::new_with(Some(Ok(build))),
            metadata: OnceCell::new_with(Some(Ok(metadata))),
            updated_metadata: Mutex::new(None),
        }
    }

    pub fn filesystem_layer(storage: Storage, build: Build, metadata: TemplateMetadata, dir: PathBuf) -> Self {
        let snapfile = BuildPaths::new(build.build_id).snapfile();
        Self {
            build_id: build.build_id,
            dir,
            storage,
            snapfile: OnceCell::new_with(Some(Err(TemplateError::NotFound(snapfile)))),
            build: OnceCell::new_with(Some(Ok(build))),
            metadata: OnceCell::new_with(Some(Ok(metadata))),
            updated_metadata: Mutex::new(None),
        }
    }

    pub fn loaded_build(&self) -> Option<Build> {
        self.build.get().and_then(|build| build.as_ref().ok()).cloned()
    }

    pub fn build_id(&self) -> Uuid {
        self.build_id
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub async fn fetch(&self) {
        let _ = tokio::join!(self.snapfile(), self.build(), self.metadata());
    }

    pub async fn snapfile(&self) -> Result<PathBuf, TemplateError> {
        self.snapfile.get_or_init(|| self.download_snapfile()).await.clone()
    }

    pub async fn build(&self) -> Result<Build, TemplateError> {
        self.build.get_or_init(|| self.load_build()).await.clone()
    }

    pub async fn metadata(&self) -> Result<TemplateMetadata, TemplateError> {
        let updated = self.updated_metadata.lock().clone();
        match updated {
            Some(updated) => Ok(updated),
            None => self.metadata.get_or_init(|| self.load_metadata()).await.clone(),
        }
    }

    pub fn update_metadata(&self, metadata: TemplateMetadata) {
        *self.updated_metadata.lock() = Some(metadata);
    }

    async fn load_metadata(&self) -> Result<TemplateMetadata, TemplateError> {
        let object = self.paths().metadata();
        match self.storage.find(&object).await.map_err(TemplateError::Failed)? {
            Some(bytes) => TemplateMetadata::from_json(&bytes).map_err(TemplateError::Failed),
            None => Ok(TemplateMetadata::v1()),
        }
    }

    fn paths(&self) -> BuildPaths {
        BuildPaths::new(self.build_id)
    }

    async fn download_snapfile(&self) -> Result<PathBuf, TemplateError> {
        let bytes = self.object(&self.paths().snapfile()).await?;
        let path = self.dir.join("snapfile");
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| TemplateError::Failed(format!("create {}: {e}", self.dir.display())))?;
        tokio::fs::write(&path, bytes)
            .await
            .map_err(|e| TemplateError::Failed(format!("write {}: {e}", path.display())))
            .map(|()| path)
    }

    async fn load_build(&self) -> Result<Build, TemplateError> {
        let paths = self.paths();
        let (memfile_header, rootfs_header) = (paths.memfile_header(), paths.rootfs_header());
        let (memfile, rootfs) = tokio::join!(self.header(&memfile_header), self.header(&rootfs_header));
        match (memfile, rootfs) {
            (Ok(memfile), Ok(rootfs)) => Ok(Build {
                build_id: self.build_id,
                memfile,
                rootfs,
            }),
            (Err(missing), rootfs) => self.without_memfile(missing, rootfs).await,
            (Ok(_), Err(e)) => Err(e),
        }
    }

    async fn without_memfile(
        &self,
        missing: TemplateError,
        rootfs: Result<Header, TemplateError>,
    ) -> Result<Build, TemplateError> {
        let filesystem_only = matches!(missing, TemplateError::NotFound(_))
            && self.metadata().await.is_ok_and(|metadata| metadata.filesystem_only);
        match filesystem_only {
            true => rootfs.map(|rootfs| Build::filesystem_only(self.build_id, rootfs)),
            false => Err(missing),
        }
    }

    async fn header(&self, object: &str) -> Result<Header, TemplateError> {
        let bytes = self.object(object).await?;
        Header::decode(&bytes).map_err(|e| TemplateError::Failed(format!("decode {object}: {e}")))
    }

    async fn object(&self, object: &str) -> Result<Bytes, TemplateError> {
        self.storage
            .find(object)
            .await
            .map_err(TemplateError::Failed)?
            .ok_or_else(|| TemplateError::NotFound(object.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::header::Metadata;

    struct Fixture {
        _dir: tempfile::TempDir,
        storage: Storage,
        build_id: Uuid,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let storage = Storage::local(&dir.path().join("store")).unwrap();
            Self {
                _dir: dir,
                storage,
                build_id: Uuid::new_v4(),
            }
        }

        fn template(&self) -> Template {
            Template::new(self.storage.clone(), self.build_id, self._dir.path().join("templates"))
        }

        async fn put_header(&self, object: String) {
            let header = Header::new(Metadata::template(self.build_id, 4096, 4096), Vec::new()).unwrap();
            self.storage
                .put(&object, Bytes::from(header.serialize().unwrap()))
                .await
                .unwrap();
        }

        async fn put_metadata(&self, bytes: &[u8]) {
            let object = BuildPaths::new(self.build_id).metadata();
            self.storage.put(&object, Bytes::copy_from_slice(bytes)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn a_normal_template_loads_even_when_its_metadata_is_unreadable() {
        let fixture = Fixture::new();
        let paths = BuildPaths::new(fixture.build_id);
        fixture.put_header(paths.memfile_header()).await;
        fixture.put_header(paths.rootfs_header()).await;
        fixture.put_metadata(b"{not json").await;
        let template = fixture.template();

        let build = template.build().await.unwrap();

        assert_eq!(build.memfile.metadata.size, 4096);
        assert!(template.metadata().await.is_err());
    }

    #[tokio::test]
    async fn a_filesystem_only_template_loads_without_a_memfile_header() {
        let fixture = Fixture::new();
        fixture
            .put_header(BuildPaths::new(fixture.build_id).rootfs_header())
            .await;
        let metadata = TemplateMetadata {
            version: metadata::CURRENT_VERSION,
            filesystem_only: true,
            ..TemplateMetadata::default()
        };
        fixture.put_metadata(&metadata.to_json()).await;

        let build = fixture.template().build().await.unwrap();

        assert_eq!(build.memfile.metadata.size, 0);
        assert_eq!(build.rootfs.metadata.size, 4096);
    }

    #[tokio::test]
    async fn a_missing_memfile_header_is_reported_for_a_memory_template() {
        let fixture = Fixture::new();
        let paths = BuildPaths::new(fixture.build_id);
        fixture.put_header(paths.rootfs_header()).await;

        let error = fixture.template().build().await.unwrap_err();

        assert_eq!(error, TemplateError::NotFound(paths.memfile_header()));
    }
}
