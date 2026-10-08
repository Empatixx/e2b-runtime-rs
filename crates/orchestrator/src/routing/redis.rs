use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use futures::FutureExt;
use futures::future::BoxFuture;
use redis::aio::ConnectionManager;
use redis::cluster_async::ClusterConnection;
use redis::{AsyncCommands, Script, TlsCertificates};

use super::publisher::{RouteStore, RoutingRecord};

const ROUTING_KEY_PREFIX: &str = "sandbox:routing:";
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
        Ok(Some(Self {
            target,
            password,
            tls,
            ca,
        }))
    }

    fn url(&self, address: &str) -> String {
        let scheme = if self.tls { "rediss" } else { "redis" };
        let address = address.split_once("://").map_or(address, |(_, rest)| rest);
        match &self.password {
            Some(password) => format!("{scheme}://:{password}@{address}"),
            None => format!("{scheme}://{address}"),
        }
    }

    fn certificates(&self) -> TlsCertificates {
        TlsCertificates {
            client_tls: None,
            root_cert: self.ca.clone(),
        }
    }
}

#[derive(Clone)]
enum Connection {
    Single(ConnectionManager),
    Cluster(ClusterConnection),
}

#[derive(Clone)]
pub struct RedisRoutes {
    connection: Connection,
}

impl RedisRoutes {
    pub async fn connect(settings: &RedisSettings) -> Result<Self, String> {
        let connection = match &settings.target {
            Target::Single(url) => {
                let url = settings.url(url);
                let client = match settings.tls {
                    true => redis::Client::build_with_tls(url, settings.certificates()),
                    false => redis::Client::open(url),
                }
                .map_err(|_| r#"failed to parse REDIS_URL: "<redacted>" is not a valid redis URL"#.to_owned())?;
                Connection::Single(
                    ConnectionManager::new(client)
                        .await
                        .map_err(|e| format!("failed to ping redis: {e}"))?,
                )
            }
            Target::Cluster(address) => {
                let builder = redis::cluster::ClusterClientBuilder::new(vec![settings.url(address)]);
                let builder = match settings.tls {
                    true => builder.certs(settings.certificates()),
                    false => builder,
                };
                let client = builder
                    .build()
                    .map_err(|e| format!("failed to connect to redis cluster: {e}"))?;
                Connection::Cluster(
                    client
                        .get_async_connection()
                        .await
                        .map_err(|e| format!("failed to ping redis: {e}"))?,
                )
            }
        };
        let routes = Self { connection };
        routes.ping().await?;
        Ok(routes)
    }

    async fn ping(&self) -> Result<(), String> {
        let pong: Result<String, _> = match self.connection.clone() {
            Connection::Single(mut connection) => redis::cmd("PING").query_async(&mut connection).await,
            Connection::Cluster(mut connection) => redis::cmd("PING").query_async(&mut connection).await,
        };
        pong.map(|_| ()).map_err(|e| format!("failed to ping redis: {e}"))
    }
}

impl RouteStore for RedisRoutes {
    fn store(
        &self,
        sandbox_id: &str,
        record: &RoutingRecord,
        ttl: Option<Duration>,
    ) -> BoxFuture<'static, Result<(), String>> {
        let key = format!("{ROUTING_KEY_PREFIX}{sandbox_id}");
        let value = serde_json::to_string(record).unwrap_or_default();
        let connection = self.connection.clone();
        async move {
            let stored: Result<(), redis::RedisError> = match (connection, ttl) {
                (Connection::Single(mut connection), Some(ttl)) => connection.set_ex(key, value, ttl.as_secs()).await,
                (Connection::Single(mut connection), None) => connection.set(key, value).await,
                (Connection::Cluster(mut connection), Some(ttl)) => connection.set_ex(key, value, ttl.as_secs()).await,
                (Connection::Cluster(mut connection), None) => connection.set(key, value).await,
            };
            stored.map_err(|e| format!("failed to store sandbox info in redis: {e}"))
        }
        .boxed()
    }

    fn delete(&self, sandbox_id: &str, execution_id: &str) -> BoxFuture<'static, Result<i64, String>> {
        let key = format!("{ROUTING_KEY_PREFIX}{sandbox_id}");
        let execution_id = execution_id.to_owned();
        let connection = self.connection.clone();
        async move {
            let script = Script::new(DELETE_IF_SAME_EXECUTION);
            let mut invocation = script.key(key);
            invocation.arg(execution_id);
            let outcome: Result<i64, redis::RedisError> = match connection {
                Connection::Single(mut connection) => invocation.invoke_async(&mut connection).await,
                Connection::Cluster(mut connection) => invocation.invoke_async(&mut connection).await,
            };
            outcome.map_err(|e| format!("failed to delete sandbox info from redis: {e}"))
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
        assert_eq!(settings.url("c:6379"), "redis://c:6379");
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
        assert_eq!(secure.url("rediss://a:6379"), "rediss://:p@a:6379");
    }
}
