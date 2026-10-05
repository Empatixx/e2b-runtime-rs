//! Things envd does to the guest VM itself (upstream `internal/host`).

pub mod hosts;
pub mod mmds;

use sha2::{Digest, Sha512};

/// Same as upstream `keys.HashAccessToken`: hex(sha512(token)).
pub fn hash_access_token(token: &str) -> String {
    hex::encode(Sha512::digest(token.as_bytes()))
}

/// Sets the guest clock if it is >50 ms behind or >5 s ahead of the host (upstream `shouldSetSystemTime`).
pub fn sync_clock(host_time: chrono::DateTime<chrono::Utc>) {
    let now = chrono::Utc::now();
    let behind = now < host_time - chrono::Duration::milliseconds(50);
    let ahead = now > host_time + chrono::Duration::seconds(5);
    if !(behind || ahead) {
        return;
    }
    #[cfg(target_os = "linux")]
    {
        let ts = nix::sys::time::TimeSpec::new(host_time.timestamp(), host_time.timestamp_subsec_nanos() as _);
        if let Err(e) = nix::time::clock_settime(nix::time::ClockId::CLOCK_REALTIME, ts) {
            tracing::error!(error = %e, "Failed to set system time");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_matches_go_sha512_hex() {
        assert_eq!(
            hash_access_token(""),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }
}
