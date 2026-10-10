use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::FutureExt;
use parking_lot::Mutex;

use super::Cache;
use super::probe::{FnProbe, Probe, ProbeFailure};
use super::scope::Scope;

pub const ELF_HEADER_LEN: usize = 64;

pub fn elf_with_payload(payload: &str) -> Vec<u8> {
    let mut header = vec![0u8; ELF_HEADER_LEN];
    header[..8].copy_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    header[16..18].copy_from_slice(&2u16.to_le_bytes());
    header[18..20].copy_from_slice(&0x3eu16.to_le_bytes());
    header[20..24].copy_from_slice(&1u32.to_le_bytes());
    header[52..54].copy_from_slice(&(ELF_HEADER_LEN as u16).to_le_bytes());
    header[54..56].copy_from_slice(&56u16.to_le_bytes());
    header[58..60].copy_from_slice(&64u16.to_le_bytes());
    header.extend_from_slice(payload.as_bytes());
    header
}

pub fn mtime(seconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds)
}

pub fn write_source(path: &Path, content: &str, modified: SystemTime) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, elf_with_payload(content)).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    stamp(path, modified);
}

pub fn stamp(path: &Path, modified: SystemTime) {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

pub fn write_raw(path: &Path, bytes: &[u8], modified: SystemTime) {
    std::fs::write(path, bytes).unwrap();
    stamp(path, modified);
}

pub fn payload_of(path: &Path) -> String {
    let bytes = std::fs::read(path).unwrap();
    assert!(bytes.len() >= ELF_HEADER_LEN, "not an image this suite wrote");
    String::from_utf8(bytes[ELF_HEADER_LEN..].to_vec()).unwrap()
}

pub fn not_executable() -> ProbeFailure {
    ProbeFailure::NotExecutable("fork/exec: exec format error".to_owned())
}

pub struct CountingProbe {
    calls: Mutex<Vec<PathBuf>>,
    version: Mutex<String>,
    fail_once: AtomicBool,
    fail_path: Mutex<Option<PathBuf>>,
}

impl CountingProbe {
    pub fn new(version: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::default(),
            version: Mutex::new(version.to_owned()),
            fail_once: AtomicBool::new(false),
            fail_path: Mutex::new(None),
        })
    }

    pub fn set_version(&self, version: &str) {
        *self.version.lock() = version.to_owned();
    }

    pub fn fail_once(&self) {
        self.fail_once.store(true, Ordering::SeqCst);
    }

    pub fn fail_path(&self, path: PathBuf) {
        *self.fail_path.lock() = Some(path);
    }

    pub fn count(&self) -> usize {
        self.calls.lock().len()
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.calls.lock().clone()
    }

    pub fn probe(self: &Arc<Self>) -> Arc<dyn Probe> {
        let this = self.clone();
        FnProbe::shared(move |_scope, path| {
            this.calls.lock().push(path.clone());
            let failing_path = this.fail_path.lock().as_ref().is_some_and(|wanted| *wanted == path);
            let result = match failing_path || this.fail_once.swap(false, Ordering::SeqCst) {
                true => Err(not_executable()),
                false => Ok(this.version.lock().clone()),
            };
            async move { result }.boxed()
        })
    }
}

pub fn ok_probe(version: &'static str) -> Arc<dyn Probe> {
    FnProbe::shared(move |_scope, _path| async move { Ok(version.to_owned()) }.boxed())
}

pub fn content_probe() -> Arc<dyn Probe> {
    FnProbe::shared(|_scope, path| {
        async move {
            let bytes = std::fs::read(&path).map_err(|e| ProbeFailure::Other(e.to_string()))?;
            let payload = bytes.get(ELF_HEADER_LEN..).unwrap_or(&bytes);
            Ok(String::from_utf8_lossy(payload).trim().to_owned())
        }
        .boxed()
    })
}

pub fn bare_cache() -> Arc<Cache> {
    Arc::new(Cache::builder().dir("").probe(ok_probe("0.0.0")).build())
}

pub struct Drained(pub Arc<Cache>);

impl Drop for Drained {
    fn drop(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while std::time::Instant::now() < deadline {
            let idle = {
                let state = self.0.state.lock();
                state.warming.is_empty() && state.in_flight.is_empty()
            };
            if idle {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("background warms must finish before the temp dir is removed");
    }
}

pub struct Rig {
    pub cache: Arc<Cache>,
    pub dir: PathBuf,
    pub src: PathBuf,
    _drain: Drained,
    _holder: tempfile::TempDir,
}

impl Rig {
    pub fn cache_dir(&self) -> PathBuf {
        self.dir.join("cache")
    }

    pub fn with(probe: Arc<dyn Probe>) -> Self {
        Self::with_cache(|base| Cache::builder().dir(base.join("cache")).probe(probe).build())
    }

    pub fn with_cache(make: impl FnOnce(&Path) -> Cache) -> Self {
        let holder = tempfile::tempdir().unwrap();
        let dir = holder.path().to_path_buf();
        let cache = Arc::new(make(&dir));
        Self {
            _drain: Drained(cache.clone()),
            cache,
            src: dir.join("envd"),
            dir,
            _holder: holder,
        }
    }
}

pub async fn eventually(mut condition: impl FnMut() -> bool, within: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + within;
    loop {
        if condition() {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn background() -> Scope {
    Scope::background()
}

type StatFn = dyn Fn(&Path) -> std::io::Result<super::identity::Identity> + Send + Sync;

pub struct FnStat(Box<StatFn>);

impl FnStat {
    pub fn shared(
        stat: impl Fn(&Path) -> std::io::Result<super::identity::Identity> + Send + Sync + 'static,
    ) -> Arc<dyn super::source::SourceStat> {
        Arc::new(Self(Box::new(stat)))
    }
}

impl super::source::SourceStat for FnStat {
    fn stat(&self, path: &Path) -> std::io::Result<super::identity::Identity> {
        (self.0)(path)
    }
}

pub fn real_stat(path: &Path) -> std::io::Result<super::identity::Identity> {
    super::source::SourceStat::stat(&super::source::Filesystem, path)
}

pub struct Gate {
    open: Mutex<bool>,
    opened: parking_lot::Condvar,
}

impl Gate {
    pub fn closed() -> Arc<Self> {
        Arc::new(Self {
            open: Mutex::new(false),
            opened: parking_lot::Condvar::new(),
        })
    }

    pub fn wait(&self) {
        let mut open = self.open.lock();
        while !*open {
            self.opened.wait(&mut open);
        }
    }

    pub fn open(&self) {
        *self.open.lock() = true;
        self.opened.notify_all();
    }
}

pub struct StalledStat {
    pub calls: std::sync::atomic::AtomicUsize,
    gate: Arc<Gate>,
    fail_on_release: AtomicBool,
}

impl StalledStat {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            gate: Gate::closed(),
            fail_on_release: AtomicBool::new(false),
        })
    }

    pub fn source(self: &Arc<Self>) -> Arc<dyn super::source::SourceStat> {
        let this = Arc::downgrade(self);
        FnStat::shared(move |path| {
            if let Some(gate) = this.upgrade().map(|stalled| stalled.enter()) {
                gate.wait();
            }
            match this
                .upgrade()
                .is_some_and(|stalled| !stalled.fail_on_release.load(Ordering::SeqCst))
            {
                true => real_stat(path),
                false => Err(std::io::Error::other("released by test cleanup")),
            }
        })
    }

    fn enter(&self) -> Arc<Gate> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.gate.clone()
    }

    pub fn unblock(&self) {
        self.gate.open();
    }

    pub fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn fail_when_released(&self) {
        self.fail_on_release.store(true, Ordering::SeqCst);
    }
}

impl Drop for StalledStat {
    fn drop(&mut self) {
        self.fail_on_release.store(true, Ordering::SeqCst);
        self.gate.open();
    }
}

pub fn write_generation(path: &Path, generation: u64) {
    let mut staged = path.as_os_str().to_owned();
    staged.push(".staging");
    let staged = PathBuf::from(staged);
    write_source(&staged, &format!("v{generation}"), mtime(1_700_000_000 + generation));
    std::fs::rename(&staged, path).unwrap();
}

pub struct OnDrop(Option<Box<dyn FnOnce() + Send>>);

impl OnDrop {
    pub fn run(action: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(Box::new(action)))
    }
}

impl Drop for OnDrop {
    fn drop(&mut self) {
        if let Some(action) = self.0.take() {
            action();
        }
    }
}
