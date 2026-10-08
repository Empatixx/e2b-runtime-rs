use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use roaring::RoaringBitmap;
use uuid::Uuid;

use super::Sandbox;
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::memory_info::GuestRegion;
use crate::memory::server::MemoryServer;
use crate::memory::snapshot::{GuestMemory, export_pages, memfile_pages};
use crate::storage::backend::Storage;
use crate::storage::build::Build;
use crate::storage::header::{DiffMetadata, Header, Metadata};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub build: Build,
    pub memfile_bytes: u64,
    pub memory_bytes: u64,
    pub rootfs_bytes: u64,
    pub tracked_writes: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFiles {
    pub snapfile: PathBuf,
    pub memfile: PathBuf,
    pub rootfs: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalSnapshot {
    pub snapshot: Snapshot,
    pub files: SnapshotFiles,
}

struct Exported {
    header: Header,
    bytes: u64,
}

struct MemoryPages {
    tracked_writes: bool,
    regions: Vec<GuestRegion>,
    dirty: RoaringBitmap,
    empty: RoaringBitmap,
}

impl SnapshotFiles {
    pub fn in_dir(dir: &Path) -> Self {
        Self {
            snapfile: dir.join("snapfile"),
            memfile: dir.join("memfile.diff"),
            rootfs: dir.join("rootfs.diff"),
        }
    }

    pub fn exist(&self) -> bool {
        [&self.snapfile, &self.memfile, &self.rootfs]
            .into_iter()
            .all(|path| path.exists())
    }

    pub fn remove(&self) {
        [&self.snapfile, &self.memfile, &self.rootfs]
            .into_iter()
            .for_each(|path| drop(std::fs::remove_file(path)));
    }
}

impl Sandbox {
    pub async fn pause(self, storage: &Storage, build_id: Uuid) -> Result<Snapshot, String> {
        let id = self.config.id.clone();
        let snapshot = self.snapshot(storage, build_id).await;
        match (snapshot, self.stop().await) {
            (Ok(snapshot), Ok(())) => Ok(snapshot),
            (Ok(snapshot), Err(teardown)) => {
                tracing::error!(sandbox = %id, error = %teardown, "paused sandbox did not tear down cleanly");
                Ok(snapshot)
            }
            (Err(e), Ok(())) => Err(e),
            (Err(e), Err(teardown)) => Err(format!("{e}; teardown: {teardown}")),
        }
    }

    pub async fn pause_into(self, dir: &Path, build_id: Uuid) -> Result<LocalSnapshot, String> {
        let id = self.config.id.clone();
        let snapshot = self.snapshot_into(dir, build_id).await;
        if let Err(teardown) = self.stop().await {
            tracing::error!(sandbox = %id, error = %teardown, "paused sandbox did not tear down cleanly");
        }
        snapshot
    }

    pub async fn snapshot(&self, storage: &Storage, build_id: Uuid) -> Result<Snapshot, String> {
        let local = self.snapshot_into(&self.config.work_dir, build_id).await?;
        let uploaded = local.upload(storage).await;
        local.files.remove();
        uploaded.map(|()| local.snapshot)
    }

    pub async fn snapshot_into(&self, dir: &Path, build_id: Uuid) -> Result<LocalSnapshot, String> {
        let vm = self.resources.vm.as_ref().ok_or("firecracker is not running")?;
        let pid = vm.pid().ok_or("firecracker has exited")?;
        let client = vm.client();
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let files = SnapshotFiles::in_dir(dir);
        self.write(&client, pid, &files, build_id)
            .await
            .map(|snapshot| LocalSnapshot {
                snapshot,
                files: files.clone(),
            })
            .inspect_err(|_| files.remove())
    }

    async fn write(
        &self,
        client: &FirecrackerClient,
        pid: u32,
        files: &SnapshotFiles,
        build_id: Uuid,
    ) -> Result<Snapshot, String> {
        client.pause().await?;
        client.create_snapshot(&files.snapfile).await?;
        let pages = self.memory_pages(client).await?;
        let memory_bytes = pages.regions.iter().map(|region| region.size).sum();
        let tracked_writes = pages.tracked_writes;
        let memfile = self.export_memory(pages, pid, &files.memfile, build_id).await?;
        self.flush_disk().await?;
        let rootfs = self.export_rootfs(&files.rootfs, build_id).await?;
        Ok(Snapshot {
            build: Build {
                build_id,
                memfile: memfile.header,
                rootfs: rootfs.header,
            },
            memfile_bytes: memfile.bytes,
            memory_bytes,
            rootfs_bytes: rootfs.bytes,
            tracked_writes,
        })
    }

    async fn memory_pages(&self, client: &FirecrackerClient) -> Result<MemoryPages, String> {
        if self
            .resources
            .memory
            .as_ref()
            .is_some_and(|memory| memory.is_finished())
        {
            return Err("the memory server has stopped; the guest memory is not consistent".to_owned());
        }
        let regions = client.memory_mappings().await?;
        let tracked = self
            .resources
            .memory
            .as_ref()
            .filter(|memory| memory.write_protecting());
        let removed = self
            .resources
            .memory
            .as_ref()
            .map(|memory| memory.removed_pages())
            .unwrap_or_default();
        let (dirty, empty) = match tracked {
            Some(_) => (client.memory_dirty().await?, RoaringBitmap::new()),
            None => {
                let info = client.memory_info().await?;
                (&info.resident - &info.empty, info.empty)
            }
        };
        Ok(MemoryPages {
            tracked_writes: tracked.is_some(),
            dirty: memfile_pages(&regions, &dirty),
            empty: memfile_pages(&regions, &empty) | removed,
            regions,
        })
    }

    async fn export_memory(
        &self,
        pages: MemoryPages,
        pid: u32,
        path: &Path,
        build_id: Uuid,
    ) -> Result<Exported, String> {
        let page_size = pages
            .regions
            .first()
            .map(|region| region.page_size)
            .ok_or("firecracker reported no memory regions")?;
        let size = pages.regions.iter().map(|region| region.size).sum();
        let path = path.to_path_buf();
        let memory = self
            .resources
            .memory
            .as_ref()
            .and_then(MemoryServer::memfd)
            .map_or(GuestMemory::Process { pid }, GuestMemory::Memfd);
        let diff = blocking("export memfile", move || {
            let mut out = BufWriter::new(File::create(&path)?);
            let diff = export_pages(&memory, &pages.regions, &pages.dirty, &pages.empty, &mut out)?;
            out.flush().map(|()| diff)
        })
        .await?;
        exported(&diff, &self.memory_parent(page_size, size)?, build_id)
    }

    fn memory_parent(&self, page_size: u64, size: u64) -> Result<Header, String> {
        self.config.memfile.as_ref().map_or_else(
            || Header::new(Metadata::template(Uuid::nil(), page_size, size), Vec::new()),
            |memfile| Ok(memfile.header().clone()),
        )
    }

    async fn flush_disk(&self) -> Result<(), String> {
        let device = self
            .resources
            .disk
            .as_ref()
            .map(|disk| disk.path())
            .ok_or("rootfs is not exposed")?;
        tokio::task::spawn_blocking(move || File::open(&device).and_then(|disk| disk.sync_all()))
            .await
            .map_err(|e| format!("flush rootfs device: {e}"))?
            .map_err(|e| format!("flush rootfs device: {e}"))
    }

    async fn export_rootfs(&self, path: &Path, build_id: Uuid) -> Result<Exported, String> {
        let overlay = self.config.rootfs.clone();
        let path = path.to_path_buf();
        let diff = blocking("export rootfs", move || {
            File::create(&path).and_then(|mut out| overlay.export_diff(&mut out))
        })
        .await?;
        exported(&diff, self.config.rootfs.base().header(), build_id)
    }
}

fn exported(diff: &DiffMetadata, parent: &Header, build_id: Uuid) -> Result<Exported, String> {
    Ok(Exported {
        header: diff.to_diff_header(parent, build_id)?,
        bytes: diff.dirty.len() * diff.block_size,
    })
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

impl LocalSnapshot {
    pub async fn upload(&self, storage: &Storage) -> Result<(), String> {
        upload(storage, &self.snapshot.build, &self.files).await
    }
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
