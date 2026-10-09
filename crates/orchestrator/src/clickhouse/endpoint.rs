use std::sync::Arc;

use klickhouse::{Client, ClientOptions, Row};
use percent_encoding::percent_decode_str;
use tokio::sync::Mutex;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub addresses: Vec<String>,
    pub username: String,
    pub password: String,
    pub database: String,
    pub secure: bool,
}

impl Endpoint {
    pub fn parse(dsn: &str) -> Result<Self, String> {
        let rest = dsn.strip_prefix("clickhouse://").ok_or("parse DSN")?;
        let (authority, path) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
        let (credentials, hosts) = authority.rsplit_once('@').unwrap_or(("", authority));
        if hosts.is_empty() {
            return Err("DSN has no addresses".to_owned());
        }
        let url = Url::parse(&format!("clickhouse://{credentials}@hosts{path}")).map_err(|_| "parse DSN")?;
        Ok(Self {
            addresses: hosts.split(',').map(with_default_port).collect(),
            username: or_default(&decode(url.username())),
            password: decode(url.password().unwrap_or_default()),
            database: or_default(url.path().trim_start_matches('/')),
            secure: url.query_pairs().any(|(key, value)| key == "secure" && value == "true"),
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

    pub fn label(&self) -> String {
        self.endpoint.addresses.join(",")
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
            .map_err(|e| format!("insert into clickhouse at {}: {e}", self.label()))
    }

    async fn connect(&self) -> Result<Client, String> {
        let mut failure = "DSN has no addresses".to_owned();
        for address in &self.endpoint.addresses {
            match self.connect_to(address).await {
                Ok(client) => return Ok(client),
                Err(e) => failure = e,
            }
        }
        Err(failure)
    }

    async fn connect_to(&self, address: &str) -> Result<Client, String> {
        let options = ClientOptions {
            username: self.endpoint.username.clone(),
            password: self.endpoint.password.clone(),
            default_database: self.endpoint.database.clone(),
            tcp_nodelay: true,
        };
        let connected = match self.endpoint.secure {
            true => {
                let host = address.rsplit_once(':').map_or(address, |(host, _)| host);
                let name = ServerName::try_from(host.to_owned()).map_err(|e| e.to_string())?;
                Client::connect_tls(address, options, name, &tls_connector()).await
            }
            false => Client::connect(address, options).await,
        };
        connected.map_err(|e| format!("connect to clickhouse at {address}: {e}"))
    }
}

fn with_default_port(host: &str) -> String {
    match host.rsplit_once(':').is_some_and(|(_, port)| !port.contains(']')) {
        true => host.to_owned(),
        false => format!("{host}:9000"),
    }
}

fn decode(text: &str) -> String {
    percent_decode_str(text).decode_utf8_lossy().into_owned()
}

fn or_default(text: &str) -> String {
    match text {
        "" => "default".to_owned(),
        text => text.to_owned(),
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
                addresses: vec!["primary.example:9000".to_owned()],
                username: "user".to_owned(),
                password: "secret".to_owned(),
                database: "db".to_owned(),
                secure: true,
            }
        );
        assert_eq!(Endpoint::parse("http://x").unwrap_err(), "parse DSN");
        assert_eq!(
            Endpoint::parse("clickhouse://host").unwrap().addresses,
            vec!["host:9000"]
        );
        assert_eq!(Endpoint::parse("clickhouse:///db").unwrap_err(), "DSN has no addresses");
    }

    #[test]
    fn every_comma_separated_host_is_an_address() {
        let endpoint = Endpoint::parse("clickhouse://u:p%40ss@h1:9000,h2,[::1]/db").unwrap();

        assert_eq!(endpoint.addresses, vec!["h1:9000", "h2:9000", "[::1]:9000"]);
        assert_eq!((endpoint.password.as_str(), endpoint.database.as_str()), ("p@ss", "db"));
    }

    #[test]
    fn credentials_are_percent_decoded() {
        let endpoint = Endpoint::parse("clickhouse://us%65r:p%40ss@host:9440").unwrap();

        assert_eq!(
            (endpoint.username.as_str(), endpoint.password.as_str()),
            ("user", "p@ss")
        );
        assert_eq!(endpoint.database, "default");
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
