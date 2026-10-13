use std::sync::Arc;
use std::time::{Duration, SystemTime};

use backon::{BackoffBuilder, ExponentialBackoff, ExponentialBuilder};
use reqwest::Client;
use serde::Deserialize;
use tokio::sync::OnceCell;
use tokio::sync::mpsc::{Receiver, Sender};

use super::super::credsfile;
use super::super::json::{self, Target};
use super::{FlowToken, TokenFlow};
use crate::go_url::escape::{Mode, escape};

const METADATA_IP: &str = "169.254.169.254";
const HOST_ENV: &str = "GCE_METADATA_HOST";
const TOKEN_PATH: &str = "instance/service-accounts/default/token";
const USER_AGENT: &str = "gcloud-golang/0.1";
const MAX_RETRIES: usize = 5;
const FIRST_PAUSE: Duration = Duration::from_millis(100);
const MAX_PAUSE: Duration = Duration::from_secs(30);
const DIAL_TIMEOUT: Duration = Duration::from_secs(2);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const SLOW_PROBE_WAIT: Duration = Duration::from_secs(5);
const DMI_PRODUCT_NAME: &str = "/sys/class/dmi/id/product_name";
const METADATA_DNS_NAME: &str = "metadata.google.internal.:80";

static ON_GCE: OnceCell<bool> = OnceCell::const_new();

#[derive(Deserialize, Default)]
#[serde(default)]
struct MetadataToken {
    access_token: String,
    expires_in: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct Messages {
    fetch_prefix: &'static str,
    invalid_json_prefix: &'static str,
    incomplete: &'static str,
    not_on_gce: Option<&'static str>,
    target: &'static Target,
}

impl Messages {
    pub const CREDENTIALS: Self = Self {
        fetch_prefix: "credentials: cannot fetch token: ",
        invalid_json_prefix: "credentials: invalid token JSON from metadata: ",
        incomplete: "credentials: incomplete token received from metadata",
        not_on_gce: None,
        target: &credsfile::METADATA_TOKEN,
    };

    pub const OAUTH2: Self = Self {
        fetch_prefix: "",
        invalid_json_prefix: "oauth2/google: invalid token JSON from metadata: ",
        incomplete: "oauth2/google: incomplete token received from metadata",
        not_on_gce: Some("oauth2/google: can't get a token from the metadata service; not running on GCE"),
        target: &credsfile::OAUTH2_METADATA_TOKEN,
    };
}

pub struct ComputeFlow {
    scopes: Vec<String>,
    host: Option<String>,
    messages: Messages,
    client: Client,
}

fn metadata_client() -> Result<Client, String> {
    Client::builder()
        .no_proxy()
        .connect_timeout(DIAL_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| e.to_string())
}

fn metadata_host() -> String {
    std::env::var(HOST_ENV)
        .ok()
        .filter(|host| !host.is_empty())
        .unwrap_or_else(|| METADATA_IP.to_owned())
}

fn retryable_status(status: u16) -> bool {
    (500..=599).contains(&status) || status == 429
}

fn transient(error: &reqwest::Error) -> bool {
    let io_kinds = std::iter::successors(Some(error as &dyn std::error::Error), |cause| cause.source())
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .map(std::io::Error::kind);
    error.is_timeout()
        || io_kinds.into_iter().any(|kind| {
            matches!(
                kind,
                std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
        })
}

fn jittered(ceiling: Duration) -> Duration {
    let nanos = u64::try_from(ceiling.as_nanos()).unwrap_or(u64::MAX).max(1);
    Duration::from_nanos(1 + rand::random::<u64>() % nanos)
}

fn pauses() -> ExponentialBackoff {
    ExponentialBuilder::default()
        .with_min_delay(FIRST_PAUSE)
        .with_max_delay(MAX_PAUSE)
        .with_max_times(MAX_RETRIES)
        .build()
}

impl ComputeFlow {
    pub fn new(scopes: &[String], messages: Messages) -> Result<Arc<Self>, String> {
        Self::build(None, scopes, messages)
    }

    fn build(host: Option<&str>, scopes: &[String], messages: Messages) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            scopes: scopes.to_vec(),
            host: host.map(str::to_owned),
            messages,
            client: metadata_client()?,
        }))
    }

    fn suffix(&self) -> String {
        match self.scopes.is_empty() {
            true => TOKEN_PATH.to_owned(),
            false => format!(
                "{TOKEN_PATH}?scopes={}",
                escape(&self.scopes.join(","), Mode::QueryComponent)
            ),
        }
    }

    async fn get(&self, suffix: &str) -> Result<String, String> {
        let host = self.host.clone().unwrap_or_else(metadata_host);
        let url = format!("http://{host}/computeMetadata/v1/{}", suffix.trim_start_matches('/'));
        let mut pauses = pauses();
        loop {
            let sent = self
                .client
                .get(&url)
                .header("Metadata-Flavor", "Google")
                .header("User-Agent", USER_AGENT)
                .send()
                .await;
            let retryable = match &sent {
                Ok(response) => retryable_status(response.status().as_u16()),
                Err(error) => transient(error),
            };
            if let Some(pause) = pauses.next().filter(|_| retryable).map(jittered) {
                tokio::time::sleep(pause).await;
                continue;
            }
            let response = sent.map_err(|e| super::client::request_error("Get", &url, &e))?;
            return Self::reply(suffix, response).await;
        }
    }

    async fn reply(suffix: &str, response: reqwest::Response) -> Result<String, String> {
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|e| e.to_string())?;
        match status {
            200 => Ok(body),
            404 => Err(format!(
                "metadata: GCE metadata {:?} not defined",
                suffix.trim_start_matches('/')
            )),
            code => Err(format!("compute: Received {code} `{body}`")),
        }
    }
}

#[async_trait::async_trait]
impl TokenFlow for ComputeFlow {
    async fn token(&self) -> Result<FlowToken, String> {
        match self.messages.not_on_gce {
            Some(text) if !on_gce().await => return Err(text.to_owned()),
            _ => {}
        }
        let body = self
            .get(&self.suffix())
            .await
            .map_err(|e| format!("{}{e}", self.messages.fetch_prefix))?;
        let parsed: MetadataToken = json::decode(body.as_bytes(), *self.messages.target)
            .map_err(|e| format!("{}{e}", self.messages.invalid_json_prefix))?;
        if parsed.expires_in == 0 || parsed.access_token.is_empty() {
            return Err(self.messages.incomplete.to_owned());
        }
        let lifetime = Duration::from_secs(u64::try_from(parsed.expires_in).unwrap_or(0));
        Ok(FlowToken {
            value: parsed.access_token,
            expiry: Some(SystemTime::now() + lifetime),
        })
    }

    fn is_compute(&self) -> bool {
        true
    }
}

pub async fn on_gce() -> bool {
    *ON_GCE.get_or_init(probe_gce).await
}

async fn probe_gce() -> bool {
    if std::env::var(HOST_ENV).is_ok_and(|host| !host.is_empty()) {
        return true;
    }
    let (sender, mut receiver) = tokio::sync::mpsc::channel::<bool>(2);
    let http = tokio::spawn(http_probe(sender.clone()));
    let dns = tokio::spawn(dns_probe(sender));
    let answer = first_answer(&mut receiver, system_info_suggests_gce()).await;
    http.abort();
    dns.abort();
    answer
}

async fn first_answer(receiver: &mut Receiver<bool>, system_suggests_gce: bool) -> bool {
    let first = receiver.recv().await.unwrap_or(false);
    match (system_suggests_gce, first) {
        (false, answer) => answer,
        (true, true) => true,
        (true, false) => tokio::time::timeout(SLOW_PROBE_WAIT, receiver.recv())
            .await
            .ok()
            .flatten()
            .unwrap_or(false),
    }
}

async fn http_probe(sender: Sender<bool>) {
    let flavored = match metadata_client() {
        Ok(client) => client
            .get(format!("http://{METADATA_IP}"))
            .header("User-Agent", USER_AGENT)
            .send()
            .await
            .is_ok_and(|response| {
                response
                    .headers()
                    .get("Metadata-Flavor")
                    .is_some_and(|value| value == "Google")
            }),
        Err(_) => false,
    };
    sender.send(flavored).await.ok();
}

async fn dns_probe(sender: Sender<bool>) {
    let found = tokio::net::lookup_host(METADATA_DNS_NAME)
        .await
        .is_ok_and(|mut addresses| addresses.any(|address| address.ip().to_string() == METADATA_IP));
    sender.send(found).await.ok();
}

fn system_info_suggests_gce() -> bool {
    std::fs::read_to_string(DMI_PRODUCT_NAME)
        .map(|name| matches!(name.trim(), "Google" | "Google Compute Engine"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;
    use crate::storage::backend::azure::testing::read_request;

    async fn serve(replies: Vec<(u16, &'static str)>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for (status, body) in replies {
                let (mut connection, _) = listener.accept().await.unwrap();
                seen.push(read_request(&mut connection).await);
                let reply = format!(
                    "HTTP/1.1 {status} X\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                connection.write_all(reply.as_bytes()).await.unwrap();
            }
            seen
        });
        (address.to_string(), handle)
    }

    fn flow(host: &str, scopes: &[&str]) -> Arc<ComputeFlow> {
        let scopes: Vec<String> = scopes.iter().map(|scope| (*scope).to_owned()).collect();
        ComputeFlow::build(Some(host), &scopes, Messages::CREDENTIALS).unwrap()
    }

    #[tokio::test]
    async fn the_token_is_fetched_with_the_scopes_and_flavor_header() {
        let body = r#"{"access_token":"gce-token","expires_in":3599,"token_type":"Bearer"}"#;
        let (host, server) = serve(vec![(200, body)]).await;

        let token = flow(&host, &["scope-a", "scope-b"]).token().await.unwrap();
        let requests = server.await.unwrap();

        assert_eq!(token.value, "gce-token");
        assert!(token.expiry.is_some());
        assert!(requests[0].starts_with(
            "GET /computeMetadata/v1/instance/service-accounts/default/token?scopes=scope-a%2Cscope-b HTTP/1.1"
        ));
        assert!(requests[0].contains("metadata-flavor: Google"));
        assert!(requests[0].contains("user-agent: gcloud-golang/0.1"));
    }

    #[tokio::test]
    async fn no_scopes_means_no_query() {
        let body = r#"{"access_token":"t","expires_in":1}"#;
        let (host, server) = serve(vec![(200, body)]).await;

        flow(&host, &[]).token().await.unwrap();

        assert!(
            server.await.unwrap()[0]
                .starts_with("GET /computeMetadata/v1/instance/service-accounts/default/token HTTP/1.1")
        );
    }

    #[tokio::test]
    async fn incomplete_metadata_tokens_are_rejected() {
        let (host, _server) = serve(vec![(200, r#"{"access_token":"","expires_in":0}"#)]).await;

        assert_eq!(
            flow(&host, &[]).token().await.unwrap_err(),
            "credentials: incomplete token received from metadata"
        );
    }

    #[tokio::test]
    async fn a_missing_token_endpoint_names_the_undefined_metadata_with_its_query() {
        let (host, _server) = serve(vec![(404, "")]).await;

        assert_eq!(
            flow(&host, &["s"]).token().await.unwrap_err(),
            "credentials: cannot fetch token: metadata: GCE metadata \"instance/service-accounts/default/token?scopes=s\" not defined"
        );
    }

    #[tokio::test]
    async fn other_statuses_carry_the_body() {
        let (host, _server) = serve(vec![(403, "denied")]).await;

        assert_eq!(
            flow(&host, &[]).token().await.unwrap_err(),
            "credentials: cannot fetch token: compute: Received 403 `denied`"
        );
    }

    #[tokio::test]
    async fn server_errors_are_retried() {
        let body = r#"{"access_token":"t","expires_in":1}"#;
        let (host, server) = serve(vec![(503, ""), (429, ""), (200, body)]).await;

        let token = flow(&host, &[]).token().await.unwrap();

        assert_eq!(token.value, "t");
        assert_eq!(server.await.unwrap().len(), 3);
    }

    #[test]
    fn the_pause_ceiling_doubles_from_100ms_for_five_retries() {
        let ceilings: Vec<u128> = pauses().map(|pause| pause.as_millis()).collect();

        assert_eq!(ceilings, vec![100, 200, 400, 800, 1600]);
    }

    #[tokio::test(start_paused = true)]
    async fn without_a_bios_hint_the_first_probe_decides() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        sender.send(false).await.unwrap();

        assert!(!first_answer(&mut receiver, false).await);
    }

    #[tokio::test(start_paused = true)]
    async fn with_a_bios_hint_the_second_probe_can_overturn_a_negative_first() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        sender.send(false).await.unwrap();
        sender.send(true).await.unwrap();

        assert!(first_answer(&mut receiver, true).await);
    }

    #[tokio::test(start_paused = true)]
    async fn with_a_bios_hint_a_silent_second_probe_means_not_on_gce() {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        sender.send(false).await.unwrap();

        assert!(!first_answer(&mut receiver, true).await);
        drop(sender);
    }
}
