use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use launchdarkly_server_sdk::{
    ApplicationInfo, Client, ConfigBuilder, Context, Detail, Reason, ServiceEndpointsBuilder,
};
use serde_json::Value;
use tokio_rustls::rustls::crypto::ring;

use super::contexts::{FlagContext, merge_contexts};
use super::flags::Flag;

pub const LAUNCH_DARKLY_API_KEY: &str = "LAUNCH_DARKLY_API_KEY";
pub const LAUNCH_DARKLY_BASE_URL: &str = "LAUNCH_DARKLY_BASE_URL";
const UNDEFINED_DEPLOYMENT_ENVIRONMENT: &str = "undefined";
pub const INIT_TIMEOUT: Duration = Duration::from_secs(5);

pub trait FlagValue: Clone {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Detail<Self>;
    fn from_json(value: &Value) -> Option<Self>;
}

impl FlagValue for bool {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Detail<Self> {
        client.bool_variation_detail(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        value.as_bool()
    }
}

impl FlagValue for i64 {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Detail<Self> {
        client.int_variation_detail(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        value.as_i64()
    }
}

impl FlagValue for String {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Detail<Self> {
        client.str_variation_detail(context, key, fallback)
    }

    fn from_json(value: &Value) -> Option<Self> {
        value.as_str().map(str::to_owned)
    }
}

impl FlagValue for Value {
    fn evaluate(client: &Client, context: &Context, key: &str, fallback: Self) -> Detail<Self> {
        client.json_variation_detail(context, key, fallback)
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

#[derive(bon::Builder)]
#[builder(builder_type = FeatureFlagsBuilder, state_mod(vis = "pub"), on(String, into))]
pub struct FlagSettings {
    #[builder(field)]
    context_providers: Vec<FlagContext>,
    #[builder(field)]
    overrides: HashMap<String, Value>,
    #[builder(default)]
    api_key: String,
    #[builder(default)]
    deployment_environment: String,
    #[builder(default)]
    service_name: String,
    #[builder(default)]
    service_version: String,
    base_url: Option<String>,
}

impl<S: feature_flags_builder::State> FeatureFlagsBuilder<S> {
    pub fn context_provider(mut self, context: FlagContext) -> Self {
        self.context_providers.push(context);
        self
    }

    pub fn override_flag(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.overrides.insert(key.to_owned(), value.into());
        self
    }
}

impl<S: feature_flags_builder::IsComplete> FeatureFlagsBuilder<S> {
    pub async fn connect(self) -> Result<FeatureFlags, String> {
        self.build().connect().await
    }

    pub fn offline(self) -> FeatureFlags {
        self.build().offline()
    }
}

impl FlagSettings {
    pub async fn connect(self) -> Result<FeatureFlags, String> {
        match self.api_key.as_str() {
            "" => Ok(self.offline()),
            api_key => Ok(FeatureFlags {
                connection: Arc::new(Connection {
                    launch_darkly: Some(start(api_key, self.base_url().as_deref(), self.application()).await?),
                    overrides: HashMap::new(),
                    extra_contexts: self.extra_contexts(),
                }),
            }),
        }
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

    fn base_url(&self) -> Option<String> {
        self.base_url
            .clone()
            .or_else(|| std::env::var(LAUNCH_DARKLY_BASE_URL).ok())
            .filter(|url| !url.is_empty())
    }

    fn application(&self) -> Option<Application> {
        Application::of(&self.deployment_environment, &self.service_name, &self.service_version)
    }

    fn extra_contexts(&self) -> Vec<FlagContext> {
        let deployment = Some(self.deployment_environment.as_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(UNDEFINED_DEPLOYMENT_ENVIRONMENT);
        let deployment = FlagContext::deployment_environment(deployment);
        let service = Some(&self.service_name)
            .filter(|name| !name.is_empty())
            .map(|name| FlagContext::service(name));
        Some(deployment)
            .into_iter()
            .chain(service)
            .chain(self.context_providers.iter().cloned())
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Application {
    id: String,
    version: String,
}

impl Application {
    fn of(deployment_environment: &str, service_name: &str, service_version: &str) -> Option<Self> {
        Some(service_name).filter(|name| !name.is_empty()).map(|name| Self {
            id: format!(
                "{name}-{}",
                Some(deployment_environment)
                    .filter(|environment| !environment.is_empty())
                    .unwrap_or(UNDEFINED_DEPLOYMENT_ENVIRONMENT)
            ),
            version: service_version.to_owned(),
        })
    }

    fn info(&self) -> ApplicationInfo {
        let mut info = ApplicationInfo::new();
        info.application_identifier(self.id.as_str())
            .application_version(self.version.as_str());
        info
    }
}

fn service_endpoints(base_url: Option<&str>) -> ServiceEndpointsBuilder {
    let mut endpoints = ServiceEndpointsBuilder::new();
    base_url.iter().for_each(|url| {
        endpoints.relay_proxy(url);
    });
    endpoints
}

async fn start(api_key: &str, base_url: Option<&str>, application: Option<Application>) -> Result<Client, String> {
    drop(ring::default_provider().install_default());
    let config = application
        .iter()
        .fold(
            ConfigBuilder::new(api_key).service_endpoints(&service_endpoints(base_url)),
            |config, application| config.application_info(application.info()),
        )
        .build()
        .map_err(|e| e.to_string())?;
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
        FlagSettings::builder()
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
            Some(client) => {
                let detail = T::evaluate(client, &self.context_for(contexts), flag.key, flag.fallback.clone());
                if let Reason::Error { error } = &detail.reason {
                    tracing::warn!(error = ?error, flag = flag.key, "error evaluating flag");
                }
                detail.value.unwrap_or_else(|| flag.fallback.clone())
            }
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
            self.connection
                .extra_contexts
                .iter()
                .chain(contexts)
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
            .deployment_environment("staging")
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
                ("deployment_environment".to_owned(), "staging".to_owned()),
                ("orchestrator".to_owned(), "node-1".to_owned()),
                ("instance-group".to_owned(), String::new()),
            ]
        );
    }

    #[test]
    fn an_empty_deployment_environment_is_undefined() {
        let flags = FeatureFlags::builder()
            .deployment_environment("")
            .service_name("")
            .offline();

        let kinds: Vec<_> = flags
            .connection
            .extra_contexts
            .iter()
            .map(|context| (context.kind().to_owned(), context.key().to_owned()))
            .collect();

        assert_eq!(
            kinds,
            vec![("deployment_environment".to_owned(), "undefined".to_owned())]
        );
    }

    #[test]
    fn the_application_id_carries_the_environment_next_to_the_service() {
        assert_eq!(
            Application::of("e2b-staging", "orchestration-api", "0.14.1"),
            Some(Application {
                id: "orchestration-api-e2b-staging".to_owned(),
                version: "0.14.1".to_owned()
            })
        );
        assert_eq!(
            Application::of("", "orchestration-api", "0.14.1"),
            Some(Application {
                id: "orchestration-api-undefined".to_owned(),
                version: "0.14.1".to_owned()
            })
        );
        assert_eq!(Application::of("e2b-staging", "", "0.14.1"), None);
    }

    #[test]
    fn a_caller_service_overrides_the_process_service() {
        let flags = FeatureFlags::builder()
            .deployment_environment("staging")
            .service_name("orchestrator_template-manager")
            .offline();

        let merged = flags.context_for(&[FlagContext::service("orchestrator")]);

        let keys: std::collections::BTreeMap<_, _> = merged
            .context_keys()
            .into_iter()
            .map(|(kind, key)| (kind.to_string(), key.to_owned()))
            .collect();
        assert_eq!(keys["service"], "orchestrator");
        assert_eq!(keys["deployment_environment"], "staging");
    }

    #[tokio::test]
    async fn a_custom_base_url_serves_streaming_polling_and_events() {
        let launch_darkly = crate::featureflags::testing::FakeLaunchDarkly::start().await;
        let flags = FeatureFlags::builder()
            .api_key("local-project")
            .deployment_environment("local")
            .service_name("test")
            .service_version("1.0.0")
            .base_url(launch_darkly.url.clone())
            .connect()
            .await
            .unwrap();
        assert!(flags.live());
        let local_bool = Flag::<bool>::new("local-bool", false);
        let local_json = Flag::<serde_json::Value>::new("local-json", serde_json::Value::Null);

        assert!(flags.get(&local_bool, &[]));
        assert_eq!(flags.get(&local_json, &[]), serde_json::json!({"enabled": true}));

        launch_darkly.patch.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            while flags.get(&local_bool, &[]) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the streamed patch reaches the client");
        flags.connection.launch_darkly.as_ref().unwrap().flush();
        tokio::time::timeout(Duration::from_secs(5), launch_darkly.event_batches.notified())
            .await
            .expect("the SDK sends events to the configured endpoint");
        assert!(
            launch_darkly
                .authorizations
                .lock()
                .unwrap()
                .iter()
                .all(|authorization| authorization == "local-project")
        );
        flags.close().await;
    }

    #[tokio::test]
    async fn a_custom_base_url_without_a_key_stays_offline() {
        let launch_darkly = crate::featureflags::testing::FakeLaunchDarkly::start().await;
        let flags = FeatureFlags::builder()
            .deployment_environment("local")
            .service_name("test")
            .base_url(launch_darkly.url.clone())
            .connect()
            .await
            .unwrap();

        assert!(!flags.live());
        assert!(flags.get(&Flag::<bool>::new("absent-local-flag", true), &[]));
        flags.close().await;
        assert_eq!(launch_darkly.requests.load(std::sync::atomic::Ordering::SeqCst), 0);
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
