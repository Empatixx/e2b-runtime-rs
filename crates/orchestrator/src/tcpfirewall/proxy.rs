use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use nix::sys::socket::{getsockopt, sockopt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::dial::Dialer;
use super::rules::{MatchType, egress_allowed};
use super::sniff::{http_head_complete, http_host, tls_record_complete, tls_sni};
use crate::network::settings::NetworkSettings;
use crate::proto::SandboxNetworkEgressConfig;

const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_PREFACE: usize = 16 * 1024;

#[derive(Debug, Clone)]
pub struct FirewallSandbox {
    pub sandbox_id: String,
    pub egress: Option<SandboxNetworkEgressConfig>,
}

pub type FirewallLookup = Arc<dyn Fn(IpAddr) -> Option<FirewallSandbox> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Http,
    Tls,
    Other,
}

pub struct TcpFirewall {
    lookup: FirewallLookup,
    dialer: Dialer,
}

impl TcpFirewall {
    pub fn new(lookup: FirewallLookup, settings: &NetworkSettings) -> Arc<Self> {
        Arc::new(Self {
            lookup,
            dialer: Dialer {
                tos: settings.egress_dscp << 2,
            },
        })
    }

    pub async fn start(self: &Arc<Self>, settings: &NetworkSettings) -> Result<(), String> {
        for (port, protocol) in [
            (settings.tcp_firewall_http_port, Protocol::Http),
            (settings.tcp_firewall_tls_port, Protocol::Tls),
            (settings.tcp_firewall_other_port, Protocol::Other),
        ] {
            let address = SocketAddr::from(([0, 0, 0, 0], port));
            let listener = TcpListener::bind(address)
                .await
                .map_err(|e| format!("listen on {address}: {e}"))?;
            tokio::spawn(self.clone().accept(listener, protocol));
        }
        tracing::info!(
            http_port = settings.tcp_firewall_http_port,
            tls_port = settings.tcp_firewall_tls_port,
            other_port = settings.tcp_firewall_other_port,
            "TCP firewall proxy started"
        );
        Ok(())
    }

    async fn accept(self: Arc<Self>, listener: TcpListener, protocol: Protocol) {
        loop {
            match listener.accept().await {
                Ok((stream, remote)) => {
                    tokio::spawn(self.clone().handle(stream, remote, protocol));
                }
                Err(e) if is_transient(&e) => {
                    tracing::error!(error = %e, "tcpfirewall listener accept error, retrying");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                }
                Err(e) => {
                    tracing::error!(error = %e, "tcpfirewall listener stopped");
                    return;
                }
            }
        }
    }

    async fn handle(self: Arc<Self>, mut stream: TcpStream, remote: SocketAddr, protocol: Protocol) {
        let Some(sandbox) = (self.lookup)(remote.ip()) else {
            tracing::error!(sandbox_ip = %remote.ip(), "failed to find sandbox for connection");
            return;
        };
        let Ok(destination) = getsockopt(&stream, sockopt::OriginalDst).map(original_destination) else {
            tracing::error!(sandbox_id = %sandbox.sandbox_id, "failed to read the original destination");
            return;
        };
        let preface = read_preface(&mut stream, protocol).await;
        let hostname = match protocol {
            Protocol::Http => http_host(&preface),
            Protocol::Tls => tls_sni(&preface),
            Protocol::Other => None,
        };
        let decision = match egress_allowed(sandbox.egress.as_ref(), hostname.as_deref(), destination.ip()) {
            Ok(decision) => decision,
            Err(e) => {
                tracing::error!(sandbox_id = %sandbox.sandbox_id, error = %e, "Egress check failed");
                return;
            }
        };
        if !decision.allowed {
            return;
        }
        let upstream = match (decision.matched, hostname) {
            (MatchType::Domain, Some(hostname)) => self.dialer.dial_hostname(&hostname, destination.port()).await,
            _ => self.dialer.dial(destination).await,
        };
        let Ok(mut upstream) = upstream.inspect_err(|e| {
            tracing::debug!(sandbox_id = %sandbox.sandbox_id, error = %e, "upstream dial failed");
        }) else {
            return;
        };
        if upstream.write_all(&preface).await.is_ok() {
            let _ = tokio::io::copy_bidirectional(&mut stream, &mut upstream).await;
        }
    }
}

fn original_destination(address: nix::libc::sockaddr_in) -> SocketAddr {
    SocketAddr::from((
        Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)),
        u16::from_be(address.sin_port),
    ))
}

async fn read_preface(stream: &mut TcpStream, protocol: Protocol) -> Vec<u8> {
    let complete = match protocol {
        Protocol::Http => http_head_complete,
        Protocol::Tls => tls_record_complete,
        Protocol::Other => return Vec::new(),
    };
    let mut preface = Vec::new();
    let mut chunk = [0u8; 4096];
    while !complete(&preface) && preface.len() < MAX_PREFACE {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => preface.extend_from_slice(&chunk[..read]),
        }
    }
    preface
}

fn is_transient(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(nix::libc::EMFILE | nix::libc::ENFILE | nix::libc::EAGAIN | nix::libc::ECONNABORTED)
    )
}
