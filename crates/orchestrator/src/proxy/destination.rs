use axum::http::uri::{Authority, Scheme};

pub const MASK_REQUEST_HOST_PORT_PLACEHOLDER: &str = "${PORT}";

#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct Destination {
    pub scheme: Scheme,
    pub authority: Authority,
    pub sandbox_id: String,
    #[builder(default)]
    pub sandbox_port: u64,
    #[builder(default)]
    pub default_to_port_error: bool,
    pub connection_key: String,
    pub mask_request_host: Option<String>,
    #[builder(default)]
    pub insecure_skip_tls_verify: bool,
}
