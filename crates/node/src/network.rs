use std::net::Ipv4Addr;

use anyhow::{Context, Result, ensure};
use tokio::process::Command;
use tokio::sync::Mutex;

const GUEST_IP: &str = "169.254.0.21";
const TAP_IP: &str = "169.254.0.22/30";
const TAP_MAC: &str = "02:fc:00:00:00:01";
const SANDBOX_RANGE: &str = "10.11.0.0/16";

#[derive(Debug, Clone)]
pub struct Slot {
    pub index: u32,
    pub namespace: String,
    pub host_ip: Ipv4Addr,
}

#[derive(Default)]
pub struct Network {
    free: Mutex<Vec<Slot>>,
    next: Mutex<u32>,
}

impl Slot {
    fn new(index: u32) -> Self {
        Self {
            index,
            namespace: format!("e2bs-{index}"),
            host_ip: Ipv4Addr::new(10, 11, (index / 256) as u8, (index % 256) as u8),
        }
    }

    fn veth_name(&self) -> String {
        format!("e2bv{}", self.index)
    }

    fn veth_host_ip(&self) -> Ipv4Addr {
        self.veth_ip(1)
    }

    fn veth_namespace_ip(&self) -> Ipv4Addr {
        self.veth_ip(2)
    }

    fn veth_ip(&self, offset: u32) -> Ipv4Addr {
        let base = u32::from(Ipv4Addr::new(10, 12, 0, 0)) + self.index * 4;
        Ipv4Addr::from(base + offset)
    }

    async fn create(&self) -> Result<()> {
        self.create_namespace().await?;
        self.create_tap().await?;
        self.create_veth().await?;
        self.translate_guest_address().await?;
        self.route_host_ip().await
    }

    async fn create_namespace(&self) -> Result<()> {
        let ns = self.namespace.as_str();
        self.remove_leftovers().await;
        run("ip", &["netns", "add", ns]).await?;
        run("ip", &["-n", ns, "link", "set", "lo", "up"]).await
    }

    async fn remove_leftovers(&self) {
        let _ = run("ip", &["netns", "del", &self.namespace]).await;
        let _ = run("ip", &["link", "del", &self.veth_name()]).await;
    }

    async fn create_tap(&self) -> Result<()> {
        let ns = self.namespace.as_str();
        run("ip", &["-n", ns, "tuntap", "add", "tap0", "mode", "tap"]).await?;
        run("ip", &["-n", ns, "link", "set", "tap0", "address", TAP_MAC]).await?;
        run("ip", &["-n", ns, "addr", "add", TAP_IP, "dev", "tap0"]).await?;
        run("ip", &["-n", ns, "link", "set", "tap0", "up"]).await
    }

    async fn create_veth(&self) -> Result<()> {
        let ns = self.namespace.as_str();
        let veth = self.veth_name();
        let host_side = format!("{}/30", self.veth_host_ip());
        let namespace_side = format!("{}/30", self.veth_namespace_ip());
        run(
            "ip",
            &[
                "link", "add", &veth, "type", "veth", "peer", "name", "eth0", "netns", ns,
            ],
        )
        .await?;
        run("ip", &["addr", "add", &host_side, "dev", &veth]).await?;
        run("ip", &["link", "set", &veth, "up"]).await?;
        run("ip", &["-n", ns, "addr", "add", &namespace_side, "dev", "eth0"]).await?;
        run("ip", &["-n", ns, "link", "set", "eth0", "up"]).await?;
        run(
            "ip",
            &[
                "-n",
                ns,
                "route",
                "add",
                "default",
                "via",
                &self.veth_host_ip().to_string(),
            ],
        )
        .await
    }

    async fn translate_guest_address(&self) -> Result<()> {
        let ns = self.namespace.as_str();
        let host_ip = self.host_ip.to_string();
        let outgoing = [
            "-t",
            "nat",
            "-A",
            "POSTROUTING",
            "-o",
            "eth0",
            "-s",
            GUEST_IP,
            "-j",
            "SNAT",
            "--to",
            &host_ip,
        ];
        let incoming = [
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
            GUEST_IP,
        ];
        iptables_in(ns, &outgoing).await?;
        iptables_in(ns, &incoming).await
    }

    async fn route_host_ip(&self) -> Result<()> {
        let destination = format!("{}/32", self.host_ip);
        let via = self.veth_namespace_ip().to_string();
        run("ip", &["route", "replace", &destination, "via", &via]).await
    }
}

impl Network {
    pub async fn prepare_host() -> Result<()> {
        run("sysctl", &["-qw", "net.ipv4.ip_forward=1"]).await?;
        for rule in [
            ["-t", "nat", "POSTROUTING", "-s", SANDBOX_RANGE, "-j", "MASQUERADE"],
            ["-t", "filter", "FORWARD", "-s", SANDBOX_RANGE, "-j", "ACCEPT"],
            ["-t", "filter", "FORWARD", "-d", SANDBOX_RANGE, "-j", "ACCEPT"],
        ] {
            ensure_rule(&rule).await?;
        }
        Ok(())
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn acquire(&self) -> Result<Slot> {
        if let Some(slot) = self.free.lock().await.pop() {
            return Ok(slot);
        }
        let index = {
            let mut next = self.next.lock().await;
            *next += 1;
            *next
        };
        let slot = Slot::new(index);
        slot.create()
            .await
            .with_context(|| format!("create network slot {index}"))?;
        Ok(slot)
    }

    pub async fn release(&self, slot: Slot) {
        self.free.lock().await.push(slot);
    }
}

async fn ensure_rule(rule: &[&str; 7]) -> Result<()> {
    let [_, table, chain, rest @ ..] = *rule;
    let check: Vec<&str> = ["-t", table, "-C", chain]
        .into_iter()
        .chain(rest.iter().copied())
        .collect();
    if run("iptables", &check).await.is_ok() {
        return Ok(());
    }
    let insert: Vec<&str> = ["-t", table, "-I", chain]
        .into_iter()
        .chain(rest.iter().copied())
        .collect();
    run("iptables", &insert).await
}

async fn iptables_in(namespace: &str, rule: &[&str]) -> Result<()> {
    let args: Vec<&str> = ["netns", "exec", namespace, "iptables"]
        .into_iter()
        .chain(rule.iter().copied())
        .collect();
    run("ip", &args).await
}

pub async fn run(program: &str, args: &[&str]) -> Result<()> {
    let output = Command::new(program).args(args).output().await?;
    ensure!(
        output.status.success(),
        "{program} {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(())
}
