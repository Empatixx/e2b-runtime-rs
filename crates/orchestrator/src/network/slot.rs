use std::net::Ipv4Addr;

#[cfg(target_os = "linux")]
mod host;

#[cfg(target_os = "linux")]
pub use host::{HostSlots, host_slot_storage};

pub const MAX_SLOT: u32 = 32766;
pub const TAP_NAME: &str = "tap0";
pub const TAP_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 22);
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 21);
pub const GUEST_MAC: &str = "02:FC:00:00:00:05";
pub const TAP_MAC: [u8; 6] = [0x02, 0xFC, 0x00, 0x00, 0x00, 0x06];

const VETH_BASE: Ipv4Addr = Ipv4Addr::new(10, 12, 0, 0);
const HOST_IP_BASE: Ipv4Addr = Ipv4Addr::new(10, 11, 0, 0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub index: u32,
}

impl Slot {
    pub fn new(index: u32) -> Result<Self, String> {
        if !(1..=MAX_SLOT).contains(&index) {
            return Err(format!("slot index {index} is outside 1..={MAX_SLOT}"));
        }
        Ok(Self { index })
    }

    pub fn namespace(&self) -> String {
        format!("ns-{}", self.index)
    }

    pub fn host_veth(&self) -> String {
        format!("veth-{}", self.index)
    }

    pub fn host_veth_ip(&self) -> Ipv4Addr {
        offset(VETH_BASE, 2 * self.index)
    }

    pub fn peer_veth_ip(&self) -> Ipv4Addr {
        offset(VETH_BASE, 2 * self.index + 1)
    }

    pub fn host_ip(&self) -> Ipv4Addr {
        offset(HOST_IP_BASE, self.index)
    }
}

fn offset(base: Ipv4Addr, by: u32) -> Ipv4Addr {
    Ipv4Addr::from(u32::from(base) + by)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_for_the_first_slot() {
        let slot = Slot::new(1).unwrap();
        assert_eq!(slot.namespace(), "ns-1");
        assert_eq!(slot.host_veth(), "veth-1");
        assert_eq!(slot.host_veth_ip(), Ipv4Addr::new(10, 12, 0, 2));
        assert_eq!(slot.peer_veth_ip(), Ipv4Addr::new(10, 12, 0, 3));
        assert_eq!(slot.host_ip(), Ipv4Addr::new(10, 11, 0, 1));
    }

    #[test]
    fn addresses_cross_octets_and_stay_short() {
        let slot = Slot::new(300).unwrap();
        assert_eq!(slot.host_veth_ip(), Ipv4Addr::new(10, 12, 2, 88));
        assert_eq!(slot.peer_veth_ip(), Ipv4Addr::new(10, 12, 2, 89));
        assert_eq!(slot.host_ip(), Ipv4Addr::new(10, 11, 1, 44));

        let last = Slot::new(MAX_SLOT).unwrap();
        assert_eq!(last.peer_veth_ip(), Ipv4Addr::new(10, 12, 255, 253));
        assert!(last.host_veth().len() <= 15);
    }

    #[test]
    fn rejects_out_of_range_indices() {
        assert_eq!(Slot::new(0).unwrap_err(), "slot index 0 is outside 1..=32766");
        assert_eq!(Slot::new(32767).unwrap_err(), "slot index 32767 is outside 1..=32766");
    }
}
