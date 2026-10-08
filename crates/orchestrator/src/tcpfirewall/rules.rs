use std::net::IpAddr;

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
        .filter_map(|cidr| Cidr::parse(cidr).ok())
        .any(|cidr| cidr.contains(ip))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Cidr {
    network: IpAddr,
    prefix: u32,
}

impl Cidr {
    fn parse(text: &str) -> Result<Self, String> {
        let invalid = || format!("invalid CIDR address: {text}");
        let (address, prefix) = text.split_once('/').ok_or_else(invalid)?;
        let network = address.parse::<IpAddr>().map_err(|_| invalid())?;
        let bits = match network {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        Some(prefix)
            .filter(|prefix| !prefix.is_empty() && prefix.bytes().all(|digit| digit.is_ascii_digit()))
            .and_then(|prefix| prefix.parse::<u32>().ok())
            .filter(|prefix| *prefix <= bits)
            .map(|prefix| Self { network, prefix })
            .ok_or_else(invalid)
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                masked(u128::from(u32::from(network)) << 96, self.prefix)
                    == masked(u128::from(u32::from(ip)) << 96, self.prefix)
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                masked(u128::from(network), self.prefix) == masked(u128::from(ip), self.prefix)
            }
            _ => false,
        }
    }
}

fn masked(address: u128, prefix: u32) -> u128 {
    address & u128::MAX.checked_shl(128 - prefix).unwrap_or(0)
}

fn any_contains(cidrs: &[String], ip: IpAddr, kind: &str) -> Result<bool, String> {
    for cidr in cidrs {
        if Cidr::parse(cidr)
            .map_err(|e| format!("invalid {kind} CIDR {cidr:?}: {e}"))?
            .contains(ip)
        {
            return Ok(true);
        }
    }
    Ok(false)
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
            "invalid allowed CIDR \"nope\": invalid CIDR address: nope"
        );
        let mixed = egress(&[], &["1.2.3.0/24", "2001:db8::/32", "bad"], &[]);
        assert!(egress_allowed(Some(&mixed), None, ip("1.2.3.4")).unwrap().allowed);
        assert_eq!(
            egress_allowed(Some(&mixed), None, ip("9.9.9.9")).unwrap_err(),
            "invalid allowed CIDR \"bad\": invalid CIDR address: bad"
        );
        let v6 = egress(&[], &[], &["2001:db8::/32", "0.0.0.0/0"]);
        assert!(!egress_allowed(Some(&v6), None, ip("2001:db8::1")).unwrap().allowed);
        assert!(!egress_allowed(Some(&v6), None, ip("::ffff:8.8.8.8")).unwrap().allowed);
        assert!(Cidr::parse("1.2.3.0/33").is_err());
        assert!(Cidr::parse("1.2.3.0/+8").is_err());
        assert!(in_always_denied(ip("10.1.2.3")));
        assert!(in_always_denied(ip("169.254.169.254")));
        assert!(!in_always_denied(ip("1.1.1.1")));
    }
}
