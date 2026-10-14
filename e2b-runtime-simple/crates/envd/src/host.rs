pub mod cacerts;
pub mod collapse;
pub mod command;
pub mod fsfreeze;
pub mod hosts;
pub mod metrics;
pub mod mmds;
pub mod mmds_route;
pub mod nfs;
pub mod oom;

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha512};

pub fn hash_access_token(token: &str) -> String {
    hex::encode(Sha512::digest(token.as_bytes()))
}

pub trait GuestClock: Send + Sync {
    fn now(&self) -> std::time::Instant;
    fn step(&self, to: DateTime<Utc>);
}

pub struct SystemClock;

impl GuestClock for SystemClock {
    fn now(&self) -> std::time::Instant {
        std::time::Instant::now()
    }

    fn step(&self, to: DateTime<Utc>) {
        set_clock(to);
    }
}

pub fn sync_clock(clock: &dyn GuestClock, host_time: DateTime<Utc>) {
    let now = Utc::now();
    let behind = now < host_time - Duration::milliseconds(50);
    let ahead = now > host_time + Duration::seconds(5);
    if behind || ahead {
        clock.step(host_time);
    }
}

#[cfg(target_os = "linux")]
fn set_clock(time: DateTime<Utc>) {
    use nix::sys::time::TimeSpec;
    use nix::time::{ClockId, clock_settime};

    let ts = TimeSpec::new(time.timestamp(), time.timestamp_subsec_nanos().into());
    if let Err(e) = clock_settime(ClockId::CLOCK_REALTIME, ts) {
        tracing::error!(error = %e, "Failed to set system time");
    }
}

#[cfg(not(target_os = "linux"))]
fn set_clock(_time: DateTime<Utc>) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_hash_is_sha512_hex() {
        assert_eq!(
            hash_access_token(""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }

    struct Steps(parking_lot::Mutex<Vec<DateTime<Utc>>>);

    impl GuestClock for Steps {
        fn now(&self) -> std::time::Instant {
            std::time::Instant::now()
        }

        fn step(&self, to: DateTime<Utc>) {
            self.0.lock().push(to);
        }
    }

    fn steps_for(offset: Duration) -> usize {
        let clock = Steps(parking_lot::Mutex::default());
        sync_clock(&clock, Utc::now() + offset);
        clock.0.lock().len()
    }

    #[test]
    fn the_clock_steps_only_outside_the_bands() {
        assert_eq!(steps_for(Duration::milliseconds(10)), 0, "inside the 50 ms band");
        assert_eq!(
            steps_for(Duration::milliseconds(200)),
            1,
            "the host is ahead of the guest"
        );
        assert_eq!(
            steps_for(Duration::seconds(-4)),
            0,
            "the guest is ahead by less than 5 s"
        );
        assert_eq!(
            steps_for(Duration::seconds(-6)),
            1,
            "the guest is ahead by more than 5 s"
        );
    }
}
