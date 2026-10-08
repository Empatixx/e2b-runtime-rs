use std::net::Ipv4Addr;

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

    pub fn initialize(&self) -> String {
        let sets: String = [ALWAYS_DENY, ALWAYS_ALLOW, USER_DENY, USER_ALLOW]
            .iter()
            .map(|set| format!("add set inet {TABLE} {set} {{ type ipv4_addr; flags interval; }}\n"))
            .collect();
        format!(
            "add table inet {TABLE}\nadd chain inet {TABLE} {CHAIN} {{ type filter hook prerouting priority -150; policy accept; }}\n{sets}{}",
            self.apply(&EgressRules::default())
        )
    }

    pub fn apply(&self, rules: &EgressRules) -> String {
        let always_deny: Vec<String> = DENIED_SANDBOX_CIDRS.iter().map(|cidr| (*cidr).to_owned()).collect();
        [
            format!("flush chain inet {TABLE} {CHAIN}\n"),
            rule("ct state established,related accept"),
            rule(&format!("ip daddr @{ALWAYS_ALLOW} accept")),
            match rules.byop {
                true => rule(&format!("meta l4proto != tcp ip daddr @{ALWAYS_DENY} drop")),
                false => rule(&format!("ip daddr @{ALWAYS_DENY} drop")),
            },
            rule(&format!("meta l4proto != tcp ip daddr @{USER_ALLOW} accept")),
            rule(&format!("meta l4proto != tcp ip daddr @{USER_DENY} drop")),
            refill(ALWAYS_DENY, &always_deny),
            refill(ALWAYS_ALLOW, &self.always_allow),
            refill(USER_DENY, &rules.denied),
            refill(USER_ALLOW, &rules.allowed),
        ]
        .concat()
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

fn rule(statement: &str) -> String {
    format!("add rule inet {TABLE} {CHAIN} iifname \"{TAP}\" {statement}\n")
}

fn refill(set: &str, cidrs: &[String]) -> String {
    let elements = set_elements(cidrs);
    let flush = format!("flush set inet {TABLE} {set}\n");
    match elements.is_empty() {
        true => flush,
        false => format!("{flush}add element inet {TABLE} {set} {{ {} }}\n", elements.join(", ")),
    }
}

pub fn set_elements(cidrs: &[String]) -> Vec<String> {
    let ranges: Vec<(&String, u32, u32)> = cidrs
        .iter()
        .filter_map(|cidr| ipv4_range(cidr).map(|(start, end)| (cidr, start, end)))
        .collect();
    let boundary = ranges
        .iter()
        .filter(|(_, _, end)| *end == u32::MAX)
        .map(|(_, start, _)| *start)
        .min();
    ranges
        .iter()
        .filter(|(_, _, end)| *end != u32::MAX)
        .filter(|(_, start, _)| boundary.is_none_or(|boundary| *start < boundary))
        .map(|(cidr, _, _)| (*cidr).clone())
        .chain(boundary.map(|start| format!("{}-255.255.255.255", Ipv4Addr::from(start))))
        .collect()
}

fn ipv4_range(cidr: &str) -> Option<(u32, u32)> {
    let (address, prefix) = cidr.split_once('/').unwrap_or((cidr, "32"));
    let address = u32::from(address.parse::<Ipv4Addr>().ok()?);
    let prefix: u32 = prefix.parse().ok().filter(|prefix| *prefix <= 32)?;
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    Some((address & mask, (address & mask) | !mask))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_follow_upstream_order_and_tcp_is_left_to_the_proxy() {
        let firewall = Firewall::new("192.0.2.1", &[]);

        let script = firewall.apply(&EgressRules {
            byop: false,
            allowed: vec!["8.8.8.8/32".to_owned()],
            denied: vec!["1.1.1.0/24".to_owned()],
        });

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
        let script = Firewall::new("192.0.2.1", &[]).apply(&EgressRules {
            byop: true,
            ..EgressRules::default()
        });

        assert!(script.contains("meta l4proto != tcp ip daddr @filtered_always_denylist drop"));
        assert!(script.contains("flush set inet slot-firewall filtered_denylist\n"));
        assert!(!script.contains("add element inet slot-firewall filtered_denylist"));
    }

    #[test]
    fn a_range_to_the_top_becomes_one_open_element_and_subsumed_cidrs_are_dropped() {
        let cidrs = ["10.0.0.0/8", "0.0.0.0/0", "8.8.8.8", "fe80::/10"].map(str::to_owned);

        assert_eq!(set_elements(&cidrs), vec!["0.0.0.0-255.255.255.255".to_owned()]);
        assert_eq!(
            set_elements(&["128.0.0.0/1", "10.0.0.0/8", "200.1.1.1/32"].map(str::to_owned)),
            vec!["10.0.0.0/8".to_owned(), "128.0.0.0-255.255.255.255".to_owned()]
        );
    }

    #[test]
    fn initialize_creates_the_table_chain_and_four_sets() {
        let script = Firewall::new("192.0.2.1", &["10.200.0.0/16".to_owned()]).initialize();

        assert!(script.starts_with("add table inet slot-firewall\nadd chain inet slot-firewall PREROUTE_FILTER { type filter hook prerouting priority -150; policy accept; }\n"));
        assert_eq!(script.matches("add set inet slot-firewall").count(), 4);
        assert!(script.contains("filtered_always_allowlist { 192.0.2.1/32, 10.200.0.0/16 }"));
    }
}
