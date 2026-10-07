use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use uuid::Uuid;

use super::chunker::Chunker;
use crate::storage::backend::Storage;
use crate::storage::header::Header;
use crate::storage::paths::BuildPaths;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Artifact {
    Memfile,
    Rootfs,
}

impl Artifact {
    pub fn object(&self, build_id: Uuid) -> String {
        let paths = BuildPaths::new(build_id);
        match self {
            Artifact::Memfile => paths.memfile(),
            Artifact::Rootfs => paths.rootfs(),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Artifact::Memfile => "memfile",
            Artifact::Rootfs => "rootfs",
        }
    }
}

pub struct BuildFile {
    header: Header,
    artifact: Artifact,
    storage: Storage,
    cache_dir: PathBuf,
    diffs: tokio::sync::Mutex<HashMap<Uuid, Arc<Chunker>>>,
}

impl BuildFile {
    pub fn new(header: Header, artifact: Artifact, storage: Storage, cache_dir: PathBuf) -> Self {
        Self {
            header,
            artifact,
            storage,
            cache_dir,
            diffs: tokio::sync::Mutex::default(),
        }
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn size(&self) -> u64 {
        self.header.metadata.size
    }

    pub async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let mut done = 0;
        while done < buf.len() {
            let location = self.header.locate(offset + done as u64)?;
            let length = (location.length as usize).min(buf.len() - done);
            let target = &mut buf[done..done + length];
            if location.build_id.is_nil() {
                target.fill(0);
            } else {
                self.diff(location.build_id)
                    .await?
                    .read_at(location.storage_offset, target)
                    .await?;
            }
            done += length;
        }
        Ok(())
    }

    async fn diff(&self, build_id: Uuid) -> Result<Arc<Chunker>, String> {
        let mut diffs = self.diffs.lock().await;
        if let Some(chunker) = diffs.get(&build_id) {
            return Ok(chunker.clone());
        }
        let cache_path = self.cache_dir.join(format!("{build_id}-{}", self.artifact.name()));
        let object = self.artifact.object(build_id);
        let chunker = Arc::new(Chunker::open(self.storage.clone(), &object, &cache_path).await?);
        diffs.insert(build_id, chunker.clone());
        Ok(chunker)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;

    use super::*;
    use crate::storage::header::{BuildMapping, Metadata};

    fn id(byte: u8) -> Uuid {
        Uuid::from_bytes([byte; 16])
    }

    fn mapping(offset: u64, build_id: Uuid, storage_offset: u64) -> BuildMapping {
        BuildMapping {
            offset,
            length: 4096,
            build_id,
            storage_offset,
        }
    }

    #[tokio::test]
    async fn reads_through_mappings_from_several_builds_and_zero_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(1)), Bytes::from(vec![1; 4 * 4096]))
            .await
            .unwrap();
        storage
            .put(&Artifact::Rootfs.object(id(2)), Bytes::from(vec![2; 4096]))
            .await
            .unwrap();

        let metadata = Metadata::template(id(1), 4096, 4 * 4096).next_generation(id(2));
        let mappings = vec![
            mapping(0, id(1), 0),
            mapping(4096, id(2), 0),
            mapping(8192, Uuid::nil(), 0),
            mapping(12288, id(1), 12288),
        ];
        let header = Header::new(metadata, mappings).unwrap();
        let file = BuildFile::new(header, Artifact::Rootfs, storage, dir.path().join("cache"));

        let mut whole = vec![9; 4 * 4096];
        file.read_at(0, &mut whole).await.unwrap();
        let expected = [vec![1; 4096], vec![2; 4096], vec![0; 4096], vec![1; 4096]].concat();
        assert_eq!(whole, expected);

        let mut across = vec![0; 200];
        file.read_at(4000, &mut across).await.unwrap();
        assert_eq!(across, [vec![1; 96], vec![2; 104]].concat());

        let mut past = vec![0; 10];
        assert_eq!(
            file.read_at(4 * 4096, &mut past).await.unwrap_err(),
            "offset 16384 is out of bounds (size 16384)"
        );
    }
}
