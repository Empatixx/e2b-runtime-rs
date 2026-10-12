use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use libublk::UblkFlags;
use libublk::ctrl::{UblkCtrl, UblkCtrlBuilder};
use libublk::sys;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::queue;
use super::wakeup::Wakeup;
use crate::exposure::block_device::BlockDevice;
use crate::storage::header::PAGE_SIZE;

const DEVICE_NAME: &str = "e2b";
const DEVICES: &str = "/dev";
const CONTROL_PREFIX: &str = "ublkc";
const QUEUE_DEPTH: u16 = 128;
const IO_BUFFER_BYTES: u32 = 512 << 10;
const MAX_ZEROES_BYTES: u32 = 32 << 20;
const SECTOR_SHIFT: u32 = 9;
const NODE_TIMEOUT: Duration = Duration::from_secs(2);
const NODE_POLL_INTERVAL: Duration = Duration::from_micros(100);

pub fn run<D: BlockDevice>(
    device: Arc<D>,
    size: u64,
    runtime: Handle,
    started: oneshot::Sender<u32>,
) -> Result<(), String> {
    let wakeup = Wakeup::new().map_err(|e| format!("create ublk wakeup: {e}"))?;
    let control = UblkCtrlBuilder::default()
        .name(DEVICE_NAME)
        .nr_queues(1)
        .depth(QUEUE_DEPTH)
        .io_buf_bytes(IO_BUFFER_BYTES)
        .dev_flags(UblkFlags::UBLK_DEV_F_ADD_DEV)
        .build()
        .map_err(|e| format!("add ublk device: {e}"))?;
    let served = control
        .run_target(
            move |dev| {
                dev.tgt.dev_size = size;
                dev.tgt.params = params(size);
                Ok(())
            },
            move |queue_id, dev| queue::serve(queue_id, dev, device, runtime, wakeup),
            move |control| {
                let _ = started.send(control.dev_info().dev_id);
            },
        )
        .map(drop)
        .map_err(|e| format!("run ublk device: {e}"));
    let deleted = control
        .del_dev()
        .map(drop)
        .map_err(|e| format!("delete ublk device: {e}"));
    served.and(deleted)
}

pub fn params(size: u64) -> sys::ublk_params {
    let block_shift = PAGE_SIZE.trailing_zeros() as u8;
    sys::ublk_params {
        types: sys::UBLK_PARAM_TYPE_BASIC | sys::UBLK_PARAM_TYPE_DISCARD,
        basic: sys::ublk_param_basic {
            logical_bs_shift: block_shift,
            physical_bs_shift: block_shift,
            io_opt_shift: block_shift,
            io_min_shift: block_shift,
            max_sectors: IO_BUFFER_BYTES >> SECTOR_SHIFT,
            dev_sectors: size >> SECTOR_SHIFT,
            ..Default::default()
        },
        discard: sys::ublk_param_discard {
            discard_granularity: PAGE_SIZE as u32,
            max_discard_sectors: MAX_ZEROES_BYTES >> SECTOR_SHIFT,
            max_write_zeroes_sectors: MAX_ZEROES_BYTES >> SECTOR_SHIFT,
            max_discard_segments: 1,
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn stop(id: u32) -> Result<(), String> {
    UblkCtrl::new_simple(id as i32)
        .and_then(|control| control.kill_dev())
        .map(drop)
        .map_err(|e| format!("stop ublk device: {e}"))
}

pub fn remove_orphans() -> Vec<u32> {
    std::fs::read_dir(DEVICES)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.strip_prefix(CONTROL_PREFIX)?.parse().ok())
        .filter(|id| is_orphan(*id))
        .filter(|id| {
            remove(*id)
                .inspect_err(|e| tracing::warn!(id, error = %e, "remove orphaned ublk device"))
                .is_ok()
        })
        .collect()
}

fn is_orphan(id: u32) -> bool {
    UblkCtrl::new_simple(id as i32)
        .is_ok_and(|control| !Path::new(&format!("/proc/{}", control.dev_info().ublksrv_pid)).exists())
}

fn remove(id: u32) -> Result<(), String> {
    let control = UblkCtrl::new_simple(id as i32).map_err(|e| format!("open ublk device {id}: {e}"))?;
    let _ = control.kill_dev();
    control
        .del_dev()
        .map(drop)
        .map_err(|e| format!("delete ublk device {id}: {e}"))
}

pub async fn wait_for_node(path: &Path) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + NODE_TIMEOUT;
    while !path.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::other(format!(
                "ublk device {} did not appear",
                path.display()
            )));
        }
        tokio::time::sleep(NODE_POLL_INTERVAL).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_match_the_nbd_geometry() {
        let params = params(64 << 20);
        assert_eq!(params.types, sys::UBLK_PARAM_TYPE_BASIC | sys::UBLK_PARAM_TYPE_DISCARD);
        assert_eq!(
            (params.basic.logical_bs_shift, params.basic.physical_bs_shift),
            (12, 12)
        );
        assert_eq!(params.basic.dev_sectors, (64 << 20) >> 9);
        assert_eq!(params.basic.attrs & sys::UBLK_ATTR_VOLATILE_CACHE, 0);
        assert_eq!(params.basic.max_sectors, 1024);
        assert_eq!(params.discard.max_discard_segments, 1);
        assert_eq!(params.discard.max_write_zeroes_sectors, 65536);
    }

    #[tokio::test]
    async fn a_missing_node_times_out_with_its_path() {
        tokio::time::pause();
        let waited = wait_for_node(Path::new("/dev/ublkb-missing")).await;
        assert_eq!(
            waited.unwrap_err().to_string(),
            "ublk device /dev/ublkb-missing did not appear"
        );
    }
}
