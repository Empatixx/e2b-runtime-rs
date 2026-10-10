use std::net::{IpAddr, SocketAddr};

use super::poll::ObservedSandbox;
use crate::envd::ENVD_PORT;
use crate::server::node::Node;

pub fn running(node: &Node) -> Vec<ObservedSandbox> {
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
