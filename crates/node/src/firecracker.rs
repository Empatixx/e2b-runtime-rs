pub mod api;
pub mod process;

pub use api::FirecrackerApi;
pub use process::FirecrackerProcess;

use std::path::Path;

use anyhow::Result;
use serde_json::json;

use crate::storage::memory::DirtyMemory;

pub const ROOTFS_DRIVE: &str = "rootfs.dev";
pub const DRIVE_SLOTS: usize = 2;
const GUEST_MAC: &str = "02:FC:00:00:00:05";
const BOOT_ARGS: &str = "console=ttyS0 reboot=k panic=1 pci=off rw quiet init=/sbin/e2b-init";

pub fn drive_file(slot: usize) -> String {
    format!("vol{slot}.dev")
}

fn drive_id(slot: usize) -> String {
    format!("vol{slot}")
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Metadata {
    #[serde(rename = "instanceID")]
    pub sandbox_id: String,
    #[serde(rename = "envID")]
    pub template_id: String,
    #[serde(rename = "address")]
    pub logs_address: String,
    #[serde(rename = "accessTokenHash")]
    pub access_token_hash: String,
}

#[derive(Debug, Clone, Copy)]
pub struct MachineSize {
    pub vcpus: u32,
    pub memory_mib: u32,
}

impl FirecrackerApi {
    pub async fn boot(&self, kernel: &Path, size: MachineSize) -> Result<()> {
        self.set_kernel(kernel).await?;
        self.attach_rootfs().await?;
        self.attach_drive_slots().await?;
        self.attach_network().await?;
        self.enable_metadata().await?;
        self.set_machine_size(size).await?;
        self.start().await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn load(&self, vm_state: &Path, memory_device: &Path) -> Result<()> {
        self.put(
            "/snapshot/load",
            json!({
                "snapshot_path": vm_state,
                "mem_backend": { "backend_type": "File", "backend_path": memory_device },
                "track_dirty_pages": true,
                "resume_vm": true,
            }),
        )
        .await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn set_metadata(&self, metadata: &Metadata) -> Result<()> {
        self.put("/mmds", serde_json::to_value(metadata)?).await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn refresh_drive(&self, slot: usize) -> Result<()> {
        let drive = json!({ "drive_id": drive_id(slot), "path_on_host": drive_file(slot) });
        self.patch(&format!("/drives/{}", drive_id(slot)), drive).await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn pause(&self) -> Result<()> {
        self.patch("/vm", json!({ "state": "Paused" })).await
    }

    pub async fn resume(&self) -> Result<()> {
        self.patch("/vm", json!({ "state": "Resumed" })).await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn save_state(&self, vm_state: &Path) -> Result<()> {
        let snapshot = json!({ "snapshot_type": "Diff", "snapshot_path": vm_state });
        self.put("/snapshot/create", snapshot).await
    }

    #[tracing::instrument(level = "debug", skip_all)]
    pub async fn dirty_memory(&self) -> Result<DirtyMemory> {
        let body = self.get("/vm/dirty-memory-ranges").await?;
        Ok(serde_json::from_slice(&body)?)
    }

    async fn set_kernel(&self, kernel: &Path) -> Result<()> {
        let boot = json!({ "kernel_image_path": kernel, "boot_args": BOOT_ARGS });
        self.put("/boot-source", boot).await
    }

    async fn attach_rootfs(&self) -> Result<()> {
        let drive = json!({ "drive_id": "rootfs", "path_on_host": ROOTFS_DRIVE, "is_root_device": true, "is_read_only": false });
        self.put("/drives/rootfs", drive).await
    }

    async fn attach_drive_slots(&self) -> Result<()> {
        for slot in 0..DRIVE_SLOTS {
            let drive = json!({ "drive_id": drive_id(slot), "path_on_host": drive_file(slot), "is_root_device": false, "is_read_only": false });
            self.put(&format!("/drives/{}", drive_id(slot)), drive).await?;
        }
        Ok(())
    }

    async fn attach_network(&self) -> Result<()> {
        let interface = json!({ "iface_id": "eth0", "host_dev_name": "tap0", "guest_mac": GUEST_MAC });
        self.put("/network-interfaces/eth0", interface).await
    }

    async fn enable_metadata(&self) -> Result<()> {
        let mmds = json!({ "version": "V2", "network_interfaces": ["eth0"] });
        self.put("/mmds/config", mmds).await
    }

    async fn set_machine_size(&self, size: MachineSize) -> Result<()> {
        let machine = json!({ "vcpu_count": size.vcpus, "mem_size_mib": size.memory_mib, "track_dirty_pages": true });
        self.put("/machine-config", machine).await
    }

    async fn start(&self) -> Result<()> {
        self.put("/actions", json!({ "action_type": "InstanceStart" })).await
    }
}
