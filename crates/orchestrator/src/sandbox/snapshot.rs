pub mod filesystem;
pub mod in_place;
pub mod provisional;

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use futures::FutureExt;
use roaring::RoaringBitmap;
use uuid::Uuid;

use self::provisional::{DedupedMemfile, InflightDedup, ProvisionalMemfile};
use super::Sandbox;
use super::config::Rootfs;
use crate::block::build_file::{BuildFile, Ready, ready_now};
use crate::block::cache::BlockCache;
use crate::block::file_diff::export_file_diff;
use crate::block::overlay::Overlay;
use crate::firecracker::client::FirecrackerClient;
use crate::firecracker::memory_info::GuestRegion;
use crate::memory::dedup::config::DedupConfig;
use crate::memory::server::{MemoryServer, Support};
use crate::memory::snapshot::{BuildFileBase, GuestMemory, diff_metadata, export_deduped, export_pages, memfile_pages};
use crate::proto::SchedulingMetadata;
use crate::server::scheduling::from_headers;
use crate::storage::backend::{ObjectMetadata, Storage};
use crate::storage::build::Build;
use crate::storage::header::{BuildMapping, DiffMetadata, Header, Metadata};
use crate::telemetry::dirty_pages::{DirtySource, Divergence, record_dirty_source};
use crate::telemetry::server::in_place::record_rootfs_seal;
use crate::telemetry::snapshot_diff::{record_snapshot_dedup, record_snapshot_diff};

const ROOTFS_BLOCK_SIZE: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub build: Build,
    pub memfile_bytes: u64,
    pub memory_bytes: u64,
    pub rootfs_bytes: u64,
    pub tracked_writes: bool,
    pub vm_vcpus: i64,
    pub scheduling_memfile: Option<SchedulingMemfile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulingMemfile {
    pub parent: Header,
    pub new_bytes: u64,
}

impl Snapshot {
    pub fn scheduling_metadata(&self) -> SchedulingMetadata {
        from_headers(
            self.build.build_id,
            self.scheduling_memfile.as_ref().map(|memfile| &memfile.parent),
            &self.build.rootfs,
            self.scheduling_memfile.as_ref().map_or(0, |memfile| memfile.new_bytes),
        )
    }
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
    pub rootfs_ready: Ready,
    pub provisional: Option<ProvisionalMemfile>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SnapshotOptions {
    pub dedup: DedupConfig,
    pub defer_rootfs: bool,
    pub inflight_serve: bool,
    pub free_index: bool,
}

struct MemfileExport {
    exported: Exported,
    ready: Ready,
    provisional: Option<ProvisionalMemfile>,
    scheduling: SchedulingMemfile,
}

pub struct DiskExport {
    exported: Exported,
    sealed: Ready,
    deferred: bool,
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
        let options = SnapshotOptions {
            dedup,
            ..SnapshotOptions::default()
        };
        self.snapshot_with(dir, build_id, options).await
    }

    pub async fn snapshot_with(
        &self,
        dir: &Path,
        build_id: Uuid,
        options: SnapshotOptions,
    ) -> Result<LocalSnapshot, String> {
        self.wait_for_rootfs_seal().await?;
        let vm = self.resources.vm.as_ref().ok_or("firecracker is not running")?;
        let pid = vm.pid().ok_or("firecracker has exited")?;
        let client = vm.client();
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        let files = SnapshotFiles::in_dir(dir);
        self.write(&client, pid, &files, build_id, options)
            .await
            .inspect_err(|_| files.remove())
    }

    async fn write(
        &self,
        client: &FirecrackerClient,
        pid: u32,
        files: &SnapshotFiles,
        build_id: Uuid,
        options: SnapshotOptions,
    ) -> Result<LocalSnapshot, String> {
        client.pause().await?;
        client.create_snapshot(&files.snapfile).await?;
        let pages = self.memory_pages(client).await?;
        let memory_bytes = pages.regions.iter().map(|region| region.size).sum();
        let tracked_writes = pages.tracked_writes;
        let MemfileExport {
            exported: memfile,
            ready: memfile_ready,
            provisional,
            scheduling,
        } = self
            .export_memory(pages, pid, &files.memfile, build_id, options)
            .await?;
        let disk = match self
            .export_disk_with(&files.rootfs, build_id, options.defer_rootfs)
            .await
        {
            Ok(disk) => disk,
            Err(e) => {
                let _ = memfile_ready.await;
                return Err(e);
            }
        };
        let rootfs = disk.exported;
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
            scheduling_memfile: Some(scheduling),
        };
        Ok(LocalSnapshot {
            snapshot,
            files: files.clone(),
            memfile_ready,
            rootfs_ready: disk.sealed,
            provisional,
        })
    }

    async fn export_disk(&self, path: &Path, build_id: Uuid) -> Result<Exported, String> {
        self.flush_disk().await?;
        self.export_rootfs(path, build_id).await
    }

    pub async fn export_disk_with(&self, path: &Path, build_id: Uuid, defer: bool) -> Result<DiskExport, String> {
        let deferrable = match (&self.config.rootfs, defer) {
            (Rootfs::Overlay(overlay), true) => Some(overlay.clone()),
            _ => None,
        };
        match deferrable {
            Some(overlay) => {
                self.flush_disk().await?;
                self.seal_in_background(overlay, path, build_id)
            }
            None => self.export_disk(path, build_id).await.map(|exported| DiskExport {
                exported,
                sealed: ready_now(),
                deferred: false,
            }),
        }
    }

    pub async fn wait_for_rootfs_seal(&self) -> Result<(), String> {
        self.rootfs_seal
            .wait()
            .await
            .map_err(|e| format!("previous rootfs seal did not complete: {e}"))
    }

    pub async fn export_disk_in_place(&self, path: &Path, build_id: Uuid, defer: bool) -> Result<DiskExport, String> {
        let deferrable = match (&self.config.rootfs, defer) {
            (Rootfs::Overlay(overlay), true) => Some(overlay.clone()),
            _ => None,
        };
        let Some(overlay) = deferrable else {
            return self.export_disk_with(path, build_id, false).await;
        };
        self.flush_disk().await?;
        let frozen = overlay.swap_for_seal()?;
        let parent = overlay.base().header().clone();
        let metadata = frozen.diff_metadata();
        record_snapshot_diff(&self.config.team_id, "rootfs", &metadata, &parent);
        let exported = match exported(&metadata, &parent, build_id) {
            Ok(exported) => exported,
            Err(e) => {
                return Err(match overlay.fold_seal() {
                    Ok(()) => e,
                    Err(fold) => format!("{e}; folding the seal back also failed: {fold}"),
                });
            }
        };
        let seal = InPlaceSeal {
            overlay,
            frozen,
            path: path.to_path_buf(),
            metadata,
            team_id: self.config.team_id.clone(),
        };
        let (sealed, folded) = seal.start();
        self.rootfs_seal.set(folded);
        Ok(DiskExport {
            exported,
            sealed,
            deferred: true,
        })
    }

    fn seal_in_background(&self, overlay: Arc<Overlay>, path: &Path, build_id: Uuid) -> Result<DiskExport, String> {
        let parent = overlay.base().header().clone();
        let metadata = overlay.diff_metadata();
        record_snapshot_diff(&self.config.team_id, "rootfs", &metadata, &parent);
        let exported = exported(&metadata, &parent, build_id)?;
        let seal = RootfsSeal {
            overlay,
            path: path.to_path_buf(),
            metadata,
            team_id: self.config.team_id.clone(),
        };
        let sealed = tokio::spawn(seal.run())
            .map(|joined| joined.map_err(|e| format!("deferred rootfs export: {e}"))?)
            .boxed()
            .shared();
        Ok(DiskExport {
            exported,
            sealed,
            deferred: true,
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
        let removed = self.settled_removals().await?;
        let source = self.dirty_source();
        record_dirty_source(source);
        let (dirty, empty) = match source {
            DirtySource::Pagemap => (self.pagemap_dirty(client, &regions).await?, RoaringBitmap::new()),
            _ => {
                let info = client.memory_info().await?;
                (memfile_pages(&regions, &(&info.resident - &info.empty))?, info.empty)
            }
        };
        Ok(MemoryPages {
            tracked_writes: source == DirtySource::Pagemap,
            dirty,
            empty: memfile_pages(&regions, &empty)? | removed,
            regions,
        })
    }

    fn dirty_source(&self) -> DirtySource {
        match self
            .resources
            .memory
            .as_ref()
            .map(|memory| memory.write_protect_support())
        {
            Some(Support::On) => DirtySource::Pagemap,
            Some(Support::Unknown) => DirtySource::Unconfirmed,
            Some(Support::Off) => DirtySource::Unsupported,
            None => DirtySource::NoUffd,
        }
    }

    async fn pagemap_dirty(
        &self,
        client: &FirecrackerClient,
        regions: &[GuestRegion],
    ) -> Result<RoaringBitmap, String> {
        let tracker = match (self.uses_sync_wp(), self.resources.memory.as_ref()) {
            (true, Some(memory)) => Some(memory.export_page_states().await?.dirty),
            _ => None,
        };
        let pagemap = memfile_pages(regions, &client.memory_dirty().await?)?;
        if let Some(tracker) = tracker {
            Divergence::between(&tracker, &pagemap).record();
        }
        Ok(pagemap)
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
        options: SnapshotOptions,
    ) -> Result<MemfileExport, String> {
        let dedup = options.dedup;
        let page_size = pages
            .regions
            .first()
            .map(|region| region.page_size)
            .ok_or("firecracker reported no memory regions")?;
        let size = pages.regions.iter().map(|region| region.size).sum();
        let path = path.to_path_buf();
        let parent = self.memory_parent(page_size, size)?;
        let pre =
            diff_metadata(&pages.regions, &pages.dirty, &pages.empty).map_err(|e| format!("export memfile: {e}"))?;
        record_snapshot_diff(&self.config.team_id, "memfile", &pre, &parent);
        let scheduling = SchedulingMemfile {
            parent: parent.clone(),
            new_bytes: pre.dirty.len() * pre.block_size,
        };
        let memfd = self.resources.memory.as_ref().and_then(MemoryServer::memfd);
        if let Some(base) = self.config.memfile.clone().filter(|_| dedup.enabled) {
            if let Some(memfd) = memfd.clone().filter(|_| options.inflight_serve) {
                let inflight = InflightDedup {
                    memfd,
                    pages,
                    path,
                    base,
                    dedup,
                    parent,
                    build_id,
                    team_id: self.config.team_id.clone(),
                    pre,
                    free_index: options.free_index,
                };
                return provisional_export(inflight, scheduling);
            }
            let memory = match memfd {
                Some(memfd) => GuestMemory::Memfd(memfd),
                None => GuestMemory::Process { pid },
            };
            let diff = deduped_export(memory, pages, path, base, dedup).await?;
            record_snapshot_dedup(&self.config.team_id, "memfile", &pre, Some(&diff), dedup.best_effort);
            return settled(exported(&diff, &parent, build_id)?, ready_now(), scheduling);
        }
        record_snapshot_dedup(&self.config.team_id, "memfile", &pre, None, false);
        match memfd {
            Some(memfd) => {
                let diff = pre;
                let ready = background_copy(GuestMemory::Memfd(memfd), pages, path);
                settled(exported(&diff, &parent, build_id)?, ready, scheduling)
            }
            None => {
                let memory = GuestMemory::Process { pid };
                let diff = blocking("export memfile", move || write_pages(&memory, &pages, &path)).await?;
                settled(exported(&diff, &parent, build_id)?, ready_now(), scheduling)
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
                record_snapshot_diff(&self.config.team_id, "rootfs", &diff, &parent);
                exported(&diff, &parent, build_id)
            }
            Rootfs::File(source) => {
                export_file(source.clone(), path.to_path_buf(), build_id, &self.config.team_id).await
            }
        }
    }
}

async fn sync_file(device: PathBuf) -> Result<(), String> {
    tokio::task::spawn_blocking(move || File::open(&device).and_then(|disk| disk.sync_all()))
        .await
        .map_err(|e| format!("flush rootfs device: {e}"))?
        .map_err(|e| format!("flush rootfs device: {e}"))
}

async fn export_file(source: PathBuf, path: PathBuf, build_id: Uuid, team_id: &str) -> Result<Exported, String> {
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
    record_snapshot_diff(team_id, "rootfs", &diff, &parent);
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

fn settled(exported: Exported, ready: Ready, scheduling: SchedulingMemfile) -> Result<MemfileExport, String> {
    Ok(MemfileExport {
        exported,
        ready,
        provisional: None,
        scheduling,
    })
}

fn provisional_export(inflight: InflightDedup, scheduling: SchedulingMemfile) -> Result<MemfileExport, String> {
    let bytes = inflight.pre.dirty.len() * inflight.pre.block_size;
    let (provisional, ready) = inflight.start()?;
    Ok(MemfileExport {
        exported: Exported {
            header: provisional.header.clone(),
            bytes,
        },
        ready,
        provisional: Some(provisional),
        scheduling,
    })
}

fn exported(diff: &DiffMetadata, parent: &Header, build_id: Uuid) -> Result<Exported, String> {
    Ok(Exported {
        header: diff.to_diff_header(parent, build_id)?,
        bytes: diff.dirty.len() * diff.block_size,
    })
}

async fn blocking<T: Send + 'static>(
    what: &'static str,
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("{what}: {e}"))?
        .map_err(|e| format!("{what}: {e}"))
}

struct InPlaceSeal {
    overlay: Arc<Overlay>,
    frozen: Arc<BlockCache>,
    path: PathBuf,
    metadata: DiffMetadata,
    team_id: String,
}

impl InPlaceSeal {
    fn start(self) -> (Ready, Ready) {
        let (exported, exported_now) = tokio::sync::oneshot::channel();
        let folded = tokio::spawn(self.run(exported))
            .map(|joined| joined.map_err(|e| format!("in-place rootfs export: {e}"))?)
            .boxed()
            .shared();
        let sealed = exported_now
            .map(|sent| sent.unwrap_or_else(|_| Err("in-place rootfs export stopped".to_owned())))
            .boxed()
            .shared();
        (sealed, folded)
    }

    async fn run(self, exported: tokio::sync::oneshot::Sender<Result<(), String>>) -> Result<(), String> {
        let started = std::time::Instant::now();
        let InPlaceSeal {
            overlay,
            frozen,
            path,
            metadata,
            team_id,
        } = self;
        let sealed = blocking("export rootfs diff", move || {
            File::create(&path).and_then(|mut out| frozen.export_with(&mut out, &metadata))
        })
        .await
        .map_err(|e| format!("in-place rootfs export failed: {e}"));
        record_rootfs_seal(true, sealed.is_ok(), &team_id, started.elapsed());
        if let Err(e) = &sealed {
            tracing::error!(error = %e, "in-place rootfs export failed");
        }
        let _ = exported.send(sealed);
        tokio::task::spawn_blocking(move || overlay.fold_seal())
            .await
            .map_err(|e| format!("fold the sealed rootfs cache back: {e}"))?
            .inspect_err(|e| tracing::error!(error = %e, "in-place rootfs export fold-back failed"))
    }
}

struct RootfsSeal {
    overlay: Arc<Overlay>,
    path: PathBuf,
    metadata: DiffMetadata,
    team_id: String,
}

impl RootfsSeal {
    async fn run(self) -> Result<(), String> {
        let started = std::time::Instant::now();
        let RootfsSeal {
            overlay,
            path,
            metadata,
            team_id,
        } = self;
        let sealed = blocking("export rootfs diff", move || {
            File::create(&path).and_then(|mut out| overlay.export_with(&mut out, &metadata))
        })
        .await
        .map_err(|e| format!("deferred rootfs export failed: {e}"));
        record_rootfs_seal(false, sealed.is_ok(), &team_id, started.elapsed());
        if let Err(e) = &sealed {
            tracing::error!(error = %e, "deferred rootfs export failed");
        }
        sealed
    }
}

impl LocalSnapshot {
    pub fn written(&self) -> Ready {
        futures::future::try_join(self.memfile_ready.clone(), self.rootfs_ready.clone())
            .map(|written| written.map(drop))
            .boxed()
            .shared()
    }

    pub async fn durable_memfile(&self) -> Result<DedupedMemfile, String> {
        match &self.provisional {
            Some(provisional) => provisional.deduped.clone().await,
            None => Ok(DedupedMemfile {
                header: self.snapshot.build.memfile.clone(),
                bytes: self.snapshot.memfile_bytes,
            }),
        }
    }

    pub fn upload_parents(&self) -> Vec<Header> {
        let build = &self.snapshot.build;
        let memfile = self
            .provisional
            .as_ref()
            .map_or(&build.memfile, |provisional| &provisional.parent);
        vec![memfile.clone(), build.rootfs.clone()]
    }

    pub async fn upload(&self, storage: &Storage, metadata: &ObjectMetadata) -> Result<(), String> {
        self.memfile_ready.clone().await?;
        self.rootfs_ready.clone().await?;
        let memfile_diff = self.durable_memfile().await?;
        let build = &self.snapshot.build;
        let paths = build.paths();
        let (snapfile, memfile, rootfs) = (paths.snapfile(), paths.memfile(), paths.rootfs());
        let (memfile_header, rootfs_header) = (paths.memfile_header(), paths.rootfs_header());
        let (memfile_bytes, rootfs_bytes) = (
            memfile_diff.header.finalized().serialize()?,
            build.rootfs.finalized().serialize()?,
        );
        let memfile_metadata = data_metadata(metadata, &memfile_diff.header, memfile_diff.bytes);
        let rootfs_metadata = data_metadata(metadata, &build.rootfs, self.snapshot.rootfs_bytes);
        tokio::try_join!(
            storage.put_file_with(&snapfile, &self.files.snapfile, metadata),
            body(
                storage,
                &memfile,
                &self.files.memfile,
                memfile_diff.bytes,
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
