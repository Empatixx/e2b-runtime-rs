use std::net::IpAddr;

use ipnet::IpNet;
use url::Url;

const CGI_REFUSAL: &str = "refusing to use HTTP_PROXY value in CGI environment; see golang.org/s/cgihttpproxy";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    Http,
    Https,
    Socks5,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProxyTarget {
    pub scheme: ProxyScheme,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: String,
}

impl ProxyTarget {
    pub fn address(&self) -> String {
        match self.host.contains(':') {
            true => format!("[{}]:{}", self.host, self.port),
            false => format!("{}:{}", self.host, self.port),
        }
    }

    pub fn basic_authorization(&self) -> Option<String> {
        use base64::Engine;
        self.username.as_ref().map(|username| {
            let credentials = format!("{username}:{}", self.password);
            format!(
                "Basic {}",
                base64::engine::general_purpose::STANDARD.encode(credentials)
            )
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Matcher {
    Cidr(IpNet),
    Ip {
        ip: IpAddr,
        port: String,
    },
    Domain {
        host: String,
        port: String,
        match_host: bool,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProxyEnv {
    http: Option<ProxyTarget>,
    https: Option<ProxyTarget>,
    all_match: bool,
    matchers: Vec<Matcher>,
    cgi: bool,
}

impl ProxyEnv {
    pub fn from_process() -> Self {
        Self::from_vars(&|name| std::env::var(name).ok())
    }

    pub fn from_vars(var: &dyn Fn(&str) -> Option<String>) -> Self {
        let any = |names: [&str; 2]| {
            names
                .iter()
                .find_map(|name| var(name).filter(|value| !value.is_empty()))
        };
        let (all_match, matchers) = parse_no_proxy(&any(["NO_PROXY", "no_proxy"]).unwrap_or_default());
        Self {
            http: any(["HTTP_PROXY", "http_proxy"]).and_then(|raw| parse_proxy(&raw)),
            https: any(["HTTPS_PROXY", "https_proxy"]).and_then(|raw| parse_proxy(&raw)),
            all_match,
            matchers,
            cgi: var("REQUEST_METHOD").is_some_and(|value| !value.is_empty()),
        }
    }

    pub fn for_target(&self, https: bool, host: &str, port: u16) -> Result<Option<&ProxyTarget>, String> {
        let proxy = match https {
            true => self.https.as_ref(),
            false => self.http.as_ref(),
        };
        match proxy {
            None => Ok(None),
            Some(_) if !https && self.cgi => Err(CGI_REFUSAL.to_owned()),
            Some(proxy) => Ok(self.uses_proxy(host, port).then_some(proxy)),
        }
    }

    fn uses_proxy(&self, host: &str, port: u16) -> bool {
        let host = host.trim().to_lowercase();
        let ip = host.trim_matches(['[', ']']).parse::<IpAddr>().ok();
        if host == "localhost" || ip.is_some_and(|ip| ip.is_loopback()) {
            return false;
        }
        if self.all_match {
            return false;
        }
        let port = port.to_string();
        !self.matchers.iter().any(|matcher| match matcher {
            Matcher::Cidr(net) => ip.is_some_and(|ip| net.contains(&ip)),
            Matcher::Ip {
                ip: wanted,
                port: wanted_port,
            } => ip == Some(*wanted) && (wanted_port.is_empty() || *wanted_port == port),
            Matcher::Domain {
                host: suffix,
                port: wanted_port,
                match_host,
            } => {
                (host.ends_with(suffix.as_str()) || (*match_host && host == suffix[1..]))
                    && (wanted_port.is_empty() || *wanted_port == port)
            }
        })
    }
}

fn parse_proxy(raw: &str) -> Option<ProxyTarget> {
    let url = Url::parse(raw)
        .ok()
        .filter(|url| url.host_str().is_some())
        .or_else(|| Url::parse(&format!("http://{raw}")).ok())?;
    let (scheme, default_port) = match url.scheme() {
        "http" => (ProxyScheme::Http, 80),
        "https" => (ProxyScheme::Https, 443),
        "socks5" | "socks5h" => (ProxyScheme::Socks5, 1080),
        _ => return None,
    };
    Some(ProxyTarget {
        scheme,
        host: url.host_str()?.trim_matches(['[', ']']).to_owned(),
        port: url.port().unwrap_or(default_port),
        username: Some(url.username())
            .filter(|username| !username.is_empty() || url.password().is_some())
            .map(|username| {
                percent_encoding::percent_decode_str(username)
                    .decode_utf8_lossy()
                    .into_owned()
            }),
        password: url
            .password()
            .map(|password| {
                percent_encoding::percent_decode_str(password)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .unwrap_or_default(),
    })
}

fn parse_no_proxy(raw: &str) -> (bool, Vec<Matcher>) {
    let mut all_match = false;
    let mut matchers = Vec::new();
    for entry in raw.split(',').map(|entry| entry.trim().to_lowercase()) {
        if entry.is_empty() {
            continue;
        }
        if entry == "*" {
            all_match = true;
            break;
        }
        matchers.extend(parse_entry(&entry));
    }
    (all_match, matchers)
}

fn parse_entry(entry: &str) -> Option<Matcher> {
    if let Ok(net) = entry.parse::<IpNet>() {
        return Some(Matcher::Cidr(net));
    }
    let (host, port) = split_host_port(entry);
    if host.is_empty() {
        return None;
    }
    if let Ok(ip) = host.trim_matches(['[', ']']).parse::<IpAddr>() {
        return Some(Matcher::Ip { ip, port });
    }
    let host = host.strip_prefix('*').unwrap_or(&host).to_owned();
    let match_host = !host.starts_with('.');
    Some(Matcher::Domain {
        host: match match_host {
            true => format!(".{host}"),
            false => host,
        },
        port,
        match_host,
    })
}

fn split_host_port(entry: &str) -> (String, String) {
    let bracketed_ipv6 = entry.starts_with('[');
    match entry.rsplit_once(':') {
        Some((host, port))
            if (bracketed_ipv6 && host.ends_with(']') || !host.contains(':'))
                && port.bytes().all(|b| b.is_ascii_digit()) =>
        {
            (host.to_owned(), port.to_owned())
        }
        _ => (entry.to_owned(), String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars_env(pairs: &[(&str, &str)]) -> ProxyEnv {
        ProxyEnv::from_vars(&|name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn chosen(env: &ProxyEnv, https: bool, host: &str, port: u16) -> Option<String> {
        env.for_target(https, host, port).unwrap().map(ProxyTarget::address)
    }

    #[test]
    fn picks_the_proxy_by_target_scheme() {
        let env = vars_env(&[("HTTP_PROXY", "http://plain:3128"), ("HTTPS_PROXY", "secure:3129")]);
        assert_eq!(chosen(&env, false, "10.1.0.2", 80).as_deref(), Some("plain:3128"));
        assert_eq!(chosen(&env, true, "10.1.0.2", 443).as_deref(), Some("secure:3129"));
    }

    #[test]
    fn no_variables_mean_no_proxy() {
        assert_eq!(chosen(&vars_env(&[]), false, "10.1.0.2", 80), None);
        assert_eq!(chosen(&vars_env(&[("HTTP_PROXY", "")]), false, "10.1.0.2", 80), None);
    }

    #[test]
    fn uppercase_wins_over_lowercase() {
        let env = vars_env(&[("HTTP_PROXY", "upper:1"), ("http_proxy", "lower:2")]);
        assert_eq!(chosen(&env, false, "10.1.0.2", 80).as_deref(), Some("upper:1"));
        let only_lower = vars_env(&[("http_proxy", "lower:2")]);
        assert_eq!(chosen(&only_lower, false, "10.1.0.2", 80).as_deref(), Some("lower:2"));
    }

    #[test]
    fn loopback_and_localhost_are_never_proxied() {
        let env = vars_env(&[("HTTP_PROXY", "proxy:3128")]);
        for host in ["localhost", "LOCALHOST", "127.0.0.1", "127.1.2.3", "[::1]", "::1"] {
            assert_eq!(chosen(&env, false, host, 80), None, "{host}");
        }
    }

    #[test]
    fn no_proxy_star_disables_everything() {
        let env = vars_env(&[("HTTP_PROXY", "proxy:3128"), ("NO_PROXY", "*")]);
        assert_eq!(chosen(&env, false, "10.1.0.2", 80), None);
    }

    #[test]
    fn no_proxy_entries_match_like_go() {
        let env = vars_env(&[
            ("HTTP_PROXY", "proxy:3128"),
            (
                "NO_PROXY",
                "foo.com, .bar.com, *.baz.com, 10.2.0.0/16, 192.168.1.5, 172.16.0.9:8080, [fd00::1]:443, qux.com:8081",
            ),
        ]);
        let proxied = |host: &str, port| chosen(&env, false, host, port).is_some();
        assert!(!proxied("foo.com", 80));
        assert!(!proxied("a.foo.com", 80));
        assert!(proxied("xfoo.com", 80));
        assert!(!proxied("x.bar.com", 80));
        assert!(proxied("bar.com", 80));
        assert!(!proxied("x.baz.com", 80));
        assert!(proxied("baz.com", 80));
        assert!(!proxied("10.2.9.9", 80));
        assert!(proxied("10.3.0.1", 80));
        assert!(!proxied("192.168.1.5", 80));
        assert!(!proxied("172.16.0.9", 8080));
        assert!(proxied("172.16.0.9", 80));
        assert!(!proxied("qux.com", 8081));
        assert!(proxied("qux.com", 80));
        assert!(!proxied("FOO.COM", 80));
    }

    #[test]
    fn userinfo_becomes_basic_authorization() {
        let env = vars_env(&[("HTTP_PROXY", "http://user:p%40ss@proxy:3128")]);
        let target = env.for_target(false, "10.1.0.2", 80).unwrap().unwrap();
        assert_eq!(target.username.as_deref(), Some("user"));
        assert_eq!(target.password, "p@ss");
        assert_eq!(target.basic_authorization().as_deref(), Some("Basic dXNlcjpwQHNz"));
    }

    #[test]
    fn schemes_default_their_ports() {
        let port = |raw: &str| parse_proxy(raw).map(|target| (target.scheme, target.port));
        assert_eq!(port("http://p"), Some((ProxyScheme::Http, 80)));
        assert_eq!(port("https://p"), Some((ProxyScheme::Https, 443)));
        assert_eq!(port("socks5://p"), Some((ProxyScheme::Socks5, 1080)));
        assert_eq!(port("socks5h://p:9"), Some((ProxyScheme::Socks5, 9)));
        assert_eq!(port("p:3128"), Some((ProxyScheme::Http, 3128)));
        assert_eq!(port("ftp://p"), None);
    }

    #[test]
    fn a_cgi_environment_refuses_http_proxy() {
        let env = vars_env(&[("HTTP_PROXY", "proxy:3128"), ("REQUEST_METHOD", "GET")]);
        assert_eq!(env.for_target(false, "10.1.0.2", 80), Err(CGI_REFUSAL.to_owned()));
        assert!(env.for_target(true, "10.1.0.2", 443).is_ok());
    }
}
