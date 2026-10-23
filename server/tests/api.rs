use std::sync::Arc;

use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use axum::Router;
use bread_tracker_server::config::Config;
use bread_tracker_server::push::{self, MockSender};
use bread_tracker_server::{api, auth, db, store, AppState};
use chrono::{Duration, TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const C1: &str = "11111111-1111-4111-8111-111111111111";
const H1: &str = "22222222-2222-4222-8222-222222222222";

struct Harness {
    app: Router,
    state: AppState,
    mock: Arc<MockSender>,
}

async fn harness() -> Harness {
    let db = db::connect_memory().await.unwrap();
    let mock = Arc::new(MockSender::default());
    let state = AppState::new(db, Config::from_env(), mock.clone());
    Harness {
        app: api::router(state.clone()),
        state,
        mock,
    }
}

async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Like `call`, but as a request from a given peer address, optionally with X-Forwarded-For.
async fn call_from(
    app: &Router,
    peer: &str,
    xff: Option<&str>,
    uri: &str,
    body: Value,
) -> StatusCode {
    let mut req = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(x) = xff {
        req = req.header("x-forwarded-for", x);
    }
    let mut req = req.body(Body::from(body.to_string())).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(req).await.unwrap().status()
}

async fn paired(h: &Harness) -> String {
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let (s, v) = call(
        &h.app,
        "POST",
        "/pair",
        None,
        Some(json!({"code": code, "device_name": "Pixel"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["token"].as_str().unwrap().to_string()
}

fn item(id: &str, kind: &str, name: &str, mask: i64, remind: Option<&str>, updated: i64) -> Value {
    json!({"id": id, "kind": kind, "name": name, "weekdays": mask, "remind_time": remind, "points": 10,
           "due_date": null, "sort": 0, "active": true, "updated_at": updated, "deleted_at": null})
}

fn comp(item_id: &str, date: &str, updated: i64, deleted: Option<i64>) -> Value {
    json!({"item_id": item_id, "date": date, "completed_at": updated, "updated_at": updated, "deleted_at": deleted})
}

#[tokio::test]
async fn unauthenticated_requests_are_rejected() {
    let h = harness().await;
    for (m, p) in [
        ("GET", "/sync"),
        ("POST", "/sync"),
        ("GET", "/stats"),
        ("GET", "/settings"),
        ("PUT", "/device"),
    ] {
        let (s, _) = call(&h.app, m, p, None, Some(json!({}))).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {p}");
    }
    let (s, _) = call(&h.app, "GET", "/sync", Some("not-a-real-token"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&h.app, "GET", "/healthz", None, None).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn pairing_code_is_single_use_and_rate_limited() {
    let h = harness().await;
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let (s, _) = call(
        &h.app,
        "POST",
        "/pair",
        None,
        Some(json!({"code": code.to_lowercase()})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "code is case and dash insensitive");
    let (s, _) = call(&h.app, "POST", "/pair", None, Some(json!({"code": code}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "second use fails");

    let h2 = harness().await;
    let mut last = StatusCode::OK;
    for _ in 0..7 {
        last = call(
            &h2.app,
            "POST",
            "/pair",
            None,
            Some(json!({"code": "AAAA-BBBB"})),
        )
        .await
        .0;
    }
    assert_eq!(last, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn sync_round_trip_last_write_wins_and_tombstones() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();

    let (s, v) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({
        "since": 0,
        "items": [item(C1, "chore", "Dishes", 127, None, now), item(H1, "habit", "Read", 127, None, now)],
        "completions": [comp(C1, "2026-10-19", now, None)]
    }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    let cursor = v["cursor"].as_i64().unwrap();
    assert!(cursor >= 3);

    // Older rename loses, newer rename wins.
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "Stale", 127, None, now - 1000)]})),
    )
    .await;
    let (_, v) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"since": cursor, "items": [item(C1, "chore", "Dishes and dinner", 127, None, now + 1000)]}))).await;
    assert_eq!(
        v["items"].as_array().unwrap().len(),
        1,
        "only the changed row comes back"
    );
    assert_eq!(v["items"][0]["name"], "Dishes and dinner");

    // Un-tick is a tombstone that syncs; an older re-tick does not resurrect it.
    let (_, v) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"completions": [comp(C1, "2026-10-19", now + 5000, Some(now + 5000))]})),
    )
    .await;
    let c = v["completions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["date"] == "2026-10-19")
        .cloned()
        .unwrap();
    assert!(!c["deleted_at"].is_null());
    let (_, v) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"completions": [comp(C1, "2026-10-19", now + 2000, None)]})),
    )
    .await;
    let c = v["completions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["date"] == "2026-10-19")
        .cloned()
        .unwrap();
    assert!(!c["deleted_at"].is_null(), "stale re-tick must lose");

    // Pull-only returns nothing new at the head.
    let (_, v) = call(
        &h.app,
        "GET",
        &format!("/sync?since={}", v["cursor"]),
        Some(&tok),
        None,
    )
    .await;
    assert!(
        v["items"].as_array().unwrap().is_empty()
            && v["completions"].as_array().unwrap().is_empty()
    );
}

#[tokio::test]
async fn sync_validates_rows() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    for bad in [
        item("not-a-uuid", "chore", "x", 127, None, now),
        item(C1, "quest", "x", 127, None, now),
        item(C1, "chore", "  ", 127, None, now),
        item(C1, "chore", "x", 128, None, now),
        item(C1, "chore", "x", 127, Some("25:99"), now),
    ] {
        let (s, _) = call(
            &h.app,
            "POST",
            "/sync",
            Some(&tok),
            Some(json!({"items": [bad]})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}

#[tokio::test]
async fn stats_endpoint_uses_shared_rules() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({
        "items": [item(C1, "chore", "Dishes", 127, None, now), item(H1, "habit", "Read", 127, None, now)],
        "completions": [comp(C1, "2026-10-19", now, None), comp(H1, "2026-10-19", now, None)]
    }))).await;
    let (s, v) = call(&h.app, "GET", "/stats?date=2026-10-19", Some(&tok), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["xp"], 10 + 10 + 50);
    assert_eq!(v["level"], 1);
    assert_eq!(v["streaks"][H1], 1);
}

#[tokio::test]
async fn settings_round_trip_and_validation() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (_, v) = call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(v["timezone"], "Australia/Perth");
    let (s, v) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "22:00", "quiet_end": "07:00", "reminders_enabled": false})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["quiet_start"], "22:00");
    assert_eq!(v["reminders_enabled"], false);
    let (s, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"timezone": "Mars/Olympus"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// Monday 2026-10-19, 17:15 Perth = 09:15 UTC.
fn perth(h: u32, m: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, h, m, 0).unwrap() - Duration::hours(8)
}

async fn seed_reminder_world(h: &Harness, tok: &str) {
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(tok),
        Some(json!({"items": [
            item(C1, "chore", "Recycling", 1, Some("17:15"), now),          // Mondays only
            item(H1, "habit", "Read", 127, Some("20:00"), now),
        ]})),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(tok),
        Some(json!({"fcm_token": "fcm-token-1"})),
    )
    .await;
}

#[tokio::test]
async fn scheduler_sends_once_skips_done_and_not_yet_due() {
    let h = harness().await;
    let tok = paired(&h).await;
    seed_reminder_world(&h, &tok).await;

    assert_eq!(
        push::tick(&h.state, perth(17, 0)).await.unwrap(),
        0,
        "before the time"
    );
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
    {
        let sent = h.mock.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "Recycling");
        assert_eq!(sent[0].token, "fcm-token-1");
        assert_eq!(sent[0].data["item_id"], C1);
        assert_eq!(sent[0].data["date"], "2026-10-19");
    }
    assert_eq!(
        push::tick(&h.state, perth(17, 20)).await.unwrap(),
        0,
        "deduped"
    );

    // The habit is done before its time, so it never fires.
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"completions": [comp(H1, "2026-10-19", now, None)]})),
    )
    .await;
    assert_eq!(
        push::tick(&h.state, perth(20, 1)).await.unwrap(),
        0,
        "already completed"
    );
    assert_eq!(h.mock.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn scheduler_honours_weekday_grace_quiet_hours_and_disable() {
    let h = harness().await;
    let tok = paired(&h).await;
    seed_reminder_world(&h, &tok).await;

    // Tuesday: the Monday-only chore must not fire.
    let tue = perth(17, 16) + Duration::days(1);
    assert_eq!(push::tick(&h.state, tue).await.unwrap(), 0);

    // Server was down for hours: the 17:15 chore is stale and dropped, not sent late.
    // (The 20:00 habit is still inside its grace window, so only it goes out.)
    assert_eq!(
        push::tick(&h.state, perth(17, 15) + Duration::minutes(361))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        h.mock
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.title.clone())
            .collect::<Vec<_>>(),
        vec!["Read"]
    );

    // Quiet hours suppress, and the reminder is still sendable once they end (within grace).
    call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "17:00", "quiet_end": "18:00"})),
    )
    .await;
    assert_eq!(push::tick(&h.state, perth(17, 30)).await.unwrap(), 0);
    assert_eq!(push::tick(&h.state, perth(18, 5)).await.unwrap(), 1);

    // Master switch.
    call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"reminders_enabled": false})),
    )
    .await;
    assert_eq!(push::tick(&h.state, perth(20, 1)).await.unwrap(), 0);
}

#[tokio::test]
async fn dead_tokens_are_pruned_and_unsent_reminders_wait_for_a_device() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "Recycling", 1, Some("17:15"), now)]})),
    )
    .await;

    // No device token yet: nothing is sent and nothing is marked sent.
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 0);

    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "dead-token"})),
    )
    .await;
    h.mock
        .dead_tokens
        .lock()
        .unwrap()
        .insert("dead-token".into());
    assert_eq!(push::tick(&h.state, perth(17, 17)).await.unwrap(), 0);
    let stored: Option<String> = sqlx::query_scalar("SELECT fcm_token FROM devices")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(stored.is_none(), "dead token cleared");

    // A fresh token registered inside the grace window still gets the reminder.
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "good-token"})),
    )
    .await;
    assert_eq!(push::tick(&h.state, perth(17, 40)).await.unwrap(), 1);
}

#[tokio::test]
async fn push_test_route_needs_a_registered_token() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (s, _) = call(&h.app, "POST", "/push/test", Some(&tok), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "t1"})),
    )
    .await;
    let (s, v) = call(&h.app, "POST", "/push/test", Some(&tok), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(h.mock.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn failed_pair_attempts_from_one_source_do_not_block_another() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for _ in 0..5 {
        assert_eq!(
            call_from(&h.app, "203.0.113.9:5000", None, "/pair", bad.clone()).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call_from(&h.app, "203.0.113.9:5001", None, "/pair", bad.clone()).await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // A different source can still pair with a valid code.
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let ok = call_from(
        &h.app,
        "198.51.100.7:4000",
        None,
        "/pair",
        json!({"code": code}),
    )
    .await;
    assert_eq!(ok, StatusCode::OK);

    // A valid code remains usable even from a source with a full failure budget.
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let blocked = call_from(
        &h.app,
        "203.0.113.9:5002",
        None,
        "/pair",
        json!({"code": code}),
    )
    .await;
    assert_eq!(blocked, StatusCode::OK);
}

#[tokio::test]
async fn successful_pairings_do_not_count_against_the_limit() {
    let h = harness().await;
    for _ in 0..8 {
        let code = auth::create_pairing_code(&h.state.db).await.unwrap();
        let s = call_from(
            &h.app,
            "203.0.113.9:5000",
            None,
            "/pair",
            json!({"code": code}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    }
}

#[tokio::test]
async fn a_flood_of_distinct_sources_does_not_block_a_valid_pairing_from_another() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    // Many distinct addresses each fail once: none is blocked, and the limiter must stay bounded.
    for n in 1..auth::MAX_SOURCES as u32 {
        let src = format!("198.51.{}.{}:4000", (n / 256) % 256, n % 256);
        let status = call_from(&h.app, &src, None, "/pair", bad.clone()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "source {n}");
    }
    let overflow = "192.0.2.66:4000";
    for _ in 0..auth::MAX_FAILURES {
        assert_eq!(
            call_from(&h.app, overflow, None, "/pair", bad.clone()).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call_from(&h.app, overflow, None, "/pair", bad.clone()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(&h.app, overflow, None, "/pair", json!({"code": code})).await,
        StatusCode::OK
    );
    // A brand-new source with a valid code still pairs.
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let ok = call_from(
        &h.app,
        "192.0.2.77:4000",
        None,
        "/pair",
        json!({"code": code}),
    )
    .await;
    assert_eq!(ok, StatusCode::OK);
}

#[tokio::test]
async fn overflow_pair_guesses_are_bounded_without_rejecting_valid_code() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for n in 1..=auth::MAX_SOURCES as u32 {
        let src = format!("198.51.{}.{}:4000", (n / 256) % 256, n % 256);
        assert_eq!(
            call_from(&h.app, &src, None, "/pair", bad.clone()).await,
            StatusCode::UNAUTHORIZED
        );
    }
    let overflow = "192.0.2.220:4000";
    for _ in 0..100 {
        assert_eq!(
            call_from(&h.app, overflow, None, "/pair", bad.clone()).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(
            &h.app,
            "192.0.2.221:4000",
            None,
            "/pair",
            json!({"code": code})
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn valid_pairing_at_full_source_capacity_is_not_rejected_by_storage_limit() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for n in 1..=auth::MAX_SOURCES as u32 {
        let src = format!("198.51.{}.{}:4000", (n / 256) % 256, n % 256);
        call_from(&h.app, &src, None, "/pair", bad.clone()).await;
    }
    for n in 0..5 {
        let src = format!("192.0.2.{}:4000", 10 + n);
        assert_eq!(
            call_from(&h.app, &src, None, "/pair", bad.clone()).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        assert_eq!(
            call_from(&h.app, &src, None, "/pair", bad.clone()).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        auth::pair_failure_count(&h.state.db).await.unwrap(),
        auth::MAX_SOURCES
    );
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(
            &h.app,
            "192.0.2.99:4000",
            None,
            "/pair",
            json!({"code": code})
        )
        .await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn a_valid_code_from_a_source_with_five_previous_failures_is_still_redeemed() {
    let h = harness().await;
    let source = "192.0.2.177:4000";
    let bad = json!({"code": "AAAA-BBBB"});
    for _ in 0..5 {
        assert_eq!(
            call_from(&h.app, source, None, "/pair", bad.clone()).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call_from(&h.app, source, None, "/pair", bad).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(&h.app, source, None, "/pair", json!({"code": code})).await,
        StatusCode::OK
    );
}

#[tokio::test]
async fn expired_source_rows_are_reclaimed_before_capacity_denies_new_sources() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for n in 1..=auth::MAX_SOURCES as u32 {
        let src = format!("198.51.{}.{}:4000", (n / 256) % 256, n % 256);
        call_from(&h.app, &src, None, "/pair", bad.clone()).await;
    }
    assert_eq!(
        auth::pair_failure_count(&h.state.db).await.unwrap(),
        auth::MAX_SOURCES
    );
    sqlx::query("UPDATE pair_rate_limits SET updated_at = 0, window_started_at = 0")
        .execute(&h.state.db)
        .await
        .unwrap();
    assert_eq!(
        call_from(&h.app, "192.0.2.201:4000", None, "/pair", bad).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(auth::pair_failure_count(&h.state.db).await.unwrap(), 1);
}

#[tokio::test]
async fn saturated_live_source_table_preserves_existing_budgets_and_allows_valid_pairing() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for n in 1..=auth::MAX_SOURCES as u32 {
        let src = format!("198.51.{}.{}:4000", (n / 256) % 256, n % 256);
        call_from(&h.app, &src, None, "/pair", bad.clone()).await;
    }
    let source = "192.0.2.190:4000";
    for _ in 0..5 {
        assert_eq!(
            call_from(&h.app, source, None, "/pair", bad.clone()).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    assert_eq!(
        call_from(&h.app, source, None, "/pair", bad.clone()).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    assert_eq!(
        call_from(&h.app, source, None, "/pair", bad).await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(
            &h.app,
            "192.0.2.222:4000",
            None,
            "/pair",
            json!({"code": code})
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        auth::pair_failure_count(&h.state.db).await.unwrap(),
        auth::MAX_SOURCES
    );
}

#[tokio::test]
async fn saturated_untracked_source_cannot_guess_repeatedly_and_valid_code_is_not_consumed() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for n in 1..=auth::MAX_SOURCES as u32 {
        let src = format!("198.51.{}.{}:4000", (n / 256) % 256, n % 256);
        call_from(&h.app, &src, None, "/pair", bad.clone()).await;
    }
    let source = "192.0.2.250:4000";
    for _ in 0..100 {
        assert_eq!(
            call_from(&h.app, source, None, "/pair", bad.clone()).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(&h.app, source, None, "/pair", json!({"code": code})).await,
        StatusCode::OK
    );
    assert_eq!(
        auth::pair_failure_count(&h.state.db).await.unwrap(),
        auth::MAX_SOURCES
    );
}

#[tokio::test]
async fn reused_or_expired_pairing_codes_are_counted_as_failures_not_successes() {
    let h = harness().await;
    let source = "192.0.2.230:4000";
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    assert_eq!(
        call_from(&h.app, source, None, "/pair", json!({"code": code})).await,
        StatusCode::OK
    );
    for _ in 0..auth::MAX_FAILURES {
        assert_eq!(
            call_from(&h.app, source, None, "/pair", json!({"code": code})).await,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        call_from(&h.app, source, None, "/pair", json!({"code": code})).await,
        StatusCode::TOO_MANY_REQUESTS
    );
}

#[tokio::test]
async fn concurrent_pair_guesses_from_one_source_have_one_atomic_budget() {
    let h = harness().await;
    let app = h.app.clone();
    let mut requests = Vec::new();
    for _ in 0..20 {
        let app = app.clone();
        requests.push(tokio::spawn(async move {
            call_from(
                &app,
                "192.0.2.240:4000",
                None,
                "/pair",
                json!({"code": "AAAA-BBBB"}),
            )
            .await
        }));
    }
    let mut unauthorized = 0;
    let mut throttled = 0;
    for request in requests {
        match request.await.unwrap() {
            StatusCode::UNAUTHORIZED => unauthorized += 1,
            StatusCode::TOO_MANY_REQUESTS => throttled += 1,
            other => panic!("unexpected concurrent response: {other}"),
        }
    }
    assert_eq!(unauthorized, auth::MAX_FAILURES as usize);
    assert_eq!(throttled, 20 - unauthorized);
    assert_eq!(auth::pair_failure_count(&h.state.db).await.unwrap(), 1);
}

#[tokio::test]
async fn forwarded_for_is_only_believed_from_a_trusted_proxy() {
    use bread_tracker_server::auth::TrustedProxy;
    let bad = json!({"code": "AAAA-BBBB"});

    // Untrusted peer: it cannot dodge the limit by rotating X-Forwarded-For.
    let h = harness().await;
    for i in 0..5 {
        let xff = format!("10.9.9.{i}");
        call_from(&h.app, "203.0.113.9:5000", Some(&xff), "/pair", bad.clone()).await;
    }
    assert_eq!(
        call_from(
            &h.app,
            "203.0.113.9:5000",
            Some("10.9.9.99"),
            "/pair",
            bad.clone()
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS
    );

    // Trusted proxy (100.64.0.0/10): the client in X-Forwarded-For is the key.
    let db = db::connect_memory().await.unwrap();
    let mut config = Config::from_env();
    config.trusted_proxies = vec![TrustedProxy::parse("100.64.0.0/10").unwrap()];
    let state = AppState::new(db, config, Arc::new(MockSender::default()));
    let app = api::router(state.clone());
    for _ in 0..5 {
        call_from(
            &app,
            "100.100.1.1:4000",
            Some("203.0.113.50"),
            "/pair",
            bad.clone(),
        )
        .await;
    }
    assert_eq!(
        call_from(
            &app,
            "100.100.1.1:4000",
            Some("203.0.113.50"),
            "/pair",
            bad.clone()
        )
        .await,
        StatusCode::TOO_MANY_REQUESTS
    );
    let code = auth::create_pairing_code(&state.db).await.unwrap();
    let other = call_from(
        &app,
        "100.100.1.1:4000",
        Some("203.0.113.51"),
        "/pair",
        json!({"code": code}),
    )
    .await;
    assert_eq!(
        other,
        StatusCode::OK,
        "the real user behind the same proxy is unaffected"
    );
}

#[test]
fn client_ip_resolution() {
    use bread_tracker_server::auth::{client_ip, TrustedProxy};
    let trusted = [
        TrustedProxy::parse("10.0.0.0/8").unwrap(),
        TrustedProxy::parse("::1").unwrap(),
    ];
    let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
    assert_eq!(
        client_ip(ip("1.2.3.4"), Some("9.9.9.9"), &trusted),
        ip("1.2.3.4"),
        "untrusted peer"
    );
    assert_eq!(
        client_ip(ip("10.0.0.2"), None, &trusted),
        ip("10.0.0.2"),
        "no header"
    );
    assert_eq!(
        client_ip(ip("10.0.0.2"), Some("5.5.5.5"), &trusted),
        ip("5.5.5.5")
    );
    assert_eq!(
        client_ip(ip("10.0.0.2"), Some("6.6.6.6, 5.5.5.5, 10.0.0.9"), &trusted),
        ip("5.5.5.5"),
        "spoofed left entry ignored"
    );
    assert_eq!(
        client_ip(ip("10.0.0.2"), Some("garbage"), &trusted),
        ip("10.0.0.2")
    );
    assert!(TrustedProxy::parse("10.0.0.0/33").is_none());
    assert!(TrustedProxy::parse("nope").is_none());
}
