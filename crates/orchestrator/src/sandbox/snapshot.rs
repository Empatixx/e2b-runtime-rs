use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use uuid::Uuid;

use super::Sandbox;
use crate::firecracker::client::FirecrackerClient;
use crate::memory::snapshot::export_memfile;
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::storage::header::{DiffMetadata, Header, Metadata};

struct SnapshotFiles {
    snapfile: PathBuf,
    memfile: PathBuf,
    rootfs: PathBuf,
}

impl SnapshotFiles {
    fn in_dir(dir: &Path) -> Self {
        Self {
            snapfile: dir.join("snapfile"),
            memfile: dir.join("memfile.diff"),
            rootfs: dir.join("rootfs.diff"),
        }
    }
}

impl Sandbox {
    pub async fn snapshot(&self, storage: &Storage, build_id: Uuid) -> Result<Build, String> {
        let vm = self.resources.vm.as_ref().ok_or("firecracker is not running")?;
        let pid = vm.pid().ok_or("firecracker has exited")?;
        let client = vm.client();
        let files = SnapshotFiles::in_dir(&self.config.work_dir);
        client.pause().await?;
        client.create_snapshot(&files.snapfile).await?;
        let memfile = self.export_memory(&client, pid, &files.memfile, build_id).await?;
        let rootfs = self.export_rootfs(&files.rootfs, build_id).await?;
        let build = Build {
            build_id,
            memfile,
            rootfs,
        };
        upload(storage, &build, &files).await?;
        Ok(build)
    }

    async fn export_memory(
        &self,
        client: &FirecrackerClient,
        pid: u32,
        path: &Path,
        build_id: Uuid,
    ) -> Result<Header, String> {
        let (regions, info) = tokio::try_join!(client.memory_mappings(), client.memory_info())?;
        let page_size = regions
            .first()
            .map(|region| region.page_size)
            .ok_or("firecracker reported no memory regions")?;
        let size = regions.iter().map(|region| region.size).sum();
        let path = path.to_path_buf();
        let diff = blocking("export memfile", move || {
            let mut out = BufWriter::new(File::create(&path)?);
            let diff = export_memfile(pid, &regions, &info, &mut out)?;
            out.flush().map(|()| diff)
        })
        .await?;
        diff.to_diff_header(&self.memory_parent(page_size, size)?, build_id)
    }

    fn memory_parent(&self, page_size: u64, size: u64) -> Result<Header, String> {
        self.config.memfile.as_ref().map_or_else(
            || Header::new(Metadata::template(Uuid::nil(), page_size, size), Vec::new()),
            |memfile| Ok(memfile.header().clone()),
        )
    }

    async fn export_rootfs(&self, path: &Path, build_id: Uuid) -> Result<Header, String> {
        let overlay = self.config.rootfs.clone();
        let path = path.to_path_buf();
        let diff = blocking("export rootfs", move || {
            File::create(&path).and_then(|mut out| overlay.export_diff(&mut out))
        })
        .await?;
        diff.to_diff_header(self.config.rootfs.base().header(), build_id)
    }
}

async fn blocking(
    what: &'static str,
    work: impl FnOnce() -> io::Result<DiffMetadata> + Send + 'static,
) -> Result<DiffMetadata, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("{what}: {e}"))?
        .map_err(|e| format!("{what}: {e}"))
}

async fn upload(storage: &Storage, build: &Build, files: &SnapshotFiles) -> Result<(), String> {
    let paths = build.paths();
    let (snapfile, memfile, rootfs) = (paths.snapfile(), paths.memfile(), paths.rootfs());
    tokio::try_join!(
        storage.put_file(&snapfile, &files.snapfile),
        storage.put_file(&memfile, &files.memfile),
        storage.put_file(&rootfs, &files.rootfs),
        build.store(storage),
    )
    .map(|_| ())
}
