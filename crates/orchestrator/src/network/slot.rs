use std::net::Ipv4Addr;

#[cfg(target_os = "linux")]
use futures::StreamExt;

#[cfg(target_os = "linux")]
use crate::system::{all_ok, output, run};

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
struct Rule {
    table: &'static str,
    chain: &'static str,
    spec: Vec<String>,
}

#[cfg(target_os = "linux")]
const BLOCKED_GUEST_DESTINATIONS: [&str; 4] = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "169.254.0.0/16"];

#[cfg(target_os = "linux")]
impl Slot {
    pub async fn create(&self) -> Result<(), String> {
        run("ip", &["netns", "add", &self.namespace()]).await?;
        if let Err(e) = self.configure().await {
            let _ = self.destroy().await;
            return Err(e);
        }
        Ok(())
    }

    pub async fn destroy(&self) -> Result<(), String> {
        all_ok([
            self.delete_host_rules().await,
            ignore_missing(
                run("ip", &["link", "delete", &self.host_veth()]).await,
                &["Cannot find device"],
            ),
            ignore_missing(
                run("ip", &["netns", "delete", &self.namespace()]).await,
                &["No such file or directory"],
            ),
        ])
    }

    async fn delete_host_rules(&self) -> Result<(), String> {
        let gateway = default_interface().await?;
        let results: Vec<Result<(), String>> = futures::stream::iter(self.host_rules(&gateway))
            .then(|rule| async move { delete_rule(&rule).await })
            .collect()
            .await;
        all_ok(results)
    }

    async fn configure(&self) -> Result<(), String> {
        self.create_veth().await?;
        self.create_tap().await?;
        self.guest_nat_and_firewall().await?;
        self.host_route_and_rules().await
    }

    async fn create_veth(&self) -> Result<(), String> {
        let veth = self.host_veth();
        let namespace = self.namespace();
        let host_cidr = format!("{}/31", self.host_veth_ip());
        let peer_cidr = format!("{}/31", self.peer_veth_ip());
        run(
            "ip",
            &[
                "link", "add", &veth, "type", "veth", "peer", "name", "eth0", "netns", &namespace,
            ],
        )
        .await?;
        run("ip", &["addr", "add", &host_cidr, "dev", &veth]).await?;
        run("ip", &["link", "set", &veth, "up"]).await?;
        self.inside(&["ip", "addr", "add", &peer_cidr, "dev", "eth0"]).await?;
        self.inside(&["ip", "link", "set", "eth0", "up"]).await?;
        self.inside(&["ip", "link", "set", "lo", "up"]).await
    }

    async fn create_tap(&self) -> Result<(), String> {
        let tap_cidr = format!("{TAP_IP}/30");
        self.inside(&["ip", "tuntap", "add", TAP_NAME, "mode", "tap"]).await?;
        self.inside(&["ip", "addr", "add", &tap_cidr, "dev", TAP_NAME]).await?;
        self.inside(&["ip", "link", "set", TAP_NAME, "up"]).await
    }

    async fn guest_nat_and_firewall(&self) -> Result<(), String> {
        let host_ip = self.host_ip().to_string();
        let guest_ip = GUEST_IP.to_string();
        let host_veth_ip = self.host_veth_ip().to_string();
        self.inside(&["sysctl", "-qw", "net.ipv4.ip_forward=1"]).await?;
        self.inside(&["ip", "route", "add", "default", "via", &host_veth_ip])
            .await?;
        self.iptables_inside(&[
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
        ])
        .await?;
        self.iptables_inside(&[
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
        ])
        .await?;
        self.iptables_inside(&[
            "-A",
            "FORWARD",
            "-i",
            TAP_NAME,
            "-m",
            "conntrack",
            "--ctstate",
            "ESTABLISHED,RELATED",
            "-j",
            "ACCEPT",
        ])
        .await?;
        self.iptables_inside(&["-A", "FORWARD", "-i", TAP_NAME, "!", "-s", &guest_ip, "-j", "DROP"])
            .await?;
        for destination in BLOCKED_GUEST_DESTINATIONS {
            self.iptables_inside(&["-A", "FORWARD", "-i", TAP_NAME, "-d", destination, "-j", "DROP"])
                .await?;
        }
        Ok(())
    }

    async fn host_route_and_rules(&self) -> Result<(), String> {
        let host_ip = self.host_ip().to_string();
        let peer_veth_ip = self.peer_veth_ip().to_string();
        run("sysctl", &["-qw", "net.ipv4.ip_forward=1"]).await?;
        run(
            "ip",
            &["route", "add", &host_ip, "via", &peer_veth_ip, "dev", &self.host_veth()],
        )
        .await?;
        let gateway = default_interface().await?;
        for rule in self.host_rules(&gateway) {
            append_rule(&rule).await?;
        }
        Ok(())
    }

    fn host_rules(&self, gateway: &str) -> Vec<Rule> {
        let veth = self.host_veth();
        let source = format!("{}/32", self.host_ip());
        let rule = |table: &'static str, chain: &'static str, spec: &[&str]| Rule {
            table,
            chain,
            spec: spec.iter().map(|part| part.to_string()).collect(),
        };
        vec![
            rule("filter", "FORWARD", &["-i", &veth, "-o", gateway, "-j", "ACCEPT"]),
            rule(
                "filter",
                "FORWARD",
                &[
                    "-i",
                    gateway,
                    "-o",
                    &veth,
                    "-m",
                    "conntrack",
                    "--ctstate",
                    "ESTABLISHED,RELATED",
                    "-j",
                    "ACCEPT",
                ],
            ),
            rule(
                "filter",
                "INPUT",
                &["-i", &veth, "-m", "conntrack", "--ctstate", "NEW", "-j", "DROP"],
            ),
            rule(
                "nat",
                "POSTROUTING",
                &["-s", &source, "-o", gateway, "-j", "MASQUERADE"],
            ),
        ]
    }

    async fn inside(&self, command: &[&str]) -> Result<(), String> {
        let namespace = self.namespace();
        let args: Vec<&str> = ["netns", "exec", namespace.as_str()]
            .into_iter()
            .chain(command.iter().copied())
            .collect();
        run("ip", &args).await
    }

    async fn iptables_inside(&self, rule: &[&str]) -> Result<(), String> {
        let command: Vec<&str> = ["iptables", "-w"].into_iter().chain(rule.iter().copied()).collect();
        self.inside(&command).await
    }
}

#[cfg(target_os = "linux")]
async fn append_rule(rule: &Rule) -> Result<(), String> {
    iptables(rule, "-A").await
}

#[cfg(target_os = "linux")]
async fn delete_rule(rule: &Rule) -> Result<(), String> {
    ignore_missing(
        iptables(rule, "-D").await,
        &["does a matching rule exist", "Bad rule", "No chain/target/match"],
    )
}

#[cfg(target_os = "linux")]
async fn iptables(rule: &Rule, action: &str) -> Result<(), String> {
    let args: Vec<&str> = ["-w", "-t", rule.table, action, rule.chain]
        .into_iter()
        .chain(rule.spec.iter().map(String::as_str))
        .collect();
    run("iptables", &args).await
}

#[cfg(target_os = "linux")]
async fn default_interface() -> Result<String, String> {
    let routes = output("ip", &["route", "show", "default"]).await?;
    routes
        .split_whitespace()
        .skip_while(|word| *word != "dev")
        .nth(1)
        .map(str::to_owned)
        .ok_or_else(|| format!("no default route to masquerade through: {routes:?}"))
}

#[cfg(target_os = "linux")]
fn ignore_missing(result: Result<(), String>, markers: &[&str]) -> Result<(), String> {
    match result {
        Err(e) if markers.iter().any(|marker| e.contains(marker)) => Ok(()),
        other => other,
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
