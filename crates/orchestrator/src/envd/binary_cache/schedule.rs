use std::path::Path;
use std::time::Instant;

use super::identity::Identity;
use super::{BACKOFF_BASE, BACKOFF_CEILING, Cache};

const MAX_DOUBLINGS: u32 = 30;

#[derive(Default)]
pub struct PathBackoff {
    pub failures: u32,
    pub until: Option<Instant>,
    pub bad: Option<Identity>,
    pub warned_at: Option<Instant>,
    pub stall_warned_at: Option<Instant>,
}

impl Cache {
    pub(crate) fn schedule_suppressed(&self, source: &Path) -> bool {
        self.state
            .lock()
            .backoff
            .get(source)
            .and_then(|backoff| backoff.until)
            .is_some_and(|until| Instant::now() < until)
    }

    pub(crate) fn identity_known_bad(&self, source: &Path, identity: Identity) -> bool {
        self.state
            .lock()
            .backoff
            .get(source)
            .is_some_and(|backoff| backoff.bad == Some(identity))
    }

    pub(crate) fn note_failure(&self, source: &Path) {
        let mut state = self.state.lock();
        let backoff = state.backoff.entry(source.to_path_buf()).or_default();
        backoff.failures += 1;
        backoff.until = Some(Instant::now() + delay_after(backoff.failures));
    }

    pub(crate) fn mark_bad_identity(&self, source: &Path, identity: Identity) {
        let mut state = self.state.lock();
        let backoff = state.backoff.entry(source.to_path_buf()).or_default();
        backoff.bad = Some(identity);
        backoff.failures = 0;
        backoff.until = None;
    }

    pub(crate) fn note_success(&self, source: &Path) {
        self.state.lock().backoff.remove(source);
    }

    pub(crate) fn should_warn(&self, source: &Path) -> bool {
        self.warn_window(source, |backoff| &mut backoff.warned_at)
    }

    pub(crate) fn should_warn_stall(&self, source: &Path) -> bool {
        self.warn_window(source, |backoff| &mut backoff.stall_warned_at)
    }

    fn warn_window(&self, source: &Path, stamp: impl FnOnce(&mut PathBackoff) -> &mut Option<Instant>) -> bool {
        let mut state = self.state.lock();
        let stamp = stamp(state.backoff.entry(source.to_path_buf()).or_default());
        let now = Instant::now();
        if stamp.is_some_and(|warned| now.duration_since(warned) < BACKOFF_CEILING) {
            return false;
        }
        *stamp = Some(now);
        true
    }
}

fn delay_after(failures: u32) -> std::time::Duration {
    match failures <= MAX_DOUBLINGS {
        true => BACKOFF_BASE.saturating_mul(1u32 << (failures - 1)).min(BACKOFF_CEILING),
        false => BACKOFF_CEILING,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::super::testing::bare_cache;
    use super::*;

    #[tokio::test]
    async fn the_schedule_doubles_from_the_base_and_stops_at_the_ceiling() {
        let cache = bare_cache();
        let source = Path::new("/fc-envd/envd");

        let mut delays = Vec::new();
        for _ in 0..12 {
            let before = Instant::now();
            cache.note_failure(source);
            let until = cache.state.lock().backoff[source].until.unwrap();
            delays.push(Duration::from_secs(
                until.duration_since(before).as_secs_f64().round() as u64
            ));
        }

        assert_eq!(delays[0], BACKOFF_BASE, "the first retry waits the base");
        assert_eq!(delays[1], 2 * BACKOFF_BASE);
        assert_eq!(delays[2], 4 * BACKOFF_BASE);
        assert!(delays.iter().all(|delay| *delay <= BACKOFF_CEILING));
        assert_eq!(
            *delays.last().unwrap(),
            BACKOFF_CEILING,
            "it must reach and hold the ceiling"
        );

        cache.note_success(source);
        assert!(!cache.schedule_suppressed(source));
    }

    #[test]
    fn the_delay_never_wraps_past_the_ceiling() {
        for failures in [29, 30, 31, 33, 34, 64, 1000] {
            assert!(delay_after(failures) <= BACKOFF_CEILING, "{failures}");
            assert!(delay_after(failures) >= BACKOFF_BASE, "{failures}");
        }
    }

    #[tokio::test]
    async fn the_failure_log_is_rate_limited_per_path() {
        let cache = bare_cache();

        assert!(cache.should_warn(Path::new("/a")), "the first failure of a path speaks");
        assert!(
            !cache.should_warn(Path::new("/a")),
            "the second inside the window does not"
        );
        assert!(
            cache.should_warn(Path::new("/b")),
            "a different path is not rate limited by /a"
        );

        cache.state.lock().backoff.get_mut(Path::new("/a")).unwrap().warned_at =
            Some(Instant::now() - 2 * BACKOFF_CEILING);
        assert!(cache.should_warn(Path::new("/a")));
        assert!(!cache.should_warn(Path::new("/b")));
    }

    #[tokio::test]
    async fn a_stall_warns_on_its_own_window() {
        let cache = bare_cache();

        assert!(cache.should_warn_stall(Path::new("/src")));
        assert!(
            !cache.should_warn_stall(Path::new("/src")),
            "stall warnings are rate-limited"
        );
        assert!(
            cache.should_warn(Path::new("/src")),
            "a failure after a stall must still get its Warn"
        );
    }

    #[tokio::test]
    async fn a_bad_identity_is_remembered_per_path_and_cleared_by_a_success() {
        let cache = bare_cache();
        let identity = Identity {
            size: 9,
            modified: std::time::UNIX_EPOCH + Duration::from_secs(5),
        };
        let other = Identity { size: 10, ..identity };

        cache.mark_bad_identity(Path::new("/a"), identity);

        assert!(cache.identity_known_bad(Path::new("/a"), identity));
        assert!(
            !cache.identity_known_bad(Path::new("/a"), other),
            "a promotion is a different identity"
        );
        assert!(!cache.identity_known_bad(Path::new("/b"), identity));
        assert!(
            !cache.schedule_suppressed(Path::new("/a")),
            "a verdict about the bytes arms no window"
        );

        cache.note_success(Path::new("/a"));
        assert!(!cache.identity_known_bad(Path::new("/a"), identity));
    }
}
