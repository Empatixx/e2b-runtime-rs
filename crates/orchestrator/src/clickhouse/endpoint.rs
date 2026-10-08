use std::sync::Arc;

use klickhouse::{Client, ClientOptions, Row};
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub address: String,
    pub username: String,
    pub password: String,
    pub database: String,
    pub secure: bool,
}

impl Endpoint {
    pub fn parse(dsn: &str) -> Result<Self, String> {
        let rest = dsn.strip_prefix("clickhouse://").ok_or("parse DSN")?;
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        let (credentials, location) = rest.rsplit_once('@').unwrap_or(("", rest));
        let (username, password) = credentials.split_once(':').unwrap_or((credentials, ""));
        let (address, database) = location.split_once('/').unwrap_or((location, ""));
        if address.is_empty() {
            return Err("DSN has no addresses".to_owned());
        }
        let address = match address.contains(':') {
            true => address.to_owned(),
            false => format!("{address}:9000"),
        };
        Ok(Self {
            address,
            username: match username {
                "" => "default".to_owned(),
                user => user.to_owned(),
            },
            password: password.to_owned(),
            database: match database {
                "" => "default".to_owned(),
                database => database.to_owned(),
            },
            secure: query.split('&').any(|pair| pair == "secure=true"),
        })
    }

    pub fn parse_list(single: Option<&str>, many: Option<&str>) -> (Option<String>, Vec<String>) {
        let primary = single.map(str::trim).filter(|dsn| !dsn.is_empty()).map(str::to_owned);
        let mut seen: Vec<String> = primary.iter().cloned().collect();
        let additional = many
            .unwrap_or_default()
            .split(';')
            .map(str::trim)
            .filter(|dsn| !dsn.is_empty())
            .filter_map(|dsn| {
                if seen.iter().any(|known| known == dsn) {
                    tracing::info!("dropped duplicate ClickHouse endpoint");
                    return None;
                }
                seen.push(dsn.to_owned());
                Some(dsn.to_owned())
            })
            .collect();
        (primary, additional)
    }
}

#[derive(Clone)]
pub struct Writer {
    endpoint: Endpoint,
    client: Arc<Mutex<Option<Client>>>,
}

impl Writer {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
            client: Arc::default(),
        }
    }

    pub fn label(&self) -> &str {
        &self.endpoint.address
    }

    pub async fn insert<T: Row + Send + Sync + 'static>(&self, query: &str, rows: Vec<T>) -> Result<(), String> {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(Client::is_closed) {
            *client = Some(self.connect().await?);
        }
        let connected = client.as_ref().ok_or("clickhouse is not connected")?;
        connected
            .insert_native_block(query, rows)
            .await
            .map_err(|e| format!("insert into clickhouse at {}: {e}", self.endpoint.address))
    }

    async fn connect(&self) -> Result<Client, String> {
        let options = ClientOptions {
            username: self.endpoint.username.clone(),
            password: self.endpoint.password.clone(),
            default_database: self.endpoint.database.clone(),
            tcp_nodelay: true,
        };
        let connected = match self.endpoint.secure {
            true => {
                let host = self
                    .endpoint
                    .address
                    .rsplit_once(':')
                    .map_or(self.endpoint.address.as_str(), |(host, _)| host);
                let name = ServerName::try_from(host.to_owned()).map_err(|e| e.to_string())?;
                Client::connect_tls(&self.endpoint.address, options, name, &tls_connector()).await
            }
            false => Client::connect(&self.endpoint.address, options).await,
        };
        connected.map_err(|e| format!("connect to clickhouse at {}: {e}", self.endpoint.address))
    }
}

fn tls_connector() -> TlsConnector {
    let roots = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dsns_parse_like_clickhouse_go() {
        assert_eq!(
            Endpoint::parse("clickhouse://user:secret@primary.example:9000/db?secure=true").unwrap(),
            Endpoint {
                address: "primary.example:9000".to_owned(),
                username: "user".to_owned(),
                password: "secret".to_owned(),
                database: "db".to_owned(),
                secure: true,
            }
        );
        assert_eq!(Endpoint::parse("http://x").unwrap_err(), "parse DSN");
        assert_eq!(Endpoint::parse("clickhouse:///db").unwrap_err(), "DSN has no addresses");
    }

    #[test]
    fn additional_endpoints_are_trimmed_and_deduplicated() {
        let (primary, additional) = Endpoint::parse_list(
            Some("clickhouse://a:9000/db"),
            Some(" clickhouse://b:9000/db ; clickhouse://a:9000/db;;clickhouse://b:9000/db"),
        );

        assert_eq!(primary.as_deref(), Some("clickhouse://a:9000/db"));
        assert_eq!(additional, vec!["clickhouse://b:9000/db".to_owned()]);
    }
}
