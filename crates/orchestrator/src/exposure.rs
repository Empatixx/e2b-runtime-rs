pub mod block_device;
pub mod nbd;

use std::path::Path;
use std::sync::OnceLock;

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
    let chosen = *TRANSPORT.get_or_init(|| transport);
    (chosen == transport)
        .then_some(())
        .ok_or_else(|| format!("rootfs exposure is already {chosen:?}"))
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
