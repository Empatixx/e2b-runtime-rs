use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, SecondsFormat, Utc};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::Serialize;

pub const STORE_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RoutingRecord {
    pub orchestrator_id: String,
    pub orchestrator_ip: String,
    pub execution_id: String,
    pub sandbox_started_at: String,
    pub sandbox_max_length_in_hours: i64,
}

pub trait RouteStore: Send + Sync {
    fn store(
        &self,
        sandbox_id: &str,
        record: &RoutingRecord,
        ttl: Option<Duration>,
    ) -> BoxFuture<'static, Result<(), String>>;
    fn delete(&self, sandbox_id: &str, execution_id: &str) -> BoxFuture<'static, Result<i64, String>>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedSandbox {
    pub sandbox_id: String,
    pub lifecycle_id: String,
    pub execution_id: String,
    pub started_at: SystemTime,
    pub max_length_hours: i64,
}

#[derive(Default)]
struct RouteState {
    inserted: bool,
    written: bool,
    stopped: bool,
}

pub struct RoutingPublisher {
    store: OnceLock<Arc<dyn RouteStore>>,
    orchestrator_id: String,
    node_ip: String,
    routes: Mutex<HashMap<String, RouteState>>,
}

impl RoutingPublisher {
    pub fn new(store: Option<Arc<dyn RouteStore>>, orchestrator_id: String, node_ip: String) -> Self {
        Self {
            store: store.map(OnceLock::from).unwrap_or_default(),
            orchestrator_id,
            node_ip,
            routes: Mutex::default(),
        }
    }

    pub fn install(&self, store: Arc<dyn RouteStore>) {
        let _ = self.store.set(store);
    }

    pub async fn on_insert(&self, sandbox: &RoutedSandbox) {
        let Some(store) = self.store.get() else {
            return;
        };
        let key = lifecycle_key(sandbox);
        let write = {
            let mut routes = self.routes.lock();
            let state = routes.entry(key.clone()).or_default();
            if state.stopped {
                routes.remove(&key);
                false
            } else {
                state.inserted = true;
                state.written = true;
                true
            }
        };
        if !write {
            return;
        }
        let record = RoutingRecord {
            orchestrator_id: self.orchestrator_id.clone(),
            orchestrator_ip: self.node_ip.clone(),
            execution_id: sandbox.execution_id.clone(),
            sandbox_started_at: DateTime::<Utc>::from(sandbox.started_at).to_rfc3339_opts(SecondsFormat::AutoSi, true),
            sandbox_max_length_in_hours: sandbox.max_length_hours,
        };
        let ttl = (sandbox.max_length_hours > 0).then(|| Duration::from_secs(sandbox.max_length_hours as u64 * 3600));
        let stored = tokio::time::timeout(STORE_TIMEOUT, store.store(&sandbox.sandbox_id, &record, ttl)).await;
        if let Err(e) = stored.unwrap_or_else(|_| Err("context deadline exceeded".to_owned())) {
            tracing::error!(sandbox_id = %sandbox.sandbox_id, lifecycle_id = %sandbox.lifecycle_id, error = %e, "failed to publish sandbox routing record");
        }
    }

    pub async fn on_stopping(&self, sandbox: &RoutedSandbox) {
        let Some(store) = self.store.get() else {
            return;
        };
        let key = lifecycle_key(sandbox);
        let delete = {
            let mut routes = self.routes.lock();
            let state = routes.entry(key.clone()).or_default();
            state.stopped = true;
            match state.inserted {
                false => false,
                true => {
                    let written = state.written;
                    routes.remove(&key);
                    written
                }
            }
        };
        if !delete {
            return;
        }
        let deleted =
            tokio::time::timeout(STORE_TIMEOUT, store.delete(&sandbox.sandbox_id, &sandbox.execution_id)).await;
        if let Err(e) = deleted.unwrap_or_else(|_| Err("context deadline exceeded".to_owned())) {
            tracing::error!(sandbox_id = %sandbox.sandbox_id, lifecycle_id = %sandbox.lifecycle_id, error = %e, "failed to delete sandbox routing record; it expires via TTL");
        }
    }
}

fn lifecycle_key(sandbox: &RoutedSandbox) -> String {
    format!("{}/{}", sandbox.sandbox_id, sandbox.lifecycle_id)
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;

    use super::*;

    #[derive(Default)]
    struct FakeStore {
        calls: Mutex<Vec<String>>,
    }

    impl RouteStore for Arc<FakeStore> {
        fn store(
            &self,
            sandbox_id: &str,
            record: &RoutingRecord,
            ttl: Option<Duration>,
        ) -> BoxFuture<'static, Result<(), String>> {
            self.calls.lock().push(format!(
                "set {sandbox_id} {} {ttl:?}",
                serde_json::to_string(record).unwrap()
            ));
            async { Ok(()) }.boxed()
        }

        fn delete(&self, sandbox_id: &str, execution_id: &str) -> BoxFuture<'static, Result<i64, String>> {
            self.calls.lock().push(format!("del {sandbox_id} {execution_id}"));
            async { Ok(1) }.boxed()
        }
    }

    fn sandbox(lifecycle: &str) -> RoutedSandbox {
        RoutedSandbox {
            sandbox_id: "sbx".to_owned(),
            lifecycle_id: lifecycle.to_owned(),
            execution_id: "exec".to_owned(),
            started_at: SystemTime::UNIX_EPOCH,
            max_length_hours: 24,
        }
    }

    fn publisher() -> (RoutingPublisher, Arc<FakeStore>) {
        let store = Arc::new(FakeStore::default());
        let publisher = RoutingPublisher::new(Some(Arc::new(store.clone())), "orch".to_owned(), "10.0.0.5".to_owned());
        (publisher, store)
    }

    #[tokio::test]
    async fn a_running_sandbox_is_published_and_removed_by_execution() {
        let (publisher, store) = publisher();

        publisher.on_insert(&sandbox("l1")).await;
        publisher.on_stopping(&sandbox("l1")).await;

        assert_eq!(
            store.calls.lock().clone(),
            vec![
                r#"set sbx {"orchestrator_id":"orch","orchestrator_ip":"10.0.0.5","execution_id":"exec","sandbox_started_at":"1970-01-01T00:00:00Z","sandbox_max_length_in_hours":24} Some(86400s)"#.to_owned(),
                "del sbx exec".to_owned(),
            ]
        );
    }

    #[tokio::test]
    async fn a_stop_before_the_insert_leaves_a_tombstone() {
        let (publisher, store) = publisher();

        publisher.on_stopping(&sandbox("l1")).await;
        publisher.on_insert(&sandbox("l1")).await;

        assert!(store.calls.lock().is_empty());
    }
}
