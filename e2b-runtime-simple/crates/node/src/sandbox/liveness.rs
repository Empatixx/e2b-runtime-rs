use std::collections::HashSet;

use anyhow::{Result, bail};

use super::Sandboxes;
use super::record::SandboxRecord;
use crate::volume::Volume;

impl Sandboxes {
    pub async fn forget_own_records_after_restart(&self) -> Result<()> {
        let mine = self.records.list().await?.into_iter().filter(|record| {
            record
                .owner
                .as_ref()
                .is_some_and(|owner| owner.node_id == self.me.node_id)
        });
        for record in mine {
            self.forget_dead(&record).await;
        }
        Ok(())
    }

    pub(super) async fn live_record(&self, sandbox_id: &str) -> Result<Option<SandboxRecord>> {
        let Some(record) = self.records.get(sandbox_id).await? else {
            return Ok(None);
        };
        if self.owner_is_alive(&record).await? {
            return Ok(Some(record));
        }
        self.forget_dead(&record).await;
        Ok(None)
    }

    pub(super) async fn live_records(&self) -> Result<Vec<SandboxRecord>> {
        let alive = self.nodes.alive().await?;
        let records = self.records.list().await?;
        Ok(records
            .into_iter()
            .filter(|record| self.owned_by(record, &alive))
            .collect())
    }

    pub(super) async fn attach_volume(&self, name: &str, sandbox_id: &str) -> Result<Volume> {
        let volume = self.host.volumes.get(name).await?;
        let holder = volume.and_then(|volume| volume.attached_to);
        if let Some(holder) = holder.filter(|holder| holder != sandbox_id)
            && self.live_record(&holder).await?.is_some()
        {
            bail!("volume {name} is mounted by sandbox {holder}");
        }
        self.host.volumes.attach(name, sandbox_id).await
    }

    async fn owner_is_alive(&self, record: &SandboxRecord) -> Result<bool> {
        match &record.owner {
            None => Ok(true),
            Some(owner) if owner.node_id == self.me.node_id => Ok(true),
            Some(owner) => self.nodes.is_alive(&owner.node_id).await,
        }
    }

    fn owned_by(&self, record: &SandboxRecord, alive: &HashSet<String>) -> bool {
        record
            .owner
            .as_ref()
            .is_none_or(|owner| owner.node_id == self.me.node_id || alive.contains(&owner.node_id))
    }

    async fn forget_dead(&self, record: &SandboxRecord) {
        let sandbox_id = &record.info.sandbox_id;
        tracing::warn!(sandbox_id, "the node of this sandbox is gone, forgetting it");
        for mount in &record.info.volume_mounts {
            self.host
                .volumes
                .release_if_attached_to(&mount.name, sandbox_id)
                .await
                .unwrap_or_else(|e| tracing::warn!(sandbox_id, error = format!("{e:#}"), "release volume failed"));
        }
        if let Some(checkpoint) = &record.info.resumed_from {
            self.host.checkpoints.delete(checkpoint).await;
        }
        self.records
            .delete(sandbox_id)
            .await
            .unwrap_or_else(|e| tracing::warn!(sandbox_id, error = format!("{e:#}"), "delete record failed"));
    }
}
