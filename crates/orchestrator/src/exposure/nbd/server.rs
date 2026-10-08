use std::future::Future;
use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;
use tokio::task::JoinSet;

use super::protocol::{BACKEND_ERROR, Command, MAX_WRITE, REPLY_SIZE, REQUEST_SIZE, Request, encode_reply};
use crate::block::overlay::Overlay;

pub trait BlockDevice: Send + Sync + 'static {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> impl Future<Output = Result<(), String>> + Send;
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String>;
    fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String>;
}

impl BlockDevice for Overlay {
    async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
        Overlay::read_at(self, offset, buf).await
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), String> {
        Overlay::write_at(self, offset, data)
    }

    fn write_zeroes_at(&self, offset: u64, length: u64) -> Result<(), String> {
        Overlay::write_zeroes_at(self, offset, length)
    }
}

struct Response {
    error: u32,
    data: Vec<u8>,
}

impl Response {
    fn from_result(result: Result<Vec<u8>, String>, request: &Request) -> Self {
        match result {
            Ok(data) => Self { error: 0, data },
            Err(e) => {
                tracing::warn!(offset = request.offset, length = request.length, error = %e, "nbd request failed");
                Self {
                    error: BACKEND_ERROR,
                    data: Vec::new(),
                }
            }
        }
    }
}

type Replies = Arc<Mutex<OwnedWriteHalf>>;

pub async fn serve<D: BlockDevice>(device: Arc<D>, socket: UnixStream) -> Result<(), String> {
    let (mut reader, writer) = socket.into_split();
    let replies: Replies = Arc::new(Mutex::new(writer));
    let mut in_flight = JoinSet::new();
    let served = dispatch(&device, &mut reader, &replies, &mut in_flight).await;
    let finished = finish(in_flight).await;
    served.and(finished)
}

async fn dispatch<D: BlockDevice>(
    device: &Arc<D>,
    reader: &mut OwnedReadHalf,
    replies: &Replies,
    in_flight: &mut JoinSet<Result<(), String>>,
) -> Result<(), String> {
    while let Some(request) = next_request(reader).await? {
        match request.command {
            Command::Disconnect => return Ok(()),
            Command::Flush => return Err("not supported: Flush".to_owned()),
            _ => {}
        }
        let payload = payload(reader, &request).await?;
        in_flight.spawn(answer(device.clone(), replies.clone(), request, payload));
        while let Some(done) = in_flight.try_join_next() {
            done.map_err(|e| format!("nbd request task: {e}"))??;
        }
    }
    Ok(())
}

async fn finish(mut in_flight: JoinSet<Result<(), String>>) -> Result<(), String> {
    let mut finished = Ok(());
    while let Some(done) = in_flight.join_next().await {
        finished = finished.and(
            done.map_err(|e| format!("nbd request task: {e}"))
                .and_then(|answered| answered),
        );
    }
    finished
}

async fn next_request(reader: &mut OwnedReadHalf) -> Result<Option<Request>, String> {
    match read_header(reader).await {
        Ok(Some(header)) => Request::decode(&header).map(Some),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("read nbd request: {e}")),
    }
}

async fn read_header(reader: &mut OwnedReadHalf) -> io::Result<Option<[u8; REQUEST_SIZE]>> {
    let mut header = [0; REQUEST_SIZE];
    let mut filled = 0;
    while filled < REQUEST_SIZE {
        match reader.read(&mut header[filled..]).await? {
            0 => break,
            count => filled += count,
        }
    }
    match filled {
        0 => Ok(None),
        REQUEST_SIZE => Ok(Some(header)),
        _ => Err(io::ErrorKind::UnexpectedEof.into()),
    }
}

async fn payload(reader: &mut OwnedReadHalf, request: &Request) -> Result<Vec<u8>, String> {
    if request.command != Command::Write {
        return Ok(Vec::new());
    }
    if request.length > MAX_WRITE {
        return Err(format!("nbd write of {} bytes exceeds {MAX_WRITE}", request.length));
    }
    let mut payload = vec![0; request.length as usize];
    reader
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read nbd write payload: {e}"))
        .map(|_| payload)
}

async fn answer<D: BlockDevice>(
    device: Arc<D>,
    replies: Replies,
    request: Request,
    payload: Vec<u8>,
) -> Result<(), String> {
    let result = perform(device, &request, payload).await;
    let response = Response::from_result(result, &request);
    send(&replies, request.handle, response).await
}

async fn perform<D: BlockDevice>(device: Arc<D>, request: &Request, payload: Vec<u8>) -> Result<Vec<u8>, String> {
    let (offset, length) = (request.offset, u64::from(request.length));
    match request.command {
        Command::Read => {
            let mut data = vec![0; request.length as usize];
            device.read_at(offset, &mut data).await.map(|()| data)
        }
        Command::Write => blocking(move || device.write_at(offset, &payload)).await,
        Command::Trim | Command::WriteZeroes => blocking(move || device.write_zeroes_at(offset, length)).await,
        Command::Flush | Command::Disconnect => Ok(Vec::new()),
    }
}

async fn blocking(work: impl FnOnce() -> Result<(), String> + Send + 'static) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| format!("nbd blocking task: {e}"))?
        .map(|()| Vec::new())
}

async fn send(replies: &Replies, handle: [u8; 8], response: Response) -> Result<(), String> {
    let reply: [u8; REPLY_SIZE] = encode_reply(handle, response.error);
    let mut writer = replies.lock().await;
    writer
        .write_all(&reply)
        .await
        .map_err(|e| format!("write nbd reply: {e}"))?;
    writer
        .write_all(&response.data)
        .await
        .map_err(|e| format!("write nbd read data: {e}"))
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::task::JoinHandle;
    use uuid::Uuid;

    use super::*;
    use crate::block::build_file::{Artifact, BuildFile, DiffStore};
    use crate::storage::backend::Storage;
    use crate::storage::header::{Header, Metadata};

    struct Client {
        socket: UnixStream,
        server: JoinHandle<Result<(), String>>,
        _dir: tempfile::TempDir,
    }

    async fn start() -> Client {
        let dir = tempfile::tempdir().unwrap();
        let storage = Storage::local(&dir.path().join("store")).unwrap();
        let build_id = Uuid::from_bytes([1; 16]);
        storage
            .put(&Artifact::Rootfs.object(build_id), Bytes::from(vec![1; 4 * 4096]))
            .await
            .unwrap();
        let header = Header::new(Metadata::template(build_id, 4096, 4 * 4096), Vec::new()).unwrap();
        let store = Arc::new(DiffStore::new(storage, dir.path().join("cache")));
        let base = Arc::new(BuildFile::new(header, Artifact::Rootfs, store));
        let overlay = Arc::new(Overlay::new(base, &dir.path().join("overlay")).unwrap());

        let (socket, server_side) = UnixStream::pair().unwrap();
        let server = tokio::spawn(serve(overlay, server_side));
        Client {
            socket,
            server,
            _dir: dir,
        }
    }

    impl Client {
        async fn send(&mut self, command: Command, offset: u64, length: u32) {
            let request = Request {
                command,
                handle: [9; 8],
                offset,
                length,
            };
            self.socket.write_all(&request.encode()).await.unwrap();
        }

        async fn reply(&mut self) -> u32 {
            let mut reply = [0; REPLY_SIZE];
            self.socket.read_exact(&mut reply).await.unwrap();
            assert_eq!(reply[8..], [9; 8]);
            u32::from_be_bytes(reply[4..8].try_into().unwrap())
        }

        async fn read(&mut self, offset: u64, length: u32) -> Vec<u8> {
            self.send(Command::Read, offset, length).await;
            assert_eq!(self.reply().await, 0);
            let mut data = vec![0; length as usize];
            self.socket.read_exact(&mut data).await.unwrap();
            data
        }
    }

    #[tokio::test]
    async fn serves_reads_writes_and_zeroes() {
        let mut client = start().await;
        assert_eq!(client.read(0, 4096).await, vec![1; 4096]);

        client.send(Command::Write, 4096, 4096).await;
        client.socket.write_all(&[7; 1000]).await.unwrap();
        client.socket.write_all(&[7; 3096]).await.unwrap();
        assert_eq!(client.reply().await, 0);
        assert_eq!(client.read(4096, 4096).await, vec![7; 4096]);

        client.send(Command::WriteZeroes, 4096, 4096).await;
        assert_eq!(client.reply().await, 0);
        client.send(Command::Trim, 8192, 4096).await;
        assert_eq!(client.reply().await, 0);
        assert_eq!(client.read(4096, 8192).await, vec![0; 8192]);

        client.send(Command::Disconnect, 0, 0).await;
        assert_eq!(client.server.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_flush_request_ends_the_server_like_upstream() {
        let mut client = start().await;
        client.send(Command::Flush, 0, 0).await;

        assert_eq!(client.server.await.unwrap(), Err("not supported: Flush".to_owned()));
    }

    #[tokio::test]
    async fn device_errors_reply_an_error_and_keep_serving() {
        let mut client = start().await;
        client.send(Command::Read, 100, 4096).await;
        assert_eq!(client.reply().await, BACKEND_ERROR);
        assert_eq!(client.read(0, 4096).await, vec![1; 4096]);

        drop(client.socket);
        assert_eq!(client.server.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn protocol_violations_end_the_server() {
        let mut client = start().await;
        client.socket.write_all(&[0; REQUEST_SIZE]).await.unwrap();
        assert_eq!(
            client.server.await.unwrap(),
            Err("bad nbd request magic 0x0".to_owned())
        );
    }

    #[tokio::test]
    async fn a_truncated_request_header_is_an_error() {
        let mut client = start().await;
        client
            .socket
            .write_all(&[0x25, 0x60, 0x95, 0x13, 0, 0, 0, 0, 0, 0])
            .await
            .unwrap();
        client.socket.shutdown().await.unwrap();
        assert_eq!(
            client.server.await.unwrap(),
            Err("read nbd request: unexpected end of file".to_owned())
        );
    }

    struct GatedDevice {
        gate: tokio::sync::Notify,
    }

    impl BlockDevice for GatedDevice {
        async fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<(), String> {
            if offset == 0 {
                self.gate.notified().await;
            }
            buf.fill(offset as u8);
            Ok(())
        }

        fn write_at(&self, _: u64, _: &[u8]) -> Result<(), String> {
            Ok(())
        }

        fn write_zeroes_at(&self, _: u64, _: u64) -> Result<(), String> {
            Ok(())
        }
    }

    async fn request(socket: &mut UnixStream, command: Command, handle: u8, offset: u64, length: u32) {
        let request = Request {
            command,
            handle: [handle; 8],
            offset,
            length,
        };
        socket.write_all(&request.encode()).await.unwrap();
    }

    async fn reply_with_data(socket: &mut UnixStream, length: usize) -> (u8, Vec<u8>) {
        let mut reply = [0; REPLY_SIZE];
        socket.read_exact(&mut reply).await.unwrap();
        let mut data = vec![0; length];
        socket.read_exact(&mut data).await.unwrap();
        (reply[8], data)
    }

    #[tokio::test]
    async fn a_slow_read_does_not_hold_back_later_replies() {
        let device = Arc::new(GatedDevice {
            gate: tokio::sync::Notify::new(),
        });
        let (mut socket, server_side) = UnixStream::pair().unwrap();
        let server = tokio::spawn(serve(device.clone(), server_side));

        request(&mut socket, Command::Read, 1, 0, 512).await;
        request(&mut socket, Command::Read, 2, 7, 512).await;

        assert_eq!(reply_with_data(&mut socket, 512).await, (2, vec![7; 512]));
        device.gate.notify_one();
        assert_eq!(reply_with_data(&mut socket, 512).await, (1, vec![0; 512]));
        request(&mut socket, Command::Disconnect, 3, 0, 0).await;
        assert_eq!(server.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn a_write_over_32_mib_ends_the_server() {
        let mut client = start().await;
        client.send(Command::Write, 0, MAX_WRITE + 1).await;
        assert_eq!(
            client.server.await.unwrap(),
            Err("nbd write of 33554433 bytes exceeds 33554432".to_owned())
        );
    }
}
