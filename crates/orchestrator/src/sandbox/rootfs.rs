pub mod envd_swap;
mod jail;
pub mod recovery;

use tokio::io::{AsyncRead, AsyncReadExt};

const NBD_DEVICE_PREFIX: &str = "/dev/nbd";

pub fn is_nbd_device(path: &str) -> bool {
    path.strip_prefix(NBD_DEVICE_PREFIX)
        .is_some_and(|index| !index.is_empty() && index.bytes().all(|byte| byte.is_ascii_digit()))
}

async fn read_capped(mut stream: impl AsyncRead + Unpin, limit: u64) -> Vec<u8> {
    let mut kept = Vec::new();
    let _ = (&mut stream).take(limit).read_to_end(&mut kept).await;
    let _ = tokio::io::copy(&mut stream, &mut tokio::io::sink()).await;
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_numbered_nbd_nodes_are_devices() {
        for path in ["/dev/nbd0", "/dev/nbd127"] {
            assert!(is_nbd_device(path), "{path}");
        }
        for path in [
            "/dev/sda",
            "/dev/nbd",
            "/dev/nbd1p1",
            "/dev/nbd0/../sda",
            "dev/nbd0",
            "/dev/nbd-1",
        ] {
            assert!(!is_nbd_device(path), "{path}");
        }
    }

    #[tokio::test]
    async fn a_capped_read_keeps_the_head_and_drains_the_rest() {
        let stream: &[u8] = b"0123456789";

        assert_eq!(read_capped(stream, 4).await, b"0123");
    }
}
