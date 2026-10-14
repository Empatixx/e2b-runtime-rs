use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::store::Store;

const PREFIX: &str = "nodes/";
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const ALIVE_FOR: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Heartbeat {
    node_id: String,
    alive_until: DateTime<Utc>,
}

pub struct Nodes {
    store: Arc<Store>,
    me: String,
}

impl Nodes {
    pub fn new(store: Arc<Store>, me: String) -> Arc<Self> {
        Arc::new(Self { store, me })
    }

    pub fn spawn_heartbeat(self: &Arc<Self>) {
        let nodes = self.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = nodes.beat().await {
                    tracing::warn!(error = format!("{e:#}"), "heartbeat failed");
                }
                tokio::time::sleep(HEARTBEAT_INTERVAL).await;
            }
        });
    }

    pub async fn beat(&self) -> Result<()> {
        let heartbeat = Heartbeat {
            node_id: self.me.clone(),
            alive_until: Utc::now() + ALIVE_FOR,
        };
        self.store.put(&key(&self.me), &heartbeat).await
    }

    pub async fn is_alive(&self, node_id: &str) -> Result<bool> {
        let found = self.store.get::<Heartbeat>(&key(node_id)).await?;
        Ok(found.is_some_and(|heartbeat| heartbeat.value.alive_until > Utc::now()))
    }

    pub async fn alive(&self) -> Result<HashSet<String>> {
        let now = Utc::now();
        let heartbeats: Vec<Heartbeat> = self.store.list(PREFIX).await?;
        Ok(heartbeats
            .into_iter()
            .filter(|heartbeat| heartbeat.alive_until > now)
            .map(|heartbeat| heartbeat.node_id)
            .collect())
    }
}

fn key(node_id: &str) -> String {
    format!("{PREFIX}{node_id}.json")
}
