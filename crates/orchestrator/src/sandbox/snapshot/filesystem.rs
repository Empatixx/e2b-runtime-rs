use std::path::PathBuf;

use bytes::Bytes;

use super::{body, data_metadata};
use crate::storage::backend::{ObjectMetadata, Storage};
use crate::storage::build::Build;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilesystemSnapshot {
    pub build: Build,
    pub rootfs_diff: Option<PathBuf>,
    pub rootfs_bytes: u64,
    pub metadata_file: PathBuf,
}

impl FilesystemSnapshot {
    pub fn exist(&self) -> bool {
        self.metadata_file.exists() && self.rootfs_diff.iter().all(|diff| diff.exists())
    }

    pub async fn upload(&self, storage: &Storage, metadata: &ObjectMetadata) -> Result<(), String> {
        let paths = self.build.paths();
        let header = Bytes::from(self.build.rootfs.serialize()?);
        let rootfs_metadata = data_metadata(metadata, &self.build.rootfs, self.rootfs_bytes);
        let (rootfs, rootfs_header, metadata_object) = (paths.rootfs(), paths.rootfs_header(), paths.metadata());
        tokio::try_join!(
            self.upload_rootfs(storage, &rootfs, &rootfs_metadata),
            storage.put_file_with(&metadata_object, &self.metadata_file, metadata),
            storage.put_with(&rootfs_header, header, metadata),
        )
        .map(|_| ())
    }

    async fn upload_rootfs(&self, storage: &Storage, object: &str, metadata: &ObjectMetadata) -> Result<(), String> {
        match &self.rootfs_diff {
            Some(diff) => body(storage, object, diff, self.rootfs_bytes, metadata).await,
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use uuid::Uuid;

    use super::*;
    use crate::storage::header::{Header, Metadata};

    struct Fixture {
        dir: tempfile::TempDir,
        storage: Storage,
        build: Build,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let build_id = Uuid::new_v4();
        let rootfs = Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap();
        std::fs::write(dir.path().join("metadata.json"), b"{}").unwrap();
        Fixture {
            dir,
            storage,
            build: Build::filesystem_only(build_id, rootfs),
        }
    }

    fn snapshot(fixture: &Fixture, diff: Option<&[u8]>) -> FilesystemSnapshot {
        let rootfs_diff = diff.map(|bytes| {
            let path = fixture.dir.path().join("rootfs.diff");
            std::fs::write(&path, bytes).unwrap();
            path
        });
        FilesystemSnapshot {
            build: fixture.build.clone(),
            rootfs_bytes: diff.map_or(0, |bytes| bytes.len() as u64),
            rootfs_diff,
            metadata_file: fixture.dir.path().join("metadata.json"),
        }
    }

    #[tokio::test]
    async fn a_layer_without_a_diff_uploads_only_the_header_and_the_metadata() {
        let fixture = fixture();
        let paths = fixture.build.paths();

        snapshot(&fixture, None)
            .upload(&fixture.storage, &ObjectMetadata::default())
            .await
            .unwrap();

        assert_eq!(fixture.storage.find(&paths.rootfs()).await, Ok(None));
        assert_eq!(fixture.storage.find(&paths.snapfile()).await, Ok(None));
        assert_eq!(fixture.storage.find(&paths.memfile()).await, Ok(None));
        assert_eq!(fixture.storage.find(&paths.memfile_header()).await, Ok(None));
        assert_eq!(
            fixture.storage.find(&paths.metadata()).await.unwrap().as_deref(),
            Some(&b"{}"[..])
        );
        assert_eq!(
            fixture.storage.load_header(&paths.rootfs_header()).await.unwrap(),
            fixture.build.rootfs
        );
    }

    #[tokio::test]
    async fn a_layer_with_a_diff_uploads_the_rootfs_body_too() {
        let fixture = fixture();
        let paths = fixture.build.paths();

        snapshot(&fixture, Some(&[7; 4096]))
            .upload(&fixture.storage, &ObjectMetadata::default())
            .await
            .unwrap();

        assert_eq!(
            fixture.storage.find(&paths.rootfs()).await.unwrap().as_deref(),
            Some(&[7; 4096][..])
        );
        assert_eq!(fixture.storage.find(&paths.memfile()).await, Ok(None));
    }

    #[test]
    fn a_layer_exists_while_its_files_do() {
        let fixture = fixture();
        let layer = snapshot(&fixture, Some(&[1; 8]));
        assert!(layer.exist());

        std::fs::remove_file(layer.rootfs_diff.as_ref().unwrap()).unwrap();

        assert!(!layer.exist());
    }
}
