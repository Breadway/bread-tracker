use std::sync::Arc;

use axum::body::Body;
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
    assert_eq!(v["utc_offset_minutes"], 480, "Perth has no DST");
    let (s, v) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"timezone": "Asia/Kolkata"})),
    )
    .await;
    assert_eq!(
        (s, v["utc_offset_minutes"].as_i64()),
        (StatusCode::OK, Some(330))
    );
    call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"timezone": "Australia/Perth"})),
    )
    .await;
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
    let (s, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "22:00", "quiet_end": "not-a-time"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, after) = call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(
        after["quiet_start"], "22:00",
        "invalid patch must not partially mutate settings"
    );
    assert_eq!(after["quiet_end"], "07:00");
    let concurrent = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"timezone":"Australia/Sydney","reminders_enabled":false})),
    );
    let concurrent2 = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start":"21:00","quiet_end":"06:00"})),
    );
    let (a, b) = tokio::join!(concurrent, concurrent2);
    assert_eq!(a.0, StatusCode::OK);
    assert_eq!(b.0, StatusCode::OK);
    let (_, final_settings) = call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(final_settings["timezone"], "Australia/Sydney");
    assert_eq!(final_settings["quiet_start"], "21:00");
    assert_eq!(final_settings["quiet_end"], "06:00");
    assert_eq!(final_settings["reminders_enabled"], false);
}

#[tokio::test]
async fn concurrent_settings_patches_preserve_both_fields() {
    let h = harness().await;
    let tok = paired(&h).await;
    let app = h.app.clone();
    let token = tok.clone();
    let first = tokio::spawn(async move {
        call(
            &app,
            "PUT",
            "/settings",
            Some(&token),
            Some(json!({"timezone": "Australia/Sydney"})),
        )
        .await
    });
    let app = h.app.clone();
    let token = tok.clone();
    let second = tokio::spawn(async move {
        call(
            &app,
            "PUT",
            "/settings",
            Some(&token),
            Some(json!({"reminders_enabled": false})),
        )
        .await
    });
    assert_eq!(first.await.unwrap().0, StatusCode::OK);
    assert_eq!(second.await.unwrap().0, StatusCode::OK);
    let (_, settings) = call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(settings["timezone"], "Australia/Sydney");
    assert_eq!(settings["reminders_enabled"], false);
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
    assert_eq!(
        devices::revoke(&h.state.db, "ab").await.unwrap(),
        Revoked::NotFound
    );
    assert_eq!(
        devices::revoke(&h.state.db, "zzzzzzzz").await.unwrap(),
        Revoked::NotFound
    );
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
    for (id, n) in [
        ("aaaa1111-0000-4000-8000-000000000001", 1),
        ("aaaa1111-0000-4000-8000-000000000002", 2),
    ] {
        sqlx::query("INSERT INTO devices (id, name, token_hash, created_at, last_seen) VALUES (?, ?, ?, 0, 0)")
            .bind(id).bind(format!("d{n}")).bind(format!("hash{n}"))
            .execute(&h.state.db).await.unwrap();
    }
    assert_eq!(
        devices::revoke(&h.state.db, "aaaa1111").await.unwrap(),
        Revoked::Ambiguous(2)
    );
    assert_eq!(devices::list(&h.state.db).await.unwrap().len(), 4);
    assert_eq!(
        devices::revoke(&h.state.db, "aaaa1111-0000-4000-8000-000000000002")
            .await
            .unwrap(),
        Revoked::Removed("d2".into())
    );
    assert_eq!(devices::list(&h.state.db).await.unwrap().len(), 3);
}

#[tokio::test]
async fn sign_out_revokes_only_the_calling_device() {
    let h = harness().await;
    let phone = paired(&h).await;
    let other = paired(&h).await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&phone),
        Some(json!({"fcm_token": "fcm-phone"})),
    )
    .await;
    let (s, v) = call(&h.app, "DELETE", "/device", Some(&phone), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, _) = call(&h.app, "GET", "/sync", Some(&phone), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&h.app, "GET", "/sync", Some(&other), None).await;
    assert_eq!(s, StatusCode::OK);
    // Its push token went with it, so it is not reminded any more.
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE fcm_token = 'fcm-phone'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(n, 0);
    let (s, _) = call(&h.app, "DELETE", "/device", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "needs auth");
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
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({
            "items": [todo, other],
            // C1 was ticked yesterday, ahead of its due date.
            "completions": [comp(C1, "2026-10-18", now, None)]
        })),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "fcm-token-1"})),
    )
    .await;

    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
    let titles: Vec<String> = h
        .mock
        .sent
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.title.clone())
        .collect();
    assert_eq!(
        titles,
        vec!["Book dentist"],
        "only the untouched to-do is reminded"
    );
}

#[tokio::test]
async fn un_ticked_todo_completion_does_not_suppress_the_reminder() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let mut todo = item(C1, "todo", "Pay rent", 127, Some("17:15"), now);
    todo["due_date"] = json!("2026-10-19");
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({
            "items": [todo],
            "completions": [comp(C1, "2026-10-18", now, Some(now))]
        })),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "fcm-token-1"})),
    )
    .await;
    assert_eq!(push::tick(&h.state, perth(17, 16)).await.unwrap(), 1);
}

#[tokio::test]
async fn quiet_hours_longer_than_grace_defer_the_reminder_to_when_they_end() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    // 22:30 chore every day; quiet hours 22:00-07:00 (9h, longer than the 6h grace).
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "Night", 127, Some("22:30"), now)]})),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "fcm-token-1"})),
    )
    .await;
    let (s, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "22:00", "quiet_end": "07:00"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    assert_eq!(
        push::tick(&h.state, perth(22, 31)).await.unwrap(),
        0,
        "quiet"
    );
    let next = |h: u32, m: u32| perth(h, m) + Duration::days(1);
    assert_eq!(
        push::tick(&h.state, next(4, 31)).await.unwrap(),
        0,
        "still quiet, past the old grace cutoff"
    );
    assert_eq!(push::tick(&h.state, next(6, 59)).await.unwrap(), 0);
    assert_eq!(
        push::tick(&h.state, next(7, 1)).await.unwrap(),
        1,
        "released when quiet hours end"
    );
    {
        let sent = h.mock.sent.lock().unwrap();
        assert_eq!(
            sent[0].data["date"], "2026-10-19",
            "keeps the date it was scheduled for"
        );
    }
    assert_eq!(
        push::tick(&h.state, next(7, 2)).await.unwrap(),
        0,
        "only once"
    );
}

#[tokio::test]
async fn held_reminder_still_expires_after_grace_from_the_end_of_quiet_hours() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "Night", 127, Some("22:30"), now)]})),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "fcm-token-1"})),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "22:00", "quiet_end": "07:00"})),
    )
    .await;
    // Server was down until 14:00 the next day: the held reminder is stale, not sent that late.
    assert_eq!(
        push::tick(&h.state, perth(14, 0) + Duration::days(1))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn ordinary_missed_reminder_is_not_carried_past_midnight() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "Late", 127, Some("23:00"), now)]})),
    )
    .await;
    call(
        &h.app,
        "PUT",
        "/device",
        Some(&tok),
        Some(json!({"fcm_token": "fcm-token-1"})),
    )
    .await;
    assert_eq!(
        push::tick(&h.state, perth(0, 30) + Duration::days(1))
            .await
            .unwrap(),
        0
    );
}

#[tokio::test]
async fn half_set_quiet_window_is_rejected() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (s, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "22:00"})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, v) = call(&h.app, "GET", "/settings", Some(&tok), None).await;
    assert_eq!(v["quiet_start"], "", "nothing was stored");
    let (s, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "22:00", "quiet_end": "07:00"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_end": ""})),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "clearing only one end");
    let (s, v) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&tok),
        Some(json!({"quiet_start": "", "quiet_end": ""})),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["quiet_start"], "");
}
