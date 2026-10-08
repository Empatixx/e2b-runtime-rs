use std::sync::Arc;
use std::time::{Duration, SystemTime};

use axum::Router;
use tonic::service::Routes;

use super::config::BuildStorage;
use super::health::with_health;
use super::info::{InfoServer, new_service_id};
use super::metrics::{HostMetrics, machine_info};
use super::node::Node;
use super::service::SandboxServer;
use super::status::NodeStatus;
use super::upload::UploadHandler;
use crate::builder::builds::BuildCache;
use crate::builder::config::BuilderConfig;
use crate::builder::oci::pull::Puller;
use crate::builder::phases::BuildEnv;
use crate::builder::pipeline::PhasePipeline;
use crate::builder::service::{TemplateServer, TemplateStorages};
use crate::proto::info_service_server::InfoServiceServer;
use crate::proto::sandbox_service_server::SandboxServiceServer;
use crate::proto::template_service_server::TemplateServiceServer;

const CONSUMER_STATUS_CHECK_GRACE: Duration = Duration::from_secs(15);

pub const COMMIT: &str = match option_env!("E2B_COMMIT") {
    Some(commit) => commit,
    None => "unknown",
};

pub struct TemplateManagerSetup {
    pub build: BuildStorage,
    pub builder: BuilderConfig,
    pub development: bool,
}

pub struct App {
    pub node: Arc<Node>,
    pub status: Arc<NodeStatus>,
    pub metrics: Arc<HostMetrics>,
    pub templates: Option<Arc<TemplateServer<PhasePipeline>>>,
    uploads: Option<UploadHandler>,
    builds: Arc<BuildCache>,
}

impl App {
    pub fn new(node: Arc<Node>, setup: Option<TemplateManagerSetup>) -> Result<Self, String> {
        let builds = Arc::new(BuildCache::default());
        let templates = setup
            .as_ref()
            .map(|setup| template_server(&node, &builds, setup))
            .transpose()?;
        Ok(Self {
            status: node.status.clone(),
            node,
            metrics: Arc::default(),
            templates,
            uploads: setup.and_then(|setup| setup.build.uploads),
            builds,
        })
    }

    pub fn start_background(&self) {
        self.node.start_background();
        tokio::spawn(self.metrics.clone().run());
        tokio::spawn(self.builds.clone().run_eviction());
    }

    pub async fn wait_for_templates(&self, force_stop: bool, local: bool) -> Result<(), String> {
        let Some(templates) = &self.templates else {
            return Ok(());
        };
        if force_stop {
            return Err("waiting for template builds: context canceled".to_owned());
        }
        tracing::info!("Waiting for all build jobs to finish");
        templates.wait_for_builds().await;
        if !local {
            tracing::info!("Waiting for consumers to check build status");
            tokio::time::sleep(CONSUMER_STATUS_CHECK_GRACE).await;
        }
        tracing::info!("Template build queue cleaned");
        Ok(())
    }

    pub fn router(&self) -> Router {
        let (_, grpc_health) = tonic_health::server::health_reporter();
        let routes = Routes::new(SandboxServiceServer::new(SandboxServer::new(self.node.clone())))
            .add_service(InfoServiceServer::new(self.info()))
            .add_service(grpc_health);
        let routes = match &self.templates {
            Some(templates) => routes.add_service(TemplateServiceServer::from_arc(templates.clone())),
            None => routes,
        };
        let router = with_health(routes, self.status.clone(), COMMIT);
        match &self.uploads {
            Some(uploads) => uploads.clone().route(router),
            None => router,
        }
    }

    fn info(&self) -> InfoServer {
        let node = self.node.clone();
        InfoServer {
            node_id: self.node.node_id.clone(),
            service_id: new_service_id(),
            commit: COMMIT.to_owned(),
            started: SystemTime::now(),
            labels: self.node.labels.clone(),
            roles: self.node.services.roles(),
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

fn template_server(
    node: &Arc<Node>,
    builds: &Arc<BuildCache>,
    setup: &TemplateManagerSetup,
) -> Result<Arc<TemplateServer<PhasePipeline>>, String> {
    let storages = TemplateStorages {
        template: node.storage.clone(),
        build: setup.build.storage.clone(),
    };
    let env = BuildEnv {
        node: node.clone(),
        config: setup.builder.clone(),
        puller: Puller::new(&setup.builder)?,
        build_storage: setup.build.storage.clone(),
        development: setup.development,
    };
    Ok(Arc::new(TemplateServer::new(
        builds.clone(),
        storages,
        Arc::new(PhasePipeline { env: Arc::new(env) }),
    )))
}
