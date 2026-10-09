use std::net::Ipv4Addr;

#[cfg(target_os = "linux")]
use futures::StreamExt;

#[cfg(target_os = "linux")]
use super::firewall::EgressRules;
#[cfg(target_os = "linux")]
use super::pool::SlotFactory;
#[cfg(target_os = "linux")]
use super::settings::NetworkSettings;
#[cfg(target_os = "linux")]
use super::storage::SlotStorage;
#[cfg(target_os = "linux")]
use crate::system::{all_ok, output, run, run_with_input};

pub const MAX_SLOT: u32 = 32766;
pub const TAP_NAME: &str = "tap0";
pub const TAP_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 22);
pub const GUEST_IP: Ipv4Addr = Ipv4Addr::new(169, 254, 0, 21);
pub const GUEST_MAC: &str = "02:FC:00:00:00:05";
pub const TAP_MAC: &str = "02:FC:00:00:00:06";

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
impl Rule {
    fn new(table: &'static str, chain: &'static str, spec: &[&str]) -> Self {
        Self {
            table,
            chain,
            spec: spec.iter().map(|part| (*part).to_owned()).collect(),
        }
    }

    fn redirect(veth: &str, destination: Option<&str>, port: Option<u16>, to: u16) -> Self {
        let destination = destination.map(|destination| ["-d".to_owned(), destination.to_owned()]);
        let port = port.map(|port| ["--dport".to_owned(), port.to_string()]);
        Self {
            table: "nat",
            chain: "PREROUTING",
            spec: ["-i", veth, "-p", "tcp"]
                .map(str::to_owned)
                .into_iter()
                .chain(destination.into_iter().flatten())
                .chain(port.into_iter().flatten())
                .chain([
                    "-j".to_owned(),
                    "REDIRECT".to_owned(),
                    "--to-port".to_owned(),
                    to.to_string(),
                ])
                .collect(),
        }
    }
}

#[cfg(target_os = "linux")]
impl Slot {
    pub async fn create(&self) -> Result<(), String> {
        if self.namespace_exists() {
            self.destroy().await?;
        }
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
        ])?;
        ignore_missing(
            run("ip", &["netns", "delete", &self.namespace()]).await,
            &["No such file or directory"],
        )
    }

    pub fn namespace_exists(&self) -> bool {
        namespace_path(self.index).exists()
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
        self.inside(&["ip", "link", "set", TAP_NAME, "address", TAP_MAC])
            .await?;
        self.inside(&["ip", "addr", "add", &tap_cidr, "dev", TAP_NAME]).await?;
        self.inside(&["ip", "link", "set", TAP_NAME, "up"]).await
    }

    async fn guest_nat_and_firewall(&self) -> Result<(), String> {
        let settings = NetworkSettings::current();
        let host_ip = self.host_ip().to_string();
        let guest_ip = GUEST_IP.to_string();
        let host_veth_ip = self.host_veth_ip().to_string();
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
        if settings.egress_dscp > 0 {
            let dscp = settings.egress_dscp.to_string();
            self.iptables_inside(&[
                "-t",
                "mangle",
                "-A",
                "POSTROUTING",
                "-o",
                "eth0",
                "-j",
                "DSCP",
                "--set-dscp",
                &dscp,
            ])
            .await?;
        }
        self.nft_inside(&settings.firewall().initialize()?).await
    }

    pub async fn apply_egress(&self, rules: &EgressRules) -> Result<(), String> {
        let applied = match NetworkSettings::current().firewall().apply(rules) {
            Ok(script) => self.nft_inside(&script).await,
            Err(e) => Err(e),
        };
        applied.map_err(|e| {
            format!(
                "error setting slot internet access: failed execution in network namespace '{}': {e}",
                self.namespace()
            )
        })
    }

    pub async fn deny_egress(&self) -> Result<(), String> {
        let script = NetworkSettings::current().firewall().deny_egress();
        self.nft_inside(&script)
            .await
            .map_err(|e| format!("failed execution in network namespace '{}': {e}", self.namespace()))
    }

    async fn nft_inside(&self, script: &str) -> Result<(), String> {
        let namespace = self.namespace();
        run_with_input("ip", &["netns", "exec", &namespace, "nft", "-f", "-"], script).await
    }

    async fn host_route_and_rules(&self) -> Result<(), String> {
        let host_ip = self.host_ip().to_string();
        let peer_veth_ip = self.peer_veth_ip().to_string();
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
        let settings = NetworkSettings::current();
        let veth = self.host_veth();
        let source = format!("{}/32", self.host_ip());
        let orchestrator = settings.orchestrator_ip.as_str();
        vec![
            Rule::new("filter", "FORWARD", &["-i", &veth, "-o", gateway, "-j", "ACCEPT"]),
            Rule::new("filter", "FORWARD", &["-i", gateway, "-o", &veth, "-j", "ACCEPT"]),
            Rule::new(
                "nat",
                "POSTROUTING",
                &["-s", &source, "-o", gateway, "-j", "MASQUERADE"],
            ),
            Rule::redirect(&veth, Some(orchestrator), Some(80), settings.hyperloop_port),
            Rule::redirect(&veth, Some(orchestrator), Some(111), settings.portmapper_port),
            Rule::redirect(&veth, Some(orchestrator), Some(2049), settings.nfs_proxy_port),
            Rule::redirect(&veth, None, Some(80), settings.tcp_firewall_http_port),
            Rule::redirect(&veth, None, Some(443), settings.tcp_firewall_tls_port),
            Rule::redirect(&veth, None, None, settings.tcp_firewall_other_port),
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
pub struct HostSlots;

#[cfg(target_os = "linux")]
impl SlotFactory for HostSlots {
    async fn create(&self, index: u32) -> Result<Slot, String> {
        let slot = Slot::new(index)?;
        slot.create().await.map(|()| slot)
    }

    async fn destroy(&self, slot: Slot) -> Result<(), String> {
        slot.destroy().await
    }

    async fn configure(&self, slot: Slot, rules: &EgressRules) -> Result<(), String> {
        slot.apply_egress(rules).await
    }

    async fn deny_egress(&self, slot: Slot) -> Result<(), String> {
        slot.deny_egress().await
    }
}

#[cfg(target_os = "linux")]
pub fn host_slot_storage(indices: std::ops::RangeInclusive<u32>) -> SlotStorage {
    SlotStorage::new(indices, Box::new(|index| namespace_path(index).exists()))
}

#[cfg(target_os = "linux")]
fn namespace_path(index: u32) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/var/run/netns/ns-{index}"))
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
        assert_eq!(last.peer_veth_ip(), Ipv4Addr::new(10, 12, 255, 253));
        assert!(last.host_veth().len() <= 15);
    }

    #[test]
    fn rejects_out_of_range_indices() {
        assert_eq!(Slot::new(0).unwrap_err(), "slot index 0 is outside 1..=32766");
        assert_eq!(Slot::new(32767).unwrap_err(), "slot index 32767 is outside 1..=32766");
    }
}
