use std::io;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::protocol::{Command, EIO, REPLY_SIZE, REQUEST_SIZE, Request, encode_reply};
use crate::block::overlay::Overlay;

struct Response {
    error: u32,
    data: Vec<u8>,
}

impl Response {
    fn ok(data: Vec<u8>) -> Self {
        Self { error: 0, data }
    }

    fn from_result(result: Result<(), String>) -> Self {
        Self {
            error: if result.is_ok() { 0 } else { EIO },
            data: Vec::new(),
        }
    }
}

pub async fn serve(device: Arc<Overlay>, mut socket: UnixStream) -> Result<(), String> {
    while let Some(request) = next_request(&mut socket).await? {
        if request.command == Command::Disconnect {
            return Ok(());
        }
        let response = handle(&device, &mut socket, request).await?;
        send(&mut socket, request.handle, response).await?;
    }
    Ok(())
}

async fn next_request(socket: &mut UnixStream) -> Result<Option<Request>, String> {
    let mut header = [0; REQUEST_SIZE];
    match socket.read_exact(&mut header).await {
        Ok(_) => Request::decode(&header).map(Some),
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => Ok(None),
        Err(e) => Err(format!("read nbd request: {e}")),
    }
}

async fn handle(device: &Overlay, socket: &mut UnixStream, request: Request) -> Result<Response, String> {
    let length = request.length as usize;
    match request.command {
        Command::Read => {
            let mut data = vec![0; length];
            let read = device.read_at(request.offset, &mut data).await;
            Ok(match read {
                Ok(()) => Response::ok(data),
                Err(e) => Response::from_result(Err(log_device_error(&request, e))),
            })
        }
        Command::Write => {
            let mut payload = vec![0; length];
            socket
                .read_exact(&mut payload)
                .await
                .map_err(|e| format!("read nbd write payload: {e}"))?;
            Ok(Response::from_result(device.write_at(request.offset, &payload)))
        }
        Command::Trim | Command::WriteZeroes => Ok(Response::from_result(
            device.write_zeroes_at(request.offset, u64::from(request.length)),
        )),
        Command::Flush | Command::Disconnect => Ok(Response::ok(Vec::new())),
    }
}

async fn send(socket: &mut UnixStream, handle: [u8; 8], response: Response) -> Result<(), String> {
    let reply: [u8; REPLY_SIZE] = encode_reply(handle, response.error);
    socket
        .write_all(&reply)
        .await
        .map_err(|e| format!("write nbd reply: {e}"))?;
    socket
        .write_all(&response.data)
        .await
        .map_err(|e| format!("write nbd read data: {e}"))
}

fn log_device_error(request: &Request, error: String) -> String {
    tracing::warn!(offset = request.offset, length = request.length, %error, "nbd request failed");
    error
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
        client.send(Command::Flush, 0, 0).await;
        assert_eq!(client.reply().await, 0);
        assert_eq!(client.read(4096, 8192).await, vec![0; 8192]);

        client.send(Command::Disconnect, 0, 0).await;
        assert_eq!(client.server.await.unwrap(), Ok(()));
    }

    #[tokio::test]
    async fn device_errors_reply_eio_and_keep_serving() {
        let mut client = start().await;
        client.send(Command::Read, 100, 4096).await;
        assert_eq!(client.reply().await, EIO);
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
}
