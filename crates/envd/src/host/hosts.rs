use std::net::{IpAddr, Ipv6Addr};
use std::path::Path;
use std::sync::LazyLock;

use regex::Regex;

use crate::util::errno;

pub const EVENTS_HOST: &str = "events.e2b.local";

static LOCALHOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"((127\.([0-9]{1,3}\.){2}[0-9]{1,3})|(::1)$)").expect("valid localhost pattern"));

pub fn rewrite_hosts_file(address: &str, path: &Path) -> Result<(), String> {
    let mut hosts = HostsFile::read(path)?;
    let family = requested_family(address)?;
    if hosts
        .address_of(EVENTS_HOST, family)
        .is_some_and(|(current, _)| current == address)
    {
        return Ok(());
    }
    hosts.add(address, EVENTS_HOST);
    hosts.save(path)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IpFamily {
    V4,
    V6,
}

fn requested_family(address: &str) -> Result<IpFamily, String> {
    let unzoned = address.split_once('%').map_or(address, |(ip, _)| ip);
    match (address.parse::<IpAddr>(), unzoned.parse::<Ipv6Addr>()) {
        (Ok(IpAddr::V4(_)), _) => Ok(IpFamily::V4),
        (Ok(IpAddr::V6(_)), _) | (_, Ok(_)) => Ok(IpFamily::V6),
        _ => Err(format!(
            "failed to get ip family: failed to parse IP address: ParseAddr({address:?}): unable to parse IP"
        )),
    }
}

fn line_family(address: &str) -> Option<IpFamily> {
    match address.parse::<IpAddr>().ok()? {
        IpAddr::V4(_) => Some(IpFamily::V4),
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some() => Some(IpFamily::V4),
        IpAddr::V6(_) => Some(IpFamily::V6),
    }
}

fn strip_line_breaks(text: &str) -> String {
    text.chars()
        .filter(|c| !matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\0'))
        .collect()
}

struct Entry {
    address: String,
    hostnames: Vec<String>,
    comment: String,
}

impl Entry {
    fn render(&self) -> String {
        let hostnames = self
            .hostnames
            .iter()
            .map(|hostname| strip_line_breaks(hostname))
            .collect::<Vec<_>>()
            .join(" ");
        match self.comment.is_empty() {
            true => format!("{:<16} {hostnames}", self.address),
            false => format!(
                "{:<16} {hostnames} # {}",
                self.address,
                strip_line_breaks(&self.comment)
            ),
        }
    }
}

enum Line {
    Verbatim(String),
    Address(Entry),
}

impl Line {
    fn parse(raw: &str) -> Self {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            return Self::Verbatim(raw.to_owned());
        }
        let (before, comment) = trimmed
            .split_once('#')
            .map_or((trimmed, ""), |(before, comment)| (before, comment.trim()));
        let mut parts = before.split_whitespace().map(str::to_lowercase);
        match (parts.next(), parts.collect::<Vec<_>>()) {
            (Some(address), hostnames) if !hostnames.is_empty() => Self::Address(Entry {
                address,
                hostnames,
                comment: comment.to_owned(),
            }),
            _ => Self::Verbatim(raw.to_owned()),
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Verbatim(raw) => raw.clone(),
            Self::Address(entry) => entry.render(),
        }
    }
}

struct HostsFile {
    lines: Vec<Line>,
}

impl HostsFile {
    fn read(path: &Path) -> Result<Self, String> {
        std::fs::read(path)
            .map(|bytes| Self::parse(&String::from_utf8_lossy(&bytes)))
            .map_err(|e| {
                let path = path.display();
                format!(
                    "failed to create hosts: read hosts file {path}: open {path}: {}",
                    errno::message(&e)
                )
            })
    }

    fn parse(text: &str) -> Self {
        let normalized = text.replace("\r\n", "\n");
        let mut raw: Vec<&str> = normalized.split('\n').collect();
        raw.pop_if(|last| last.is_empty());
        Self {
            lines: raw.into_iter().map(Line::parse).collect(),
        }
    }

    fn address_of(&self, host: &str, family: IpFamily) -> Option<(&str, usize)> {
        let host = host.trim().to_lowercase();
        self.lines.iter().enumerate().find_map(|(index, line)| match line {
            Line::Address(entry) if entry.hostnames.contains(&host) && line_family(&entry.address) == Some(family) => {
                Some((entry.address.as_str(), index))
            }
            _ => None,
        })
    }

    fn add(&mut self, address: &str, host: &str) {
        let host = strip_line_breaks(host.to_lowercase().trim());
        let address = address.to_lowercase().trim().to_owned();
        let Some(family) = line_family(&address) else {
            return;
        };
        match self
            .address_of(&host, family)
            .map(|(current, index)| (current == address, index))
        {
            Some((true, _)) => return,
            Some((false, index)) => self.remove_host(index, &host, &address),
            None => {}
        }
        let same_address = self.lines.iter_mut().find_map(|line| match line {
            Line::Address(entry) if entry.address == address && entry.comment.is_empty() => Some(entry),
            _ => None,
        });
        match same_address {
            Some(entry) => entry.hostnames.push(host),
            None => self.lines.push(Line::Address(Entry {
                address,
                hostnames: vec![host],
                comment: String::new(),
            })),
        }
    }

    fn remove_host(&mut self, index: usize, host: &str, new_address: &str) {
        if LOCALHOST.is_match(new_address) {
            return;
        }
        let Some(Line::Address(entry)) = self.lines.get_mut(index) else {
            return;
        };
        if let Some(position) = entry.hostnames.iter().position(|hostname| hostname == host) {
            entry.hostnames.remove(position);
        }
        if entry.hostnames.is_empty() {
            self.lines.remove(index);
        }
    }

    fn render(&self) -> String {
        self.lines.iter().map(|line| line.render() + "\n").collect()
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        std::fs::write(path, self.render()).map_err(|e| {
            let path = path.display();
            format!("write hosts file {path}: open {path}: {}", errno::message(&e))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rewritten(input: &str, address: &str) -> String {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), input).unwrap();
        rewrite_hosts_file(address, tmp.path()).unwrap();
        std::fs::read_to_string(tmp.path()).unwrap()
    }

    #[test]
    fn simple_cases_match_go() {
        let value = "\n# comment\n127.0.0.1        one.host\n127.0.0.2        two.host\n";
        let preprocessors: [fn(&str) -> &str; 4] = [
            |s| s,
            |s| s.strip_prefix('\n').unwrap_or(s),
            |s| s.strip_suffix('\n').unwrap_or(s),
            str::trim,
        ];
        for preprocess in preprocessors {
            assert_eq!(
                rewritten(preprocess(value), "127.0.0.3").trim(),
                "# comment\n127.0.0.1        one.host\n127.0.0.2        two.host\n127.0.0.3        events.e2b.local"
            );
        }
    }

    #[test]
    fn address_lines_are_reformatted_like_txeh() {
        assert_eq!(
            rewritten("127.0.0.1\tlocalhost\r\n::1 ip6-localhost # loop\r\n", "10.0.0.2"),
            "127.0.0.1        localhost\n::1              ip6-localhost # loop\n10.0.0.2         events.e2b.local\n"
        );
    }

    #[test]
    fn the_events_host_moves_off_a_shared_line() {
        assert_eq!(
            rewritten("10.0.0.1\tother events.e2b.local\n10.0.0.2 keep\n", "10.0.0.2"),
            "10.0.0.1         other\n10.0.0.2         keep events.e2b.local\n"
        );
    }

    #[test]
    fn a_line_left_without_hosts_is_dropped() {
        assert_eq!(
            rewritten("127.0.0.1\tlocalhost\n10.0.0.1\tevents.e2b.local\n", "10.0.0.2"),
            "127.0.0.1        localhost\n10.0.0.2         events.e2b.local\n"
        );
    }

    #[test]
    fn a_loopback_address_keeps_the_old_entry() {
        assert_eq!(
            rewritten("10.0.0.1 events.e2b.local\n", "127.0.0.5"),
            "10.0.0.1         events.e2b.local\n127.0.0.5        events.e2b.local\n"
        );
    }

    #[test]
    fn an_address_already_in_place_leaves_the_file_untouched() {
        let input = "127.0.0.1\tlocalhost\n10.0.0.2\tevents.e2b.local\n";
        assert_eq!(rewritten(input, "10.0.0.2"), input);
    }

    #[test]
    fn single_words_and_comments_are_kept_verbatim() {
        assert_eq!(
            rewritten("  # spaced comment\nlonely\n\n", "10.0.0.2"),
            "  # spaced comment\nlonely\n\n10.0.0.2         events.e2b.local\n"
        );
    }

    #[test]
    fn a_missing_file_fails_like_go() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts");
        assert_eq!(
            rewrite_hosts_file("10.0.0.2", &path).unwrap_err(),
            format!(
                "failed to create hosts: read hosts file {0}: open {0}: no such file or directory",
                path.display()
            )
        );
    }

    #[test]
    fn an_invalid_address_fails_like_go() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        assert_eq!(
            rewrite_hosts_file("nope", tmp.path()).unwrap_err(),
            "failed to get ip family: failed to parse IP address: ParseAddr(\"nope\"): unable to parse IP"
        );
    }
}
