use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Args;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::checkpoint::{Checkpoint, checkpoint_id};
use crate::firecracker::MachineSize;
use crate::host::Host;
use crate::layers::Layer;
use crate::network::run;
use crate::sandbox::envd::Envd;
use crate::sandbox::vm::Vm;
use crate::sandbox::warm_pool::WarmVm;
use crate::storage::package_raw_file;
use crate::store::Store;

const SETTLE_AFTER_BOOT: Duration = Duration::from_secs(2);
const INIT_SCRIPT: &str = include_str!("template/e2b-init.sh");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub template_id: String,
    pub checkpoint: String,
}

pub struct Templates {
    store: Arc<Store>,
}

#[derive(Args, Debug, Clone)]
pub struct BuildRequest {
    #[arg(long)]
    pub name: String,
    #[arg(long, default_value = "ubuntu:24.04")]
    pub image: String,
    #[arg(long)]
    pub envd: PathBuf,
    #[arg(long, default_value_t = 2)]
    pub vcpus: u32,
    #[arg(long, default_value_t = 1024)]
    pub memory_mib: u32,
    #[arg(long, default_value_t = 4096)]
    pub disk_mb: u64,
}

impl Templates {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub async fn get(&self, template_id: &str) -> Result<Template> {
        let found = self.store.get(&record_key(&normalize(template_id))).await?;
        Ok(found
            .with_context(|| format!("template {template_id} not found"))?
            .value)
    }

    pub async fn save(&self, template: &Template) -> Result<()> {
        self.store.put(&record_key(&template.template_id), template).await
    }
}

fn record_key(template_id: &str) -> String {
    format!("templates/{template_id}.json")
}

fn normalize(template_id: &str) -> String {
    let without_tag = template_id.split(':').next().unwrap_or(template_id);
    without_tag.rsplit('/').next().unwrap_or(without_tag).to_owned()
}

pub async fn build(host: &Host, request: &BuildRequest) -> Result<Template> {
    let build_dir = host.config.builds_dir().join(&request.name);
    let checkpoint_dir = host.checkpoints.new_dir();
    let rootfs = build_rootfs(&build_dir, request).await?;
    let base_layer = publish_rootfs(host, &rootfs, &checkpoint_dir.join("base.commit")).await?;
    let checkpoint = boot_and_capture(host, request, &checkpoint_dir, base_layer).await?;
    host.checkpoints.save(&checkpoint).await?;
    let template = Template {
        template_id: request.name.clone(),
        checkpoint: checkpoint.id,
    };
    host.templates.save(&template).await?;
    let _ = std::fs::remove_dir_all(&build_dir);
    Ok(template)
}

async fn publish_rootfs(host: &Host, rootfs: &Path, sealed: &Path) -> Result<Layer> {
    std::fs::create_dir_all(sealed.parent().context("layer without parent")?)?;
    package_raw_file(rootfs, sealed).await.context("package rootfs layer")?;
    host.storage.publish(sealed).await
}

async fn build_rootfs(build_dir: &Path, request: &BuildRequest) -> Result<PathBuf> {
    let context = build_dir.join("context");
    let tree = build_dir.join("tree");
    let rootfs = build_dir.join("rootfs.ext4");
    let _ = std::fs::remove_dir_all(build_dir);
    std::fs::create_dir_all(&context)?;
    std::fs::create_dir_all(&tree)?;
    write_docker_context(&context, request)?;
    let image = format!("e2b-simple/{}", request.name);
    run("docker", &["build", "-q", "-t", &image, &context.display().to_string()]).await?;
    export_image(&image, &tree).await?;
    format_rootfs(&tree, &rootfs, request.disk_mb).await?;
    Ok(rootfs)
}

fn write_docker_context(context: &Path, request: &BuildRequest) -> Result<()> {
    std::fs::copy(&request.envd, context.join("envd")).context("copy envd binary")?;
    std::fs::write(context.join("e2b-init"), INIT_SCRIPT)?;
    std::fs::write(context.join("Dockerfile"), dockerfile(&request.image))?;
    Ok(())
}

fn dockerfile(image: &str) -> String {
    format!(
        "FROM {image}\n\
         RUN apt-get update && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
         sudo iproute2 ca-certificates curl python3 && rm -rf /var/lib/apt/lists/*\n\
         RUN (id -u ubuntu >/dev/null 2>&1 && userdel -r ubuntu || true) && useradd -m -u 1000 -s /bin/bash user \
         && echo 'user ALL=(ALL) NOPASSWD:ALL' > /etc/sudoers.d/user\n\
         COPY envd /usr/bin/envd\n\
         COPY e2b-init /sbin/e2b-init\n\
         RUN chmod 755 /usr/bin/envd /sbin/e2b-init\n"
    )
}

async fn export_image(image: &str, tree: &Path) -> Result<()> {
    let container = docker_output(&["create", image]).await?;
    let export = format!("docker export {container} | tar -x -C {}", tree.display());
    let exported = run("sh", &["-c", &export]).await;
    run("docker", &["rm", &container]).await?;
    exported
}

async fn format_rootfs(tree: &Path, rootfs: &Path, disk_mb: u64) -> Result<()> {
    let rootfs = rootfs.display().to_string();
    run("truncate", &["-s", &format!("{disk_mb}M"), &rootfs]).await?;
    run("mkfs.ext4", &["-q", "-F", "-d", &tree.display().to_string(), &rootfs]).await
}

async fn boot_and_capture(
    host: &Host,
    request: &BuildRequest,
    checkpoint_dir: &Path,
    base_layer: Layer,
) -> Result<Checkpoint> {
    let size = MachineSize {
        vcpus: request.vcpus,
        memory_mib: request.memory_mib,
    };
    let warm = WarmVm::spawn(host).await?;
    let mut vm = Vm::boot(host, warm, vec![base_layer], size).await?;
    let captured = async {
        Envd::new(vm.slot().host_ip).wait_ready().await?;
        tokio::time::sleep(SETTLE_AFTER_BOOT).await;
        vm.capture(host, checkpoint_dir).await
    }
    .await;
    vm.shutdown(host).await;
    let captured = captured?;
    Ok(Checkpoint {
        id: checkpoint_id(checkpoint_dir)?,
        memory_layers: captured.memory_layers,
        rootfs_layers: captured.rootfs_layers,
        drives: Vec::new(),
        vcpus: request.vcpus,
        memory_mib: request.memory_mib,
        disk_mb: request.disk_mb,
        envd_version: envd_version(&request.envd).await?,
    })
}

async fn envd_version(envd: &Path) -> Result<String> {
    let output = Command::new(envd).arg("-version").output().await?;
    ensure!(output.status.success(), "envd -version failed");
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

async fn docker_output(args: &[&str]) -> Result<String> {
    let output = Command::new("docker").args(args).output().await?;
    ensure!(
        output.status.success(),
        "docker {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_snapshot_names() {
        assert_eq!(normalize("team/my-snap:default"), "my-snap");
        assert_eq!(normalize("base"), "base");
    }
}
