use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use nfsserve::context::RPCContext;
use nfsserve::tcp::process_socket;
use parking_lot::Mutex;
use tokio::net::{TcpListener, TcpStream};
use uuid::Uuid;

use super::sandbox_fs::{SandboxVolumes, VolumeRoot};
use crate::server::config::volume_mounts::VolumeMounts;

const ADVERTISED_PORT: u16 = 2049;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub name: String,
    pub id: Uuid,
    pub volume_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountingSandbox {
    pub lifecycle_id: String,
    pub team_id: Uuid,
    pub volumes: Vec<VolumeMount>,
}

pub type MountSource = Arc<dyn Fn(IpAddr) -> Option<MountingSandbox> + Send + Sync>;
pub type LiveLifecycles = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

pub struct NfsProxy {
    source: MountSource,
    live: LiveLifecycles,
    paths: VolumeMounts,
    mounted: Mutex<HashMap<String, Arc<SandboxVolumes>>>,
}

impl NfsProxy {
    pub fn new(source: MountSource, live: LiveLifecycles, paths: VolumeMounts) -> Arc<Self> {
        Arc::new(Self {
            source,
            live,
            paths,
            mounted: Mutex::default(),
        })
    }

    pub async fn serve(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((socket, peer)) => self.clone().handle(socket, peer),
                Err(error) => tracing::warn!(%error, "nfs proxy accept failed"),
            }
        }
    }

    pub async fn serve_portmap(self: Arc<Self>, listener: TcpListener) {
        loop {
            match listener.accept().await {
                Ok((socket, peer)) => spawn_connection(socket, peer, Arc::new(SandboxVolumes::new(Vec::new()))),
                Err(error) => tracing::warn!(%error, "portmap accept failed"),
            }
        }
    }

    fn handle(self: Arc<Self>, socket: TcpStream, peer: SocketAddr) {
        match self.volumes_for(peer.ip()) {
            Some(volumes) => spawn_connection(socket, peer, volumes),
            None => tracing::warn!(sandbox_ip = %peer.ip(), "nfs connection from an unknown sandbox"),
        }
    }

    fn volumes_for(&self, ip: IpAddr) -> Option<Arc<SandboxVolumes>> {
        let sandbox = (self.source)(ip)?;
        let live = (self.live)();
        let mut mounted = self.mounted.lock();
        mounted.retain(|lifecycle, _| live.contains(lifecycle));
        Some(
            mounted
                .entry(sandbox.lifecycle_id.clone())
                .or_insert_with(|| Arc::new(SandboxVolumes::new(self.roots(&sandbox))))
                .clone(),
        )
    }

    fn roots(&self, sandbox: &MountingSandbox) -> Vec<VolumeRoot> {
        sandbox
            .volumes
            .iter()
            .filter_map(|volume| {
                self.paths
                    .volume_path(&volume.volume_type, sandbox.team_id, volume.id)
                    .map(|path| VolumeRoot::new(volume.name.clone(), path))
                    .inspect_err(
                        |error| tracing::warn!(volume = %volume.name, %error, "failed to resolve the volume path"),
                    )
                    .ok()
            })
            .collect()
    }
}

fn spawn_connection(socket: TcpStream, peer: SocketAddr, volumes: Arc<SandboxVolumes>) {
    let context = RPCContext::new(ADVERTISED_PORT, peer.to_string(), volumes);
    tokio::spawn(async move {
        if let Err(error) = process_socket(socket, context).await {
            tracing::debug!(%error, %peer, "nfs connection closed");
        }
    });
}
