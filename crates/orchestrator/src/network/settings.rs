use std::collections::HashMap;
use std::sync::OnceLock;

use super::firewall::Firewall;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkSettings {
    pub orchestrator_ip: String,
    pub internal_cidrs: Vec<String>,
    pub egress_dscp: u8,
    pub hyperloop_port: u16,
    pub nfs_proxy_port: u16,
    pub portmapper_port: u16,
    pub tcp_firewall_http_port: u16,
    pub tcp_firewall_tls_port: u16,
    pub tcp_firewall_other_port: u16,
}

static CURRENT: OnceLock<NetworkSettings> = OnceLock::new();

impl NetworkSettings {
    pub fn current() -> &'static NetworkSettings {
        CURRENT.get_or_init(|| Self::from_vars(&std::env::vars().collect()))
    }

    pub fn from_vars(vars: &HashMap<String, String>) -> Self {
        let port = |name: &str, default: u16| vars.get(name).and_then(|value| value.parse().ok()).unwrap_or(default);
        Self {
            orchestrator_ip: vars
                .get("SANDBOX_ORCHESTRATOR_IP")
                .cloned()
                .unwrap_or_else(|| "192.0.2.1".to_owned()),
            internal_cidrs: vars
                .get("ALLOW_SANDBOX_INTERNAL_CIDRS")
                .map(|cidrs| {
                    cidrs
                        .split(',')
                        .map(str::trim)
                        .filter(|cidr| !cidr.is_empty())
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            egress_dscp: vars
                .get("SANDBOX_EGRESS_DSCP")
                .and_then(|value| value.parse().ok())
                .filter(|dscp| *dscp <= 63)
                .unwrap_or(0),
            hyperloop_port: port("SANDBOX_HYPERLOOP_PROXY_PORT", 5010),
            nfs_proxy_port: port("SANDBOX_NFS_PROXY_PORT", 5011),
            portmapper_port: port("SANDBOX_PORTMAPPER_PORT", 5012),
            tcp_firewall_http_port: port("SANDBOX_TCP_FIREWALL_HTTP_PORT", 5016),
            tcp_firewall_tls_port: port("SANDBOX_TCP_FIREWALL_TLS_PORT", 5017),
            tcp_firewall_other_port: port("SANDBOX_TCP_FIREWALL_OTHER_PORT", 5018),
        }
    }

    pub fn firewall(&self) -> Firewall {
        Firewall::new(&self.orchestrator_ip, &self.internal_cidrs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_upstream() {
        let settings = NetworkSettings::from_vars(&HashMap::new());

        assert_eq!(settings.orchestrator_ip, "192.0.2.1");
        assert_eq!(
            (
                settings.hyperloop_port,
                settings.nfs_proxy_port,
                settings.portmapper_port,
                settings.tcp_firewall_http_port,
                settings.tcp_firewall_tls_port,
                settings.tcp_firewall_other_port,
                settings.egress_dscp
            ),
            (5010, 5011, 5012, 5016, 5017, 5018, 0)
        );
    }

    #[test]
    fn internal_cidrs_and_dscp_come_from_env() {
        let vars = HashMap::from([
            (
                "ALLOW_SANDBOX_INTERNAL_CIDRS".to_owned(),
                "10.1.0.0/16, 10.2.0.0/16".to_owned(),
            ),
            ("SANDBOX_EGRESS_DSCP".to_owned(), "64".to_owned()),
        ]);

        let settings = NetworkSettings::from_vars(&vars);

        assert_eq!(settings.internal_cidrs, vec!["10.1.0.0/16", "10.2.0.0/16"]);
        assert_eq!(settings.egress_dscp, 0);
    }
}
