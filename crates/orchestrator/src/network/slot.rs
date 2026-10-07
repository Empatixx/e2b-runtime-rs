use std::net::Ipv4Addr;

pub const MAX_SLOT: u32 = 16383;
pub const TAP_NAME: &str = "tap0";
pub const TAP_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 22);
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 21);
pub const GUEST_MAC: &str = "02:FC:00:00:00:05";

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

#[cfg(target_os = "linux")]
impl Slot {
    pub fn create(&self) -> Result<(), String> {
        crate::system::run("ip", &["netns", "add", &self.namespace()])?;
        self.configure().inspect_err(|_| {
            let _ = self.destroy();
        })
    }

    pub fn destroy(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let _ = crate::system::run("iptables", &["-D", "FORWARD", "-i", &veth, "-j", "ACCEPT"]);
        let _ = crate::system::run("iptables", &["-D", "FORWARD", "-o", &veth, "-j", "ACCEPT"]);
        crate::system::run("ip", &["netns", "delete", &self.namespace()])
    }

    fn configure(&self) -> Result<(), String> {
        self.create_veth()?;
        self.create_tap()?;
        self.route_and_nat()
    }

    fn create_veth(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let namespace = self.namespace();
        let host_cidr = format!("{}/31", self.host_veth_ip());
        let peer_cidr = format!("{}/31", self.peer_veth_ip());
        crate::system::run(
            "ip",
            &[
                "link", "add", &veth, "type", "veth", "peer", "name", "eth0", "netns", &namespace,
            ],
        )?;
        crate::system::run("ip", &["addr", "add", &host_cidr, "dev", &veth])?;
        crate::system::run("ip", &["link", "set", &veth, "up"])?;
        self.inside(&["ip", "addr", "add", &peer_cidr, "dev", "eth0"])?;
        self.inside(&["ip", "link", "set", "eth0", "up"])?;
        self.inside(&["ip", "link", "set", "lo", "up"])
    }

    fn create_tap(&self) -> Result<(), String> {
        let tap_cidr = format!("{TAP_IP}/30");
        self.inside(&["ip", "tuntap", "add", TAP_NAME, "mode", "tap"])?;
        self.inside(&["ip", "addr", "add", &tap_cidr, "dev", TAP_NAME])?;
        self.inside(&["ip", "link", "set", TAP_NAME, "up"])
    }

    fn route_and_nat(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let host_ip = self.host_ip().to_string();
        let guest_ip = GUEST_IP.to_string();
        let host_veth_ip = self.host_veth_ip().to_string();
        let peer_veth_ip = self.peer_veth_ip().to_string();
        self.inside(&["sysctl", "-qw", "net.ipv4.ip_forward=1"])?;
        self.inside(&["ip", "route", "add", "default", "via", &host_veth_ip])?;
        self.inside(&[
            "iptables",
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            "eth0",
            "-s",
            &guest_ip,
            "-j",
            "SNAT",
            "--to",
            &host_ip,
        ])?;
        self.inside(&[
            "iptables",
            "-t",
            "nat",
            "-A",
            "PREROUTING",
            "-i",
            "eth0",
            "-d",
            &host_ip,
            "-j",
            "DNAT",
            "--to",
            &guest_ip,
        ])?;
        crate::system::run("sysctl", &["-qw", "net.ipv4.ip_forward=1"])?;
        crate::system::run("ip", &["route", "add", &host_ip, "via", &peer_veth_ip, "dev", &veth])?;
        crate::system::run("iptables", &["-I", "FORWARD", "-i", &veth, "-j", "ACCEPT"])?;
        crate::system::run("iptables", &["-I", "FORWARD", "-o", &veth, "-j", "ACCEPT"])
    }

    fn inside(&self, command: &[&str]) -> Result<(), String> {
        let namespace = self.namespace();
        let args: Vec<&str> = ["netns", "exec", namespace.as_str()]
            .into_iter()
            .chain(command.iter().copied())
            .collect();
        crate::system::run("ip", &args)
    }
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
        assert_eq!(last.peer_veth_ip(), Ipv4Addr::new(10, 12, 127, 255));
        assert!(last.host_veth().len() <= 15);
    }

    #[test]
    fn rejects_out_of_range_indices() {
        assert_eq!(Slot::new(0).unwrap_err(), "slot index 0 is outside 1..=16383");
        assert_eq!(Slot::new(16384).unwrap_err(), "slot index 16384 is outside 1..=16383");
    }
}
