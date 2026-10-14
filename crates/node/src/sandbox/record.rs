use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::SandboxInfo;
use crate::store::{Store, Versioned};

const PREFIX: &str = "sandboxes/";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRecord {
    pub info: SandboxInfo,
    pub owner: Option<Owner>,
    pub checkpoint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub node_id: String,
    pub api_url: String,
    pub domain: String,
}

pub enum Claim {
    Claimed(Box<SandboxRecord>),
    Taken,
    Missing,
}

pub struct Records {
    store: Arc<Store>,
}

impl SandboxRecord {
    pub fn running(info: SandboxInfo, owner: Owner) -> Self {
        Self {
            info: SandboxInfo { paused: false, ..info },
            owner: Some(owner),
            checkpoint: None,
        }
    }

    pub fn paused(info: SandboxInfo, checkpoint: String) -> Self {
        Self {
            info: SandboxInfo { paused: true, ..info },
            owner: None,
            checkpoint: Some(checkpoint),
        }
    }
}

impl Records {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub async fn get(&self, sandbox_id: &str) -> Result<Option<SandboxRecord>> {
        Ok(self.versioned(sandbox_id).await?.map(|found| found.value))
    }

    pub async fn list(&self) -> Result<Vec<SandboxRecord>> {
        self.store.list(PREFIX).await
    }

    pub async fn save(&self, record: &SandboxRecord) -> Result<()> {
        self.store.put(&key(&record.info.sandbox_id), record).await
    }

    pub async fn delete(&self, sandbox_id: &str) -> Result<()> {
        self.store.delete(&key(sandbox_id)).await
    }

    pub async fn claim_paused(&self, sandbox_id: &str, owner: &Owner) -> Result<Claim> {
        let Some(current) = self.versioned(sandbox_id).await? else {
            return Ok(Claim::Missing);
        };
        if current.value.owner.is_some() {
            return Ok(Claim::Taken);
        }
        let claimed = SandboxRecord {
            owner: Some(owner.clone()),
            ..current.value.clone()
        };
        match self.store.replace(&key(sandbox_id), &current, &claimed).await? {
            true => Ok(Claim::Claimed(Box::new(claimed))),
            false => Ok(Claim::Taken),
        }
    }

    async fn versioned(&self, sandbox_id: &str) -> Result<Option<Versioned<SandboxRecord>>> {
        self.store.get(&key(sandbox_id)).await
    }
}

fn key(sandbox_id: &str) -> String {
    format!("{PREFIX}{sandbox_id}.json")
}
