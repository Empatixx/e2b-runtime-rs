mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use common::{SteppingClock, serve_state, state_with_clock};
use envd::app::AppState;
use envd::cgroups::ProcessType;
use envd::cgroups::freeze::{FreezeMode, FreezeOptions};
use envd::controllers::init::presence::{Client, Presence};
use envd::controllers::init::serve;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn state() -> Arc<AppState> {
    state_with_clock(SteppingClock::new(Duration::ZERO))
}

fn no_wait() -> FreezeOptions {
    FreezeOptions {
        max_wait: Duration::ZERO,
        mode: FreezeMode::Legacy,
        max_cgroups: 0,
    }
}

async fn freeze(state: &AppState) {
    let (_, errors) = state.freezer.freeze(no_wait()).await;
    assert!(errors.is_empty(), "{errors:?}");
}

async fn frozen(state: &AppState) -> bool {
    state.freezer.begin_spawn(ProcessType::User).await.is_err()
}

async fn post(state: &Arc<AppState>, body: Value) -> axum::response::Response {
    post_as(state, body, Arc::new(Client::default())).await
}

async fn post_as(state: &Arc<AppState>, body: Value, presence: Arc<dyn Presence>) -> axum::response::Response {
    serve(state, Body::from(body.to_string()), presence).await
}

fn header(response: &axum::response::Response, name: &str) -> Option<String> {
    response
        .headers()
        .get(name)
        .map(|value| value.to_str().unwrap().to_owned())
}

struct LeavesAfter {
    checks: AtomicUsize,
    stays_for: usize,
}

impl LeavesAfter {
    fn new(stays_for: usize) -> Arc<Self> {
        Arc::new(Self {
            checks: AtomicUsize::new(0),
            stays_for,
        })
    }
}

impl Presence for LeavesAfter {
    fn gone(&self) -> bool {
        self.checks.fetch_add(1, Ordering::SeqCst) >= self.stays_for
    }
}

mod clock_correction {
    use chrono::{DateTime, Duration as Chrono, Utc};

    use super::*;

    const DELAY: Duration = Duration::from_micros(300_900);

    fn stepping(delay: Duration) -> (Arc<AppState>, Arc<SteppingClock>) {
        let clock = SteppingClock::new(delay);
        (state_with_clock(clock.clone()), clock)
    }

    fn stamp(timestamp: DateTime<Utc>) -> Value {
        json!({ "timestamp": timestamp.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true) })
    }

    #[tokio::test]
    async fn the_clock_is_stepped_to_the_timestamp_plus_the_handler_delay() {
        let (state, clock) = stepping(DELAY);
        let timestamp = Utc::now() + Chrono::hours(1);

        let response = post(&state, stamp(timestamp)).await;

        assert_eq!(response.status(), 204);
        assert_eq!(clock.steps(), vec![timestamp + Chrono::microseconds(300_900)]);
        assert_eq!(
            header(&response, "x-envd-init-clock-correction").as_deref(),
            Some("300")
        );
    }

    #[tokio::test]
    async fn the_bands_apply_to_the_corrected_target() {
        for (name, offset, steps) in [
            ("40 ms ahead steps", Chrono::milliseconds(40), true),
            ("5.5 s behind is ignored", Chrono::milliseconds(-5500), false),
        ] {
            let (state, clock) = stepping(Duration::from_secs(1));
            let timestamp = Utc::now() + offset;

            let response = post(&state, stamp(timestamp)).await;

            assert_eq!(
                header(&response, "x-envd-init-clock-correction").as_deref(),
                Some("1000"),
                "{name}"
            );
            let expected = if steps {
                vec![timestamp + Chrono::seconds(1)]
            } else {
                vec![]
            };
            assert_eq!(clock.steps(), expected, "{name}");
        }
    }

    #[tokio::test]
    async fn newest_wins_compares_corrected_values() {
        let (state, clock) = stepping(Duration::from_millis(300));
        let timestamp = Utc::now() + Chrono::hours(1);
        post(&state, stamp(timestamp)).await;
        assert_eq!(clock.steps().len(), 1);

        let older = post(&state, stamp(timestamp - Chrono::seconds(1))).await;
        assert_eq!(header(&older, "x-envd-init-clock-correction").as_deref(), Some("300"));
        assert_eq!(clock.steps().len(), 1, "an older corrected timestamp is refused");

        let equal = post(&state, stamp(timestamp)).await;
        assert_eq!(header(&equal, "x-envd-init-clock-correction").as_deref(), Some("300"));
        assert_eq!(clock.steps().len(), 2, "an equal corrected timestamp is accepted");
    }

    #[tokio::test]
    async fn a_request_without_a_timestamp_has_nothing_to_correct() {
        let (state, clock) = stepping(Duration::from_millis(300));

        let response = post(&state, json!({})).await;

        assert_eq!(response.status(), 204);
        assert!(header(&response, "x-envd-init-clock-correction").is_none());
        assert!(clock.steps().is_empty());
    }
}

mod thaw_field {
    use super::*;

    #[tokio::test]
    async fn anything_but_skip_thaws_before_the_answer() {
        for (name, body) in [
            ("absent", json!({})),
            ("inline", json!({ "thaw": "inline" })),
            ("unknown value", json!({ "thaw": "later" })),
        ] {
            let state = state();
            freeze(&state).await;

            let response = post(&state, body).await;

            assert_eq!(response.status(), 204, "{name}");
            assert!(!frozen(&state).await, "{name}: the handler must thaw before it answers");
            assert!(header(&response, "x-envd-thaw").is_none(), "{name}");
        }
    }

    #[tokio::test]
    async fn skip_leaves_the_workload_frozen_and_says_so() {
        let state = state();
        freeze(&state).await;

        let response = post(&state, json!({ "thaw": "skip" })).await;

        assert_eq!(response.status(), 204);
        assert_eq!(header(&response, "x-envd-thaw").as_deref(), Some("skip"));
        assert!(frozen(&state).await, "a skipping /init thaws nothing");
    }

    #[tokio::test]
    async fn a_non_string_thaw_is_a_bad_request() {
        let response = post(&state(), json!({ "thaw": 5 })).await;

        assert_eq!(response.status(), 400);
    }

    #[tokio::test]
    async fn failing_after_auth_thaws_inline_and_a_skip_still_says_so() {
        let failing = json!({ "volumeMounts": [{ "nfs_target": "10.0.0.1:/v", "path": "/proc/version/nope" }] });
        for (name, thaw, thawed, header_value) in [
            ("inline thaws", None, true, None),
            ("skip thaws nothing", Some("skip"), false, Some("skip")),
        ] {
            let state = state();
            freeze(&state).await;
            let mut body = failing.clone();
            if let Some(thaw) = thaw {
                body["thaw"] = json!(thaw);
            }

            let response = post(&state, body).await;

            assert_eq!(response.status(), 400, "{name}");
            assert_eq!(frozen(&state).await, !thawed, "{name}");
            assert_eq!(header(&response, "x-envd-thaw").as_deref(), header_value, "{name}");
        }
    }
}

mod freeze_generation {
    use super::*;

    async fn headers_with_expect(addr: std::net::SocketAddr, length: usize) -> tokio::net::TcpStream {
        let mut connection = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "POST /init HTTP/1.1\r\nHost: envd\r\nContent-Type: application/json\r\nContent-Length: {length}\r\nExpect: 100-continue\r\n\r\n"
        );
        connection.write_all(request.as_bytes()).await.unwrap();
        let mut received = Vec::new();
        let mut chunk = [0u8; 256];
        while !received.ends_with(b"\r\n\r\n") {
            let read = tokio::time::timeout(Duration::from_secs(5), connection.read(&mut chunk))
                .await
                .expect("the 100 Continue never came")
                .unwrap();
            assert!(read > 0, "the connection closed before the 100 Continue");
            received.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(String::from_utf8_lossy(&received).trim(), "HTTP/1.1 100 Continue");
        connection
    }

    async fn status_line(mut connection: tokio::net::TcpStream) -> String {
        let mut response = String::new();
        let mut chunk = [0u8; 1024];
        while !response.contains("\r\n") {
            let read = tokio::time::timeout(Duration::from_secs(5), connection.read(&mut chunk))
                .await
                .expect("no response")
                .unwrap();
            assert!(read > 0, "the connection closed without a response");
            response.push_str(&String::from_utf8_lossy(&chunk[..read]));
        }
        response.lines().next().unwrap().to_owned()
    }

    #[tokio::test]
    async fn a_freeze_between_the_continue_and_the_body_withholds_the_thaw() {
        let state = state();
        let addr = serve_state(state.clone()).await;
        let mut connection = headers_with_expect(addr, 2).await;

        freeze(&state).await;
        connection.write_all(b"{}").await.unwrap();

        assert_eq!(status_line(connection).await, "HTTP/1.1 204 No Content");
        assert!(
            frozen(&state).await,
            "the freeze landed after the handler read its generation"
        );
    }

    #[tokio::test]
    async fn a_client_gone_before_the_body_thaws_nothing() {
        let state = state();
        let addr = serve_state(state.clone()).await;
        freeze(&state).await;
        let connection = headers_with_expect(addr, 2).await;

        drop(connection);
        tokio::time::sleep(Duration::from_millis(300)).await;

        assert!(frozen(&state).await, "a handler with no body never reaches the thaw");
    }

    #[tokio::test]
    async fn a_handler_a_freeze_overtook_answers_204_and_thaws_nothing() {
        let state = state();
        let reading = Arc::new(tokio::sync::Notify::new());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let body = Body::from_stream(futures::stream::once({
            let reading = reading.clone();
            async move {
                reading.notify_one();
                let _ = released.await;
                Ok::<_, std::io::Error>(Bytes::from_static(b"{}"))
            }
        }));
        let serving = tokio::spawn({
            let state = state.clone();
            async move { serve(&state, body, Arc::new(Client::default())).await }
        });
        reading.notified().await;

        freeze(&state).await;
        release.send(()).unwrap();
        let response = serving.await.unwrap();

        assert_eq!(response.status(), 204);
        assert!(
            frozen(&state).await,
            "the handler must not undo a freeze that came after it entered"
        );
    }

    #[tokio::test]
    async fn entry_after_the_freeze_thaws() {
        let state = state();
        freeze(&state).await;

        let response = post(&state, json!({})).await;

        assert_eq!(response.status(), 204);
        assert!(!frozen(&state).await);
    }

    #[tokio::test]
    async fn the_first_init_of_a_new_process_thaws() {
        let state = state();
        state.freezer.resume_frozen().await.unwrap();
        assert!(frozen(&state).await);

        let response = post(&state, json!({})).await;

        assert_eq!(response.status(), 204);
        assert!(!frozen(&state).await);
    }
}

mod unobserved {
    use super::*;

    async fn live(state: &Arc<AppState>) -> String {
        let response = post(state, json!({})).await;
        assert_eq!(response.status(), 204);
        header(&response, "x-envd-init-unobserved").expect("every 204 to a live client carries the count")
    }

    async fn gone_after_the_lock(state: &Arc<AppState>, body: Value) -> axum::response::Response {
        post_as(state, body, LeavesAfter::new(1)).await
    }

    #[tokio::test]
    async fn a_live_inline_handler_reports_zero_and_keeps_reporting_zero() {
        let state = state();

        assert_eq!(live(&state).await, "0");
        assert_eq!(live(&state).await, "0");
    }

    #[tokio::test]
    async fn a_client_that_leaves_during_the_thaw_is_counted_on_the_next_live_response() {
        let state = state();
        freeze(&state).await;

        let response = gone_after_the_lock(&state, json!({})).await;

        assert_eq!(response.status(), 204);
        assert!(
            header(&response, "x-envd-init-unobserved").is_none(),
            "the client was already gone when the response was decided"
        );
        assert_eq!(live(&state).await, "1", "the handler completed with its client gone");
        assert_eq!(live(&state).await, "0", "the live response reset the count");
    }

    #[tokio::test]
    async fn clients_that_left_accumulate_until_a_live_response() {
        let state = state();
        gone_after_the_lock(&state, json!({})).await;
        gone_after_the_lock(&state, json!({})).await;
        gone_after_the_lock(&state, json!({})).await;

        assert_eq!(
            live(&state).await,
            "3",
            "the next live response carries every unobserved handler"
        );
    }

    #[tokio::test]
    async fn a_request_already_gone_never_takes_the_lock_and_counts_nothing() {
        let state = state();
        let response = post_as(&state, json!({}), LeavesAfter::new(0)).await;
        assert_eq!(response.status(), 503);

        gone_after_the_lock(&state, json!({})).await;

        assert_eq!(
            live(&state).await,
            "1",
            "the completed handler counts; the one refused at the lock does not"
        );
    }

    #[tokio::test]
    async fn a_client_that_leaves_after_the_thaw_is_not_counted_and_resets_nothing() {
        let state = state();
        gone_after_the_lock(&state, json!({})).await;

        let response = post_as(&state, json!({}), LeavesAfter::new(2)).await;

        assert_eq!(response.status(), 204);
        assert!(header(&response, "x-envd-init-unobserved").is_none());
        assert_eq!(live(&state).await, "1");
    }

    #[tokio::test]
    async fn a_freeze_zeroes_the_count() {
        let state = state();
        gone_after_the_lock(&state, json!({})).await;

        freeze(&state).await;

        assert_eq!(live(&state).await, "0", "no count is carried across a freeze");
    }

    #[tokio::test]
    async fn a_skipping_handler_is_never_counted() {
        let state = state();
        freeze(&state).await;

        let response = gone_after_the_lock(&state, json!({ "thaw": "skip" })).await;

        assert_eq!(response.status(), 204);
        assert_eq!(live(&state).await, "0");
    }

    #[tokio::test]
    async fn a_handler_a_freeze_overtook_is_never_counted() {
        let state = state();
        let reading = Arc::new(tokio::sync::Notify::new());
        let (release, released) = tokio::sync::oneshot::channel::<()>();
        let body = Body::from_stream(futures::stream::once({
            let reading = reading.clone();
            async move {
                reading.notify_one();
                let _ = released.await;
                Ok::<_, std::io::Error>(Bytes::from_static(b"{}"))
            }
        }));
        let serving = tokio::spawn({
            let state = state.clone();
            async move { serve(&state, body, LeavesAfter::new(1)).await }
        });
        reading.notified().await;
        freeze(&state).await;
        release.send(()).unwrap();
        serving.await.unwrap();

        assert_eq!(live(&state).await, "0");
    }

    #[tokio::test]
    async fn an_unauthorized_request_neither_counts_nor_resets() {
        let state = state();
        let authorized = json!({ "accessToken": "real" });
        post(&state, authorized.clone()).await;
        gone_after_the_lock(&state, authorized.clone()).await;

        let wrong = post(&state, json!({ "accessToken": "wrong" })).await;
        assert_eq!(wrong.status(), 401);
        assert!(header(&wrong, "x-envd-init-unobserved").is_none());

        let response = post(&state, authorized).await;
        assert_eq!(
            header(&response, "x-envd-init-unobserved").as_deref(),
            Some("1"),
            "the 401 neither added to the count nor reset it"
        );
    }
}
