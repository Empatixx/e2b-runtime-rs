pub mod hosts;
pub mod mmds;

use chrono::{DateTime, Duration, Utc};
use sha2::{Digest, Sha512};

pub fn hash_access_token(token: &str) -> String {
    hex::encode(Sha512::digest(token.as_bytes()))
}

pub fn sync_clock(host_time: DateTime<Utc>) {
    let now = Utc::now();
    let behind = now < host_time - Duration::milliseconds(50);
    let ahead = now > host_time + Duration::seconds(5);
    if behind || ahead {
        set_clock(host_time);
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
}
