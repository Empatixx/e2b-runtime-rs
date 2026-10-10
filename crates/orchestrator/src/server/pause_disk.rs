use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::storage::spec::{Provider, StorageSpec};
use nix::sys::statvfs::statvfs;
use parking_lot::Mutex;

pub const SNAPFILE_ALLOWANCE: i64 = 64 << 20;
pub const MIB: i64 = 1 << 20;
const BLOCK_UNIT: i64 = 512;

type Available = Box<dyn Fn() -> Result<i64, String> + Send + Sync>;
type StorageShares = Box<dyn Fn() -> bool + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSize {
    pub ram_bytes: i64,
    pub rootfs_cache_bytes: i64,
    pub filesystem_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PauseCapture {
    ram_bytes: i64,
    rootfs_cache_bytes: i64,
    filesystem_only: bool,
    storage_shares_disk: bool,
}

impl PauseCapture {
    fn need(&self) -> i64 {
        let memory = match self.filesystem_only {
            true => 0,
            false => self.ram_bytes,
        };
        let copies = match self.storage_shares_disk {
            true => 2,
            false => 1,
        };
        (SNAPFILE_ALLOWANCE + memory + self.rootfs_cache_bytes) * copies
    }

    fn second_copy(&self) -> i64 {
        match self.storage_shares_disk {
            true => self.need() / 2,
            false => 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Refusal {
    Short(DiskShort),
    Unreadable(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskShort {
    pub need: i64,
    pub available: i64,
    pub reserved: i64,
    pub headroom: i64,
}

impl std::fmt::Display for DiskShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "build filesystem short of disk: {} MiB needed, {} MiB free, {} MiB reserved, {} MiB headroom",
            self.need / MIB,
            self.available / MIB,
            self.reserved / MIB,
            self.headroom / MIB
        )
    }
}

pub struct BuildDisk {
    reserved: Mutex<i64>,
    available: Available,
    storage_shares: StorageShares,
}

impl BuildDisk {
    pub fn on_directory(dir: PathBuf, template_storage: StorageSpec) -> Arc<Self> {
        let storage_dir = dir.clone();
        Self::new(
            Box::new(move || disk_available_bytes(&dir)),
            Box::new(move || storage_shares_disk(&template_storage, &storage_dir)),
        )
    }

    pub fn new(available: Available, storage_shares: StorageShares) -> Arc<Self> {
        Arc::new(Self {
            reserved: Mutex::new(0),
            available,
            storage_shares,
        })
    }

    pub fn reserved(&self) -> i64 {
        *self.reserved.lock()
    }

    pub fn admit(
        self: &Arc<Self>,
        headroom_mib: i64,
        size: &CaptureSize,
    ) -> Result<Option<PauseDiskReservation>, DiskShort> {
        if headroom_mib < 0 {
            return Ok(None);
        }
        let capture = PauseCapture {
            ram_bytes: size.ram_bytes,
            rootfs_cache_bytes: size.rootfs_cache_bytes,
            filesystem_only: size.filesystem_only,
            storage_shares_disk: (self.storage_shares)(),
        };
        match self.reserve(&capture, headroom_mib * MIB) {
            Ok(reservation) => Ok(Some(reservation)),
            Err(Refusal::Short(short)) => Err(short),
            Err(Refusal::Unreadable(e)) => {
                tracing::warn!(error = %e, "Cannot read the build filesystem's free space, admitting the pause");
                Ok(None)
            }
        }
    }

    fn reserve(self: &Arc<Self>, capture: &PauseCapture, headroom: i64) -> Result<PauseDiskReservation, Refusal> {
        let mut reserved = self.reserved.lock();
        let available = (self.available)().map_err(Refusal::Unreadable)?;
        let need = capture.need();
        let short = DiskShort {
            need,
            available,
            reserved: *reserved,
            headroom,
        };
        match available - *reserved - need < headroom {
            true => Err(Refusal::Short(short)),
            false => {
                *reserved += need;
                Ok(PauseDiskReservation {
                    disk: self.clone(),
                    held: Mutex::new(Held {
                        remaining: need,
                        second_copy: capture.second_copy(),
                    }),
                })
            }
        }
    }
}

struct Held {
    remaining: i64,
    second_copy: i64,
}

pub struct PauseDiskReservation {
    disk: Arc<BuildDisk>,
    held: Mutex<Held>,
}

impl PauseDiskReservation {
    pub fn written(&self) {
        let mut reserved = self.disk.reserved.lock();
        let mut held = self.held.lock();
        if held.remaining > held.second_copy {
            *reserved += held.second_copy - held.remaining;
            held.remaining = held.second_copy;
        }
    }

    pub fn release(&self) {
        let mut reserved = self.disk.reserved.lock();
        let mut held = self.held.lock();
        *reserved -= held.remaining;
        held.remaining = 0;
    }
}

impl Drop for PauseDiskReservation {
    fn drop(&mut self) {
        self.release();
    }
}

pub fn disk_available_bytes(path: &Path) -> Result<i64, String> {
    statvfs(path)
        .map(|stat| {
            i64::try_from(u128::from(stat.blocks_available()) * u128::from(stat.fragment_size())).unwrap_or(i64::MAX)
        })
        .map_err(|e| format!("statfs {}: {e}", path.display()))
}

pub fn storage_shares_disk(spec: &StorageSpec, dir: &Path) -> bool {
    spec.provider == Provider::Local
        && nearest_existing_device(Path::new(&spec.base_path))
            .zip(std::fs::metadata(dir).ok().map(|stat| stat.dev()))
            .is_some_and(|(storage, build)| storage == build)
}

fn nearest_existing_device(path: &Path) -> Option<u64> {
    Some(path)
        .filter(|path| !path.as_os_str().is_empty())
        .and_then(|path| {
            path.ancestors()
                .map(|ancestor| match ancestor.as_os_str().is_empty() {
                    true => Path::new("."),
                    false => ancestor,
                })
                .map(std::fs::metadata)
                .find(|stat| !matches!(stat, Err(e) if e.kind() == io::ErrorKind::NotFound))
        })
        .and_then(Result::ok)
        .map(|stat| stat.dev())
}

pub fn rootfs_cache_bytes(cache_file: &Path) -> io::Result<i64> {
    match std::fs::metadata(cache_file) {
        Ok(stat) => Ok(i64::try_from(stat.blocks())
            .unwrap_or(i64::MAX)
            .saturating_mul(BLOCK_UNIT)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::time::Duration;

    use super::*;

    const GIB: i64 = 1 << 30;

    fn disk(available: i64) -> Arc<BuildDisk> {
        BuildDisk::new(Box::new(move || Ok(available)), Box::new(|| false))
    }

    fn size(ram_mb: i64, filesystem_only: bool) -> CaptureSize {
        CaptureSize {
            ram_bytes: ram_mb << 20,
            rootfs_cache_bytes: 0,
            filesystem_only,
        }
    }

    #[test]
    fn a_capture_the_build_filesystem_cannot_hold_is_refused_and_reserves_nothing() {
        let disk = disk(GIB);

        let Err(refused) = disk.admit(1024, &size(2048, false)) else {
            panic!("the capture does not fit");
        };

        assert_eq!(refused.need, 2 * GIB + SNAPFILE_ALLOWANCE);
        assert_eq!(disk.reserved(), 0);
    }

    #[test]
    fn a_capture_that_fits_holds_its_estimate_until_released() {
        let disk = disk(10 * GIB);

        let reservation = disk.admit(1024, &size(1024, false)).unwrap().unwrap();

        assert_eq!(disk.reserved(), GIB + SNAPFILE_ALLOWANCE);
        drop(reservation);
        assert_eq!(disk.reserved(), 0);
    }

    #[test]
    fn admitted_pauses_count_against_the_free_space_and_a_reservation_is_released_once() {
        let disk = disk(4 * GIB);

        let first = disk.admit(0, &size(2048, false)).unwrap().unwrap();
        assert_eq!(disk.reserved(), 2 * GIB + SNAPFILE_ALLOWANCE);
        assert!(disk.admit(0, &size(2048, false)).is_err());

        first.release();
        first.release();
        assert_eq!(disk.reserved(), 0);
        assert!(disk.admit(0, &size(2048, false)).unwrap().is_some());
    }

    #[test]
    fn a_negative_headroom_disables_the_check() {
        let disk = disk(1);

        assert!(disk.admit(-1, &size(2048, false)).unwrap().is_none());
        assert_eq!(disk.reserved(), 0);
    }

    #[test]
    fn a_filesystem_only_pause_excludes_the_guest_memory() {
        let disk = disk(GIB);

        let reservation = disk.admit(0, &size(2048, true)).unwrap().unwrap();

        assert_eq!(disk.reserved(), SNAPFILE_ALLOWANCE);
        drop(reservation);
    }

    #[test]
    fn the_rootfs_cache_is_part_of_the_estimate() {
        let disk = disk(10 * GIB);
        let capture = CaptureSize {
            rootfs_cache_bytes: GIB,
            ..size(1024, false)
        };

        let reservation = disk.admit(0, &capture).unwrap().unwrap();

        assert_eq!(disk.reserved(), 2 * GIB + SNAPFILE_ALLOWANCE);
        drop(reservation);
    }

    #[test]
    fn whether_storage_shares_the_disk_is_asked_at_every_admission() {
        let shared = Arc::new(AtomicBool::new(false));
        let asked = shared.clone();
        let disk = BuildDisk::new(
            Box::new(|| Ok(10 * GIB)),
            Box::new(move || asked.load(Ordering::SeqCst)),
        );

        let first = disk.admit(0, &size(1024, false)).unwrap().unwrap();
        assert_eq!(disk.reserved(), GIB + SNAPFILE_ALLOWANCE);
        drop(first);

        shared.store(true, Ordering::SeqCst);
        let second = disk.admit(0, &size(1024, false)).unwrap().unwrap();
        assert_eq!(disk.reserved(), 2 * (GIB + SNAPFILE_ALLOWANCE));
        drop(second);
    }

    #[test]
    fn shared_storage_doubles_the_estimate() {
        let shared = Arc::new(AtomicBool::new(true));
        let asked = shared.clone();
        let disk = BuildDisk::new(Box::new(|| Ok(2 * GIB)), Box::new(move || asked.load(Ordering::SeqCst)));

        assert!(disk.admit(0, &size(1024, false)).is_err());
        shared.store(false, Ordering::SeqCst);
        assert!(disk.admit(0, &size(1024, false)).unwrap().is_some());
    }

    #[test]
    fn unreadable_free_space_admits_the_pause() {
        let disk = BuildDisk::new(
            Box::new(|| Err("statfs: permission denied".to_owned())),
            Box::new(|| false),
        );

        assert!(disk.admit(0, &size(2048, false)).unwrap().is_none());
        assert_eq!(disk.reserved(), 0);
    }

    #[test]
    fn one_answer_serves_the_estimate_and_the_second_copy() {
        let asked = Arc::new(AtomicI32::new(0));
        let counting = asked.clone();
        let disk = BuildDisk::new(
            Box::new(|| Ok(10 * GIB)),
            Box::new(move || counting.fetch_add(1, Ordering::SeqCst) == 0),
        );

        let reservation = disk.admit(0, &size(1024, false)).unwrap().unwrap();

        assert_eq!(asked.load(Ordering::SeqCst), 1);
        let need = 2 * (GIB + SNAPFILE_ALLOWANCE);
        assert_eq!(disk.reserved(), need);
        reservation.written();
        assert_eq!(disk.reserved(), need / 2);
        drop(reservation);
        assert_eq!(disk.reserved(), 0);
    }

    #[test]
    fn written_keeps_only_the_uploads_copy_and_a_remote_upload_keeps_nothing() {
        let shared = BuildDisk::new(Box::new(|| Ok(10 * GIB)), Box::new(|| true));
        let need = 2 * (GIB + SNAPFILE_ALLOWANCE);

        let reservation = shared.admit(0, &size(1024, false)).unwrap().unwrap();
        assert_eq!(shared.reserved(), need);
        reservation.written();
        reservation.written();
        assert_eq!(shared.reserved(), need / 2);
        reservation.release();
        reservation.release();
        assert_eq!(shared.reserved(), 0);

        let remote = disk(10 * GIB);
        let reservation = remote.admit(0, &size(1024, false)).unwrap().unwrap();
        reservation.written();
        assert_eq!(remote.reserved(), 0);
    }

    #[test]
    fn no_release_lands_between_the_sample_and_the_reservation() {
        let (sampling_sender, sampling) = std::sync::mpsc::channel();
        let (proceed_sender, proceed) = std::sync::mpsc::channel::<()>();
        let proceed = Mutex::new(proceed);
        let slow = Arc::new(AtomicBool::new(false));
        let sampled = slow.clone();
        let disk = BuildDisk::new(
            Box::new(move || {
                if sampled.load(Ordering::SeqCst) {
                    sampling_sender.send(()).unwrap();
                    proceed.lock().recv().unwrap();
                }
                Ok(10 * GIB)
            }),
            Box::new(|| false),
        );
        let held = disk.admit(0, &size(1024, false)).unwrap().unwrap();
        slow.store(true, Ordering::SeqCst);

        let admitting = {
            let disk = disk.clone();
            std::thread::spawn(move || drop(disk.admit(0, &size(1024, false)).unwrap()))
        };
        sampling.recv().unwrap();
        let written = std::thread::spawn(move || held.written());
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !written.is_finished(),
            "a capture landing on disk must wait for the admission in progress"
        );

        proceed_sender.send(()).unwrap();
        admitting.join().unwrap();
        written.join().unwrap();
        assert_eq!(disk.reserved(), 0);
    }

    #[test]
    fn the_free_space_of_a_directory_is_readable_and_a_missing_one_is_not() {
        let dir = tempfile::tempdir().unwrap();

        assert!(disk_available_bytes(dir.path()).unwrap() > 0);
        assert!(disk_available_bytes(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn storage_shares_the_disk_with_a_directory_on_the_same_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let local = |path: &Path| StorageSpec::local(&path.display().to_string());
        let bucket = StorageSpec {
            provider: Provider::Gcp,
            bucket: "snapshots".to_owned(),
            ..StorageSpec::local("")
        };

        assert!(storage_shares_disk(
            &local(tempfile::tempdir().unwrap().path()),
            dir.path()
        ));
        assert!(!storage_shares_disk(&bucket, dir.path()));
        assert!(storage_shares_disk(
            &local(&dir.path().join("not/yet/created")),
            dir.path()
        ));
        assert!(!storage_shares_disk(&StorageSpec::local(""), dir.path()));
        assert!(!storage_shares_disk(
            &local(&dir.path().join("missing")),
            &dir.path().join("missing-too")
        ));
    }

    #[test]
    fn the_rootfs_cache_counts_the_blocks_on_disk_and_a_missing_file_counts_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("rootfs.overlay");
        std::fs::write(&cache, vec![1u8; 1 << 20]).unwrap();

        assert!(rootfs_cache_bytes(&cache).unwrap() >= 1 << 20);
        assert_eq!(rootfs_cache_bytes(&dir.path().join("missing")).unwrap(), 0);
    }
}
