use std::collections::HashMap;
use std::time::Duration;

use orchestrator::events::{EventSandbox, EventSink, STREAM, SandboxEvent, delivery_key};
use orchestrator::routing::publisher::{RouteStore, RoutingRecord};
use orchestrator::routing::redis::{Redis, RedisSettings};
use redissun::{Client, Object, StreamId, StringCodec};

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
    let client = Client::builder().url(&url).codec(StringCodec).build().await.unwrap();
    let route = client.bucket::<String>(format!("sandbox:routing:{sandbox_id}"));
    let stored = route.get().await.unwrap().unwrap();
    let ttl = route.ttl().await.unwrap().unwrap().as_secs();

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

#[tokio::test]
async fn events_are_streamed_as_a_plain_payload_field_only_for_teams_with_a_webhook() {
    let Some(url) = redis_url() else {
        eprintln!("skipping: E2B_TEST_REDIS_URL is not set");
        return;
    };
    let settings = RedisSettings::from_vars(&HashMap::from([("REDIS_URL".to_owned(), url.clone())]))
        .unwrap()
        .unwrap();
    let events = Redis::connect(&settings).await.unwrap().event_stream();
    let client = Client::builder().url(&url).codec(StringCodec).build().await.unwrap();
    let stream = client.stream::<String, String>(STREAM);
    let sandbox = EventSandbox {
        sandbox_id: format!("contract-{}", uuid::Uuid::new_v4()),
        execution_id: "exec-1".to_owned(),
        base_template_id: "base".to_owned(),
        build_id: "build".to_owned(),
        team_id: uuid::Uuid::new_v4().to_string(),
        events_ttl_days: 0,
        metadata: Default::default(),
    };
    let event = SandboxEvent::new("sandbox.lifecycle.created", &sandbox);
    let entries_of_sandbox = || async {
        stream
            .range(&StreamId::min(), &StreamId::max(), None)
            .await
            .unwrap()
            .into_iter()
            .filter(|entry| {
                entry
                    .fields
                    .iter()
                    .any(|(_, value)| value.contains(&sandbox.sandbox_id))
            })
            .map(|entry| entry.fields)
            .collect::<Vec<_>>()
    };

    events.publish(&event).await.unwrap();
    let without_webhook = entries_of_sandbox().await;
    client
        .bucket::<String>(delivery_key(event.sandbox_team_id))
        .set("1")
        .await
        .unwrap();
    events.publish(&event).await.unwrap();

    assert!(without_webhook.is_empty());
    assert_eq!(
        entries_of_sandbox().await,
        vec![vec![("payload".to_owned(), serde_json::to_string(&event).unwrap())]]
    );
}
