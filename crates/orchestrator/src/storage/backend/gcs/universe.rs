const DEFAULT_UNIVERSE_DOMAIN: &str = "googleapis.com";
const UNIVERSE_DOMAIN_ENV: &str = "GOOGLE_CLOUD_UNIVERSE_DOMAIN";

pub fn client_universe_domain() -> String {
    std::env::var(UNIVERSE_DOMAIN_ENV)
        .ok()
        .filter(|domain| !domain.is_empty())
        .unwrap_or_else(|| DEFAULT_UNIVERSE_DOMAIN.to_owned())
}

pub fn validate_universe_domain(client: &str, credentials: &str) -> Result<(), String> {
    let credentials = match credentials.is_empty() {
        true => DEFAULT_UNIVERSE_DOMAIN,
        false => credentials,
    };
    match client == credentials {
        true => Ok(()),
        false => Err(format!(
            "the configured universe domain ({client:?}) does not match the universe domain found in the credentials ({credentials:?}). If you haven't configured the universe domain explicitly, \"googleapis.com\" is the default"
        )),
    }
}

pub fn storage_base_url(client_universe: &str) -> Option<String> {
    Some(client_universe)
        .filter(|domain| *domain != DEFAULT_UNIVERSE_DOMAIN)
        .map(|domain| format!("https://storage.{domain}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mismatched_universe_domains_are_refused_with_the_sdk_text() {
        assert_eq!(validate_universe_domain("googleapis.com", ""), Ok(()));
        assert_eq!(validate_universe_domain("example.com", "example.com"), Ok(()));
        assert_eq!(
            validate_universe_domain("googleapis.com", "example.com").unwrap_err(),
            "the configured universe domain (\"googleapis.com\") does not match the universe domain found in the credentials (\"example.com\"). If you haven't configured the universe domain explicitly, \"googleapis.com\" is the default"
        );
        assert_eq!(
            validate_universe_domain("example.com", "").unwrap_err(),
            "the configured universe domain (\"example.com\") does not match the universe domain found in the credentials (\"googleapis.com\"). If you haven't configured the universe domain explicitly, \"googleapis.com\" is the default"
        );
    }

    #[test]
    fn only_a_custom_universe_moves_the_storage_endpoint() {
        assert_eq!(storage_base_url("googleapis.com"), None);
        assert_eq!(
            storage_base_url("example.com").as_deref(),
            Some("https://storage.example.com")
        );
    }
}
