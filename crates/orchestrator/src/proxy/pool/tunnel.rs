use std::io;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::proxy_env::ProxyTarget;

pub trait Duplex: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T: AsyncRead + AsyncWrite + Send + Unpin> Duplex for T {}

pub type Stream = Pin<Box<dyn Duplex>>;

const MAX_RESPONSE_HEAD: usize = 64 * 1024;

pub async fn http_connect(stream: &mut Stream, proxy: &ProxyTarget, host: &str, port: u16) -> io::Result<()> {
    let target = match host.contains(':') {
        true => format!("[{host}]:{port}"),
        false => format!("{host}:{port}"),
    };
    let authorization = proxy
        .basic_authorization()
        .map(|credentials| format!("Proxy-Authorization: {credentials}\r\n"))
        .unwrap_or_default();
    let request =
        format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n{authorization}User-Agent: Go-http-client/1.1\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;
    let head = read_head(stream).await?;
    let status_line = head.lines().next().unwrap_or_default();
    let status = status_line.split(' ').nth(1).unwrap_or_default();
    match status {
        "200" => Ok(()),
        _ => Err(io::Error::other(
            status_line.splitn(3, ' ').nth(2).unwrap_or(status_line).to_owned(),
        )),
    }
}

async fn read_head(stream: &mut Stream) -> io::Result<String> {
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        if head.len() >= MAX_RESPONSE_HEAD {
            return Err(io::Error::other("proxy response head too large"));
        }
        head.push(stream.read_u8().await?);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

pub async fn socks5_connect(stream: &mut Stream, proxy: &ProxyTarget, host: &str, port: u16) -> io::Result<()> {
    let methods: &[u8] = match proxy.username {
        Some(_) => &[0, 2],
        None => &[0],
    };
    stream.write_all(&[&[5, methods.len() as u8], methods].concat()).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    check_version(choice[0])?;
    match choice[1] {
        0 => {}
        2 => authenticate(stream, proxy).await?,
        0xFF => return Err(io::Error::other("no acceptable authentication methods")),
        other => return Err(io::Error::other(format!("unsupported authentication method {other}"))),
    }
    stream.write_all(&connect_request(host, port)?).await?;
    read_reply(stream).await
}

fn check_version(version: u8) -> io::Result<()> {
    match version {
        5 => Ok(()),
        other => Err(io::Error::other(format!("unexpected protocol version {other}"))),
    }
}

async fn authenticate(stream: &mut Stream, proxy: &ProxyTarget) -> io::Result<()> {
    let username = proxy.username.clone().unwrap_or_default();
    if username.is_empty() || username.len() > 255 || proxy.password.is_empty() || proxy.password.len() > 255 {
        return Err(io::Error::other("invalid username/password"));
    }
    let request = [
        &[1, username.len() as u8][..],
        username.as_bytes(),
        &[proxy.password.len() as u8],
        proxy.password.as_bytes(),
    ]
    .concat();
    stream.write_all(&request).await?;
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    match reply {
        [1, 0] => Ok(()),
        [1, _] => Err(io::Error::other("authentication failed")),
        [version, _] => Err(io::Error::other(format!("unexpected protocol version {version}"))),
    }
}

fn connect_request(host: &str, port: u16) -> io::Result<Vec<u8>> {
    let address = match host.trim_matches(['[', ']']).parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => [&[1][..], &ip.octets()].concat(),
        Ok(std::net::IpAddr::V6(ip)) => [&[4][..], &ip.octets()].concat(),
        Err(_) if host.len() <= 255 => [&[3, host.len() as u8][..], host.as_bytes()].concat(),
        Err(_) => return Err(io::Error::other("FQDN too long")),
    };
    Ok([&[5, 1, 0][..], &address, &port.to_be_bytes()].concat())
}

async fn read_reply(stream: &mut Stream) -> io::Result<()> {
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    check_version(head[0])?;
    if head[1] != 0 {
        return Err(io::Error::other(reply_text(head[1])));
    }
    let address_length = match head[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await?),
        other => return Err(io::Error::other(format!("unknown address type {other}"))),
    };
    let mut rest = vec![0u8; address_length + 2];
    stream.read_exact(&mut rest).await?;
    Ok(())
}

fn reply_text(code: u8) -> String {
    match code {
        1 => "general SOCKS server failure".to_owned(),
        2 => "connection not allowed by ruleset".to_owned(),
        3 => "network unreachable".to_owned(),
        4 => "host unreachable".to_owned(),
        5 => "connection refused".to_owned(),
        6 => "TTL expired".to_owned(),
        7 => "command not supported".to_owned(),
        8 => "address type not supported".to_owned(),
        other => format!("unknown error code {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socks5_requests_encode_the_three_address_kinds() {
        assert_eq!(
            connect_request("10.1.2.3", 80).unwrap(),
            [5, 1, 0, 1, 10, 1, 2, 3, 0, 80]
        );
        assert_eq!(
            connect_request("a.test", 443).unwrap(),
            [&[5, 1, 0, 3, 6][..], b"a.test", &[1, 187]].concat()
        );
        assert_eq!(connect_request("::1", 1).unwrap().len(), 4 + 16 + 2);
        assert_eq!(connect_request("::1", 1).unwrap()[3], 4);
    }

    #[test]
    fn reply_codes_have_the_go_texts() {
        assert_eq!(reply_text(5), "connection refused");
        assert_eq!(reply_text(99), "unknown error code 99");
    }
}
