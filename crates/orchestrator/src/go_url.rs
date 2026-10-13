pub mod escape;
pub mod query;

use std::fmt;

use escape::{Mode, escape, unescape, valid_encoded};
use query::Query;

const USERINFO_EXTRA: &[u8] = b"-._:~!$&'()*+,;=%@";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GoUrl {
    pub scheme: String,
    pub opaque: String,
    pub user: Option<String>,
    pub host: String,
    pub path: String,
    pub raw_path: String,
    pub omit_host: bool,
    pub force_query: bool,
    pub raw_query: String,
    pub fragment: String,
    pub raw_fragment: String,
}

impl GoUrl {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let (reference, fragment) = raw
            .split_once('#')
            .map_or((raw, None), |(head, tail)| (head, Some(tail)));
        let url = Self::parse_reference(reference)?;
        match fragment.filter(|fragment| !fragment.is_empty()) {
            Some(fragment) => url.with_fragment(fragment),
            None => Ok(url),
        }
    }

    pub fn query(&self) -> Query {
        Query::parse(&self.raw_query)
    }

    pub fn escaped_path(&self) -> String {
        match (self.raw_path_matches(), self.path.as_str()) {
            (true, _) => self.raw_path.clone(),
            (false, "*") => "*".to_owned(),
            (false, path) => escape(path, Mode::Path),
        }
    }

    fn raw_path_matches(&self) -> bool {
        !self.raw_path.is_empty()
            && valid_encoded(&self.raw_path, Mode::Path)
            && unescape(&self.raw_path, Mode::Path).is_ok_and(|path| path == self.path)
    }

    fn escaped_fragment(&self) -> String {
        let raw_matches = !self.raw_fragment.is_empty()
            && valid_encoded(&self.raw_fragment, Mode::Fragment)
            && unescape(&self.raw_fragment, Mode::Fragment).is_ok_and(|fragment| fragment == self.fragment);
        match raw_matches {
            true => self.raw_fragment.clone(),
            false => escape(&self.fragment, Mode::Fragment),
        }
    }

    fn parse_reference(raw: &str) -> Result<Self, String> {
        if raw.bytes().any(|c| c < 0x20 || c == 0x7f) {
            return Err("net/url: invalid control character in URL".to_owned());
        }
        if raw == "*" {
            return Ok(Self {
                path: "*".to_owned(),
                ..Self::default()
            });
        }
        let (scheme, rest) = split_scheme(raw)?;
        let mut url = Self {
            scheme: scheme.to_ascii_lowercase(),
            ..Self::default()
        };
        let rest = url.take_query(rest);
        if !rest.starts_with('/') {
            if !url.scheme.is_empty() {
                url.opaque = rest.to_owned();
                return Ok(url);
            }
            if rest.split('/').next().is_some_and(|segment| segment.contains(':')) {
                return Err("first path segment in URL cannot contain colon".to_owned());
            }
        }
        let path = match (!url.scheme.is_empty() || !rest.starts_with("///")) && rest.starts_with("//") {
            true => url.take_authority(&rest[2..])?,
            false => {
                url.omit_host = !url.scheme.is_empty() && rest.starts_with('/');
                rest
            }
        };
        url.set_path(path)?;
        Ok(url)
    }

    fn take_query<'a>(&mut self, rest: &'a str) -> &'a str {
        if rest.ends_with('?') && rest.matches('?').count() == 1 {
            self.force_query = true;
            return &rest[..rest.len() - 1];
        }
        let (rest, query) = rest.split_once('?').unwrap_or((rest, ""));
        self.raw_query = query.to_owned();
        rest
    }

    fn take_authority<'a>(&mut self, rest: &'a str) -> Result<&'a str, String> {
        let (authority, path) = rest.find('/').map_or((rest, ""), |index| rest.split_at(index));
        let (userinfo, host) = authority.rfind('@').map_or((None, authority), |index| {
            (Some(&authority[..index]), &authority[index + 1..])
        });
        self.host = parse_host(host)?;
        self.user = userinfo.map(parse_userinfo).transpose()?;
        Ok(path)
    }

    fn set_path(&mut self, raw: &str) -> Result<(), String> {
        self.path = unescape(raw, Mode::Path)?;
        self.raw_path = match escape(&self.path, Mode::Path) == raw {
            true => String::new(),
            false => raw.to_owned(),
        };
        Ok(())
    }

    fn with_fragment(mut self, raw: &str) -> Result<Self, String> {
        self.fragment = unescape(raw, Mode::Fragment)?;
        self.raw_fragment = match escape(&self.fragment, Mode::Fragment) == raw {
            true => String::new(),
            false => raw.to_owned(),
        };
        Ok(self)
    }

    fn write_authority_and_path(&self, out: &mut String) {
        if !self.scheme.is_empty() || !self.host.is_empty() || self.user.is_some() {
            let omitted = self.omit_host && self.host.is_empty() && self.user.is_none();
            if !omitted && (!self.host.is_empty() || !self.path.is_empty() || self.user.is_some()) {
                out.push_str("//");
            }
            if let Some(user) = self.user.as_deref().filter(|_| !omitted) {
                out.push_str(user);
                out.push('@');
            }
            if !omitted {
                out.push_str(&escape(&self.host, Mode::Host));
            }
        }
        let path = self.escaped_path();
        if !path.is_empty() && !path.starts_with('/') && !self.host.is_empty() {
            out.push('/');
        }
        if out.is_empty() && path.split('/').next().is_some_and(|segment| segment.contains(':')) {
            out.push_str("./");
        }
        out.push_str(&path);
    }
}

impl fmt::Display for GoUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut out = String::new();
        if !self.opaque.is_empty() {
            out.push_str(&self.scheme);
            out.push(':');
            out.push_str(&self.opaque);
        } else {
            if !self.scheme.is_empty() {
                out.push_str(&self.scheme);
                out.push(':');
            }
            self.write_authority_and_path(&mut out);
        }
        if self.force_query || !self.raw_query.is_empty() {
            out.push('?');
            out.push_str(&self.raw_query);
        }
        if !self.fragment.is_empty() {
            out.push('#');
            out.push_str(&self.escaped_fragment());
        }
        f.write_str(&out)
    }
}

fn split_scheme(raw: &str) -> Result<(&str, &str), String> {
    for (index, c) in raw.bytes().enumerate() {
        match c {
            c if c.is_ascii_alphabetic() => {}
            b'0'..=b'9' | b'+' | b'-' | b'.' if index == 0 => return Ok(("", raw)),
            b'0'..=b'9' | b'+' | b'-' | b'.' => {}
            b':' if index == 0 => return Err("missing protocol scheme".to_owned()),
            b':' => return Ok((&raw[..index], &raw[index + 1..])),
            _ => return Ok(("", raw)),
        }
    }
    Ok(("", raw))
}

fn parse_host(host: &str) -> Result<String, String> {
    match host.strip_prefix('[') {
        Some(_) => parse_bracketed_host(host),
        None => {
            if let Some(index) = host.rfind(':') {
                check_port(&host[index..])?;
            }
            unescape(host, Mode::Host)
        }
    }
}

fn parse_bracketed_host(host: &str) -> Result<String, String> {
    let close = host.rfind(']').ok_or("missing ']' in host")?;
    check_port(&host[close + 1..])?;
    match host[..close].find("%25") {
        Some(zone) => Ok([
            unescape(&host[..zone], Mode::Host)?,
            unescape(&host[zone..close], Mode::Zone)?,
            unescape(&host[close..], Mode::Host)?,
        ]
        .concat()),
        None => unescape(host, Mode::Host),
    }
}

fn check_port(colon_port: &str) -> Result<(), String> {
    let valid = colon_port.is_empty()
        || colon_port
            .strip_prefix(':')
            .is_some_and(|port| port.bytes().all(|c| c.is_ascii_digit()));
    match valid {
        true => Ok(()),
        false => Err(format!("invalid port {colon_port:?} after host")),
    }
}

fn parse_userinfo(userinfo: &str) -> Result<String, String> {
    if !userinfo
        .bytes()
        .all(|c| c.is_ascii_alphanumeric() || USERINFO_EXTRA.contains(&c))
    {
        return Err("net/url: invalid userinfo".to_owned());
    }
    match userinfo.split_once(':') {
        Some((name, password)) => Ok(format!(
            "{}:{}",
            escape(&unescape(name, Mode::UserPassword)?, Mode::UserPassword),
            escape(&unescape(password, Mode::UserPassword)?, Mode::UserPassword)
        )),
        None => Ok(escape(&unescape(userinfo, Mode::UserPassword)?, Mode::UserPassword)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(raw: &str) -> GoUrl {
        GoUrl::parse(raw).unwrap()
    }

    #[test]
    fn parses_storage_shapes() {
        let s3 = parsed("S3://bucket?endpoint=http://minio:9000&s3ForcePathStyle=true");
        assert_eq!(
            (s3.scheme.as_str(), s3.host.as_str(), s3.path.as_str()),
            ("s3", "bucket", "")
        );
        assert_eq!(s3.raw_query, "endpoint=http://minio:9000&s3ForcePathStyle=true");

        let file = parsed("file:///var/lib/a%20b");
        assert_eq!((file.host.as_str(), file.path.as_str()), ("", "/var/lib/a b"));
        assert_eq!(file.to_string(), "file:///var/lib/a%20b");

        let relative = parsed("file:relative/path");
        assert_eq!(relative.opaque, "relative/path");
        assert_eq!(relative.to_string(), "file:relative/path");

        let user = parsed("gs://key:secret@bucket/prefix#frag");
        assert_eq!(user.user.as_deref(), Some("key:secret"));
        assert_eq!((user.host.as_str(), user.path.as_str()), ("bucket", "/prefix"));
        assert_eq!(user.to_string(), "gs://key:secret@bucket/prefix#frag");
    }

    #[test]
    fn errors_match_net_url() {
        let error = |raw: &str| GoUrl::parse(raw).unwrap_err();
        assert_eq!(error(":x"), "missing protocol scheme");
        assert_eq!(error("1a:b"), "first path segment in URL cannot contain colon");
        assert_eq!(error("s3://b\n"), "net/url: invalid control character in URL");
        assert_eq!(error("s3://b:x1"), "invalid port \":x1\" after host");
        assert_eq!(error("s3://b%zz"), "invalid URL escape \"%zz\"");
        assert_eq!(error("s3://[::1"), "missing ']' in host");
        assert_eq!(error("s3://a b@c"), "net/url: invalid userinfo");
    }

    #[test]
    fn prints_like_url_string() {
        let mut url = GoUrl {
            scheme: "s3".to_owned(),
            host: "bucket".to_owned(),
            ..GoUrl::default()
        };
        assert_eq!(url.to_string(), "s3://bucket");
        url.raw_query = Query::single("s3ForcePathStyle", "true").encode();
        assert_eq!(url.to_string(), "s3://bucket?s3ForcePathStyle=true");
        let opaque = GoUrl {
            scheme: "file".to_owned(),
            opaque: "tmp/x".to_owned(),
            ..GoUrl::default()
        };
        assert_eq!(opaque.to_string(), "file:tmp/x");
        let absolute = GoUrl {
            scheme: "file".to_owned(),
            path: "/tmp/a?b".to_owned(),
            ..GoUrl::default()
        };
        assert_eq!(absolute.to_string(), "file:///tmp/a%3Fb");
    }
}
