pub mod filesystem;
pub mod in_place;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::FutureExt;
use roaring::RoaringBitmap;
use uuid::Uuid;

use super::Sandbox;
use super::config::Rootfs;
use crate::block::build_file::{BuildFile, Ready, ready_now};
use crate::block::file_diff::export_file_diff;
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::memory_info::GuestRegion;
use crate::memory::dedup::config::DedupConfig;
use crate::memory::server::MemoryServer;
use crate::memory::snapshot::{BuildFileBase, GuestMemory, diff_metadata, export_deduped, export_pages, memfile_pages};
use crate::storage::backend::{ObjectMetadata, Storage};
use crate::storage::build::Build;
use crate::storage::header::{BuildMapping, DiffMetadata, Header, Metadata};

const ROOTFS_BLOCK_SIZE: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub build: Build,
    pub memfile_bytes: u64,
    pub memory_bytes: u64,
    pub rootfs_bytes: u64,
    pub tracked_writes: bool,
    pub vm_vcpus: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotFiles {
    pub snapfile: PathBuf,
    pub memfile: PathBuf,
    pub rootfs: PathBuf,
}

#[derive(Clone)]
pub struct LocalSnapshot {
    pub snapshot: Snapshot,
    pub files: SnapshotFiles,
    pub memfile_ready: Ready,
}

impl std::fmt::Debug for LocalSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalSnapshot")
            .field("snapshot", &self.snapshot)
            .field("files", &self.files)
            .finish()
    }
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

    async fn snapshot(&self, storage: &Storage, build_id: Uuid) -> Result<Snapshot, String> {
        let local = self.snapshot_into(&self.config.work_dir, build_id).await?;
        let uploaded = local.upload(storage, &ObjectMetadata::default()).await;
        local.files.remove();
        uploaded.map(|()| local.snapshot)
    }

    pub async fn snapshot_into(&self, dir: &Path, build_id: Uuid) -> Result<LocalSnapshot, String> {
        self.snapshot_deduped_into(dir, build_id, DedupConfig::default()).await
    }

    pub async fn snapshot_deduped_into(
        &self,
        dir: &Path,
        build_id: Uuid,
        dedup: DedupConfig,
    ) -> Result<LocalSnapshot, String> {
        let vm = self.resources.vm.as_ref().ok_or("firecracker is not running")?;
        let pid = vm.pid().ok_or("firecracker has exited")?;
        let client = vm.client();
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let files = SnapshotFiles::in_dir(dir);
        self.write(&client, pid, &files, build_id, dedup)
            .await
            .map(|(snapshot, memfile_ready)| LocalSnapshot {
                snapshot,
                files: files.clone(),
                memfile_ready,
            })
            .inspect_err(|_| files.remove())
    }

    async fn write(
        &self,
        client: &FirecrackerClient,
        pid: u32,
        files: &SnapshotFiles,
        build_id: Uuid,
        dedup: DedupConfig,
    ) -> Result<(Snapshot, Ready), String> {
        client.pause().await?;
        client.create_snapshot(&files.snapfile).await?;
        let pages = self.memory_pages(client).await?;
        let memory_bytes = pages.regions.iter().map(|region| region.size).sum();
        let tracked_writes = pages.tracked_writes;
        let (memfile, memfile_ready) = self.export_memory(pages, pid, &files.memfile, build_id, dedup).await?;
        let rootfs = match self.export_disk(&files.rootfs, build_id).await {
            Ok(rootfs) => rootfs,
            Err(e) => {
                let _ = memfile_ready.await;
                return Err(e);
            }
        };
        let snapshot = Snapshot {
            build: Build {
                build_id,
                memfile: memfile.header,
                rootfs: rootfs.header,
            },
            memfile_bytes: memfile.bytes,
            memory_bytes,
            rootfs_bytes: rootfs.bytes,
            tracked_writes,
            vm_vcpus: self.resolved_vm_vcpus,
        };
        Ok((snapshot, memfile_ready))
    }

    async fn export_disk(&self, path: &Path, build_id: Uuid) -> Result<Exported, String> {
        self.flush_disk().await?;
        self.export_rootfs(path, build_id).await
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
        let removed = self.settled_removals().await?;
        let tracked = self
            .resources
            .memory
            .as_ref()
            .filter(|memory| memory.write_protecting());
        let (dirty, empty) = match tracked {
            Some(_) => (client.memory_dirty().await?, RoaringBitmap::new()),
            None => {
                let info = client.memory_info().await?;
                (&info.resident - &info.empty, info.empty)
            }
        };
        Ok(MemoryPages {
            tracked_writes: tracked.is_some(),
            dirty: memfile_pages(&regions, &dirty)?,
            empty: memfile_pages(&regions, &empty)? | removed,
            regions,
        })
    }

    async fn tracker_pages(&self, client: &FirecrackerClient) -> Result<MemoryPages, String> {
        if !self.uses_sync_wp() {
            return Err("the tracker dirty source needs a sandbox resumed with synchronous write-protect".to_owned());
        }
        let memory = self
            .resources
            .memory
            .as_ref()
            .ok_or("the memory server is not running")?;
        if memory.is_finished() {
            return Err("the memory server has stopped; the guest memory is not consistent".to_owned());
        }
        let regions = client.memory_mappings().await?;
        let exported = memory.export_page_states().await?;
        Ok(MemoryPages {
            tracked_writes: true,
            regions,
            dirty: exported.dirty,
            empty: exported.empty,
        })
    }

    async fn settled_removals(&self) -> Result<RoaringBitmap, String> {
        match self.resources.memory.as_ref() {
            Some(memory) => memory.settle().await.map(|()| memory.empty_pages()),
            None => Ok(RoaringBitmap::new()),
        }
    }

    async fn export_memory(
        &self,
        pages: MemoryPages,
        pid: u32,
        path: &Path,
        build_id: Uuid,
        dedup: DedupConfig,
    ) -> Result<(Exported, Ready), String> {
        let page_size = pages
            .regions
            .first()
            .map(|region| region.page_size)
            .ok_or("firecracker reported no memory regions")?;
        let size = pages.regions.iter().map(|region| region.size).sum();
        let path = path.to_path_buf();
        let parent = self.memory_parent(page_size, size)?;
        if let Some(base) = self.config.memfile.clone().filter(|_| dedup.enabled) {
            let memory = match self.resources.memory.as_ref().and_then(MemoryServer::memfd) {
                Some(memfd) => GuestMemory::Memfd(memfd),
                None => GuestMemory::Process { pid },
            };
            let diff = deduped_export(memory, pages, path, base, dedup).await?;
            return Ok((exported(&diff, &parent, build_id)?, ready_now()));
        }
        match self.resources.memory.as_ref().and_then(MemoryServer::memfd) {
            Some(memfd) => {
                let diff = diff_metadata(&pages.regions, &pages.dirty, &pages.empty)
                    .map_err(|e| format!("export memfile: {e}"))?;
                let ready = background_copy(GuestMemory::Memfd(memfd), pages, path);
                Ok((exported(&diff, &parent, build_id)?, ready))
            }
            None => {
                let memory = GuestMemory::Process { pid };
                let diff = blocking("export memfile", move || write_pages(&memory, &pages, &path)).await?;
                Ok((exported(&diff, &parent, build_id)?, ready_now()))
            }
        }
    }

    fn memory_parent(&self, page_size: u64, size: u64) -> Result<Header, String> {
        self.config.memfile.as_ref().map_or_else(
            || {
                let base = self.config.memory_base.unwrap_or_else(Uuid::nil);
                let empty = BuildMapping {
                    offset: 0,
                    length: size,
                    build_id: Uuid::nil(),
                    storage_offset: 0,
                };
                Header::new(Metadata::template(base, page_size, size), vec![empty])
            },
            |memfile| Ok(memfile.header().clone()),
        )
    }

    async fn flush_disk(&self) -> Result<(), String> {
        match &self.config.rootfs {
            Rootfs::File(path) => sync_file(path.clone()).await,
            Rootfs::Overlay(_) => {
                let disk = self
                    .resources
                    .disk
                    .as_ref()
                    .ok_or("failed to get cow path: rootfs is not exposed")?;
                disk.flush()
                    .await
                    .map_err(|e| format!("flushing COW device failed: {e}"))
            }
        }
    }

    async fn export_rootfs(&self, path: &Path, build_id: Uuid) -> Result<Exported, String> {
        match &self.config.rootfs {
            Rootfs::Overlay(overlay) => {
                let overlay = overlay.clone();
                let parent = overlay.base().header().clone();
                let path = path.to_path_buf();
                let diff = blocking("export rootfs", move || {
                    File::create(&path).and_then(|mut out| overlay.export_diff(&mut out))
                })
                .await?;
                exported(&diff, &parent, build_id)
            }
            Rootfs::File(source) => export_file(source.clone(), path.to_path_buf(), build_id).await,
        }
    }
}

async fn sync_file(device: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || File::open(&device).and_then(|disk| disk.sync_all()))
        .await
        .map_err(|e| format!("flush rootfs device: {e}"))?
        .map_err(|e| format!("flush rootfs device: {e}"))
}

async fn export_file(source: PathBuf, path: PathBuf, build_id: Uuid) -> Result<Exported, String> {
    let size = std::fs::metadata(&source)
        .map_err(|e| format!("stat rootfs {}: {e}", source.display()))?
        .len();
    let diff = blocking("export rootfs", move || {
        let mut out = BufWriter::new(File::create(&path)?);
        let diff = export_file_diff(&source, &mut out, ROOTFS_BLOCK_SIZE)?;
        out.flush().map(|()| diff)
    })
    .await?;
    let parent = Header::new(Metadata::template(build_id, ROOTFS_BLOCK_SIZE, size), Vec::new())?;
    exported(&diff, &parent, build_id)
}

fn write_pages(memory: &GuestMemory, pages: &MemoryPages, path: &Path) -> io::Result<DiffMetadata> {
    let mut out = BufWriter::new(File::create(path)?);
    let diff = export_pages(memory, &pages.regions, &pages.dirty, &pages.empty, &mut out)?;
    out.flush().map(|()| diff)
}

async fn deduped_export(
    memory: GuestMemory,
    pages: MemoryPages,
    path: PathBuf,
    base: Arc<BuildFile>,
    dedup: DedupConfig,
) -> Result<DiffMetadata, String> {
    let runtime = tokio::runtime::Handle::current();
    blocking("export deduped memfile", move || {
        let base = BuildFileBase { file: base, runtime };
        let mut out = BufWriter::new(File::create(&path)?);
        let diff = export_deduped(
            &memory,
            &pages.regions,
            &pages.dirty,
            &pages.empty,
            &base,
            &dedup,
            &mut out,
        )?;
        out.flush().map(|()| diff)
    })
    .await
}

fn background_copy(memory: GuestMemory, pages: MemoryPages, path: PathBuf) -> Ready {
    let copy = tokio::task::spawn_blocking(move || write_pages(&memory, &pages, &path));
    async move {
        copy.await
            .map_err(|e| format!("copy memfile from memfd: {e}"))?
            .map(|_| ())
            .map_err(|e| format!("copy memfile from memfd: {e}"))
    }
    .boxed()
    .shared()
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
    pub async fn upload(&self, storage: &Storage, metadata: &ObjectMetadata) -> Result<(), String> {
        self.memfile_ready.clone().await?;
        let build = &self.snapshot.build;
        let paths = build.paths();
        let (snapfile, memfile, rootfs) = (paths.snapfile(), paths.memfile(), paths.rootfs());
        let (memfile_header, rootfs_header) = (paths.memfile_header(), paths.rootfs_header());
        let (memfile_bytes, rootfs_bytes) = (
            build.memfile.finalized().serialize()?,
            build.rootfs.finalized().serialize()?,
        );
        let memfile_metadata = data_metadata(metadata, &build.memfile, self.snapshot.memfile_bytes);
        let rootfs_metadata = data_metadata(metadata, &build.rootfs, self.snapshot.rootfs_bytes);
        tokio::try_join!(
            storage.put_file_with(&snapfile, &self.files.snapfile, metadata),
            body(
                storage,
                &memfile,
                &self.files.memfile,
                self.snapshot.memfile_bytes,
                &memfile_metadata
            ),
            body(
                storage,
                &rootfs,
                &self.files.rootfs,
                self.snapshot.rootfs_bytes,
                &rootfs_metadata
            ),
            storage.put_with(&memfile_header, Bytes::from(memfile_bytes), metadata),
            storage.put_with(&rootfs_header, Bytes::from(rootfs_bytes), metadata),
        )
        .map(|_| ())
    }
}

async fn body(
    storage: &Storage,
    object: &str,
    file: &Path,
    bytes: u64,
    metadata: &ObjectMetadata,
) -> Result<(), String> {
    match bytes {
        0 => Ok(()),
        _ => storage.put_file_with(object, file, metadata).await,
    }
}

fn data_metadata(base: &ObjectMetadata, header: &Header, diff_bytes: u64) -> ObjectMetadata {
    let mapped: u64 = header
        .mappings
        .iter()
        .filter(|mapping| !mapping.build_id.is_nil())
        .map(|mapping| mapping.length)
        .sum();
    base.clone()
        .with("logical-size", header.metadata.size)
        .with("mapped-size", mapped)
        .with("diff-size", diff_bytes)
}
