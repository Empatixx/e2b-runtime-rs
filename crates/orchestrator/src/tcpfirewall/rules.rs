use std::net::{IpAddr, Ipv4Addr};

use crate::network::firewall::DENIED_SANDBOX_CIDRS;
use crate::proto::SandboxNetworkEgressConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchType {
    None,
    Domain,
    Cidr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decision {
    pub allowed: bool,
    pub matched: MatchType,
}

pub fn egress_allowed(
    egress: Option<&SandboxNetworkEgressConfig>,
    hostname: Option<&str>,
    ip: IpAddr,
) -> Result<Decision, String> {
    let Some(egress) = egress else {
        return Ok(Decision {
            allowed: true,
            matched: MatchType::None,
        });
    };
    if hostname.is_some_and(|hostname| {
        egress
            .allowed_domains
            .iter()
            .any(|domain| matches_domain(hostname, domain))
    }) {
        return Ok(Decision {
            allowed: true,
            matched: MatchType::Domain,
        });
    }
    if any_contains(&egress.allowed_cidrs, ip, "allowed")? {
        return Ok(Decision {
            allowed: true,
            matched: MatchType::Cidr,
        });
    }
    if any_contains(&egress.denied_cidrs, ip, "denied")? {
        return Ok(Decision {
            allowed: false,
            matched: MatchType::Cidr,
        });
    }
    Ok(Decision {
        allowed: true,
        matched: MatchType::None,
    })
}

pub fn matches_domain(hostname: &str, pattern: &str) -> bool {
    match pattern {
        "" => false,
        "*" => true,
        _ if pattern.eq_ignore_ascii_case(hostname) => true,
        _ => pattern
            .strip_prefix('*')
            .filter(|suffix| suffix.starts_with('.'))
            .is_some_and(|suffix| hostname.to_lowercase().ends_with(&suffix.to_lowercase())),
    }
}

pub fn in_always_denied(ip: IpAddr) -> bool {
    DENIED_SANDBOX_CIDRS
        .iter()
        .filter_map(|cidr| parse_cidr(cidr).ok())
        .any(|(network, prefix)| contains(network, prefix, ip))
}

fn any_contains(cidrs: &[String], ip: IpAddr, kind: &str) -> Result<bool, String> {
    cidrs.iter().try_fold(false, |found, cidr| {
        parse_cidr(cidr)
            .map(|(network, prefix)| found || contains(network, prefix, ip))
            .map_err(|e| format!("invalid {kind} CIDR {cidr:?}: {e}"))
    })
}

fn parse_cidr(cidr: &str) -> Result<(Ipv4Addr, u32), String> {
    let (address, prefix) = cidr.split_once('/').ok_or("invalid CIDR address")?;
    let address = address
        .parse::<Ipv4Addr>()
        .map_err(|_| "invalid CIDR address".to_owned())?;
    let prefix = prefix
        .parse::<u32>()
        .ok()
        .filter(|prefix| *prefix <= 32)
        .ok_or("invalid CIDR address")?;
    Ok((address, prefix))
}

fn contains(network: Ipv4Addr, prefix: u32, ip: IpAddr) -> bool {
    let IpAddr::V4(ip) = ip else {
        return false;
    };
    let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
    u32::from(network) & mask == u32::from(ip) & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn egress(domains: &[&str], allowed: &[&str], denied: &[&str]) -> SandboxNetworkEgressConfig {
        let owned = |values: &[&str]| values.iter().map(|value| (*value).to_owned()).collect();
        SandboxNetworkEgressConfig {
            allowed_domains: owned(domains),
            allowed_cidrs: owned(allowed),
            denied_cidrs: owned(denied),
            ..SandboxNetworkEgressConfig::default()
        }
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn domains_then_allowed_then_denied_cidrs_decide() {
        let rules = egress(&["*.example.com"], &["8.8.8.0/24"], &["0.0.0.0/0"]);

        let decide = |host: Option<&str>, address: &str| egress_allowed(Some(&rules), host, ip(address)).unwrap();

        assert_eq!(
            decide(Some("API.example.com"), "1.2.3.4"),
            Decision {
                allowed: true,
                matched: MatchType::Domain
            }
        );
        assert_eq!(
            decide(None, "8.8.8.8"),
            Decision {
                allowed: true,
                matched: MatchType::Cidr
            }
        );
        assert_eq!(
            decide(Some("other.org"), "1.2.3.4"),
            Decision {
                allowed: false,
                matched: MatchType::Cidr
            }
        );
        assert_eq!(
            egress_allowed(None, None, ip("1.2.3.4")).unwrap(),
            Decision {
                allowed: true,
                matched: MatchType::None
            }
        );
    }

    #[test]
    fn domain_patterns_match_like_upstream() {
        assert!(matches_domain("a.example.com", "*.example.com"));
        assert!(matches_domain("Example.com", "example.COM"));
        assert!(matches_domain("anything", "*"));
        assert!(!matches_domain("example.com", "*.example.com"));
        assert!(!matches_domain("example.com", ""));
    }

    #[test]
    fn invalid_cidrs_and_internal_addresses() {
        let rules = egress(&[], &["nope"], &[]);
        assert_eq!(
            egress_allowed(Some(&rules), None, ip("1.1.1.1")).unwrap_err(),
            "invalid allowed CIDR \"nope\": invalid CIDR address"
        );
        assert!(in_always_denied(ip("10.1.2.3")));
        assert!(in_always_denied(ip("169.254.169.254")));
        assert!(!in_always_denied(ip("1.1.1.1")));
    }
}
