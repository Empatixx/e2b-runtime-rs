use std::net::{IpAddr, Ipv4Addr};

use super::netip::{Prefix, parse_addr, parse_prefix};

pub const TABLE: &str = "slot-firewall";
pub const CHAIN: &str = "PREROUTE_FILTER";
pub const ALWAYS_DENY: &str = "filtered_always_denylist";
pub const ALWAYS_ALLOW: &str = "filtered_always_allowlist";
pub const USER_DENY: &str = "filtered_denylist";
pub const USER_ALLOW: &str = "filtered_allowlist";
pub const DENIED_SANDBOX_CIDRS: [&str; 6] = [
    "10.0.0.0/8",
    "100.64.0.0/10",
    "127.0.0.0/8",
    "169.254.0.0/16",
    "172.16.0.0/12",
    "192.168.0.0/16",
];
const TAP: &str = "tap0";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EgressRules {
    pub byop: bool,
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firewall {
    pub always_allow: Vec<String>,
}

impl Firewall {
    pub fn new(orchestrator_ip: &str, internal_cidrs: &[String]) -> Self {
        Self {
            always_allow: std::iter::once(format!("{orchestrator_ip}/32"))
                .chain(internal_cidrs.iter().cloned())
                .collect(),
        }
    }

    pub fn initialize(&self) -> Result<String, String> {
        let sets: String = [ALWAYS_DENY, ALWAYS_ALLOW, USER_DENY, USER_ALLOW]
            .iter()
            .map(|set| format!("add set inet {TABLE} {set} {{ type ipv4_addr; flags interval; }}\n"))
            .collect();
        self.apply(&EgressRules::default()).map(|rules| {
            format!(
                "add table inet {TABLE}\nadd chain inet {TABLE} {CHAIN} {{ type filter hook prerouting priority -150; policy accept; }}\n{sets}{rules}"
            )
        })
    }

    pub fn apply(&self, rules: &EgressRules) -> Result<String, String> {
        let always_deny: Vec<String> = DENIED_SANDBOX_CIDRS.iter().map(|cidr| (*cidr).to_owned()).collect();
        let always_allow = set_data(&self.always_allow).map_err(|e| format!("parse initial allowed CIDRs: {e}"))?;
        Ok([
            format!("flush chain inet {TABLE} {CHAIN}\n"),
            rule("ct state established,related accept"),
            rule(&format!("ip daddr @{ALWAYS_ALLOW} accept")),
            match rules.byop {
                true => rule(&format!("meta l4proto != tcp ip daddr @{ALWAYS_DENY} drop")),
                false => rule(&format!("ip daddr @{ALWAYS_DENY} drop")),
            },
            rule(&format!("meta l4proto != tcp ip daddr @{USER_ALLOW} accept")),
            rule(&format!("meta l4proto != tcp ip daddr @{USER_DENY} drop")),
            refill(ALWAYS_DENY, user_set_elements(ALWAYS_DENY, &always_deny)?),
            refill(
                ALWAYS_ALLOW,
                elements(ALWAYS_ALLOW, &always_allow).map_err(|e| format!("reset predefined allow set: {e}"))?,
            ),
            refill(
                USER_DENY,
                user_set_elements(USER_DENY, &rules.denied).map_err(|e| format!("replace user deny set: {e}"))?,
            ),
            refill(
                USER_ALLOW,
                user_set_elements(USER_ALLOW, &rules.allowed).map_err(|e| format!("replace user allow set: {e}"))?,
            ),
        ]
        .concat())
    }

    pub fn deny_egress(&self) -> String {
        [
            format!("flush chain inet {TABLE} {CHAIN}\n"),
            rule("ct state established,related accept"),
            rule("drop"),
        ]
        .concat()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetData {
    Address(IpAddr),
    Prefix(Prefix),
    Range { start: IpAddr, end: IpAddr },
}

fn rule(statement: &str) -> String {
    format!("add rule inet {TABLE} {CHAIN} iifname \"{TAP}\" {statement}\n")
}

fn refill(set: &str, elements: Vec<String>) -> String {
    let flush = format!("flush set inet {TABLE} {set}\n");
    match elements.is_empty() {
        true => flush,
        false => format!("{flush}add element inet {TABLE} {set} {{ {} }}\n", elements.join(", ")),
    }
}

pub fn user_set_elements(set: &str, cidrs: &[String]) -> Result<Vec<String>, String> {
    let boundary = cidrs
        .iter()
        .filter_map(|cidr| ipv4_range(cidr))
        .filter(|(_, end)| *end == u32::MAX)
        .map(|(start, _)| start)
        .min();
    let toolkit: Vec<String> = cidrs
        .iter()
        .filter(|cidr| ipv4_range(cidr).is_none_or(|(_, end)| end != u32::MAX))
        .filter(|cidr| boundary.is_none_or(|boundary| ipv4_range(cidr).is_none_or(|(start, _)| start < boundary)))
        .cloned()
        .collect();
    let mut elements = match toolkit.is_empty() {
        true => Vec::new(),
        false => elements(set, &set_data(&toolkit)?)?,
    };
    elements.extend(boundary.map(|start| format!("{}-255.255.255.255", Ipv4Addr::from(start))));
    Ok(elements)
}

fn set_data(cidrs: &[String]) -> Result<Vec<SetData>, String> {
    cidrs.iter().map(|cidr| one_set_data(cidr)).collect()
}

fn one_set_data(cidr: &str) -> Result<SetData, String> {
    if cidr.contains('/') {
        return parse_prefix(cidr).map(SetData::Prefix);
    }
    let parts: Vec<&str> = cidr.split('-').collect();
    match parts.as_slice() {
        [start, end, ..] => Ok(SetData::Range {
            start: parse_addr(start)?,
            end: parse_addr(end)?,
        }),
        _ => parse_addr(cidr).map(SetData::Address),
    }
}

fn elements(set: &str, data: &[SetData]) -> Result<Vec<String>, String> {
    data.iter()
        .map(|data| element(data).map_err(|e| format!("generating set elements failed for {set}: {e}")))
        .filter_map(Result::transpose)
        .collect()
}

fn element(data: &SetData) -> Result<Option<String>, String> {
    match *data {
        SetData::Range { start, end } => {
            validate_range(start, end)?;
            Ok(match (start, end) {
                (IpAddr::V4(start), IpAddr::V4(end)) => Some(format!("{start}-{end}")),
                _ => None,
            })
        }
        SetData::Address(address) => {
            validate_address(address)?;
            Ok(match address {
                IpAddr::V4(address) => Some(address.to_string()),
                IpAddr::V6(_) => None,
            })
        }
        SetData::Prefix(Prefix {
            address: IpAddr::V4(address),
            bits,
        }) => {
            let (start, end) = masked_range(u32::from(address), u32::from(bits));
            validate_range(IpAddr::V4(start.into()), IpAddr::V4(end.into()))?;
            Ok(Some(format!("{}/{bits}", Ipv4Addr::from(start))))
        }
        SetData::Prefix(_) => Ok(None),
    }
}

fn validate_address(address: IpAddr) -> Result<(), String> {
    match address.is_unspecified() {
        true => Err(format!("address is unspecified {address}")),
        false => Ok(()),
    }
}

fn validate_range(start: IpAddr, end: IpAddr) -> Result<(), String> {
    validate_address(start)?;
    validate_address(end)?;
    match go_less(end, start) {
        true => Err(format!("start address ({start}) is after end address ({end})")),
        false => Ok(()),
    }
}

fn go_less(left: IpAddr, right: IpAddr) -> bool {
    match (left, right) {
        (IpAddr::V4(left), IpAddr::V4(right)) => left < right,
        (IpAddr::V6(left), IpAddr::V6(right)) => left < right,
        (IpAddr::V4(_), IpAddr::V6(_)) => true,
        (IpAddr::V6(_), IpAddr::V4(_)) => false,
    }
}

fn ipv4_range(cidr: &str) -> Option<(u32, u32)> {
    match parse_prefix(cidr) {
        Ok(Prefix {
            address: IpAddr::V4(address),
            bits,
        }) => Some(masked_range(u32::from(address), u32::from(bits))),
        Ok(_) => None,
        Err(_) => match parse_addr(cidr) {
            Ok(IpAddr::V4(address)) => Some((u32::from(address), u32::from(address))),
            _ => None,
        },
    }
}

fn masked_range(address: u32, bits: u32) -> (u32, u32) {
    let mask = u32::MAX.checked_shl(32 - bits).unwrap_or(0);
    (address & mask, (address & mask) | !mask)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_follow_upstream_order_and_tcp_is_left_to_the_proxy() {
        let firewall = Firewall::new("192.0.2.1", &[]);

        let script = firewall
            .apply(&EgressRules {
                byop: false,
                allowed: vec!["8.8.8.8/32".to_owned()],
                denied: vec!["1.1.1.0/24".to_owned()],
            })
            .unwrap();

        let rules: Vec<&str> = script.lines().filter(|line| line.starts_with("add rule")).collect();
        assert_eq!(
            rules,
            vec![
                "add rule inet slot-firewall PREROUTE_FILTER iifname \"tap0\" ct state established,related accept",
                "add rule inet slot-firewall PREROUTE_FILTER iifname \"tap0\" ip daddr @filtered_always_allowlist accept",
                "add rule inet slot-firewall PREROUTE_FILTER iifname \"tap0\" ip daddr @filtered_always_denylist drop",
                "add rule inet slot-firewall PREROUTE_FILTER iifname \"tap0\" meta l4proto != tcp ip daddr @filtered_allowlist accept",
                "add rule inet slot-firewall PREROUTE_FILTER iifname \"tap0\" meta l4proto != tcp ip daddr @filtered_denylist drop",
            ]
        );
        assert!(script.contains("add element inet slot-firewall filtered_always_allowlist { 192.0.2.1/32 }"));
        assert!(script.contains("add element inet slot-firewall filtered_allowlist { 8.8.8.8/32 }"));
        assert!(script.starts_with("flush chain inet slot-firewall PREROUTE_FILTER\n"));
    }

    #[test]
    fn byop_only_drops_non_tcp_to_the_always_denied_ranges() {
        let script = Firewall::new("192.0.2.1", &[])
            .apply(&EgressRules {
                byop: true,
                ..EgressRules::default()
            })
            .unwrap();

        assert!(script.contains("meta l4proto != tcp ip daddr @filtered_always_denylist drop"));
        assert!(script.contains("flush set inet slot-firewall filtered_denylist\n"));
        assert!(!script.contains("add element inet slot-firewall filtered_denylist"));
    }

    #[test]
    fn a_range_to_the_top_becomes_one_open_element_and_subsumed_cidrs_are_dropped() {
        let cidrs = ["10.0.0.0/8", "0.0.0.0/0", "8.8.8.8", "fe80::/10"].map(str::to_owned);

        assert_eq!(
            user_set_elements(USER_DENY, &cidrs),
            Ok(vec!["0.0.0.0-255.255.255.255".to_owned()])
        );
        assert_eq!(
            user_set_elements(
                USER_DENY,
                &["128.0.0.0/1", "10.0.0.1/8", "200.1.1.1/32"].map(str::to_owned)
            ),
            Ok(vec!["10.0.0.0/8".to_owned(), "128.0.0.0-255.255.255.255".to_owned()])
        );
        assert_eq!(
            user_set_elements(USER_DENY, &["1.1.1.1", "2.2.2.2-2.2.2.9", "fe80::1"].map(str::to_owned)),
            Ok(vec!["1.1.1.1".to_owned(), "2.2.2.2-2.2.2.9".to_owned()])
        );
    }

    #[test]
    fn initialize_creates_the_table_chain_and_four_sets() {
        let script = Firewall::new("192.0.2.1", &["10.200.0.0/16".to_owned()])
            .initialize()
            .unwrap();

        assert!(script.starts_with("add table inet slot-firewall\nadd chain inet slot-firewall PREROUTE_FILTER { type filter hook prerouting priority -150; policy accept; }\n"));
        assert_eq!(script.matches("add set inet slot-firewall").count(), 4);
        assert!(script.contains("filtered_always_allowlist { 192.0.2.1/32, 10.200.0.0/16 }"));
    }

    #[test]
    fn bad_entries_fail_like_the_upstream_toolkit() {
        let firewall = Firewall::new("192.0.2.1", &[]);
        let denied = |cidrs: &[&str]| {
            firewall.apply(&EgressRules {
                byop: false,
                allowed: Vec::new(),
                denied: cidrs.iter().map(|cidr| (*cidr).to_owned()).collect(),
            })
        };

        assert_eq!(
            denied(&["nope"]).unwrap_err(),
            "replace user deny set: ParseAddr(\"nope\"): unable to parse IP"
        );
        assert_eq!(
            denied(&["1.2.3/8"]).unwrap_err(),
            "replace user deny set: netip.ParsePrefix(\"1.2.3/8\"): ParseAddr(\"1.2.3\"): IPv4 address too short"
        );
        assert_eq!(
            denied(&["0.0.0.0/8"]).unwrap_err(),
            "replace user deny set: generating set elements failed for filtered_denylist: address is unspecified 0.0.0.0"
        );
        assert_eq!(
            denied(&["1.1.1.9-1.1.1.1"]).unwrap_err(),
            "replace user deny set: generating set elements failed for filtered_denylist: start address (1.1.1.9) is after end address (1.1.1.1)"
        );
        assert_eq!(
            Firewall::new("bad", &[]).apply(&EgressRules::default()).unwrap_err(),
            "parse initial allowed CIDRs: netip.ParsePrefix(\"bad/32\"): ParseAddr(\"bad\"): unable to parse IP"
        );
    }
}
