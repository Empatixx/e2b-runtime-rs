use std::fs::File;
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use parking_lot::RwLock;
use roaring::RoaringBitmap;

use crate::storage::header::{Header, PAGE_SIZE};
use crate::telemetry::dead_structure::{DropOutcome, record_dedup_index};
use crate::telemetry::memfd::{ServePhase, record_inflight_serve};

pub const BYTES_NOT_AVAILABLE: &str = "bytes not available";
const PACKED_SEGMENT_BYTES: usize = 24;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PackedSegment {
    packed_start: u64,
    absolute_start: u64,
    length: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PackedIndex {
    segments: Vec<PackedSegment>,
}

impl PackedIndex {
    pub fn of_pages(page_dirty: &RoaringBitmap) -> Self {
        let mut packed = 0;
        let segments = page_runs(page_dirty)
            .into_iter()
            .map(|(first, count)| {
                let segment = PackedSegment {
                    packed_start: packed,
                    absolute_start: first * PAGE_SIZE,
                    length: count * PAGE_SIZE,
                };
                packed += segment.length;
                segment
            })
            .collect();
        Self { segments }
    }

    fn byte_size(&self) -> u64 {
        (self.segments.capacity() * PACKED_SEGMENT_BYTES) as u64
    }

    pub fn translate(&self, offset: u64, length: u64) -> Option<u64> {
        let at = self
            .segments
            .partition_point(|segment| segment.packed_start + segment.length <= offset);
        self.segments
            .get(at)
            .filter(|segment| {
                segment.packed_start <= offset && offset + length <= segment.packed_start + segment.length
            })
            .map(|segment| segment.absolute_start + offset - segment.packed_start)
    }
}

fn page_runs(pages: &RoaringBitmap) -> Vec<(u64, u64)> {
    pages.iter().fold(Vec::new(), |mut runs: Vec<(u64, u64)>, page| {
        match runs.last_mut() {
            Some((first, count)) if *first + *count == u64::from(page) => *count += 1,
            _ => runs.push((u64::from(page), 1)),
        }
        runs
    })
}

struct Mapped {
    memfd: Option<Arc<File>>,
    index: Option<PackedIndex>,
}

pub struct InflightMemfd {
    mapped: RwLock<Mapped>,
    durable: std::sync::OnceLock<Header>,
    drained: std::sync::atomic::AtomicBool,
    free_index: bool,
}

impl InflightMemfd {
    pub fn new(memfd: Arc<File>, free_index: bool) -> Self {
        Self {
            mapped: RwLock::new(Mapped {
                memfd: Some(memfd),
                index: None,
            }),
            drained: std::sync::atomic::AtomicBool::new(false),
            durable: std::sync::OnceLock::new(),
            free_index,
        }
    }

    pub fn set_durable(&self, header: Header) {
        let _ = self.durable.set(header);
    }

    pub fn durable(&self) -> Option<Header> {
        self.durable.get().cloned()
    }

    pub fn install_index(&self, index: PackedIndex) {
        self.mapped.write().index = Some(index);
    }

    pub fn mark_drained(&self) {
        self.drained.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn serve_identity(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        let mapped = self.mapped.read();
        let memfd = mapped.memfd.as_ref().ok_or(BYTES_NOT_AVAILABLE)?;
        memfd
            .read_exact_at(buf, offset)
            .map_err(|e| format!("read provisional memfd at {offset}: {e}"))?;
        record_inflight_serve(ServePhase::Provisional, buf.len() as u64 / PAGE_SIZE);
        Ok(())
    }

    pub fn serve_packed(&self, offset: u64, buf: &mut [u8]) -> Option<()> {
        if self.drained.load(std::sync::atomic::Ordering::SeqCst) {
            return None;
        }
        let mapped = self.mapped.read();
        let absolute = mapped.index.as_ref()?.translate(offset, buf.len() as u64)?;
        mapped.memfd.as_ref()?.read_exact_at(buf, absolute).ok()?;
        record_inflight_serve(ServePhase::Drain, buf.len() as u64 / PAGE_SIZE);
        Some(())
    }

    pub fn release(&self) {
        let mut mapped = self.mapped.write();
        if mapped.memfd.take().is_some() {
            self.release_index(&mut mapped);
        }
    }

    fn release_index(&self, mapped: &mut Mapped) {
        let size = mapped.index.as_ref().map_or(0, PackedIndex::byte_size);
        let outcome = DropOutcome::of(mapped.index.is_some(), self.free_index);
        if outcome == DropOutcome::Dropped {
            mapped.index = None;
        }
        record_dedup_index(outcome, size);
    }

    pub fn is_mapped(&self) -> bool {
        self.mapped.read().memfd.is_some()
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    const PAGE: u64 = PAGE_SIZE;

    fn pages(indices: &[u32]) -> RoaringBitmap {
        indices.iter().copied().collect()
    }

    fn memfd_with(pages: u8) -> Arc<File> {
        let mut file = tempfile::tempfile().unwrap();
        (0..pages).for_each(|page| file.write_all(&vec![page + 1; PAGE as usize]).unwrap());
        Arc::new(file)
    }

    #[test]
    fn the_packed_index_maps_a_read_inside_one_dirty_run_back_to_the_memfd() {
        let index = PackedIndex::of_pages(&pages(&[2, 3, 7]));

        assert_eq!(index.translate(0, PAGE), Some(2 * PAGE));
        assert_eq!(index.translate(PAGE, PAGE), Some(3 * PAGE));
        assert_eq!(index.translate(2 * PAGE, PAGE), Some(7 * PAGE));
        assert_eq!(
            index.translate(PAGE, 2 * PAGE),
            None,
            "a read across two runs waits for the drain"
        );
        assert_eq!(index.translate(3 * PAGE, PAGE), None);
    }

    #[test]
    fn provisional_reads_come_from_the_memfd_until_it_is_released() {
        let inflight = InflightMemfd::new(memfd_with(4), false);
        let mut buf = vec![0; PAGE as usize];

        inflight.serve_identity(2 * PAGE, &mut buf).unwrap();
        assert!(buf.iter().all(|byte| *byte == 3));

        inflight.release();
        assert_eq!(
            inflight.serve_identity(2 * PAGE, &mut buf),
            Err(BYTES_NOT_AVAILABLE.to_owned())
        );
    }

    #[test]
    fn drain_reads_use_the_index_and_stop_once_the_drain_is_done() {
        let inflight = InflightMemfd::new(memfd_with(8), true);
        let mut buf = vec![0; PAGE as usize];
        assert_eq!(inflight.serve_packed(0, &mut buf), None, "no index before the compare");

        inflight.install_index(PackedIndex::of_pages(&pages(&[5])));
        assert_eq!(inflight.serve_packed(0, &mut buf), Some(()));
        assert!(buf.iter().all(|byte| *byte == 6));

        inflight.mark_drained();
        assert_eq!(inflight.serve_packed(0, &mut buf), None);
    }

    #[test]
    fn the_free_index_flag_drops_the_index_with_the_memfd() {
        let kept = InflightMemfd::new(memfd_with(1), false);
        let freed = InflightMemfd::new(memfd_with(1), true);
        [&kept, &freed]
            .iter()
            .for_each(|inflight| inflight.install_index(PackedIndex::of_pages(&pages(&[0]))));

        kept.release();
        freed.release();

        assert!(kept.mapped.read().index.is_some());
        assert!(freed.mapped.read().index.is_none());
        assert!(!kept.is_mapped());
    }
}
