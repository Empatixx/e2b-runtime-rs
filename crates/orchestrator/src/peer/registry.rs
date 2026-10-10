use std::time::Duration;

use futures::future::BoxFuture;

const KEY_PREFIX: &str = "peer:";

pub fn redis_key(build_id: &str) -> String {
    format!("{KEY_PREFIX}{build_id}")
}

pub trait PeerRegistry: Send + Sync {
    fn register(&self, build_id: &str, ttl: Duration) -> BoxFuture<'static, Result<(), String>>;
    fn lookup(&self, build_id: &str) -> BoxFuture<'static, Result<Option<String>, String>>;
    fn unregister(&self, build_id: &str) -> BoxFuture<'static, Result<(), String>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_redis_key_is_peer_colon_build_id() {
        assert_eq!(redis_key("build-1"), "peer:build-1");
    }
}
