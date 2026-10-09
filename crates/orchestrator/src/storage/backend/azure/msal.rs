use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use super::aad::Grant;
use super::token_source::{BearerToken, SourceError, TokenRequest};

const VALIDITY_MARGIN: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    pub scope: String,
    pub tenant: String,
    pub cae: bool,
}

#[derive(Default)]
pub struct TokenCache {
    access: HashMap<CacheKey, BearerToken>,
    refresh: HashMap<bool, String>,
}

impl CacheKey {
    pub fn of(request: &TokenRequest<'_>, tenant: &str) -> Self {
        Self {
            scope: request.scope.to_owned(),
            tenant: tenant.to_owned(),
            cae: request.cae,
        }
    }
}

impl BearerToken {
    pub fn refresh_due(&self) -> bool {
        self.refresh_on.is_some_and(|refresh_on| SystemTime::now() > refresh_on)
    }
}

impl TokenCache {
    pub fn valid(&self, key: &CacheKey) -> Option<BearerToken> {
        self.access
            .get(key)
            .filter(|token| token.expires_at >= SystemTime::now() + VALIDITY_MARGIN)
            .cloned()
    }

    pub fn refresh_token(&self, cae: bool) -> Option<String> {
        self.refresh.get(&cae).cloned()
    }

    pub fn store(&mut self, key: CacheKey, grant: &Grant) {
        if let Some(refresh_token) = &grant.refresh_token {
            self.refresh.insert(key.cae, refresh_token.clone());
        }
        self.access.insert(key, grant.token.clone());
    }
}

pub async fn confidential_token<F, Fut>(
    cache: &mut TokenCache,
    key: CacheKey,
    acquire: F,
) -> Result<BearerToken, SourceError>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<BearerToken, SourceError>>,
{
    let cached = cache.valid(&key);
    if let Some(token) = cached.clone().filter(|token| !token.refresh_due()) {
        return Ok(token);
    }
    match (acquire().await, cached) {
        (Ok(token), _) => {
            cache.store(key, &Grant::without_refresh_token(token.clone()));
            Ok(token)
        }
        (Err(_), Some(token)) => Ok(token),
        (Err(error), None) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::time::UNIX_EPOCH;

    use super::*;

    fn key() -> CacheKey {
        CacheKey::of(&TokenRequest::new("s"), "t")
    }

    fn grant(expires_in: u64, refresh_in: Option<u64>, refresh_token: Option<&str>) -> Grant {
        let now = SystemTime::now();
        Grant {
            token: BearerToken {
                secret: "a".to_owned(),
                expires_at: now + Duration::from_secs(expires_in),
                refresh_on: refresh_in.map(|seconds| now + Duration::from_secs(seconds)),
            },
            refresh_token: refresh_token.map(str::to_owned),
        }
    }

    #[test]
    fn tokens_within_five_minutes_of_expiry_are_not_served() {
        let mut cache = TokenCache::default();

        cache.store(key(), &grant(299, None, None));
        assert!(cache.valid(&key()).is_none());
        cache.store(key(), &grant(3600, None, None));
        assert!(cache.valid(&key()).is_some());
    }

    #[test]
    fn refresh_tokens_are_kept_per_cae_partition_and_survive_responses_without_one() {
        let mut cache = TokenCache::default();

        cache.store(key(), &grant(3600, None, Some("r1")));
        cache.store(key(), &grant(3600, None, None));

        assert_eq!(cache.refresh_token(false).as_deref(), Some("r1"));
        assert_eq!(cache.refresh_token(true), None);
    }

    #[test]
    fn a_token_is_due_for_refresh_once_its_refresh_instant_has_passed() {
        let past = BearerToken {
            secret: String::new(),
            expires_at: UNIX_EPOCH + Duration::from_secs(4_000_000_000),
            refresh_on: Some(SystemTime::now() - Duration::from_secs(1)),
        };

        assert!(past.refresh_due());
        assert!(!grant(3600, Some(1800), None).token.refresh_due());
        assert!(!grant(3600, None, None).token.refresh_due());
    }
}
