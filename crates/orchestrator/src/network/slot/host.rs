use std::net::Ipv4Addr;
use std::os::fd::AsRawFd;

use futures::TryStreamExt;
use iptables::IPTables;
use netns_rs::NetNs;
use rtnetlink::packet_route::address::AddressAttribute;
use rtnetlink::packet_route::link::{InfoData, InfoVeth};
use rtnetlink::packet_route::route::RouteProtocol;
use rtnetlink::{Handle, LinkMessageBuilder, LinkUnspec, LinkVeth, RouteMessageBuilder};
use tracing::Instrument;

use super::{GUEST_IP, Slot, TAP_IP, TAP_MAC, TAP_NAME};
use crate::network::firewall::EgressRules;
use crate::network::pool::SlotFactory;
use crate::network::settings::NetworkSettings;
use crate::network::storage::SlotStorage;
use crate::system::all_ok;

struct Rule {
    table: &'static str,
    chain: &'static str,
    spec: String,
}

impl Rule {
    fn new(table: &'static str, chain: &'static str, spec: String) -> Self {
        Self { table, chain, spec }
    }

    fn append(&self, iptables: &IPTables) -> Result<(), String> {
        iptables
            .append(self.table, self.chain, &self.spec)
            .map_err(|e| self.failed("-A", e.as_ref()))
    }

    fn delete(&self, iptables: &IPTables) -> Result<(), String> {
        ignore_missing(
            iptables
                .delete(self.table, self.chain, &self.spec)
                .map_err(|e| self.failed("-D", e.as_ref())),
            &["does a matching rule exist", "Bad rule", "No chain/target/match"],
        )
    }

    fn failed(&self, action: &str, error: &dyn std::error::Error) -> String {
        format!("iptables -t {} {action} {} {error}", self.table, self.chain)
    }
}

impl Slot {
    pub async fn create(&self) -> Result<(), String> {
        if self.namespace_exists() {
            self.destroy().await?;
        }
        let namespace = self.namespace();
        blocking(move || NetNs::new(namespace).map(drop).map_err(|e| e.to_string())).await?;
        if let Err(e) = self.configure().await {
            let _ = self.destroy().await;
            return Err(e);
        }
        Ok(())
    }

    pub async fn destroy(&self) -> Result<(), String> {
        all_ok([self.delete_host_rules().await, self.delete_host_veth().await])?;
        self.delete_namespace().await
    }

    pub fn namespace_exists(&self) -> bool {
        namespace_path(self.index).exists()
    }

    async fn delete_host_rules(&self) -> Result<(), String> {
        let rules = self.host_rules(&default_interface()?);
        blocking(move || {
            let iptables = iptables()?;
            all_ok(rules.iter().map(|rule| rule.delete(&iptables)))
        })
        .await
    }

    async fn delete_host_veth(&self) -> Result<(), String> {
        let veth = LinkUnspec::new_with_name(&self.host_veth()).build();
        let deleted = netlink()?.link().del_with_message(veth).execute().await;
        ignore_missing(
            deleted.map_err(|e| format!("ip link delete {}: {e}", self.host_veth())),
            &["No such device"],
        )
    }

    async fn delete_namespace(&self) -> Result<(), String> {
        let namespace = self.namespace();
        match self.namespace_exists() {
            true => blocking(move || NetNs::get(namespace).and_then(NetNs::remove).map_err(|e| e.to_string())).await,
            false => Ok(()),
        }
    }

    async fn configure(&self) -> Result<(), String> {
        let host = netlink()?;
        let guest = self.inside(netlink).await?;
        self.create_veth(&host, &guest).await?;
        self.create_tap(&guest).await?;
        self.guest_nat_and_firewall(&guest).await?;
        self.host_route_and_rules(&host).await
    }

    async fn create_veth(&self, host: &Handle, guest: &Handle) -> Result<(), String> {
        let veth = self.host_veth();
        let namespace = std::fs::File::open(namespace_path(self.index)).map_err(|e| e.to_string())?;
        let peer = LinkUnspec::new_with_name("eth0")
            .setns_by_fd(namespace.as_raw_fd())
            .build();
        let pair = LinkVeth::new(&veth, "eth0").set_info_data(InfoData::Veth(InfoVeth::Peer(peer)));
        host.link()
            .add(pair.build())
            .execute()
            .await
            .map_err(|e| format!("ip link add {veth} type veth peer name eth0: {e}"))?;
        add_address(host, &veth, self.host_veth_ip(), 31).await?;
        link_up(host, LinkUnspec::new_with_name(&veth)).await?;
        add_address(guest, "eth0", self.peer_veth_ip(), 31).await?;
        link_up(guest, LinkUnspec::new_with_name("eth0")).await?;
        link_up(guest, LinkUnspec::new_with_name("lo")).await
    }

    async fn create_tap(&self, guest: &Handle) -> Result<(), String> {
        self.inside(persistent_tap).await?;
        add_address(guest, TAP_NAME, TAP_IP, 30).await?;
        link_up(guest, LinkUnspec::new_with_name(TAP_NAME).address(TAP_MAC.to_vec())).await
    }

    async fn guest_nat_and_firewall(&self, guest: &Handle) -> Result<(), String> {
        let settings = NetworkSettings::current();
        add_route(
            guest,
            RouteMessageBuilder::<Ipv4Addr>::new().gateway(self.host_veth_ip()),
        )
        .await?;
        let rules = self.guest_rules(settings.egress_dscp);
        let script = settings.firewall().initialize()?;
        self.inside(move || append_all(&rules).and_then(|()| nft(&script)))
            .await
    }

    fn guest_rules(&self, dscp: u8) -> Vec<Rule> {
        let host_ip = self.host_ip();
        [
            Some(Rule::new(
                "nat",
                "POSTROUTING",
                format!("-o eth0 -s {GUEST_IP} -j SNAT --to {host_ip}"),
            )),
            Some(Rule::new(
                "nat",
                "PREROUTING",
                format!("-i eth0 -d {host_ip} -j DNAT --to {GUEST_IP}"),
            )),
            (dscp > 0).then(|| Rule::new("mangle", "POSTROUTING", format!("-o eth0 -j DSCP --set-dscp {dscp}"))),
        ]
        .into_iter()
        .flatten()
        .collect()
    }

    pub async fn apply_egress(&self, rules: &EgressRules) -> Result<(), String> {
        let applied = match NetworkSettings::current().firewall().apply(rules) {
            Ok(script) => self.inside(move || nft(&script)).await,
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
        let span = tracing::info_span!("slot-internet-deny-egress", namespace_id = %self.namespace());
        self.inside(move || nft(&script))
            .instrument(span)
            .await
            .map_err(|e| format!("failed execution in network namespace '{}': {e}", self.namespace()))
    }

    async fn host_route_and_rules(&self, host: &Handle) -> Result<(), String> {
        let veth = link_index(host, &self.host_veth()).await?;
        let route = RouteMessageBuilder::<Ipv4Addr>::new()
            .destination_prefix(self.host_ip(), 32)
            .gateway(self.peer_veth_ip())
            .output_interface(veth);
        add_route(host, route).await?;
        let rules = self.host_rules(&default_interface()?);
        blocking(move || append_all(&rules)).await
    }

    fn host_rules(&self, gateway: &str) -> Vec<Rule> {
        let settings = NetworkSettings::current();
        let veth = self.host_veth();
        let orchestrator = &settings.orchestrator_ip;
        let redirect = |filter: String, to: u16| {
            Rule::new(
                "nat",
                "PREROUTING",
                format!("-i {veth} -p tcp {filter} -j REDIRECT --to-port {to}"),
            )
        };
        vec![
            Rule::new("filter", "FORWARD", format!("-i {veth} -o {gateway} -j ACCEPT")),
            Rule::new("filter", "FORWARD", format!("-i {gateway} -o {veth} -j ACCEPT")),
            Rule::new(
                "nat",
                "POSTROUTING",
                format!("-s {}/32 -o {gateway} -j MASQUERADE", self.host_ip()),
            ),
            redirect(format!("-d {orchestrator} --dport 80"), settings.hyperloop_port),
            redirect(format!("-d {orchestrator} --dport 111"), settings.portmapper_port),
            redirect(format!("-d {orchestrator} --dport 2049"), settings.nfs_proxy_port),
            redirect("--dport 80".to_owned(), settings.tcp_firewall_http_port),
            redirect("--dport 443".to_owned(), settings.tcp_firewall_tls_port),
            redirect(String::new(), settings.tcp_firewall_other_port),
        ]
    }

    async fn inside<T: Send + 'static>(
        &self,
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        let namespace = self.namespace();
        blocking(move || {
            NetNs::get(&namespace)
                .and_then(|netns| netns.run(|_| work()))
                .map_err(|e| format!("netns {namespace}: {e}"))?
        })
        .await
    }
}

pub struct HostSlots;

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

pub fn host_slot_storage(indices: std::ops::RangeInclusive<u32>) -> SlotStorage {
    SlotStorage::new(indices, Box::new(|index| namespace_path(index).exists()))
}

fn namespace_path(index: u32) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/var/run/netns/ns-{index}"))
}

async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tokio::task::spawn_blocking(work).await.map_err(|e| e.to_string())?
}

fn netlink() -> Result<Handle, String> {
    let (connection, handle, _) = rtnetlink::new_connection().map_err(|e| format!("netlink: {e}"))?;
    tokio::spawn(connection);
    Ok(handle)
}

fn iptables() -> Result<IPTables, String> {
    iptables::new(false).map_err(|e| format!("iptables: {e}"))
}

fn append_all(rules: &[Rule]) -> Result<(), String> {
    let iptables = iptables()?;
    rules.iter().try_for_each(|rule| rule.append(&iptables))
}

fn persistent_tap() -> Result<(), String> {
    tun::create(tun::Configuration::default().tun_name(TAP_NAME).layer(tun::Layer::L2))
        .and_then(|mut tap| tap.persist())
        .map_err(|e| format!("ip tuntap add {TAP_NAME} mode tap: {e}"))
}

fn nft(script: &str) -> Result<(), String> {
    let output = std::process::Command::new("nft")
        .arg(script)
        .output()
        .map_err(|e| format!("nft: {e}"))?;
    output.status.success().then_some(()).ok_or_else(|| {
        format!(
            "nft: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })
}

async fn link_index(handle: &Handle, name: &str) -> Result<u32, String> {
    handle
        .link()
        .get()
        .match_name(name.to_owned())
        .execute()
        .try_next()
        .await
        .map_err(|e| format!("ip link show {name}: {e}"))?
        .map(|link| link.header.index)
        .ok_or_else(|| format!("ip link show {name}: no such device"))
}

async fn link_up(handle: &Handle, link: LinkMessageBuilder<LinkUnspec>) -> Result<(), String> {
    handle
        .link()
        .set(link.up().build())
        .execute()
        .await
        .map_err(|e| format!("ip link set up: {e}"))
}

async fn add_address(handle: &Handle, device: &str, address: Ipv4Addr, bits: u8) -> Result<(), String> {
    let mut request = handle
        .address()
        .add(link_index(handle, device).await?, address.into(), bits);
    request
        .message_mut()
        .attributes
        .retain(|attribute| !matches!(attribute, AddressAttribute::Broadcast(_)));
    request
        .execute()
        .await
        .map_err(|e| format!("ip addr add {address}/{bits} dev {device}: {e}"))
}

async fn add_route(handle: &Handle, route: RouteMessageBuilder<Ipv4Addr>) -> Result<(), String> {
    handle
        .route()
        .add(route.protocol(RouteProtocol::Boot).build())
        .execute()
        .await
        .map_err(|e| format!("ip route add: {e}"))
}

fn default_interface() -> Result<String, String> {
    std::fs::read_to_string("/proc/net/route")
        .map_err(|e| format!("/proc/net/route: {e}"))?
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .find(|fields| fields.get(1) == Some(&"00000000") && fields.get(7) == Some(&"00000000"))
        .and_then(|fields| fields.first().map(|name| (*name).to_owned()))
        .ok_or_else(|| "no default route to masquerade through".to_owned())
}

fn ignore_missing(result: Result<(), String>, markers: &[&str]) -> Result<(), String> {
    match result {
        Err(e) if markers.iter().any(|marker| e.contains(marker)) => Ok(()),
        other => other,
    }
}
