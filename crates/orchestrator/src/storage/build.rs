use uuid::Uuid;

use super::backend::Storage;
use super::header::Header;
use super::paths::BuildPaths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub build_id: Uuid,
    pub memfile: Header,
    pub rootfs: Header,
}

impl Build {
    pub async fn load(storage: &Storage, build_id: Uuid) -> Result<Self, String> {
        let paths = BuildPaths::new(build_id);
        let (memfile_header, rootfs_header) = (paths.memfile_header(), paths.rootfs_header());
        let (memfile, rootfs) = tokio::try_join!(
            storage.load_header(&memfile_header),
            storage.load_header(&rootfs_header)
        )?;
        Ok(Self {
            build_id,
            memfile,
            rootfs,
        })
    }

    pub async fn store(&self, storage: &Storage) -> Result<(), String> {
        let paths = self.paths();
        let (memfile_header, rootfs_header) = (paths.memfile_header(), paths.rootfs_header());
        tokio::try_join!(
            storage.store_header(&memfile_header, &self.memfile),
            storage.store_header(&rootfs_header, &self.rootfs)
        )
        .map(|_| ())
    }

    pub fn paths(&self) -> BuildPaths {
        BuildPaths::new(self.build_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::header::Metadata;

    #[tokio::test]
    async fn headers_round_trip_through_storage() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(dir.path()).unwrap();
        let build_id = Uuid::new_v4();
        let build = Build {
            build_id,
            memfile: Header::new(Metadata::template(build_id, 4096, 8192), Vec::new()).unwrap(),
            rootfs: Header::new(Metadata::template(build_id, 4096, 16384), Vec::new()).unwrap(),
        };

        build.store(&storage).await.unwrap();

        assert_eq!(Build::load(&storage, build_id).await.unwrap(), build);
        assert!(Build::load(&storage, Uuid::new_v4()).await.is_err());
    }
}
