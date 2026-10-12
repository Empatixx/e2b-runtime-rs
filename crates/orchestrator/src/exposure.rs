pub mod block_device;
pub mod nbd;
#[cfg(target_os = "linux")]
pub mod ublk;

use std::path::Path;
use std::sync::OnceLock;

#[cfg(target_os = "linux")]
pub use linux::{Exposure, expose};

const UBLK_CONTROL: &str = "/dev/ublk-control";

static TRANSPORT: OnceLock<Transport> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    #[default]
    Nbd,
    Ublk,
}

impl Transport {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None | Some("nbd") => Ok(Self::Nbd),
            Some("ublk") => Ok(Self::Ublk),
            Some(other) => Err(format!("invalid ROOTFS_EXPOSURE \"{other}\": want nbd or ublk")),
        }
    }
}

pub fn use_transport(transport: Transport) -> Result<(), String> {
    available(transport, Path::new(UBLK_CONTROL))?;
    let chosen = *TRANSPORT.get_or_init(|| prepared(transport));
    (chosen == transport)
        .then_some(())
        .ok_or_else(|| format!("rootfs exposure is already {chosen:?}"))
}

fn prepared(transport: Transport) -> Transport {
    #[cfg(target_os = "linux")]
    if transport == Transport::Ublk {
        let removed = ublk::device::remove_orphans();
        if !removed.is_empty() {
            tracing::info!(?removed, "removed orphaned ublk devices");
        }
    }
    transport
}

pub fn transport() -> Transport {
    TRANSPORT.get().copied().unwrap_or_default()
}

fn available(transport: Transport, control: &Path) -> Result<(), String> {
    match transport == Transport::Ublk && !control.exists() {
        true => Err(format!(
            "ublk is not available: {} is missing, load the ublk_drv module",
            control.display()
        )),
        false => Ok(()),
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::{Transport, nbd, transport, ublk};
    use crate::block::overlay::Overlay;

    pub enum Exposure {
        Nbd(nbd::Exposure),
        Ublk(ublk::Exposure),
    }

    pub async fn expose(overlay: Arc<Overlay>) -> io::Result<Exposure> {
        match transport() {
            Transport::Nbd => nbd::expose(overlay).await.map(Exposure::Nbd),
            Transport::Ublk => ublk::expose(overlay).await.map(Exposure::Ublk),
        }
    }

    impl Exposure {
        pub fn path(&self) -> PathBuf {
            match self {
                Exposure::Nbd(exposure) => exposure.path(),
                Exposure::Ublk(exposure) => exposure.path(),
            }
        }

        pub async fn flush(&self) -> Result<(), String> {
            match self {
                Exposure::Nbd(exposure) => exposure.flush().await,
                Exposure::Ublk(exposure) => exposure.flush().await,
            }
        }

        pub async fn ended(&self) {
            match self {
                Exposure::Nbd(exposure) => exposure.ended().await,
                Exposure::Ublk(exposure) => exposure.ended().await,
            }
        }

        pub async fn close(self) -> Result<(), String> {
            match self {
                Exposure::Nbd(exposure) => exposure.close().await,
                Exposure::Ublk(exposure) => exposure.close().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rootfs_exposure_defaults_to_nbd_and_names_both_transports() {
        assert_eq!(Transport::parse(None), Ok(Transport::Nbd));
        assert_eq!(Transport::parse(Some("nbd")), Ok(Transport::Nbd));
        assert_eq!(Transport::parse(Some("ublk")), Ok(Transport::Ublk));
        assert_eq!(
            Transport::parse(Some("NBD")),
            Err("invalid ROOTFS_EXPOSURE \"NBD\": want nbd or ublk".to_owned())
        );
    }

    #[test]
    fn ublk_needs_the_control_device() {
        let missing = Path::new("/nonexistent/ublk-control");
        assert_eq!(
            available(Transport::Ublk, missing),
            Err("ublk is not available: /nonexistent/ublk-control is missing, load the ublk_drv module".to_owned())
        );
        assert_eq!(available(Transport::Nbd, missing), Ok(()));
    }
}
