use std::rc::Rc;
use std::sync::Arc;

use libublk::helpers::IoBuf;
use libublk::io::{UblkDev, UblkQueue};
use libublk::{BufDesc, UblkError, sys};
use nix::libc;
use tokio::runtime::Handle;
use tokio::sync::oneshot;

use super::wakeup::{Signal, Wakeup};
use crate::exposure::block_device::BlockDevice;

const SECTOR_SHIFT: u32 = 9;
const OP_MASK: u32 = 0xff;
const IDLE_SECONDS: u64 = 20;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Request {
    pub op: u32,
    pub offset: u64,
    pub length: usize,
}

#[derive(Debug, PartialEq)]
pub struct Answer {
    pub result: i32,
    pub data: Vec<u8>,
}

struct Reply {
    sender: oneshot::Sender<Answer>,
    _wake: Signal,
}

impl Request {
    pub fn from_descriptor(descriptor: &sys::ublksrv_io_desc) -> Self {
        Self {
            op: descriptor.op_flags & OP_MASK,
            offset: descriptor.start_sector << SECTOR_SHIFT,
            length: (descriptor.nr_sectors as usize) << SECTOR_SHIFT,
        }
    }
}

impl Answer {
    fn failed(errno: i32) -> Self {
        Self {
            result: -errno,
            data: Vec::new(),
        }
    }
}

pub async fn perform<D: BlockDevice>(device: Arc<D>, request: Request, payload: Vec<u8>) -> Answer {
    let done = match request.op {
        sys::UBLK_IO_OP_READ => read(device.as_ref(), request).await,
        sys::UBLK_IO_OP_WRITE => blocking(move || device.write_at(request.offset, &payload)).await,
        sys::UBLK_IO_OP_DISCARD | sys::UBLK_IO_OP_WRITE_ZEROES => {
            blocking(move || device.write_zeroes_at(request.offset, request.length as u64)).await
        }
        sys::UBLK_IO_OP_FLUSH => Ok(Vec::new()),
        _ => return Answer::failed(libc::EINVAL),
    };
    done.map_or_else(
        |e| {
            tracing::warn!(offset = request.offset, length = request.length, error = %e, "ublk request failed");
            Answer::failed(libc::EIO)
        },
        |data| Answer {
            result: request.length as i32,
            data,
        },
    )
}

async fn read<D: BlockDevice>(device: &D, request: Request) -> Result<Vec<u8>, String> {
    let mut data = vec![0; request.length];
    device.read_at(request.offset, &mut data).await.map(|()| data)
}

async fn blocking(work: impl FnOnce() -> Result<(), String> + Send + 'static) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("ublk blocking task: {e}"))?
        .map(|()| Vec::new())
}

pub fn serve<D: BlockDevice>(queue_id: u16, dev: &UblkDev, device: Arc<D>, runtime: Handle, wakeup: Wakeup) {
    let queue = match UblkQueue::new(queue_id, dev) {
        Ok(queue) => Rc::new(queue),
        Err(e) => return tracing::error!(error = %e, "open ublk queue"),
    };
    let executor = Rc::new(smol::LocalExecutor::new());
    let listener = {
        let (queue, wakeup) = (queue.clone(), wakeup.clone());
        executor.spawn(async move { wakeup.listen(&queue).await })
    };
    let tags: Vec<_> = (0..dev.dev_info.queue_depth)
        .map(|tag| {
            let (queue, device, runtime, wakeup) = (queue.clone(), device.clone(), runtime.clone(), wakeup.clone());
            executor.spawn(async move { serve_tag(&queue, tag, &device, &runtime, &wakeup).await })
        })
        .collect();
    smol::block_on(executor.run(async {
        let tick = || while executor.try_tick() {};
        let done = || tags.iter().all(|tag| tag.is_finished());
        if let Err(e) = libublk::wait_and_handle_io_events(&queue, Some(IDLE_SECONDS), tick, done).await {
            tracing::debug!(error = %e, "ublk queue ended");
        }
    }));
    drop(listener);
}

async fn serve_tag<D: BlockDevice>(
    queue: &UblkQueue<'_>,
    tag: u16,
    device: &Arc<D>,
    runtime: &Handle,
    wakeup: &Wakeup,
) -> Result<(), UblkError> {
    let mut buffer = IoBuf::<u8>::new(queue.dev.dev_info.max_io_buf_bytes as usize);
    queue
        .submit_io_prep_cmd(tag, BufDesc::Slice(buffer.as_slice()), 0, Some(&buffer))
        .await?;
    loop {
        let request = Request::from_descriptor(queue.get_iod(tag));
        let payload = match request.op {
            sys::UBLK_IO_OP_WRITE => buffer.as_slice()[..request.length].to_vec(),
            _ => Vec::new(),
        };
        let answer = answer_on(runtime, device.clone(), request, payload, wakeup).await;
        buffer.as_mut_slice()[..answer.data.len()].copy_from_slice(&answer.data);
        queue
            .submit_io_commit_cmd(tag, BufDesc::Slice(buffer.as_slice()), answer.result)
            .await?;
    }
}

async fn answer_on<D: BlockDevice>(
    runtime: &Handle,
    device: Arc<D>,
    request: Request,
    payload: Vec<u8>,
    wakeup: &Wakeup,
) -> Answer {
    let (sender, answer) = oneshot::channel();
    let reply = Reply {
        sender,
        _wake: wakeup.signal(),
    };
    runtime.spawn(async move {
        let answer = perform(device, request, payload).await;
        let Reply { sender, _wake } = reply;
        let _ = sender.send(answer);
    });
    answer.await.unwrap_or_else(|_| Answer::failed(libc::EIO))
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    const BROKEN: u64 = 1 << 40;

    #[derive(Default)]
    struct Recorder {
        calls: Mutex<Vec<String>>,
    }

    impl BlockDevice for Recorder {
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            self.calls.lock().push(format!("read {offset} {}", buf.len()));
            if offset == BROKEN {
                return Err("storage is gone".to_owned());
            }
            buf.fill(7);
            Ok(())
        }

        fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
            self.calls.lock().push(format!("write {offset} {data:?}"));
            Ok(())
        }

        fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String> {
            self.calls.lock().push(format!("zeroes {offset} {length}"));
            Ok(())
        }
    }

    fn request(op: u32, offset: u64, length: usize) -> Request {
        Request { op, offset, length }
    }

    #[test]
    fn a_request_reads_sectors_from_the_descriptor() {
        let descriptor = sys::ublksrv_io_desc {
            op_flags: sys::UBLK_IO_OP_WRITE | 1 << 8,
            nr_sectors: 16,
            start_sector: 8,
            addr: 0,
        };
        assert_eq!(
            Request::from_descriptor(&descriptor),
            request(sys::UBLK_IO_OP_WRITE, 4096, 8192)
        );
    }

    #[tokio::test]
    async fn reads_return_the_device_data_and_the_length() {
        let device = Arc::new(Recorder::default());
        let answer = perform(device.clone(), request(sys::UBLK_IO_OP_READ, 4096, 8192), Vec::new()).await;
        assert_eq!(
            answer,
            Answer {
                result: 8192,
                data: vec![7; 8192]
            }
        );
        assert_eq!(*device.calls.lock(), vec!["read 4096 8192"]);
    }

    #[tokio::test]
    async fn writes_pass_the_payload_and_answer_the_length() {
        let device = Arc::new(Recorder::default());
        let answer = perform(device.clone(), request(sys::UBLK_IO_OP_WRITE, 0, 3), vec![1, 2, 3]).await;
        assert_eq!(
            answer,
            Answer {
                result: 3,
                data: Vec::new()
            }
        );
        assert_eq!(*device.calls.lock(), vec!["write 0 [1, 2, 3]"]);
    }

    #[tokio::test]
    async fn discard_and_write_zeroes_write_zeroes() {
        let device = Arc::new(Recorder::default());
        for op in [sys::UBLK_IO_OP_DISCARD, sys::UBLK_IO_OP_WRITE_ZEROES] {
            let answer = perform(device.clone(), request(op, 8192, 4096), Vec::new()).await;
            assert_eq!(answer.result, 4096);
        }
        assert_eq!(*device.calls.lock(), vec!["zeroes 8192 4096", "zeroes 8192 4096"]);
    }

    #[tokio::test]
    async fn flush_answers_zero() {
        let device = Arc::new(Recorder::default());
        let answer = perform(device.clone(), request(sys::UBLK_IO_OP_FLUSH, 0, 0), Vec::new()).await;
        assert_eq!(answer.result, 0);
        assert!(device.calls.lock().is_empty());
    }

    #[tokio::test]
    async fn a_device_error_answers_eio() {
        let answer = perform(
            Arc::new(Recorder::default()),
            request(sys::UBLK_IO_OP_READ, BROKEN, 4096),
            Vec::new(),
        )
        .await;
        assert_eq!(answer, Answer::failed(libc::EIO));
        assert_eq!(answer.result, -5);
    }

    #[tokio::test]
    async fn an_unknown_op_answers_einval() {
        let device = Arc::new(Recorder::default());
        let answer = perform(device.clone(), request(99, 0, 4096), Vec::new()).await;
        assert_eq!(answer.result, -libc::EINVAL);
        assert!(device.calls.lock().is_empty());
    }
}
