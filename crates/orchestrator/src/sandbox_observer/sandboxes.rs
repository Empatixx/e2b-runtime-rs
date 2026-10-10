use std::net::{IpAddr, SocketAddr};

use super::poll::ObservedSandbox;
use crate::envd::ENVD_PORT;
use crate::server::node::Node;

pub fn running(node: &Node) -> Vec<ObservedSandbox> {
    let mut observed = serving(node);
    observed.extend(building(node));
    observed
}

fn building(node: &Node) -> Vec<ObservedSandbox> {
    node.building()
        .into_iter()
        .map(|(host_ip, build)| ObservedSandbox {
            sandbox_id: build.sandbox_id,
            team_id: build.team_id,
            template_id: build.template_id,
            build_id: String::new(),
            sandbox_type: BUILD_SANDBOX_TYPE.to_owned(),
            envd_version: build.envd_version,
            envd: SocketAddr::new(IpAddr::V4(host_ip), ENVD_PORT),
            access_token: None,
            started_at: build.started_at,
        })
        .collect()
}

fn serving(node: &Node) -> Vec<ObservedSandbox> {
    node.registry
        .running()
        .iter()
        .map(|handle| {
            let info = handle.info.lock();
            ObservedSandbox {
                sandbox_id: info.config.sandbox_id.clone(),
                team_id: info.config.team_id.clone(),
                template_id: info.config.template_id.clone(),
                build_id: info.config.build_id.clone(),
                sandbox_type: SANDBOX_TYPE.to_owned(),
                envd_version: info.config.envd_version.clone(),
                envd: SocketAddr::new(IpAddr::V4(info.host_ip), ENVD_PORT),
                access_token: info.config.envd_access_token.clone(),
                started_at: info.started_at,
            }
        })
        .collect()
}

const SANDBOX_TYPE: &str = "sandbox";
const BUILD_SANDBOX_TYPE: &str = "build";

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::time::SystemTime;

    use super::*;
    use crate::server::config::NodeConfig;
    use crate::server::node::BuildSandbox;

    fn node(dir: &tempfile::TempDir) -> Arc<Node> {
        let path = |name: &str| dir.path().join(name).display().to_string();
        let vars = [
            ("STORAGE_PROVIDER", "Local".to_owned()),
            ("LOCAL_TEMPLATE_STORAGE_BASE_PATH", path("store")),
            ("E2B_CACHE_DIR", path("cache")),
            ("E2B_WORK_DIR", path("sandboxes")),
            ("E2B_FIRECRACKER", "/unused".to_owned()),
            ("E2B_KERNEL", "/unused".to_owned()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
        Arc::new(Node::new(&NodeConfig::from_vars(&vars).unwrap()).unwrap())
    }

    #[tokio::test]
    async fn template_build_sandboxes_are_observed_like_upstream_and_leave_with_their_network() {
        let dir = tempfile::tempdir().unwrap();
        let node = node(&dir);
        let host_ip = Ipv4Addr::new(10, 11, 0, 2);
        let build = BuildSandbox {
            sandbox_id: "build-sbx".to_owned(),
            team_id: "team-1".to_owned(),
            template_id: "tpl-1".to_owned(),
            envd_version: "0.6.13".to_owned(),
            started_at: SystemTime::now(),
        };

        let network = node.assign_build_network(host_ip, build);
        let observed = running(&node);

        assert_eq!(observed.len(), 1);
        assert_eq!(observed[0].sandbox_id, "build-sbx");
        assert_eq!(observed[0].sandbox_type, "build");
        assert_eq!(observed[0].team_id, "team-1");
        assert_eq!(observed[0].envd, SocketAddr::new(IpAddr::V4(host_ip), ENVD_PORT));
        assert_eq!(observed[0].access_token, None);
        drop(network);
        assert!(running(&node).is_empty());
    }
}
