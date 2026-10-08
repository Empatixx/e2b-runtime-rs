pub mod cache;
pub mod metadata;

use std::fmt;
use std::path::{Path, PathBuf};

use bytes::Bytes;
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
        }
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
        self.metadata.get_or_init(|| self.load_metadata()).await.clone()
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
        let (memfile, rootfs) = tokio::try_join!(self.header(&memfile_header), self.header(&rootfs_header))?;
        Ok(Build {
            build_id: self.build_id,
            memfile,
            rootfs,
        })
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
