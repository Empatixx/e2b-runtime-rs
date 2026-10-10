use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::future::BoxFuture;

use super::Sandbox;
use crate::firecracker::client::{BalloonCaps, FirecrackerClient};
use crate::memory::balloon_mode::BalloonMode;

const READ_TIMEOUT: Duration = Duration::from_secs(5);
const RESOLVE_TIMEOUT: Duration = Duration::from_millis(500);
const RETRY_AFTER: Duration = Duration::from_secs(30);

pub type CapsReader = Arc<dyn Fn() -> BoxFuture<'static, Result<BalloonCaps, String>> + Send + Sync>;

pub struct BalloonProbe {
    mode: AtomicU8,
    retry_at: AtomicU64,
    reader: OnceLock<CapsReader>,
}

impl Default for BalloonProbe {
    fn default() -> Self {
        Self::new(None)
    }
}

pub fn cached_reader(client: FirecrackerClient) -> CapsReader {
    let client = Arc::new(client);
    let cached = Arc::new(tokio::sync::OnceCell::new());
    Arc::new(move || {
        let client = client.clone();
        let cached = cached.clone();
        Box::pin(async move { cached.get_or_try_init(|| client.balloon_caps()).await.copied() })
    })
}

pub fn mode_of(caps: BalloonCaps) -> BalloonMode {
    match caps {
        BalloonCaps { reporting: true, .. } => BalloonMode::Reporting,
        BalloonCaps { hinting: true, .. } => BalloonMode::Hinting,
        BalloonCaps { .. } => BalloonMode::None,
    }
}

impl BalloonProbe {
    pub fn new(reader: Option<CapsReader>) -> Self {
        Self {
            mode: AtomicU8::new(BalloonMode::Unknown.raw()),
            retry_at: AtomicU64::new(0),
            reader: reader.map(OnceLock::from).unwrap_or_default(),
        }
    }

    pub fn install_reader(&self, reader: CapsReader) {
        let _ = self.reader.set(reader);
    }

    pub async fn caps(&self) -> Result<BalloonCaps, String> {
        match self.reader.get() {
            Some(reader) => reader().await,
            None => Err("firecracker is not running".to_owned()),
        }
    }

    pub fn stamp(&self, mode: BalloonMode) {
        self.mode.store(mode.raw(), Ordering::Relaxed);
    }

    pub fn value(&self) -> BalloonMode {
        BalloonMode::from_raw(self.mode.load(Ordering::Relaxed))
    }

    pub fn label(&self) -> &'static str {
        self.value().as_str()
    }

    pub async fn prime(&self) {
        let Some(reader) = self.reader.get() else { return };
        let stamped = self.value();
        match self.read(reader, READ_TIMEOUT).await {
            Err(e) => tracing::warn!(error = %e, stamped = stamped.as_str(), "balloon mode unread after start"),
            Ok(mode) if stamped != BalloonMode::Unknown && stamped != mode => tracing::warn!(
                stamped = stamped.as_str(),
                device = mode.as_str(),
                "balloon mode stamp disagrees with the device; device wins"
            ),
            Ok(_) => {}
        }
    }

    pub async fn resolve(&self) -> BalloonMode {
        let Some(reader) = self.reader.get() else {
            return BalloonMode::Unknown;
        };
        let retry_at = self.retry_at.load(Ordering::Relaxed);
        if retry_at != 0 && now_nanos() < retry_at {
            return BalloonMode::Unknown;
        }
        let stamped = self.value();
        match self.read(reader, RESOLVE_TIMEOUT).await {
            Err(e) => {
                self.retry_at
                    .store(now_nanos() + RETRY_AFTER.as_nanos() as u64, Ordering::Relaxed);
                tracing::warn!(error = %e, "balloon mode unread at checkpoint; treated as unknown");
                BalloonMode::Unknown
            }
            Ok(mode) => {
                if stamped != BalloonMode::Unknown && stamped != mode {
                    tracing::warn!(
                        stamped = stamped.as_str(),
                        device = mode.as_str(),
                        "balloon mode stamp disagrees with the device; device wins"
                    );
                }
                mode
            }
        }
    }

    async fn read(&self, reader: &CapsReader, timeout: Duration) -> Result<BalloonMode, String> {
        let caps = tokio::time::timeout(timeout, reader())
            .await
            .unwrap_or_else(|_| Err("context deadline exceeded".to_owned()))?;
        let mode = mode_of(caps);
        self.stamp(mode);
        self.retry_at.store(0, Ordering::Relaxed);
        Ok(mode)
    }

    #[cfg(test)]
    pub fn expire_retry_window(&self) {
        self.retry_at.store(1, Ordering::Relaxed);
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |elapsed| elapsed.as_nanos() as u64)
        .max(1)
}

impl Sandbox {
    pub fn stamp_balloon_mode(&self, mode: BalloonMode) {
        self.balloon.stamp(mode);
        self.label_memory_with_balloon_mode();
    }

    pub fn balloon_mode(&self) -> &'static str {
        self.balloon.label()
    }

    pub async fn prime_balloon_mode(&self) {
        self.balloon.prime().await;
        self.label_memory_with_balloon_mode();
    }

    pub async fn resolve_balloon_mode(&self) -> BalloonMode {
        let mode = self.balloon.resolve().await;
        self.label_memory_with_balloon_mode();
        mode
    }

    pub(super) fn label_memory_with_balloon_mode(&self) {
        if let Some(memory) = &self.resources.memory {
            memory.set_balloon_mode(self.balloon.value());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU32;

    use super::*;

    fn probe(
        read: impl Fn(u32) -> Result<BalloonCaps, String> + Send + Sync + 'static,
    ) -> (BalloonProbe, Arc<AtomicU32>) {
        let reads = Arc::new(AtomicU32::new(0));
        let counted = reads.clone();
        let reader: CapsReader = Arc::new(move || {
            let read_number = counted.fetch_add(1, Ordering::SeqCst) + 1;
            let outcome = read(read_number);
            Box::pin(async move { outcome })
        });
        (BalloonProbe::new(Some(reader)), reads)
    }

    fn caps(reporting: bool, hinting: bool) -> BalloonCaps {
        BalloonCaps { reporting, hinting }
    }

    #[test]
    fn reporting_wins_because_it_discards_on_its_own() {
        assert_eq!(mode_of(caps(true, true)), BalloonMode::Reporting);
        assert_eq!(mode_of(caps(false, true)), BalloonMode::Hinting);
        assert_eq!(mode_of(caps(false, false)), BalloonMode::None);
    }

    #[tokio::test]
    async fn a_sandbox_without_a_device_read_keeps_the_unknown_label() {
        let probe = BalloonProbe::default();

        probe.prime().await;
        assert_eq!(probe.label(), "unknown");

        probe.stamp(BalloonMode::Hinting);
        assert_eq!(probe.label(), "hinting");
    }

    #[tokio::test]
    async fn resolving_reads_the_device_and_stamps_the_answer() {
        let (probe, reads) = probe(|_| Ok(caps(false, true)));

        assert_eq!(probe.resolve().await, BalloonMode::Hinting);
        assert_eq!(probe.label(), "hinting");
        assert_eq!(probe.resolve().await, BalloonMode::Hinting);
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_failed_read_stays_unknown_and_leaves_the_stamp_alone() {
        let (probe, _) = probe(|_| Err("api busy".to_owned()));
        probe.stamp(BalloonMode::None);

        assert_eq!(probe.resolve().await, BalloonMode::Unknown);
        assert_eq!(probe.label(), "none");
    }

    #[tokio::test]
    async fn the_device_corrects_a_contradicting_stamp() {
        let (probe, _) = probe(|_| Ok(caps(true, false)));
        probe.stamp(BalloonMode::Hinting);

        assert_eq!(probe.resolve().await, BalloonMode::Reporting);
        assert_eq!(probe.label(), "reporting");
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_read_is_bounded_by_the_resolve_timeout() {
        let reader: CapsReader = Arc::new(|| Box::pin(std::future::pending()));
        let probe = BalloonProbe::new(Some(reader));
        let started = tokio::time::Instant::now();

        assert_eq!(probe.resolve().await, BalloonMode::Unknown);

        assert!(started.elapsed() < 2 * RESOLVE_TIMEOUT);
    }

    #[tokio::test]
    async fn priming_reads_the_device_even_when_a_stamp_exists_and_corrects_it() {
        let (probe, reads) = probe(|_| Ok(caps(true, false)));
        probe.stamp(BalloonMode::Hinting);

        probe.prime().await;

        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the device is read even though a stamp exists"
        );
        assert_eq!(probe.label(), "reporting");
    }

    #[tokio::test]
    async fn a_failed_checkpoint_read_is_not_repeated_until_the_retry_window_passes() {
        let (probe, reads) = probe(|_| Err("api busy".to_owned()));

        assert_eq!(probe.resolve().await, BalloonMode::Unknown);
        assert_eq!(probe.resolve().await, BalloonMode::Unknown);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "the second resolve inside the retry window reads nothing"
        );

        probe.expire_retry_window();
        assert_eq!(probe.resolve().await, BalloonMode::Unknown);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "past the window the device is asked again"
        );
    }

    #[tokio::test]
    async fn a_successful_prime_reopens_the_checkpoint_read() {
        let (probe, _) = probe(|read| {
            if read == 1 {
                Err("api busy".to_owned())
            } else {
                Ok(caps(false, true))
            }
        });

        assert_eq!(probe.resolve().await, BalloonMode::Unknown);
        probe.prime().await;

        assert_eq!(
            probe.resolve().await,
            BalloonMode::Hinting,
            "the prime's success reopens the checkpoint read"
        );
    }
}
