use std::sync::Arc;
use std::time::SystemTime;

use axum::Router;
use tonic::service::Routes;

use super::health::with_health;
use super::info::{InfoServer, new_service_id};
use super::metrics::{HostMetrics, machine_info};
use super::node::Node;
use super::service::SandboxServer;
use super::status::NodeStatus;
use crate::proto::info_service_server::InfoServiceServer;
use crate::proto::sandbox_service_server::SandboxServiceServer;

pub const COMMIT: &str = match option_env!("E2B_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

pub struct App {
    pub node: Arc<Node>,
    pub status: Arc<NodeStatus>,
    pub metrics: Arc<HostMetrics>,
}

impl App {
    pub fn new(node: Arc<Node>) -> Self {
        Self {
            node,
            status: Arc::default(),
            metrics: Arc::default(),
        }
    }

    pub fn start_background(&self) {
        self.node.start_background();
        tokio::spawn(self.metrics.clone().run());
    }

    pub fn router(&self) -> Router {
        let (_, grpc_health) = tonic_health::server::health_reporter();
        let routes = Routes::new(SandboxServiceServer::new(SandboxServer::new(self.node.clone())))
            .add_service(InfoServiceServer::new(self.info()))
            .add_service(grpc_health);
        with_health(routes, self.status.clone(), COMMIT)
    }

    fn info(&self) -> InfoServer {
        let node = self.node.clone();
        InfoServer {
            node_id: self.node.node_id.clone(),
            service_id: new_service_id(),
            commit: COMMIT.to_owned(),
            started: SystemTime::now(),
            labels: self.node.labels.clone(),
            max_sandboxes: self.node.max_sandboxes,
            machine: machine_info(
                &std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default(),
                std::env::consts::ARCH,
            ),
            status: self.status.clone(),
            metrics: self.metrics.clone(),
            running: Box::new(move || node.running_configs()),
        }
    }
}
