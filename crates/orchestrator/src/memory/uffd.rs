use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use nix::errno::Errno;

const UFFD_MSG_SIZE: usize = 32;
const EVENTS_PER_READ: usize = 64;
const UFFD_EVENT_PAGEFAULT: u8 = 0x12;
const UFFD_EVENT_REMOVE: u8 = 0x15;
const UFFD_PAGEFAULT_FLAG_WRITE: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    PageFault { address: u64, write: bool },
    Remove { start: u64, end: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied,
    AlreadyPresent,
    Retry,
    Gone,
}

#[derive(Debug)]
pub struct Uffd {
    fd: OwnedFd,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioCopy {
    dst: u64,
    src: u64,
    len: u64,
    mode: u64,
    copy: i64,
}

mod ioctls {
    use super::{UffdioCopy, UffdioRange};

    nix::ioctl_read!(wake, 0xAA, 0x02, UffdioRange);
    nix::ioctl_readwrite!(copy, 0xAA, 0x03, UffdioCopy);
}

impl Uffd {
    pub fn from_fd(fd: OwnedFd) -> Self {
        Self { fd }
    }

    pub fn read_events(&self) -> io::Result<Vec<Event>> {
        let mut events = Vec::new();
        loop {
            let mut buf = [0u8; UFFD_MSG_SIZE * EVENTS_PER_READ];
            match nix::unistd::read(self.fd.as_raw_fd(), &mut buf) {
                Ok(0) if events.is_empty() => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "uffd closed")),
                Ok(0) | Err(Errno::EAGAIN) => return Ok(events),
                Ok(read) => events.extend(parse_events(&buf[..read])?),
                Err(Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    pub fn copy(&self, dst: u64, src: &[u8]) -> io::Result<CopyOutcome> {
        let len = src.len() as u64;
        let mut request = UffdioCopy {
            dst,
            src: src.as_ptr() as u64,
            len,
            mode: 0,
            copy: 0,
        };
        match unsafe { ioctls::copy(self.fd.as_raw_fd(), &mut request) } {
            Ok(_) if request.copy == len as i64 => Ok(CopyOutcome::Copied),
            Ok(_) | Err(Errno::EAGAIN) => Ok(CopyOutcome::Retry),
            Err(Errno::EEXIST) => Ok(CopyOutcome::AlreadyPresent),
            Err(Errno::ESRCH) => Ok(CopyOutcome::Gone),
            Err(e) => Err(e.into()),
        }
    }

    pub fn wake(&self, start: u64, len: u64) -> io::Result<()> {
        let mut range = UffdioRange { start, len };
        unsafe { ioctls::wake(self.fd.as_raw_fd(), &mut range) }
            .map(|_| ())
            .map_err(io::Error::from)
    }
}

impl AsRawFd for Uffd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

fn parse_events(bytes: &[u8]) -> io::Result<Vec<Event>> {
    let (messages, _) = bytes.as_chunks::<UFFD_MSG_SIZE>();
    messages.iter().map(parse_event).collect()
}

fn parse_event(message: &[u8; UFFD_MSG_SIZE]) -> io::Result<Event> {
    let word = |index: usize| u64::from_ne_bytes(message[8 + index * 8..16 + index * 8].try_into().unwrap_or_default());
    match message[0] {
        UFFD_EVENT_PAGEFAULT => Ok(Event::PageFault {
            address: word(1),
            write: word(0) & UFFD_PAGEFAULT_FLAG_WRITE != 0,
        }),
        UFFD_EVENT_REMOVE => Ok(Event::Remove {
            start: word(0),
            end: word(1),
        }),
        other => Err(io::Error::other(format!("unexpected uffd event {other:#x}"))),
    }
}

#[cfg(test)]
pub mod testing {
    use std::io;
    use std::num::NonZeroUsize;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::ptr::NonNull;

    use nix::sys::mman::{MapFlags, ProtFlags, mmap_anonymous, munmap};

    use super::Uffd;

    const UFFD_API: u64 = 0xAA;
    const UFFD_FEATURE_EVENT_REMOVE: u64 = 1 << 3;
    const UFFDIO_REGISTER_MODE_MISSING: u64 = 1;

    #[repr(C)]
    struct UffdioApi {
        api: u64,
        features: u64,
        ioctls: u64,
    }

    #[repr(C)]
    struct UffdioRegister {
        start: u64,
        len: u64,
        mode: u64,
        ioctls: u64,
    }

    nix::ioctl_readwrite!(api, 0xAA, 0x3F, UffdioApi);
    nix::ioctl_readwrite!(register, 0xAA, 0x00, UffdioRegister);

    pub struct Pages {
        start: NonNull<std::ffi::c_void>,
        len: usize,
    }

    unsafe impl Send for Pages {}
    unsafe impl Sync for Pages {}

    impl Pages {
        pub fn map(len: usize) -> Self {
            let start = unsafe {
                mmap_anonymous(
                    None,
                    NonZeroUsize::new(len).unwrap(),
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_PRIVATE,
                )
            }
            .unwrap();
            Self { start, len }
        }

        pub fn address(&self) -> u64 {
            self.start.as_ptr() as u64
        }

        pub fn read_byte(address: u64) -> u8 {
            unsafe { (address as *const u8).read_volatile() }
        }

        pub fn write_byte(address: u64, value: u8) {
            unsafe { (address as *mut u8).write_volatile(value) }
        }

        pub fn discard(&self, offset: usize, len: usize) {
            let start = NonNull::new((self.address() as usize + offset) as *mut std::ffi::c_void).unwrap();
            unsafe { nix::sys::mman::madvise(start, len, nix::sys::mman::MmapAdvise::MADV_DONTNEED) }.unwrap();
        }

        pub fn register(&self) -> io::Result<Uffd> {
            let raw = userfaultfd();
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let mut handshake = UffdioApi {
                api: UFFD_API,
                features: UFFD_FEATURE_EVENT_REMOVE,
                ioctls: 0,
            };
            unsafe { api(std::os::fd::AsRawFd::as_raw_fd(&fd), &mut handshake) }?;
            let mut registration = UffdioRegister {
                start: self.address(),
                len: self.len as u64,
                mode: UFFDIO_REGISTER_MODE_MISSING,
                ioctls: 0,
            };
            unsafe { register(std::os::fd::AsRawFd::as_raw_fd(&fd), &mut registration) }?;
            Ok(Uffd::from_fd(fd))
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            let _ = unsafe { munmap(self.start, self.len) };
        }
    }

    fn userfaultfd() -> i32 {
        unsafe { nix::libc::syscall(nix::libc::SYS_userfaultfd, nix::libc::O_NONBLOCK | nix::libc::O_CLOEXEC) as i32 }
    }

    pub fn available() -> bool {
        nix::unistd::geteuid().is_root()
            || std::fs::read_to_string("/proc/sys/vm/unprivileged_userfaultfd").is_ok_and(|value| value.trim() == "1")
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::testing::{Pages, available};
    use super::*;

    const PAGE: u64 = 4096;

    fn next_events(uffd: &Uffd) -> Vec<Event> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let events = uffd.read_events().unwrap();
            if !events.is_empty() {
                return events;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("no uffd event within 5s");
    }

    #[test]
    fn copy_resolves_a_fault_from_another_thread() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let pages = Pages::map(4 * PAGE as usize);
        let uffd = pages.register().unwrap();
        let faulting = pages.address() + 2 * PAGE + 7;

        let reader = std::thread::spawn(move || Pages::read_byte(faulting));
        let events = next_events(&uffd);

        assert_eq!(
            events,
            vec![Event::PageFault {
                address: pages.address() + 2 * PAGE,
                write: false
            }]
        );
        let source = vec![0xAB; PAGE as usize];
        assert_eq!(
            uffd.copy(pages.address() + 2 * PAGE, &source).unwrap(),
            CopyOutcome::Copied
        );
        assert_eq!(reader.join().unwrap(), 0xAB);
    }

    #[test]
    fn copying_a_present_page_reports_already_present() {
        if !available() {
            eprintln!("skipping: userfaultfd needs root");
            return;
        }
        let pages = Pages::map(2 * PAGE as usize);
        let uffd = pages.register().unwrap();
        let source = vec![1; PAGE as usize];
        assert_eq!(uffd.copy(pages.address(), &source).unwrap(), CopyOutcome::Copied);
        assert_eq!(
            uffd.copy(pages.address(), &source).unwrap(),
            CopyOutcome::AlreadyPresent
        );
        uffd.wake(pages.address(), PAGE).unwrap();
    }

    #[test]
    fn unknown_events_are_errors() {
        let mut message = [0u8; UFFD_MSG_SIZE];
        message[0] = 0x13;
        assert_eq!(
            parse_events(&message).unwrap_err().to_string(),
            "unexpected uffd event 0x13"
        );
    }
}
