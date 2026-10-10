use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use futures::FutureExt;
use futures::future::BoxFuture;
use redissun::{Client, Object, StringCodec};

use super::publisher::{RouteStore, RoutingRecord};
use crate::events::{EventSink, STREAM, SandboxEvent, delivery_key};
use crate::peer::registry::{PeerRegistry, redis_key};

const ROUTING_KEY_PREFIX: &str = "sandbox:routing:";
const DEFAULT_POOL_SIZE: usize = 5;
const INVALID_URL: &str = r#"failed to parse REDIS_URL: "<redacted>" is not a valid redis URL"#;
const INVALID_CA: &str = "failed to parse Redis TLS CA certificate";
const DELETE_IF_SAME_EXECUTION: &str = r"
local v = redis.call('GET', KEYS[1])
if not v then
  return 0
end
local ok, info = pcall(cjson.decode, v)
if not (ok and type(info) == 'table' and type(info.execution_id) == 'string') then
  return 3
end
if info.execution_id == ARGV[1] then
  redis.call('DEL', KEYS[1])
  return 1
end
return 2
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Cluster(String),
    Single(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisSettings {
    pub target: Target,
    pub password: Option<String>,
    pub tls: bool,
    pub ca: Option<Vec<u8>>,
    pub pool_size: usize,
}

impl RedisSettings {
    pub fn from_vars(vars: &HashMap<String, String>) -> Result<Option<Self>, String> {
        let var = |name: &str| vars.get(name).filter(|value| !value.is_empty()).cloned();
        let target = match (var("REDIS_CLUSTER_URL"), var("REDIS_URL")) {
            (Some(cluster), _) => Target::Cluster(cluster),
            (None, Some(url)) => Target::Single(url),
            (None, None) => return Ok(None),
        };
        let url_tls = matches!(&target, Target::Single(url) if url.starts_with("rediss://"));
        let tls = url_tls || var("REDIS_TLS_ENABLED").is_some_and(|value| value == "true" || value == "1");
        let ca = var("REDIS_TLS_CA_BASE64");
        if ca.is_some() && !tls {
            return Err("REDIS_TLS_CA_BASE64 is set but REDIS_TLS_ENABLED is not: a CA without TLS is meaningless -- set REDIS_TLS_ENABLED=true or remove the CA".to_owned());
        }
        let password = var("REDIS_PASSWORD");
        if password.is_some() && !tls {
            return Err("a Redis password is set but TLS is not enabled: refusing to send credentials over plaintext (set REDIS_TLS_ENABLED or use the rediss:// scheme, or drop the password)".to_owned());
        }
        let ca = ca
            .map(|ca| {
                base64::engine::general_purpose::STANDARD
                    .decode(ca)
                    .map_err(|e| e.to_string())
            })
            .transpose()?;
        let pool_size = var("REDIS_POOL_SIZE")
            .and_then(|size| size.parse().ok())
            .filter(|size| *size > 0)
            .unwrap_or(DEFAULT_POOL_SIZE);
        Ok(Some(Self {
            target,
            password,
            tls,
            ca,
            pool_size,
        }))
    }

    fn redis_url(&self) -> String {
        let scheme = if self.tls { "rediss" } else { "redis" };
        let flavor = match &self.target {
            Target::Single(_) => "",
            Target::Cluster(_) => "-cluster",
        };
        let address = match &self.target {
            Target::Single(address) | Target::Cluster(address) => address,
        };
        let address = address.split_once("://").map_or(address.as_str(), |(_, rest)| rest);
        format!("{scheme}{flavor}://{address}")
    }
}

#[derive(Clone)]
pub struct Redis {
    client: Client<StringCodec>,
}

impl Redis {
    pub async fn connect(settings: &RedisSettings) -> Result<Self, String> {
        let builder = Client::builder()
            .url(settings.redis_url())
            .pool_size(settings.pool_size)
            .codec(StringCodec);
        let builder = settings
            .password
            .iter()
            .fold(builder, |builder, password| builder.password(password));
        let client = settings
            .ca
            .iter()
            .fold(builder, |builder, ca| builder.tls_ca_pem(ca.clone()))
            .build()
            .await
            .map_err(connect_error)?;
        Ok(Self { client })
    }

    pub fn routing_catalog(&self) -> RoutingCatalog {
        RoutingCatalog {
            redis: self.client.clone(),
        }
    }

    pub fn peer_registry(&self, node_address: String) -> RedisPeerRegistry {
        RedisPeerRegistry {
            redis: self.client.clone(),
            node_address,
        }
    }

    pub fn event_stream(&self) -> EventStream {
        EventStream {
            redis: self.client.clone(),
        }
    }
}

fn connect_error(error: redissun::Error) -> String {
    match error {
        redissun::Error::Config(text) if text.contains("PEM") || text.contains("certificate") => INVALID_CA.to_owned(),
        redissun::Error::Config(_) => INVALID_URL.to_owned(),
        other => format!("failed to ping redis: {other}"),
    }
}

#[derive(Clone)]
pub struct RoutingCatalog {
    redis: Client<StringCodec>,
}

#[derive(Clone)]
pub struct EventStream {
    redis: Client<StringCodec>,
}

#[derive(Clone)]
pub struct RedisPeerRegistry {
    redis: Client<StringCodec>,
    node_address: String,
}

impl PeerRegistry for RedisPeerRegistry {
    fn register(&self, build_id: &str, ttl: Duration) -> BoxFuture<'static, Result<(), String>> {
        let entry = self.redis.bucket::<String>(redis_key(build_id));
        let address = self.node_address.clone();
        async move { entry.set_ex(&address, ttl).await.map_err(|e| e.to_string()) }.boxed()
    }

    fn lookup(&self, build_id: &str) -> BoxFuture<'static, Result<Option<String>, String>> {
        let entry = self.redis.bucket::<String>(redis_key(build_id));
        async move { entry.get().await.map_err(|e| e.to_string()) }.boxed()
    }

    fn unregister(&self, build_id: &str) -> BoxFuture<'static, Result<(), String>> {
        let entry = self.redis.bucket::<String>(redis_key(build_id));
        async move { entry.get_del().await.map(|_| ()).map_err(|e| e.to_string()) }.boxed()
    }
}

impl RouteStore for RoutingCatalog {
    fn store(
        &self,
        sandbox_id: &str,
        record: &RoutingRecord,
        ttl: Option<Duration>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let route = self.redis.bucket::<String>(format!("{ROUTING_KEY_PREFIX}{sandbox_id}"));
        let value = serde_json::to_string(record).unwrap_or_default();
        async move {
            match ttl {
                Some(ttl) => route.set_ex(&value, ttl).await,
                None => route.set(&value).await,
            }
            .map_err(|e| format!("failed to store sandbox info in redis: {e}"))
        }
        .boxed()
    }

    fn delete(&self, sandbox_id: &str, execution_id: &str) -> BoxFuture<'static, Result<i64, String>> {
        let key = format!("{ROUTING_KEY_PREFIX}{sandbox_id}");
        let execution_id = execution_id.to_owned();
        let redis = self.redis.clone();
        async move {
            redis
                .script()
                .eval::<i64>(DELETE_IF_SAME_EXECUTION)
                .key(key)
                .arg(&execution_id)
                .await
                .map_err(|e| format!("failed to delete sandbox info from redis: {e}"))
        }
        .boxed()
    }
}

impl EventSink for EventStream {
    fn publish(&self, event: &SandboxEvent) -> BoxFuture<'static, Result<(), String>> {
        let delivery = self.redis.bucket::<String>(delivery_key(event.sandbox_team_id));
        let stream = self.redis.stream::<String, String>(STREAM);
        let payload = serde_json::to_string(event).unwrap_or_default();
        async move {
            let subscribed = delivery
                .exists()
                .await
                .map_err(|e| format!("could not determine if redis stream is published: {e}"))?;
            if !subscribed {
                return Ok(());
            }
            stream
                .add([("payload", payload.as_str())])
                .await
                .map(|_| ())
                .map_err(|e| e.to_string())
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn redis_is_disabled_without_a_url_and_the_cluster_url_wins() {
        assert_eq!(RedisSettings::from_vars(&vars(&[])), Ok(None));
        let settings = RedisSettings::from_vars(&vars(&[("REDIS_URL", "a:6379"), ("REDIS_CLUSTER_URL", "c:6379")]))
            .unwrap()
            .unwrap();
        assert_eq!(settings.target, Target::Cluster("c:6379".to_owned()));
        assert_eq!(settings.redis_url(), "redis-cluster://c:6379");
    }

    #[test]
    fn a_password_or_a_ca_needs_tls() {
        assert!(
            RedisSettings::from_vars(&vars(&[("REDIS_URL", "a:6379"), ("REDIS_PASSWORD", "p")]))
                .unwrap_err()
                .starts_with("a Redis password is set but TLS is not enabled")
        );
        assert!(
            RedisSettings::from_vars(&vars(&[("REDIS_URL", "a:6379"), ("REDIS_TLS_CA_BASE64", "eA==")]))
                .unwrap_err()
                .starts_with("REDIS_TLS_CA_BASE64 is set but REDIS_TLS_ENABLED is not")
        );
        let secure = RedisSettings::from_vars(&vars(&[("REDIS_URL", "rediss://a:6379"), ("REDIS_PASSWORD", "p")]))
            .unwrap()
            .unwrap();
        assert_eq!(secure.redis_url(), "rediss://a:6379");
        assert_eq!(secure.password.as_deref(), Some("p"));
    }
}
