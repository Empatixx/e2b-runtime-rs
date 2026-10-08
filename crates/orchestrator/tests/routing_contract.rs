use std::collections::HashMap;
use std::time::Duration;

use fred::prelude::{ClientLike, Config, KeysInterface};
use fred::types::Builder;
use orchestrator::routing::publisher::{RouteStore, RoutingRecord};
use orchestrator::routing::redis::{Redis, RedisSettings};

fn redis_url() -> Option<String> {
    let url = std::env::var("E2B_TEST_REDIS_URL").ok();
    assert!(
        url.is_some() || std::env::var("CI").is_err(),
        "routing contract needs E2B_TEST_REDIS_URL in CI"
    );
    url
}

#[tokio::test]
async fn routing_records_are_stored_with_ttl_and_deleted_only_by_their_execution() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: E2B_TEST_REDIS_URL is not set");
        return;
    };
    let settings = RedisSettings::from_vars(&HashMap::from([("REDIS_URL".to_owned(), url.clone())]))
        .unwrap()
        .unwrap();
    let routes = Redis::connect(&settings).await.unwrap().routing_catalog();
    let sandbox_id = format!("contract-{}", uuid::Uuid::new_v4());
    let record = RoutingRecord {
        orchestrator_id: "orch".to_owned(),
        orchestrator_ip: "10.0.0.5".to_owned(),
        execution_id: "exec-1".to_owned(),
        sandbox_started_at: "2026-10-08T00:00:00Z".to_owned(),
        sandbox_max_length_in_hours: 1,
    };

    routes
        .store(&sandbox_id, &record, Some(Duration::from_secs(3600)))
        .await
        .unwrap();
    let client = Builder::from_config(Config::from_url(&url).unwrap()).build().unwrap();
    client.init().await.unwrap();
    let key = format!("sandbox:routing:{sandbox_id}");
    let stored: String = client.get(&key).await.unwrap();
    let ttl: i64 = client.ttl(&key).await.unwrap();

    let mismatch = routes.delete(&sandbox_id, "exec-2").await.unwrap();
    let deleted = routes.delete(&sandbox_id, "exec-1").await.unwrap();
    let absent = routes.delete(&sandbox_id, "exec-1").await.unwrap();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&stored).unwrap()["execution_id"],
        "exec-1"
    );
    assert!(ttl > 3500 && ttl <= 3600, "ttl {ttl}");
    assert_eq!((mismatch, deleted, absent), (2, 1, 0));
}
