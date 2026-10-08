use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

const LISTEN: &str = "0A";
const FORWARDED_IPS: [&str; 3] = ["127.0.0.1", "localhost", "::1"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listener {
    pub pid: i32,
    pub ip: String,
    pub port: u32,
    pub family: u32,
}

pub fn local_listeners(proc_root: &Path) -> Vec<Listener> {
    let owners = socket_owners(proc_root);
    [("net/tcp", 4), ("net/tcp6", 6)]
        .iter()
        .flat_map(|(table, family)| {
            std::fs::read_to_string(proc_root.join(table))
                .map(|content| parse_table(&content, *family, &owners))
                .unwrap_or_default()
        })
        .filter(|listener| FORWARDED_IPS.contains(&listener.ip.as_str()))
        .collect()
}

fn parse_table(content: &str, family: u32, owners: &HashMap<u64, i32>) -> Vec<Listener> {
    content
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, state, inode) = (fields.get(1)?, fields.get(3)?, fields.get(9)?);
            (*state == LISTEN).then_some(())?;
            let (address, port) = local.split_once(':')?;
            Some(Listener {
                pid: inode
                    .parse()
                    .ok()
                    .and_then(|inode| owners.get(&inode).copied())
                    .unwrap_or(0),
                ip: format_ip(parse_address(address)?),
                port: u32::from_str_radix(port, 16).ok()?,
                family,
            })
        })
        .collect()
}

fn parse_address(hex: &str) -> Option<IpAddr> {
    let words: Vec<u32> = (0..hex.len() / 8)
        .map(|index| u32::from_str_radix(&hex[index * 8..index * 8 + 8], 16).map(u32::swap_bytes))
        .collect::<Result<_, _>>()
        .ok()?;
    match words.as_slice() {
        [v4] => Some(IpAddr::V4(Ipv4Addr::from(*v4))),
        [a, b, c, d] => {
            let bytes: Vec<u8> = [a, b, c, d].iter().flat_map(|word| word.to_be_bytes()).collect();
            Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes).ok()?)))
        }
        _ => None,
    }
}

fn format_ip(ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or_else(|| v6.to_string(), |v4| v4.to_string()),
        IpAddr::V4(v4) => v4.to_string(),
    }
}

fn socket_owners(proc_root: &Path) -> HashMap<u64, i32> {
    std::fs::read_dir(proc_root)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .parse::<i32>()
                .ok()
                .map(|pid| (pid, entry.path()))
        })
        .flat_map(|(pid, path)| {
            std::fs::read_dir(path.join("fd"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .filter_map(|fd| std::fs::read_link(fd.path()).ok())
                .filter_map(|target| {
                    target
                        .to_str()?
                        .strip_prefix("socket:[")?
                        .strip_suffix(']')?
                        .parse::<u64>()
                        .ok()
                })
                .map(move |inode| (inode, pid))
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TCP: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4242 1 0000000000000000 100 0 0 10 0
   1: 00000000:0016 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 4243 1 0000000000000000 100 0 0 10 0
   2: 0100007F:1F91 0100007F:C350 01 00000000:00000000 00:00000000 00000000     0        0 4244 1 0000000000000000 100 0 0 10 0
";
    const TCP6: &str = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode
   0: 00000000000000000000000001000000:0BB8 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 5000 1 0000000000000000 100 0 0 10 0
   1: 0000000000000000FFFF00000100007F:0BB9 00000000000000000000000000000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 5001 1 0000000000000000 100 0 0 10 0
";

    #[test]
    fn listening_sockets_on_loopback_are_found_with_their_owner() {
        let owners = HashMap::from([(4242, 77)]);
        let v4 = parse_table(TCP, 4, &owners);
        let v6 = parse_table(TCP6, 6, &owners);

        assert_eq!(
            v4[0],
            Listener {
                pid: 77,
                ip: "127.0.0.1".to_owned(),
                port: 8080,
                family: 4,
            }
        );
        assert_eq!(v4[1].ip, "0.0.0.0");
        assert_eq!(v4.len(), 2);
        assert_eq!((v6[0].ip.as_str(), v6[0].port, v6[0].pid), ("::1", 3000, 0));
        assert_eq!((v6[1].ip.as_str(), v6[1].family), ("127.0.0.1", 6));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_real_loopback_listener_is_found_with_this_process_as_owner() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = u32::from(listener.local_addr().unwrap().port());

        let found = local_listeners(Path::new("/proc"));

        assert!(
            found.contains(&Listener {
                pid: std::process::id() as i32,
                ip: "127.0.0.1".to_owned(),
                port,
                family: 4,
            }),
            "{found:?}"
        );
    }
}
