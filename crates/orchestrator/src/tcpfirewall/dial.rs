use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use nix::sys::socket::{setsockopt, sockopt};
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::Instant;

use super::rules::in_always_denied;

pub const UPSTREAM_DIAL_TIMEOUT: Duration = Duration::from_secs(30);
const FALLBACK_DELAY: Duration = Duration::from_millis(300);
const MIN_PARTIAL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy)]
pub struct Dialer {
    pub tos: u8,
}

struct Families {
    primaries: Vec<SocketAddr>,
    fallbacks: Vec<SocketAddr>,
}

impl Dialer {
    pub async fn dial(&self, address: SocketAddr) -> io::Result<TcpStream> {
        tokio::time::timeout(UPSTREAM_DIAL_TIMEOUT, self.connect(address))
            .await
            .map_err(|_| timed_out())?
    }

    pub async fn dial_hostname(&self, hostname: &str, port: u16) -> io::Result<TcpStream> {
        let deadline = Instant::now() + UPSTREAM_DIAL_TIMEOUT;
        let dialing = async {
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((hostname, port)).await?.collect();
            let families = split_families(addresses)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("lookup {hostname}: no such host")))?;
            self.dial_parallel(hostname, families, deadline).await
        };
        tokio::time::timeout_at(deadline, dialing)
            .await
            .map_err(|_| timed_out())?
    }

    async fn dial_parallel(&self, hostname: &str, families: Families, deadline: Instant) -> io::Result<TcpStream> {
        if families.fallbacks.is_empty() {
            return self.dial_serial(hostname, &families.primaries, deadline).await;
        }
        let primary = self.dial_serial(hostname, &families.primaries, deadline);
        let fallback = async {
            tokio::time::sleep(FALLBACK_DELAY).await;
            self.dial_serial(hostname, &families.fallbacks, deadline).await
        };
        tokio::pin!(primary, fallback);
        tokio::select! {
            dialed = &mut primary => match dialed {
                Ok(stream) => Ok(stream),
                Err(e) => self.dial_serial(hostname, &families.fallbacks, deadline).await.map_err(|_| e),
            },
            dialed = &mut fallback => match dialed {
                Ok(stream) => Ok(stream),
                Err(_) => primary.await,
            },
        }
    }

    async fn dial_serial(&self, hostname: &str, addresses: &[SocketAddr], deadline: Instant) -> io::Result<TcpStream> {
        let mut first_error = None;
        for (index, address) in addresses.iter().enumerate() {
            let attempt = partial_deadline(deadline, addresses.len() - index);
            let dialed = match self.verified(hostname, *address) {
                Ok(()) => tokio::time::timeout_at(attempt, self.connect(*address))
                    .await
                    .unwrap_or_else(|_| Err(timed_out())),
                Err(e) => Err(e),
            };
            match dialed {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    first_error.get_or_insert(e);
                }
            }
        }
        Err(first_error.unwrap_or_else(|| io::Error::other("missing address")))
    }

    fn verified(&self, hostname: &str, address: SocketAddr) -> io::Result<()> {
        match in_always_denied(address.ip()) {
            true => {
                tracing::warn!(upstream_addr = %hostname, resolved_ip = %address.ip(), "Blocked connection to internal IP via hostname");
                Err(io::Error::other(format!(
                    "hostname resolved to internal IP {}",
                    address.ip()
                )))
            }
            false => Ok(()),
        }
    }

    async fn connect(&self, address: SocketAddr) -> io::Result<TcpStream> {
        let socket = match address {
            SocketAddr::V4(_) => TcpSocket::new_v4()?,
            SocketAddr::V6(_) => TcpSocket::new_v6()?,
        };
        self.mark_dscp(&socket)?;
        socket.connect(address).await
    }

    fn mark_dscp(&self, socket: &TcpSocket) -> io::Result<()> {
        if self.tos == 0 {
            return Ok(());
        }
        let tos = i32::from(self.tos);
        let v4 = setsockopt(socket, sockopt::IpTos, &tos);
        let v6 = setsockopt(socket, sockopt::Ipv6TClass, &tos);
        match (v4, v6) {
            (Err(v4), Err(v6)) => Err(io::Error::other(format!(
                "setsockopt IP_TOS / IPV6_TCLASS both failed: {v4}\n{v6}"
            ))),
            _ => Ok(()),
        }
    }
}

fn split_families(addresses: Vec<SocketAddr>) -> Option<Families> {
    let first_is_v4 = addresses.first()?.is_ipv4();
    let (primaries, fallbacks) = addresses
        .into_iter()
        .partition(|address| address.is_ipv4() == first_is_v4);
    Some(Families { primaries, fallbacks })
}

fn partial_deadline(deadline: Instant, remaining_addresses: usize) -> Instant {
    let now = Instant::now();
    let left = deadline.saturating_duration_since(now);
    let share = left / remaining_addresses.max(1) as u32;
    match share < MIN_PARTIAL_TIMEOUT {
        true => now + left.min(MIN_PARTIAL_TIMEOUT),
        false => now + share,
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "upstream dial timed out")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(text: &str) -> SocketAddr {
        text.parse().unwrap()
    }

    #[test]
    fn the_first_family_is_dialled_first() {
        let families = split_families(vec![
            address("[2001:db8::1]:443"),
            address("1.1.1.1:443"),
            address("[2001:db8::2]:443"),
        ])
        .unwrap();

        assert_eq!(
            families.primaries,
            vec![address("[2001:db8::1]:443"), address("[2001:db8::2]:443")]
        );
        assert_eq!(families.fallbacks, vec![address("1.1.1.1:443")]);
        assert!(split_families(Vec::new()).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn each_address_gets_a_share_of_the_deadline_but_at_least_two_seconds() {
        let now = Instant::now();
        assert_eq!(
            partial_deadline(now + Duration::from_secs(30), 3),
            now + Duration::from_secs(10)
        );
        assert_eq!(
            partial_deadline(now + Duration::from_secs(3), 3),
            now + Duration::from_secs(2)
        );
        assert_eq!(
            partial_deadline(now + Duration::from_secs(1), 3),
            now + Duration::from_secs(1)
        );
    }

    #[tokio::test]
    async fn internal_addresses_are_skipped_and_the_next_one_is_dialled() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let open = listener.local_addr().unwrap();
        let dialer = Dialer { tos: 0 };
        let deadline = Instant::now() + UPSTREAM_DIAL_TIMEOUT;

        let blocked = dialer
            .dial_serial("internal.test", &[address("10.0.0.1:80")], deadline)
            .await
            .unwrap_err();
        let skipped = dialer
            .dial_serial("mixed.test", &[address("10.0.0.1:80"), open], deadline)
            .await;

        assert_eq!(blocked.to_string(), "hostname resolved to internal IP 10.0.0.1");
        assert!(skipped.is_err() == in_always_denied(open.ip()));
    }
}
