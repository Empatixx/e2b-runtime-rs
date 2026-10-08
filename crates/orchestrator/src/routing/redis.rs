use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use fred::prelude::{
    ClientLike, Config, Expiration, KeysInterface, LuaInterface, Pool, ReconnectPolicy, StreamsInterface, TlsConnector,
};
use fred::rustls::pki_types::CertificateDer;
use fred::rustls::pki_types::pem::PemObject;
use fred::rustls::{ClientConfig, RootCertStore};
use fred::types::Builder;
use futures::FutureExt;
use futures::future::BoxFuture;

use super::publisher::{RouteStore, RoutingRecord};
use crate::events::{EventSink, STREAM, SandboxEvent, delivery_key};

const ROUTING_KEY_PREFIX: &str = "sandbox:routing:";
const DEFAULT_POOL_SIZE: usize = 5;
const INVALID_URL: &str = r#"failed to parse REDIS_URL: "<redacted>" is not a valid redis URL"#;
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

    fn fred_url(&self) -> String {
        match &self.target {
            Target::Single(address) => self.url(address, ""),
            Target::Cluster(address) => self.url(address, "-cluster"),
        }
    }

    fn tls_connector(&self) -> Result<TlsConnector, String> {
        let Some(ca) = &self.ca else {
            return TlsConnector::default_rustls().map_err(|e| e.to_string());
        };
        let mut roots = RootCertStore::empty();
        CertificateDer::pem_slice_iter(ca)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| "failed to parse Redis TLS CA certificate".to_owned())?
            .into_iter()
            .try_for_each(|certificate| roots.add(certificate))
            .map_err(|_| "failed to parse Redis TLS CA certificate".to_owned())?;
        Ok(ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth()
            .into())
    }

    fn url(&self, address: &str, flavor: &str) -> String {
        let scheme = if self.tls { "rediss" } else { "redis" };
        let scheme = format!("{scheme}{flavor}");
        let address = address.split_once("://").map_or(address, |(_, rest)| rest);
        match &self.password {
            Some(password) => format!("{scheme}://:{password}@{address}"),
            None => format!("{scheme}://{address}"),
        }
    }
}

#[derive(Clone)]
pub struct Redis {
    pool: Pool,
}

impl Redis {
    pub async fn connect(settings: &RedisSettings) -> Result<Self, String> {
        let mut config = Config::from_url(&settings.fred_url()).map_err(|_| INVALID_URL.to_owned())?;
        if settings.tls {
            config.tls = Some(settings.tls_connector()?.into());
        }
        let pool = Builder::from_config(config)
            .set_policy(ReconnectPolicy::default())
            .build_pool(settings.pool_size)
            .map_err(|e| format!("failed to create redis client: {e}"))?;
        pool.init().await.map_err(|e| format!("failed to ping redis: {e}"))?;
        let _: String = pool
            .ping(None)
            .await
            .map_err(|e| format!("failed to ping redis: {e}"))?;
        Ok(Self { pool })
    }

    pub fn routing_catalog(&self) -> RoutingCatalog {
        RoutingCatalog {
            redis: self.pool.clone(),
        }
    }

    pub fn event_stream(&self) -> EventStream {
        EventStream {
            redis: self.pool.clone(),
        }
    }
}

#[derive(Clone)]
pub struct RoutingCatalog {
    redis: Pool,
}

#[derive(Clone)]
pub struct EventStream {
    redis: Pool,
}

impl RouteStore for RoutingCatalog {
    fn store(
        &self,
        sandbox_id: &str,
        record: &RoutingRecord,
        ttl: Option<Duration>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let key = format!("{ROUTING_KEY_PREFIX}{sandbox_id}");
        let value = serde_json::to_string(record).unwrap_or_default();
        let expiration = ttl.map(|ttl| Expiration::EX(ttl.as_secs() as i64));
        let redis = self.redis.clone();
        async move {
            redis
                .set::<(), _, _>(key, value, expiration, None, false)
                .await
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
                .eval::<i64, _, _, _>(DELETE_IF_SAME_EXECUTION, key, execution_id)
                .await
                .map_err(|e| format!("failed to delete sandbox info from redis: {e}"))
        }
        .boxed()
    }
}

impl EventSink for EventStream {
    fn publish(&self, event: &SandboxEvent) -> BoxFuture<'static, Result<(), String>> {
        let key = delivery_key(event.sandbox_team_id);
        let payload = serde_json::to_string(event).unwrap_or_default();
        let redis = self.redis.clone();
        async move {
            let subscribed: i64 = redis
                .exists(key)
                .await
                .map_err(|e| format!("could not determine if redis stream is published: {e}"))?;
            if subscribed == 0 {
                return Ok(());
            }
            redis
                .xadd::<String, _, _, _, _>(STREAM, false, None::<()>, "*", vec![("payload", payload)])
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
        assert_eq!(settings.fred_url(), "redis-cluster://c:6379");
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
        assert_eq!(secure.fred_url(), "rediss://:p@a:6379");
    }
}
