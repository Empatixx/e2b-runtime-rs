use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use launchdarkly_server_sdk::{Client, ConfigBuilder, Context};
use serde_json::Value;

use super::contexts::{FlagContext, merge_contexts};
use super::flags::Flag;

pub const LAUNCH_DARKLY_API_KEY: &str = "LAUNCH_DARKLY_API_KEY";
pub const INIT_TIMEOUT: Duration = Duration::from_secs(5);

pub trait FlagValue: Clone {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Self;
    fn from_json(value: &Value) -> Option<Self>;
}

impl FlagValue for bool {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Self {
        client.bool_variation(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        value.as_bool()
    }
}

impl FlagValue for i64 {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Self {
        client.int_variation(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        value.as_i64()
    }
}

impl FlagValue for String {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Self {
        client.str_variation(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_owned)
    }
}

impl FlagValue for Value {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Self {
        client.json_variation(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(value.clone())
    }
}

struct Connection {
    launch_darkly: Option<Client>,
    overrides: HashMap<String, Value>,
    extra_contexts: Vec<FlagContext>,
}

#[derive(Clone)]
pub struct FeatureFlags {
    connection: Arc<Connection>,
}

#[derive(Debug, Default)]
pub struct FeatureFlagsBuilder {
    api_key: String,
    deployment_name: String,
    service_name: String,
    context_providers: Vec<FlagContext>,
    overrides: HashMap<String, Value>,
}

impl FeatureFlagsBuilder {
    pub fn api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = api_key.into();
        self
    }

    pub fn api_key_from_env(self) -> Self {
        self.api_key(std::env::var(LAUNCH_DARKLY_API_KEY).unwrap_or_default())
    }

    pub fn deployment_name(mut self, deployment_name: impl Into<String>) -> Self {
        self.deployment_name = deployment_name.into();
        self
    }

    pub fn service_name(mut self, service_name: impl Into<String>) -> Self {
        self.service_name = service_name.into();
        self
    }

    pub fn context_provider(mut self, context: FlagContext) -> Self {
        self.context_providers.push(context);
        self
    }

    pub fn override_flag(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.overrides.insert(key.to_owned(), value.into());
        self
    }

    pub async fn connect(self) -> Result<FeatureFlags, String> {
        let launch_darkly = match self.api_key.as_str() {
            "" => None,
            api_key => Some(start(api_key).await?),
        };
        Ok(FeatureFlags {
            connection: Arc::new(Connection {
                launch_darkly,
                overrides: HashMap::new(),
                extra_contexts: self.extra_contexts(),
            }),
        })
    }

    pub fn offline(self) -> FeatureFlags {
        FeatureFlags {
            connection: Arc::new(Connection {
                launch_darkly: None,
                extra_contexts: self.extra_contexts(),
                overrides: self.overrides,
            }),
        }
    }

    fn extra_contexts(&self) -> Vec<FlagContext> {
        let deployment = Some(&self.deployment_name)
            .filter(|name| !name.is_empty())
            .map(|name| FlagContext::deployment(name));
        let service = Some(&self.service_name)
            .filter(|name| !name.is_empty())
            .map(|name| FlagContext::service(name));
        deployment
            .into_iter()
            .chain(service)
            .chain(self.context_providers.iter().cloned())
            .collect()
    }
}

async fn start(api_key: &str) -> Result<Client, String> {
    let config = ConfigBuilder::new(api_key).build().map_err(|e| e.to_string())?;
    let client = Client::build(config).map_err(|e| e.to_string())?;
    client.start_with_default_executor();
    match client.wait_for_initialization(INIT_TIMEOUT).await {
        Some(true) => Ok(client),
        Some(false) => Err("LaunchDarkly client initialization failed".to_owned()),
        None => Err("timeout encountered waiting for LaunchDarkly client initialization".to_owned()),
    }
}

impl FeatureFlags {
    pub fn builder() -> FeatureFlagsBuilder {
        FeatureFlagsBuilder::default()
    }

    pub fn offline() -> Self {
        Self::builder().offline()
    }

    pub fn live(&self) -> bool {
        self.connection.launch_darkly.is_some()
    }

    pub fn get<T: FlagValue>(&self, flag: &Flag<T>, contexts: &[FlagContext]) -> T {
        match &self.connection.launch_darkly {
            None => self.override_of(flag).unwrap_or_else(|| flag.fallback.clone()),
            Some(client) => T::evaluate(client, &self.context_for(contexts), flag.key, flag.fallback.clone()),
        }
    }

    pub async fn close(&self) {
        let connection = self.connection.clone();
        let closing = tokio::task::spawn_blocking(move || connection.launch_darkly.iter().for_each(Client::close));
        if let Err(e) = closing.await {
            tracing::error!(error = %e, "Error during launch-darkly client shutdown");
        }
    }

    fn override_of<T: FlagValue>(&self, flag: &Flag<T>) -> Option<T> {
        self.connection.overrides.get(flag.key).and_then(T::from_json)
    }

    fn context_for(&self, contexts: &[FlagContext]) -> Context {
        merge_contexts(
            contexts
                .iter()
                .chain(&self.connection.extra_contexts)
                .cloned()
                .collect::<Vec<_>>(),
        )
    }
}

impl Default for FeatureFlags {
    fn default() -> Self {
        Self::offline()
    }
}

impl std::fmt::Debug for FeatureFlags {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureFlags")
            .field("live", &self.live())
            .field("extra_contexts", &self.connection.extra_contexts)
            .finish()
    }
}

impl PartialEq for FeatureFlags {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection, &other.connection)
            || (!self.live() && !other.live() && self.connection.extra_contexts == other.connection.extra_contexts)
    }
}

impl Eq for FeatureFlags {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::featureflags::flags::{BUILD_PROVISION_VERSION, build_io_engine};

    #[test]
    fn an_offline_client_serves_the_fallbacks() {
        let flags = FeatureFlags::offline();

        assert!(!flags.live());
        assert_eq!(flags.get(&BUILD_PROVISION_VERSION, &[]), 0);
        assert_eq!(flags.get(&build_io_engine(), &[FlagContext::team("t")]), "Sync");
    }

    #[test]
    fn deployment_service_and_providers_follow_the_evaluation_contexts() {
        let flags = FeatureFlags::builder()
            .deployment_name("e2b.dev")
            .service_name("")
            .context_provider(FlagContext::orchestrator("node-1", "abc"))
            .context_provider(FlagContext::instance_group(""))
            .offline();

        let kinds: Vec<_> = flags
            .connection
            .extra_contexts
            .iter()
            .map(|context| (context.kind().to_owned(), context.key().to_owned()))
            .collect();

        assert_eq!(
            kinds,
            vec![
                ("deployment".to_owned(), "e2b.dev".to_owned()),
                ("orchestrator".to_owned(), "node-1".to_owned()),
                ("instance-group".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn an_offline_override_replaces_the_fallback_only_for_a_matching_type() {
        let flags = FeatureFlags::builder()
            .override_flag("build-ensure-free-disk-space", true)
            .override_flag("build-provision-version", "seven")
            .override_flag("build-io-engine", "Async")
            .offline();

        assert!(flags.get(&crate::featureflags::BUILD_ENSURE_FREE_DISK_SPACE, &[]));
        assert_eq!(flags.get(&BUILD_PROVISION_VERSION, &[]), 0);
        assert_eq!(flags.get(&build_io_engine(), &[]), "Async");
    }

    #[tokio::test]
    async fn closing_an_offline_client_is_a_no_op() {
        FeatureFlags::offline().close().await;
    }
}
