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
    Harness { app: api::router(state.clone()), state, mock }
}

async fn call(app: &Router, method: &str, uri: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Like `call`, but as a request from a given peer address, optionally with X-Forwarded-For.
async fn call_from(app: &Router, peer: &str, xff: Option<&str>, uri: &str, body: Value) -> StatusCode {
    let mut req = Request::builder().method("POST").uri(uri).header("content-type", "application/json");
    if let Some(x) = xff {
        req = req.header("x-forwarded-for", x);
    }
    let mut req = req.body(Body::from(body.to_string())).unwrap();
    req.extensions_mut().insert(ConnectInfo(peer.parse::<SocketAddr>().unwrap()));
    app.clone().oneshot(req).await.unwrap().status()
}

async fn paired(h: &Harness) -> String {
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let (s, v) = call(&h.app, "POST", "/pair", None, Some(json!({"code": code, "device_name": "Pixel"}))).await;
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
    for (m, p) in [("GET", "/sync"), ("POST", "/sync"), ("GET", "/stats"), ("GET", "/settings"), ("PUT", "/device")] {
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
    let (s, _) = call(&h.app, "POST", "/pair", None, Some(json!({"code": code.to_lowercase()}))).await;
    assert_eq!(s, StatusCode::OK, "code is case and dash insensitive");
    let (s, _) = call(&h.app, "POST", "/pair", None, Some(json!({"code": code}))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "second use fails");

    let h2 = harness().await;
    let mut last = StatusCode::OK;
    for _ in 0..7 {
        last = call(&h2.app, "POST", "/pair", None, Some(json!({"code": "AAAA-BBBB"}))).await.0;
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
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "Stale", 127, None, now - 1000)]}))).await;
    let (_, v) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"since": cursor, "items": [item(C1, "chore", "Dishes and dinner", 127, None, now + 1000)]}))).await;
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "only the changed row comes back");
    assert_eq!(v["items"][0]["name"], "Dishes and dinner");

    // Un-tick is a tombstone that syncs; an older re-tick does not resurrect it.
    let (_, v) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"completions": [comp(C1, "2026-10-19", now + 5000, Some(now + 5000))]}))).await;
    let c = v["completions"].as_array().unwrap().iter().find(|c| c["date"] == "2026-10-19").cloned().unwrap();
    assert!(!c["deleted_at"].is_null());
    let (_, v) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"completions": [comp(C1, "2026-10-19", now + 2000, None)]}))).await;
    let c = v["completions"].as_array().unwrap().iter().find(|c| c["date"] == "2026-10-19").cloned().unwrap();
    assert!(!c["deleted_at"].is_null(), "stale re-tick must lose");

    // Pull-only returns nothing new at the head.
    let (_, v) = call(&h.app, "GET", &format!("/sync?since={}", v["cursor"]), Some(&tok), None).await;
    assert!(v["items"].as_array().unwrap().is_empty() && v["completions"].as_array().unwrap().is_empty());
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
        let (s, _) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [bad]}))).await;
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
    assert_eq!(v["utc_offset_minutes"], 480, "Perth has no DST");
    let (s, v) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"timezone": "Asia/Kolkata"}))).await;
    assert_eq!((s, v["utc_offset_minutes"].as_i64()), (StatusCode::OK, Some(330)));
    call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"timezone": "Australia/Perth"}))).await;
    let (s, v) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "22:00", "quiet_end": "07:00", "reminders_enabled": false}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["quiet_start"], "22:00");
    assert_eq!(v["reminders_enabled"], false);
    let (s, _) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"timezone": "Mars/Olympus"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

// Monday 2026-10-19, 17:15 Perth = 09:15 UTC.
fn perth(h: u32, m: u32) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 19, h, m, 0).unwrap() - Duration::hours(8)
}

async fn seed_reminder_world(h: &Harness, tok: &str) {
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(tok), Some(json!({"items": [
        item(C1, "chore", "Recycling", 1, Some("17:15"), now),          // Mondays only
        item(H1, "habit", "Read", 127, Some("20:00"), now),
    ]}))).await;
    call(&h.app, "PUT", "/device", Some(tok), Some(json!({"fcm_token": "fcm-token-1"}))).await;
}

#[tokio::test]
async fn scheduler_sends_once_skips_done_and_not_yet_due() {
    let h = harness().await;
    let tok = paired(&h).await;
    seed_reminder_world(&h, &tok).await;

    assert_eq!(push::tick(&h.state, perth(17, 0)).await.unwrap(), 0, "before the time");
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
    {
        let sent = h.mock.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].title, "Recycling");
        assert_eq!(sent[0].token, "fcm-token-1");
        assert_eq!(sent[0].data["item_id"], C1);
        assert_eq!(sent[0].data["date"], "2026-10-19");
    }
    assert_eq!(push::tick(&h.state, perth(17, 20)).await.unwrap(), 0, "deduped");

    // The habit is done before its time, so it never fires.
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"completions": [comp(H1, "2026-10-19", now, None)]}))).await;
    assert_eq!(push::tick(&h.state, perth(20, 1)).await.unwrap(), 0, "already completed");
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
    assert_eq!(push::tick(&h.state, perth(17, 15) + Duration::minutes(361)).await.unwrap(), 1);
    assert_eq!(h.mock.sent.lock().unwrap().iter().map(|m| m.title.clone()).collect::<Vec<_>>(), vec!["Read"]);

    // Quiet hours suppress, and the reminder is still sendable once they end (within grace).
    call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "17:00", "quiet_end": "18:00"}))).await;
    assert_eq!(push::tick(&h.state, perth(17, 30)).await.unwrap(), 0);
    assert_eq!(push::tick(&h.state, perth(18, 5)).await.unwrap(), 1);

    // Master switch.
    call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"reminders_enabled": false}))).await;
    assert_eq!(push::tick(&h.state, perth(20, 1)).await.unwrap(), 0);
}

#[tokio::test]
async fn dead_tokens_are_pruned_and_unsent_reminders_wait_for_a_device() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "Recycling", 1, Some("17:15"), now)]}))).await;

    // No device token yet: nothing is sent and nothing is marked sent.
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 0);

    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "dead-token"}))).await;
    h.mock.dead_tokens.lock().unwrap().insert("dead-token".into());
    assert_eq!(push::tick(&h.state, perth(17, 17)).await.unwrap(), 0);
    let stored: Option<String> = sqlx::query_scalar("SELECT fcm_token FROM devices").fetch_one(&h.state.db).await.unwrap();
    assert!(stored.is_none(), "dead token cleared");

    // A fresh token registered inside the grace window still gets the reminder.
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "good-token"}))).await;
    assert_eq!(push::tick(&h.state, perth(17, 40)).await.unwrap(), 1);
}

#[tokio::test]
async fn push_test_route_needs_a_registered_token() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (s, _) = call(&h.app, "POST", "/push/test", Some(&tok), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "t1"}))).await;
    let (s, v) = call(&h.app, "POST", "/push/test", Some(&tok), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(h.mock.sent.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn sync_reports_a_stable_epoch_that_differs_between_databases() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (_, a) = call(&h.app, "GET", "/sync", Some(&tok), None).await;
    let epoch = a["epoch"].as_str().unwrap().to_string();
    assert!(!epoch.is_empty() && epoch != "0");
    let now = store::now_ms();
    let (_, b) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "x", 127, None, now)]}))).await;
    assert_eq!(b["epoch"], epoch, "writes do not change it");

    // Opening the same database again keeps it; a different database gets its own.
    bread_tracker_server::db::ensure_epoch(&h.state.db).await.unwrap();
    let (_, c) = call(&h.app, "GET", "/sync", Some(&tok), None).await;
    assert_eq!(c["epoch"], epoch);
    let other = harness().await;
    let tok2 = paired(&other).await;
    let (_, d) = call(&other.app, "GET", "/sync", Some(&tok2), None).await;
    assert_ne!(d["epoch"], epoch);
}

#[tokio::test]
async fn a_cursor_ahead_of_the_database_gets_everything() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "x", 127, None, now)]}))).await;
    // A phone that synced against a later state of a database that was then restored from an older copy.
    let (s, v) = call(&h.app, "GET", "/sync?since=999999", Some(&tok), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["items"].as_array().unwrap().len(), 1, "full pull instead of an empty one");
    assert!(v["cursor"].as_i64().unwrap() < 999999, "cursor tells the client the database went backwards");
}

#[tokio::test]
async fn todo_ticked_on_an_earlier_day_gets_no_reminder() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let mut todo = item(C1, "todo", "Pay rent", 127, Some("17:15"), now);
    todo["due_date"] = json!("2026-10-19");
    let mut other = item(H1, "todo", "Book dentist", 127, Some("17:15"), now);
    other["due_date"] = json!("2026-10-19");
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({
        "items": [todo, other],
        // C1 was ticked yesterday, ahead of its due date.
        "completions": [comp(C1, "2026-10-18", now, None)]
    }))).await;
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "fcm-token-1"}))).await;

    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
    let titles: Vec<String> = h.mock.sent.lock().unwrap().iter().map(|m| m.title.clone()).collect();
    assert_eq!(titles, vec!["Book dentist"], "only the untouched to-do is reminded");
}

#[tokio::test]
async fn un_ticked_todo_completion_does_not_suppress_the_reminder() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let mut todo = item(C1, "todo", "Pay rent", 127, Some("17:15"), now);
    todo["due_date"] = json!("2026-10-19");
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({
        "items": [todo],
        "completions": [comp(C1, "2026-10-18", now, Some(now))]
    }))).await;
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "fcm-token-1"}))).await;
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
}

#[tokio::test]
async fn failed_pair_attempts_from_one_source_do_not_block_another() {
    let h = harness().await;
    let bad = json!({"code": "AAAA-BBBB"});
    for _ in 0..5 {
        assert_eq!(call_from(&h.app, "203.0.113.9:5000", None, "/pair", bad.clone()).await, StatusCode::UNAUTHORIZED);
    }
    assert_eq!(call_from(&h.app, "203.0.113.9:5001", None, "/pair", bad.clone()).await, StatusCode::TOO_MANY_REQUESTS);

    // A different source can still pair with a valid code.
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let ok = call_from(&h.app, "198.51.100.7:4000", None, "/pair", json!({"code": code})).await;
    assert_eq!(ok, StatusCode::OK);

    // The blocked source is refused even with a valid code until its window passes.
    let code = auth::create_pairing_code(&h.state.db).await.unwrap();
    let blocked = call_from(&h.app, "203.0.113.9:5002", None, "/pair", json!({"code": code})).await;
    assert_eq!(blocked, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn successful_pairings_do_not_count_against_the_limit() {
    let h = harness().await;
    for _ in 0..8 {
        let code = auth::create_pairing_code(&h.state.db).await.unwrap();
        let s = call_from(&h.app, "203.0.113.9:5000", None, "/pair", json!({"code": code})).await;
        assert_eq!(s, StatusCode::OK);
    }
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
    assert_eq!(call_from(&h.app, "203.0.113.9:5000", Some("10.9.9.99"), "/pair", bad.clone()).await, StatusCode::TOO_MANY_REQUESTS);

    // Trusted proxy (100.64.0.0/10): the client in X-Forwarded-For is the key.
    let db = db::connect_memory().await.unwrap();
    let mut config = Config::from_env();
    config.trusted_proxies = vec![TrustedProxy::parse("100.64.0.0/10").unwrap()];
    let state = AppState::new(db, config, Arc::new(MockSender::default()));
    let app = api::router(state.clone());
    for _ in 0..5 {
        call_from(&app, "100.100.1.1:4000", Some("203.0.113.50"), "/pair", bad.clone()).await;
    }
    assert_eq!(call_from(&app, "100.100.1.1:4000", Some("203.0.113.50"), "/pair", bad.clone()).await, StatusCode::TOO_MANY_REQUESTS);
    let code = auth::create_pairing_code(&state.db).await.unwrap();
    let other = call_from(&app, "100.100.1.1:4000", Some("203.0.113.51"), "/pair", json!({"code": code})).await;
    assert_eq!(other, StatusCode::OK, "the real user behind the same proxy is unaffected");
}

#[test]
fn client_ip_resolution() {
    use bread_tracker_server::auth::{client_ip, TrustedProxy};
    let trusted = [TrustedProxy::parse("10.0.0.0/8").unwrap(), TrustedProxy::parse("::1").unwrap()];
    let ip = |s: &str| s.parse::<std::net::IpAddr>().unwrap();
    assert_eq!(client_ip(ip("1.2.3.4"), Some("9.9.9.9"), &trusted), ip("1.2.3.4"), "untrusted peer");
    assert_eq!(client_ip(ip("10.0.0.2"), None, &trusted), ip("10.0.0.2"), "no header");
    assert_eq!(client_ip(ip("10.0.0.2"), Some("5.5.5.5"), &trusted), ip("5.5.5.5"));
    assert_eq!(client_ip(ip("10.0.0.2"), Some("6.6.6.6, 5.5.5.5, 10.0.0.9"), &trusted), ip("5.5.5.5"), "spoofed left entry ignored");
    assert_eq!(client_ip(ip("10.0.0.2"), Some("garbage"), &trusted), ip("10.0.0.2"));
    assert!(TrustedProxy::parse("10.0.0.0/33").is_none());
    assert!(TrustedProxy::parse("nope").is_none());
}

#[tokio::test]
async fn quiet_hours_longer_than_grace_defer_the_reminder_to_when_they_end() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    // 22:30 chore every day; quiet hours 22:00-07:00 (9h, longer than the 6h grace).
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "Night", 127, Some("22:30"), now)]}))).await;
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "fcm-token-1"}))).await;
    let (s, _) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "22:00", "quiet_end": "07:00"}))).await;
    assert_eq!(s, StatusCode::OK);

    assert_eq!(push::tick(&h.state, perth(22, 31)).await.unwrap(), 0, "quiet");
    let next = |h: u32, m: u32| perth(h, m) + Duration::days(1);
    assert_eq!(push::tick(&h.state, next(4, 31)).await.unwrap(), 0, "still quiet, past the old grace cutoff");
    assert_eq!(push::tick(&h.state, next(6, 59)).await.unwrap(), 0);
    assert_eq!(push::tick(&h.state, next(7, 1)).await.unwrap(), 1, "released when quiet hours end");
    {
        let sent = h.mock.sent.lock().unwrap();
        assert_eq!(sent[0].data["date"], "2026-10-19", "keeps the date it was scheduled for");
    }
    assert_eq!(push::tick(&h.state, next(7, 2)).await.unwrap(), 0, "only once");
}

#[tokio::test]
async fn held_reminder_still_expires_after_grace_from_the_end_of_quiet_hours() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "Night", 127, Some("22:30"), now)]}))).await;
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "fcm-token-1"}))).await;
    call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "22:00", "quiet_end": "07:00"}))).await;
    // Server was down until 14:00 the next day: the held reminder is stale, not sent that late.
    assert_eq!(push::tick(&h.state, perth(14, 0) + Duration::days(1)).await.unwrap(), 0);
}

#[tokio::test]
async fn ordinary_missed_reminder_is_not_carried_past_midnight() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"items": [item(C1, "chore", "Late", 127, Some("23:00"), now)]}))).await;
    call(&h.app, "PUT", "/device", Some(&tok), Some(json!({"fcm_token": "fcm-token-1"}))).await;
    assert_eq!(push::tick(&h.state, perth(0, 30) + Duration::days(1)).await.unwrap(), 0);
}

#[tokio::test]
async fn half_set_quiet_window_is_rejected() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (s, _) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "22:00"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, v) = call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(v["quiet_start"], "", "nothing was stored");
    let (s, _) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "22:00", "quiet_end": "07:00"}))).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_end": ""}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "clearing only one end");
    let (s, v) = call(&h.app, "PUT", "/settings", Some(&tok), Some(json!({"quiet_start": "", "quiet_end": ""}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["quiet_start"], "");
}

#[tokio::test]
async fn completion_dates_far_in_the_future_or_past_are_rejected() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let far = (Utc::now() + Duration::days(30)).format("%Y-%m-%d").to_string();
    let tomorrow = (Utc::now() + Duration::days(1)).format("%Y-%m-%d").to_string();
    for bad in ["2999-01-01", far.as_str(), "1999-12-31"] {
        let (s, _) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"completions": [comp(C1, bad, now, None)]}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
    let (s, _) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"completions": [comp(C1, &tomorrow, now, None)]}))).await;
    assert_eq!(s, StatusCode::OK, "a phone a day ahead is normal");
}

#[tokio::test]
async fn completed_at_is_clamped_like_updated_at() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let future = now + 400 * 86_400_000;
    let mut c = comp(C1, "2026-10-19", now, None);
    c["completed_at"] = json!(future);
    let (_, v) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"completions": [c]}))).await;
    let got = v["completions"][0]["completed_at"].as_i64().unwrap();
    assert!(got <= now + 86_400_000 + 5_000, "completed_at {got} was not clamped");
}

#[tokio::test]
async fn last_seen_is_written_at_most_once_per_interval() {
    let h = harness().await;
    let tok = paired(&h).await;
    let seen = |h: &Harness| {
        let db = h.state.db.clone();
        async move { sqlx::query_scalar::<_, i64>("SELECT last_seen FROM devices").fetch_one(&db).await.unwrap() }
    };
    // Pretend the device was last seen two minutes ago: the next request refreshes it.
    sqlx::query("UPDATE devices SET last_seen = ?").bind(store::now_ms() - 120_000).execute(&h.state.db).await.unwrap();
    call(&h.app, "GET", "/settings", Some(&tok), None).await;
    let first = seen(&h).await;
    assert!(first > store::now_ms() - 5_000, "stale last_seen was refreshed");
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(seen(&h).await, first, "a request inside the interval does not write");
}

#[tokio::test]
async fn prune_removes_only_what_can_never_matter_again() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let day = 86_400_000;
    let old = now - 200 * day;
    let recent = now - 10 * day;
    const C2: &str = "33333333-3333-4333-8333-333333333333";
    let mut gone = item(C1, "chore", "Old gone", 127, None, old);
    gone["deleted_at"] = json!(old);
    let mut fresh_gone = item(C2, "chore", "Recently gone", 127, None, recent);
    fresh_gone["deleted_at"] = json!(recent);
    call(&h.app, "POST", "/sync", Some(&tok), Some(json!({
        "items": [gone, fresh_gone, item(H1, "habit", "Kept", 127, None, now)],
        "completions": [comp(C1, "2026-01-01", old, Some(old)), comp(C2, "2026-10-01", recent, Some(recent)), comp(H1, "2026-10-19", now, None)]
    }))).await;
    sqlx::query("INSERT INTO reminders_sent (user_id, item_id, date, sent_at) VALUES (1, ?, '2020-01-01', 0), (1, ?, ?, 0)")
        .bind(H1).bind(H1).bind(Utc::now().format("%Y-%m-%d").to_string())
        .execute(&h.state.db).await.unwrap();
    sqlx::query("INSERT INTO pairing_codes (code_hash, user_id, expires_at) VALUES ('expired', 1, ?), ('live', 1, ?)")
        .bind(now - 1000).bind(now + 600_000)
        .execute(&h.state.db).await.unwrap();

    let p = store::prune(&h.state.db, now, 180).await.unwrap();
    assert_eq!((p.reminders, p.pairing_codes, p.items, p.completions), (1, 1, 1, 1), "{p:?}");

    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM items ORDER BY id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(ids, vec![H1.to_string(), C2.to_string()], "old tombstone gone, recent tombstone and live item kept");
    let comps: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completions").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(comps, 2);
    let codes: Vec<String> = sqlx::query_scalar("SELECT code_hash FROM pairing_codes").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(codes, vec!["live".to_string()]);
    let again = store::prune(&h.state.db, now, 180).await.unwrap();
    assert_eq!(again, store::Pruned::default(), "idempotent");
}

#[tokio::test]
async fn revoked_device_loses_access_immediately() {
    use bread_tracker_server::devices::{self, Revoked};
    let h = harness().await;
    let phone = paired(&h).await;
    let tablet = paired(&h).await;
    let list = devices::list(&h.state.db).await.unwrap();
    assert_eq!(list.len(), 2);

    let (s, _) = call(&h.app, "GET", "/sync", Some(&phone), None).await;
    assert_eq!(s, StatusCode::OK);
    // Too short a prefix and unknown ids do nothing.
    assert_eq!(devices::revoke(&h.state.db, "ab").await.unwrap(), Revoked::NotFound);
    assert_eq!(devices::revoke(&h.state.db, "zzzzzzzz").await.unwrap(), Revoked::NotFound);
    assert_eq!(devices::list(&h.state.db).await.unwrap().len(), 2);

    // Revoke the first device by a unique prefix of its id.
    let victim = &list[0];
    let revoked = devices::revoke(&h.state.db, &victim.id[..8]).await.unwrap();
    assert_eq!(revoked, Revoked::Removed(victim.name.clone()));
    let (s, _) = call(&h.app, "GET", "/sync", Some(&phone), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "revoked token is dead");
    let (s, _) = call(&h.app, "GET", "/sync", Some(&tablet), None).await;
    assert_eq!(s, StatusCode::OK, "the other device is untouched");
}

#[tokio::test]
async fn ambiguous_prefix_revokes_nothing() {
    use bread_tracker_server::devices::{self, Revoked};
    let h = harness().await;
    paired(&h).await;
    paired(&h).await;
    for (id, n) in [("aaaa1111-0000-4000-8000-000000000001", 1), ("aaaa1111-0000-4000-8000-000000000002", 2)] {
        sqlx::query("INSERT INTO devices (id, user_id, name, token_hash, created_at, last_seen) VALUES (?, 1, ?, ?, 0, 0)")
            .bind(id).bind(format!("d{n}")).bind(format!("hash{n}"))
            .execute(&h.state.db).await.unwrap();
    }
    assert_eq!(devices::revoke(&h.state.db, "aaaa1111").await.unwrap(), Revoked::Ambiguous(2));
    assert_eq!(devices::list(&h.state.db).await.unwrap().len(), 4);
    assert_eq!(devices::revoke(&h.state.db, "aaaa1111-0000-4000-8000-000000000002").await.unwrap(), Revoked::Removed("d2".into()));
    assert_eq!(devices::list(&h.state.db).await.unwrap().len(), 3);
}

#[tokio::test]
async fn sign_out_revokes_only_the_calling_device() {
    let h = harness().await;
    let phone = paired(&h).await;
    let other = paired(&h).await;
    call(&h.app, "PUT", "/device", Some(&phone), Some(json!({"fcm_token": "fcm-phone"}))).await;
    let (s, v) = call(&h.app, "DELETE", "/device", Some(&phone), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = call(&h.app, "GET", "/sync", Some(&phone), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&h.app, "GET", "/sync", Some(&other), None).await;
    assert_eq!(s, StatusCode::OK);
    // Its push token went with it, so it is not reminded any more.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE fcm_token = 'fcm-phone'").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(n, 0);
    let (s, _) = call(&h.app, "DELETE", "/device", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "needs auth");
}

// ---- multi-user scoping ----

/// Create another user and pair a device for them; returns (user id, bearer token).
async fn user_with_device(h: &Harness, name: &str) -> (i64, String) {
    let uid: i64 = sqlx::query_scalar("INSERT INTO users (name, is_admin, created_at) VALUES (?, 0, 0) RETURNING id")
        .bind(name)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    let code = auth::create_pairing_code_for(&h.state.db, uid).await.unwrap();
    let (s, v) = call(&h.app, "POST", "/pair", None, Some(json!({"code": code, "device_name": name}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (uid, v["token"].as_str().unwrap().to_string())
}

#[tokio::test]
async fn pairing_code_binds_the_device_to_the_code_owner() {
    let h = harness().await;
    let admin = paired(&h).await;
    let (uid, bob) = user_with_device(&h, "Bob").await;
    let owner = |tok: String| {
        let db = h.state.db.clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT user_id FROM devices WHERE token_hash = ?").bind(auth::hash(&tok)).fetch_one(&db).await.unwrap()
        }
    };
    assert_eq!(owner(admin).await, 1);
    assert_eq!(owner(bob).await, uid);
}

#[tokio::test]
async fn pushing_the_same_item_id_never_touches_another_users_row() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (_, bob) = user_with_device(&h, "Bob").await;
    let now = store::now_ms();

    call(&h.app, "POST", "/sync", Some(&alice), Some(json!({
        "items": [item(C1, "chore", "Alice's dishes", 127, None, now)],
        "completions": [comp(C1, "2026-10-19", now, None)]
    }))).await;

    // Bob pushes the same ids with a NEWER timestamp, which would win last-write-wins on a global key.
    let (s, v) = call(&h.app, "POST", "/sync", Some(&bob), Some(json!({
        "items": [item(C1, "habit", "Bob's version", 3, None, now + 5000)],
        "completions": [comp(C1, "2026-10-19", now + 5000, Some(now + 5000))]
    }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["items"].as_array().unwrap().len(), 1);
    assert_eq!(v["items"][0]["name"], "Bob's version");

    // Alice's rows are exactly as she left them.
    let (_, a) = call(&h.app, "GET", "/sync?since=0", Some(&alice), None).await;
    assert_eq!(a["items"].as_array().unwrap().len(), 1);
    assert_eq!(a["items"][0]["name"], "Alice's dishes");
    assert_eq!(a["items"][0]["kind"], "chore");
    assert_eq!(a["completions"].as_array().unwrap().len(), 1);
    assert!(a["completions"][0]["deleted_at"].is_null(), "Bob's un-tick did not reach Alice's completion");
}

#[tokio::test]
async fn sync_and_stats_return_only_the_callers_data() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (_, bob) = user_with_device(&h, "Bob").await;
    let now = store::now_ms();
    call(&h.app, "POST", "/sync", Some(&alice), Some(json!({
        "items": [item(C1, "chore", "A", 127, None, now)], "completions": [comp(C1, "2026-10-19", now, None)]
    }))).await;
    call(&h.app, "POST", "/sync", Some(&bob), Some(json!({"items": [item(H1, "habit", "B", 127, None, now)]}))).await;
    // Bob also has an item with Alice's id but none of her completions: a stats query that
    // forgot the user filter would credit him with her tick.
    call(&h.app, "POST", "/sync", Some(&bob), Some(json!({"items": [item(C1, "chore", "B copy", 127, None, now)]}))).await;

    for (tok, names) in [(&alice, vec!["A"]), (&bob, vec!["B", "B copy"])] {
        let (_, v) = call(&h.app, "GET", "/sync?since=0", Some(tok), None).await;
        let mut got: Vec<&str> = v["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
        got.sort();
        assert_eq!(got, names);
    }
    let (_, bob_pull) = call(&h.app, "GET", "/sync?since=0", Some(&bob), None).await;
    assert!(bob_pull["completions"].as_array().unwrap().is_empty(), "Alice's completion is invisible to Bob");

    let (_, a) = call(&h.app, "GET", "/stats?date=2026-10-19", Some(&alice), None).await;
    let (_, b) = call(&h.app, "GET", "/stats?date=2026-10-19", Some(&bob), None).await;
    assert!(a["xp"].as_i64().unwrap() > 0);
    assert_eq!(b["xp"], 0, "Bob has no completions, so no XP");
}

#[tokio::test]
async fn the_cursor_may_have_gaps_but_never_hides_the_callers_rows() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (_, bob) = user_with_device(&h, "Bob").await;
    let now = store::now_ms();
    let (_, first) = call(&h.app, "POST", "/sync", Some(&alice), Some(json!({"items": [item(C1, "chore", "a1", 127, None, now)]}))).await;
    let cursor = first["cursor"].as_i64().unwrap();
    // Bob writes many rows, advancing the global counter past Alice's cursor.
    for i in 0..5 {
        let id = format!("3333333{i}-3333-4333-8333-333333333333");
        call(&h.app, "POST", "/sync", Some(&bob), Some(json!({"items": [item(&id, "chore", "b", 127, None, now)]}))).await;
    }
    let (_, second) = call(&h.app, "POST", "/sync", Some(&alice), Some(json!({
        "since": cursor, "items": [item(H1, "habit", "a2", 127, None, now)]
    }))).await;
    let names: Vec<&str> = second["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["a2"], "only her new row, none of Bob's");
    assert!(second["cursor"].as_i64().unwrap() > cursor + 5, "cursor skipped over Bob's seq values");
}

#[tokio::test]
async fn device_routes_act_only_on_the_callers_own_device() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (bob_id, bob) = user_with_device(&h, "Bob").await;
    call(&h.app, "PUT", "/device", Some(&alice), Some(json!({"fcm_token": "fcm-alice"}))).await;
    call(&h.app, "PUT", "/device", Some(&bob), Some(json!({"fcm_token": "fcm-bob"}))).await;
    let tokens: Vec<(i64, Option<String>)> = sqlx::query_as("SELECT user_id, fcm_token FROM devices ORDER BY user_id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(tokens, vec![(1, Some("fcm-alice".into())), (bob_id, Some("fcm-bob".into()))]);

    // The same app install registering under another account takes the token over, and only the token.
    call(&h.app, "PUT", "/device", Some(&bob), Some(json!({"fcm_token": "fcm-alice"}))).await;
    let tokens: Vec<(i64, Option<String>)> = sqlx::query_as("SELECT user_id, fcm_token FROM devices ORDER BY user_id").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(tokens, vec![(1, None), (bob_id, Some("fcm-alice".into()))]);
    assert_eq!(devices_of(&h, 1).await, 1, "Alice's device is still there");

    // Sign out removes only the caller's device.
    let (s, _) = call(&h.app, "DELETE", "/device", Some(&bob), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(devices_of(&h, bob_id).await, 0);
    assert_eq!(devices_of(&h, 1).await, 1);
    let (s, _) = call(&h.app, "GET", "/stats", Some(&bob), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&h.app, "GET", "/stats", Some(&alice), None).await;
    assert_eq!(s, StatusCode::OK);
}

async fn devices_of(h: &Harness, uid: i64) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE user_id = ?").bind(uid).fetch_one(&h.state.db).await.unwrap()
}

#[tokio::test]
async fn push_test_goes_to_the_callers_device_only() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (_, bob) = user_with_device(&h, "Bob").await;
    call(&h.app, "PUT", "/device", Some(&alice), Some(json!({"fcm_token": "fcm-alice"}))).await;
    call(&h.app, "PUT", "/device", Some(&bob), Some(json!({"fcm_token": "fcm-bob"}))).await;
    let (s, _) = call(&h.app, "POST", "/push/test", Some(&bob), None).await;
    assert_eq!(s, StatusCode::OK);
    let sent: Vec<String> = h.mock.sent.lock().unwrap().iter().map(|m| m.token.clone()).collect();
    assert_eq!(sent, vec!["fcm-bob".to_string()]);
}

#[tokio::test]
async fn reminders_go_only_to_the_owners_devices_and_are_tracked_per_user() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (_, bob) = user_with_device(&h, "Bob").await;
    let now = store::now_ms();
    // Both users have a chore due at 17:15 on the same day, with the same item id.
    for tok in [&alice, &bob] {
        call(&h.app, "POST", "/sync", Some(tok), Some(json!({"items": [item(C1, "chore", "Dishes", 127, Some("17:15"), now)]}))).await;
    }
    call(&h.app, "PUT", "/device", Some(&alice), Some(json!({"fcm_token": "fcm-alice"}))).await;
    call(&h.app, "PUT", "/device", Some(&bob), Some(json!({"fcm_token": "fcm-bob"}))).await;

    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 2);
    let mut targets: Vec<String> = h.mock.sent.lock().unwrap().iter().map(|m| m.token.clone()).collect();
    targets.sort();
    assert_eq!(targets, vec!["fcm-alice".to_string(), "fcm-bob".to_string()], "each user got their own reminder once");
    // Sent markers are per user, so neither is reminded again.
    assert_eq!(push::tick(&h.state, perth(17, 17)).await.unwrap(), 0);
    let marked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reminders_sent").fetch_one(&h.state.db).await.unwrap();
    assert_eq!(marked, 2);
}

#[tokio::test]
async fn a_user_without_devices_does_not_block_or_consume_anothers_reminder() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (bob_id, bob) = user_with_device(&h, "Bob").await;
    let now = store::now_ms();
    for tok in [&alice, &bob] {
        call(&h.app, "POST", "/sync", Some(tok), Some(json!({"items": [item(C1, "chore", "Dishes", 127, Some("17:15"), now)]}))).await;
    }
    // Only Bob has a push target.
    call(&h.app, "PUT", "/device", Some(&bob), Some(json!({"fcm_token": "fcm-bob"}))).await;
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
    let sent: Vec<String> = h.mock.sent.lock().unwrap().iter().map(|m| m.token.clone()).collect();
    assert_eq!(sent, vec!["fcm-bob".to_string()]);
    let owners: Vec<i64> = sqlx::query_scalar("SELECT user_id FROM reminders_sent").fetch_all(&h.state.db).await.unwrap();
    assert_eq!(owners, vec![bob_id], "Alice's reminder stays unsent so it can go out once she registers");
}

#[tokio::test]
async fn one_users_completion_does_not_suppress_anothers_reminder() {
    let h = harness().await;
    let alice = paired(&h).await;
    let (_, bob) = user_with_device(&h, "Bob").await;
    let now = store::now_ms();
    for tok in [&alice, &bob] {
        call(&h.app, "POST", "/sync", Some(tok), Some(json!({"items": [item(C1, "chore", "Dishes", 127, Some("17:15"), now)]}))).await;
    }
    // Alice already did it today (2026-10-19 is the day `perth` uses); Bob has not.
    call(&h.app, "POST", "/sync", Some(&alice), Some(json!({"completions": [comp(C1, "2026-10-19", now, None)]}))).await;
    call(&h.app, "PUT", "/device", Some(&alice), Some(json!({"fcm_token": "fcm-alice"}))).await;
    call(&h.app, "PUT", "/device", Some(&bob), Some(json!({"fcm_token": "fcm-bob"}))).await;

    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
    let sent: Vec<String> = h.mock.sent.lock().unwrap().iter().map(|m| m.token.clone()).collect();
    assert_eq!(sent, vec!["fcm-bob".to_string()], "Alice is skipped because she is done; Bob is still reminded");
}
