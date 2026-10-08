use std::fs::File;
use std::io::{self, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Duration;

use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use tokio::io::Interest;
use tokio::net::{UnixListener, UnixStream};

use super::region::Regions;
use super::uffd::Uffd;

const MAX_REGIONS_JSON: usize = 1024;

pub struct UffdListener {
    listener: UnixListener,
}

pub struct Handshake {
    pub uffd: Uffd,
    pub regions: Regions,
    pub memfd: Option<File>,
}

impl UffdListener {
    pub fn bind(path: &Path) -> io::Result<Self> {
        match std::fs::remove_file(path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o777))?;
        Ok(Self { listener })
    }

    pub async fn accept(self, timeout: Duration) -> Result<Handshake, String> {
        let (stream, _) = tokio::time::timeout(timeout, self.listener.accept())
            .await
            .map_err(|_| format!("uffd handshake: no connection within {timeout:?}"))?
            .map_err(|e| format!("uffd handshake: accept: {e}"))?;
        let received = tokio::time::timeout(timeout, receive(&stream))
            .await
            .map_err(|_| format!("uffd handshake: no message within {timeout:?}"))??;
        let regions = Regions::parse(&received.payload).map_err(|e| format!("uffd handshake: {e}"))?;
        let mut fds = received.fds.into_iter();
        let uffd = fds
            .next()
            .ok_or_else(|| "uffd handshake: no file descriptor".to_owned())
            .and_then(|fd| Uffd::adopt(fd).map_err(|e| format!("uffd handshake: set non-blocking: {e}")))?;
        Ok(Handshake {
            uffd,
            regions,
            memfd: fds.next().map(File::from),
        })
    }
}

struct Received {
    payload: Vec<u8>,
    fds: Vec<OwnedFd>,
}

async fn receive(stream: &UnixStream) -> Result<Received, String> {
    loop {
        stream.readable().await.map_err(|e| format!("uffd handshake: {e}"))?;
        match stream.try_io(Interest::READABLE, || receive_now(stream.as_raw_fd())) {
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            other => return other.map_err(|e| format!("uffd handshake: {e}")),
        }
    }
}

fn receive_now(socket: RawFd) -> io::Result<Received> {
    let mut payload = vec![0u8; MAX_REGIONS_JSON];
    let mut control = nix::cmsg_space!([RawFd; 2]);
    let mut iov = [IoSliceMut::new(&mut payload)];
    let message = recvmsg::<()>(socket, &mut iov, Some(&mut control), MsgFlags::MSG_CMSG_CLOEXEC)?;
    let length = message.bytes;
    let fds: Vec<OwnedFd> = message
        .cmsgs()?
        .filter_map(|cmsg| match cmsg {
            ControlMessageOwned::ScmRights(fds) => Some(fds),
            _ => None,
        })
        .flatten()
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    payload.truncate(length);
    Ok(Received { payload, fds })
}

#[cfg(test)]
mod tests {
    use std::io::IoSlice;
    use std::os::fd::AsRawFd;

    use nix::fcntl::{FcntlArg, OFlag, fcntl};
    use nix::sys::socket::{ControlMessage, sendmsg};

    use super::*;

    const REGIONS: &str = r#"[{"base_host_virt_addr":4096,"size":8192,"offset":0,"page_size_kib":4096}]"#;

    #[tokio::test]
    async fn accepts_regions_and_the_uffd() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("uffd.sock");
        let listener = UffdListener::bind(&socket).unwrap();

        let client = std::thread::spawn(move || {
            let stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
            let passed = std::fs::File::open("/dev/null").unwrap();
            let fds = [passed.as_raw_fd()];
            let iov = [IoSlice::new(REGIONS.as_bytes())];
            sendmsg::<()>(
                stream.as_raw_fd(),
                &iov,
                &[ControlMessage::ScmRights(&fds)],
                MsgFlags::empty(),
                None,
            )
            .unwrap();
            stream
        });
        let handshake = listener.accept(Duration::from_secs(5)).await.unwrap();
        drop(client.join());

        assert_eq!(handshake.regions, Regions::parse(REGIONS.as_bytes()).unwrap());
        assert!(handshake.uffd.as_raw_fd() >= 0);
        let flags = OFlag::from_bits_retain(fcntl(handshake.uffd.as_raw_fd(), FcntlArg::F_GETFL).unwrap());
        assert!(
            flags.contains(OFlag::O_NONBLOCK),
            "the received uffd must be non-blocking"
        );
    }

    #[tokio::test]
    async fn times_out_without_a_connection() {
        let dir = tempfile::tempdir().unwrap();
        let listener = UffdListener::bind(&dir.path().join("uffd.sock")).unwrap();

        let error = listener.accept(Duration::from_millis(100)).await.err().unwrap();

        assert_eq!(error, "uffd handshake: no connection within 100ms");
    }

    #[tokio::test]
    async fn binding_again_replaces_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("uffd.sock");
        drop(UffdListener::bind(&socket).unwrap());

        assert!(UffdListener::bind(&socket).is_ok());
    }
}
