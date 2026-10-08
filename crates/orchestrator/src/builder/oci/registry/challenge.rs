use std::collections::BTreeMap;
use std::net::IpAddr;

use reqwest::header::{HeaderMap, WWW_AUTHENTICATE};
use reqwest::redirect::Policy;
use serde::Deserialize;

use super::{RegistryError, retry, transport_error};
use crate::builder::oci::ecr::BasicCredentials;
use crate::go_url::GoUrl;

const MAX_REDIRECTS: usize = 10;
const ALLOWED_SCHEMES: [&str; 2] = ["basic", "bearer"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Challenge {
    pub scheme: String,
    pub params: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    #[serde(default)]
    token: String,
    #[serde(default)]
    access_token: String,
}

pub struct TokenRequest<'a> {
    pub realm: &'a str,
    pub service: &'a str,
    pub scopes: &'a [String],
    pub registry: &'a str,
    pub insecure: bool,
    pub credentials: Option<&'a BasicCredentials>,
}

pub fn all(headers: &HeaderMap) -> Vec<Challenge> {
    headers
        .get_all(WWW_AUTHENTICATE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(parse_header)
        .collect()
}

pub fn pick(headers: &HeaderMap) -> Option<Challenge> {
    let challenges = all(headers);
    challenges
        .iter()
        .find(|challenge| ALLOWED_SCHEMES.contains(&challenge.scheme.to_ascii_lowercase().as_str()))
        .or(challenges.first())
        .cloned()
}

pub fn parse_header(value: &str) -> Vec<Challenge> {
    let mut challenges = Vec::new();
    let mut rest = value.trim_start();
    while !rest.is_empty() {
        let (scheme, after) = split_token(rest);
        if scheme.is_empty() {
            break;
        }
        let (params, remaining) = parse_params(after);
        challenges.push(Challenge {
            scheme: scheme.to_owned(),
            params,
        });
        rest = remaining.trim_start_matches([',', ' ', '\t']);
    }
    challenges
}

fn parse_params(text: &str) -> (BTreeMap<String, String>, &str) {
    let mut params = BTreeMap::new();
    let mut rest = text.trim_start();
    loop {
        let (key, after) = split_token(rest);
        let Some(after) = after.trim_start().strip_prefix('=').filter(|_| !key.is_empty()) else {
            return (params, rest);
        };
        let (value, remaining) = parse_value(after.trim_start());
        params.insert(key.to_ascii_lowercase(), value);
        rest = remaining.trim_start();
        match rest.strip_prefix(',') {
            Some(next) => rest = next.trim_start(),
            None => return (params, rest),
        }
    }
}

fn parse_value(text: &str) -> (String, &str) {
    let Some(quoted) = text.strip_prefix('"') else {
        let (token, rest) = split_token(text);
        return (token.to_owned(), rest);
    };
    let mut value = String::new();
    let mut chars = quoted.char_indices();
    while let Some((index, c)) = chars.next() {
        match c {
            '\\' => value.extend(chars.next().map(|(_, escaped)| escaped)),
            '"' => return (value, &quoted[index + 1..]),
            c => value.push(c),
        }
    }
    (value, "")
}

fn split_token(text: &str) -> (&str, &str) {
    let end = text
        .find(|c: char| c.is_whitespace() || matches!(c, ',' | '=' | '"' | ';'))
        .unwrap_or(text.len());
    text.split_at(end)
}

pub fn validate_realm(realm: &str, registry: &str, insecure: bool) -> Result<(), String> {
    let url = GoUrl::parse(realm).map_err(|e| format!("parsing realm {}: {e}", quote(realm)))?;
    match url.scheme.as_str() {
        "https" => {}
        "http" if insecure => {}
        "http" => {
            return Err(format!(
                "realm scheme {} not allowed for a secure registry; use https",
                quote(&url.scheme)
            ));
        }
        scheme => {
            return Err(format!(
                "realm scheme {} not allowed; must be https (or http for insecure registries)",
                quote(scheme)
            ));
        }
    }
    if !registry.is_empty() && url.host == registry {
        return Ok(());
    }
    let host = hostname(&url.host);
    match host.parse::<IpAddr>() {
        Ok(ip) if is_internal(ip) => Err(format!("realm host {} is a private or link-local address", quote(host))),
        _ => Ok(()),
    }
}

pub async fn bearer_token(request: TokenRequest<'_>) -> Result<String, RegistryError> {
    validate_realm(request.realm, request.registry, request.insecure)
        .map_err(|e| RegistryError::Other(format!("invalid realm in www-authenticate: {e}")))?;
    let client = reqwest::Client::builder()
        .redirect(redirect_policy(request.registry, request.insecure))
        .build()
        .map_err(|e| RegistryError::Other(e.to_string()))?;
    let url = token_url(&request)?;
    let builder = client.get(&url);
    let builder = match request.credentials.filter(|basic| sendable(basic)) {
        Some(basic) => builder.basic_auth(&basic.username, Some(&basic.password)),
        None => builder,
    };
    let response = retry::send(builder)
        .await
        .map_err(|e| RegistryError::Other(e.to_string()))?;
    if response.status() != reqwest::StatusCode::OK {
        return Err(transport_error("GET", &url, response).await);
    }
    let body = response
        .bytes()
        .await
        .map_err(|e| RegistryError::Other(e.to_string()))?;
    let token: TokenResponse = serde_json::from_slice(&body).map_err(|e| RegistryError::Other(e.to_string()))?;
    match (token.access_token.is_empty(), token.token.is_empty()) {
        (false, _) => Ok(token.access_token),
        (true, false) => Ok(token.token),
        (true, true) => Err(RegistryError::Other(format!(
            "no token in bearer response:\n{}",
            String::from_utf8_lossy(&body)
        ))),
    }
}

pub fn sendable(basic: &BasicCredentials) -> bool {
    !basic.username.is_empty() && !basic.password.is_empty()
}

fn token_url(request: &TokenRequest) -> Result<String, RegistryError> {
    let realm = GoUrl::parse(request.realm).map_err(|e| RegistryError::Other(e.to_string()))?;
    let mut query = realm.query();
    query.values.insert("scope".to_owned(), request.scopes.to_vec());
    query
        .values
        .insert("service".to_owned(), vec![request.service.to_owned()]);
    Ok(GoUrl {
        raw_query: query.encode(),
        ..realm
    }
    .to_string())
}

fn redirect_policy(registry: &str, insecure: bool) -> Policy {
    let registry = registry.to_owned();
    Policy::custom(move |attempt| {
        if attempt.previous().len() >= MAX_REDIRECTS {
            return attempt.error("stopped after 10 redirects");
        }
        let target = attempt.url().to_string();
        match validate_realm(&target, &registry, insecure) {
            Ok(()) => attempt.follow(),
            Err(e) => attempt.error(format!("refusing token-server redirect to {}: {e}", quote(&target))),
        }
    })
}

fn hostname(host: &str) -> &str {
    match host.strip_prefix('[') {
        Some(bracketed) => bracketed.split(']').next().unwrap_or_default(),
        None => host.rsplit_once(':').map_or(host, |(name, _)| name),
    }
}

fn is_internal(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_unspecified()
                || v4.octets()[..3] == [224, 0, 0]
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xff0f) == 0xff02
        }
    }
}

fn quote(text: &str) -> String {
    crate::go_strconv::quote(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn challenges_parse_quoted_params_and_several_schemes() {
        let challenges = parse_header(
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/ubuntu:pull", Basic realm="r\"q""#,
        );

        assert_eq!(challenges.len(), 2);
        assert_eq!(challenges[0].params["realm"], "https://auth.docker.io/token");
        assert_eq!(challenges[0].params["service"], "registry.docker.io");
        assert_eq!(challenges[1].scheme, "Basic");
        assert_eq!(challenges[1].params["realm"], "r\"q");
    }

    #[test]
    fn realms_must_be_https_and_public_unless_they_are_the_registry() {
        assert_eq!(
            validate_realm("https://auth.docker.io/token", "index.docker.io", false),
            Ok(())
        );
        assert_eq!(validate_realm("http://10.0.0.5/token", "10.0.0.5", true), Ok(()));
        assert_eq!(
            validate_realm("http://auth/token", "r", false),
            Err("realm scheme \"http\" not allowed for a secure registry; use https".to_owned())
        );
        assert_eq!(
            validate_realm("ftp://auth/token", "r", true),
            Err("realm scheme \"ftp\" not allowed; must be https (or http for insecure registries)".to_owned())
        );
        assert_eq!(
            validate_realm("https://169.254.169.254:443/token", "r", false),
            Err("realm host \"169.254.169.254\" is a private or link-local address".to_owned())
        );
        assert_eq!(
            validate_realm("https://192.168.1.2:5000/t", "192.168.1.2:5000", false),
            Ok(())
        );
    }

    #[test]
    fn the_token_url_replaces_scope_and_always_sets_service() {
        let request = TokenRequest {
            realm: "https://auth.example/token?scope=old&x=1",
            service: "",
            scopes: &["repository:a/b:pull".to_owned()],
            registry: "r",
            insecure: false,
            credentials: None,
        };

        assert_eq!(
            token_url(&request).unwrap(),
            "https://auth.example/token?scope=repository%3Aa%2Fb%3Apull&service=&x=1"
        );
    }
}
