use std::sync::Arc;
use std::time::SystemTime;

use tonic::{Request, Response, Status};
use uuid::Uuid;

use super::metrics::HostMetrics;
use super::status::NodeStatus;
use crate::proto::info_service_server::InfoService;
use crate::proto::{
    MachineInfo, SandboxConfig, ServiceInfoResponse, ServiceInfoRole, ServiceInfoStatus, ServiceStatusChangeRequest,
};

const MIB: u64 = 1024 * 1024;

pub type RunningConfigs = Box<dyn Fn() -> Vec<SandboxConfig> + Send + Sync>;

pub struct InfoServer {
    pub node_id: String,
    pub service_id: String,
    pub commit: String,
    pub started: SystemTime,
    pub labels: Vec<String>,
    pub max_sandboxes: i64,
    pub machine: MachineInfo,
    pub status: Arc<NodeStatus>,
    pub metrics: Arc<HostMetrics>,
    pub running: RunningConfigs,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Allocated {
    sandboxes: u32,
    vcpus: u64,
    memory_mb: u64,
    disk_mb: u64,
}

impl Allocated {
    fn of(configs: &[SandboxConfig]) -> Self {
        configs.iter().fold(Self::default(), |sum, config| Self {
            sandboxes: sum.sandboxes + 1,
            vcpus: sum.vcpus + config.vcpu.max(0) as u64,
            memory_mb: sum.memory_mb + config.ram_mb.max(0) as u64,
            disk_mb: sum.disk_mb + config.total_disk_size_mb.max(0) as u64,
        })
    }
}

pub fn new_service_id() -> String {
    Uuid::new_v4().to_string()
}

impl InfoServer {
    fn response(&self) -> ServiceInfoResponse {
        let state = self.status.state();
        let allocated = Allocated::of(&(self.running)());
        let host = self.metrics.latest();
        ServiceInfoResponse {
            node_id: self.node_id.clone(),
            service_id: self.service_id.clone(),
            service_version: crate::version().to_owned(),
            service_commit: self.commit.clone(),
            service_status: state.status.into(),
            service_roles: vec![ServiceInfoRole::Orchestrator.into()],
            service_startup: Some(self.started.into()),
            machine_info: Some(self.machine.clone()),
            labels: self.labels.clone(),
            service_status_changed_at: Some(state.changed_at.into()),
            outstanding_work: self.status.outstanding_work(),
            max_sandboxes: self.max_sandboxes,
            metric_vcpu_used: allocated.vcpus as i64,
            metric_memory_used_mb: allocated.memory_mb as i64,
            metric_disk_mb: allocated.disk_mb as i64,
            metric_sandboxes_running: allocated.sandboxes,
            metric_cpu_percent: host.cpu_percent,
            metric_memory_used_bytes: host.memory_used_bytes,
            metric_cpu_count: host.cpu_count,
            metric_memory_total_bytes: host.memory_total_bytes,
            metric_cpu_allocated: allocated.vcpus as u32,
            metric_memory_allocated_bytes: allocated.memory_mb * MIB,
            metric_disk_allocated_bytes: allocated.disk_mb * MIB,
            metric_disks: host.disks,
            metric_hugepages_total: host.hugepages.total,
            metric_hugepages_used: host.hugepages.used,
            metric_hugepages_reserved: host.hugepages.reserved,
            metric_hugepage_size_bytes: host.hugepages.size_bytes,
        }
    }
}

#[tonic::async_trait]
impl InfoService for InfoServer {
    async fn service_info(&self, _: Request<()>) -> Result<Response<ServiceInfoResponse>, Status> {
        Ok(Response::new(self.response()))
    }

    async fn service_status_override(
        &self,
        request: Request<ServiceStatusChangeRequest>,
    ) -> Result<Response<()>, Status> {
        let requested = ServiceInfoStatus::try_from(request.into_inner().service_status)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        self.status
            .override_to(requested)
            .map(Response::new)
            .map_err(Status::failed_precondition)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vcpu: i64, ram_mb: i64, total_disk_size_mb: i64) -> SandboxConfig {
        SandboxConfig {
            vcpu,
            ram_mb,
            total_disk_size_mb,
            ..SandboxConfig::default()
        }
    }

    fn server(running: Vec<SandboxConfig>) -> InfoServer {
        InfoServer {
            node_id: "node-1".to_owned(),
            service_id: "svc".to_owned(),
            commit: "abc".to_owned(),
            started: SystemTime::UNIX_EPOCH,
            labels: vec!["gpu".to_owned()],
            max_sandboxes: 200,
            machine: MachineInfo::default(),
            status: Arc::default(),
            metrics: Arc::default(),
            running: Box::new(move || running.clone()),
        }
    }

    #[tokio::test]
    async fn service_info_sums_the_running_sandboxes() {
        let info = server(vec![config(2, 1024, 5000), config(4, 512, 3000)])
            .service_info(Request::new(()))
            .await
            .unwrap()
            .into_inner();

        assert_eq!(
            (
                info.metric_sandboxes_running,
                info.metric_cpu_allocated,
                info.metric_memory_allocated_bytes,
                info.metric_disk_allocated_bytes
            ),
            (2, 6, 1536 * MIB, 8000 * MIB)
        );
        assert_eq!(
            (info.metric_vcpu_used, info.metric_memory_used_mb, info.metric_disk_mb),
            (6, 1536, 8000)
        );
        assert_eq!(info.service_roles, vec![ServiceInfoRole::Orchestrator as i32]);
        assert_eq!(info.service_status, ServiceInfoStatus::Healthy as i32);
        assert_eq!((info.node_id.as_str(), info.max_sandboxes), ("node-1", 200));
    }

    #[tokio::test]
    async fn overriding_to_shutting_down_is_a_failed_precondition() {
        let error = server(Vec::new())
            .service_status_override(Request::new(ServiceStatusChangeRequest {
                service_status: ServiceInfoStatus::ShuttingDown.into(),
            }))
            .await
            .unwrap_err();

        assert_eq!(
            (error.code(), error.message()),
            (
                tonic::Code::FailedPrecondition,
                "cannot override node status to ShuttingDown"
            )
        );
    }
}
