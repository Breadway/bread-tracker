use std::process::{Command, Stdio};
use std::str::FromStr;
use std::sync::Arc;
use std::thread;
use std::time::Duration as StdDuration;

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
    let generation = db::current_epoch(&db).await.unwrap().unwrap();
    let state = AppState::new(db, generation, Config::from_env(), mock.clone());
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
async fn sync_reports_a_stable_epoch_that_differs_between_databases() {
    let h = harness().await;
    let tok = paired(&h).await;
    let (_, a) = call(&h.app, "GET", "/sync", Some(&tok), None).await;
    let epoch = a["epoch"].as_str().unwrap().to_string();
    assert!(!epoch.is_empty() && epoch != "0");
    let now = store::now_ms();
    let (_, b) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "x", 127, None, now)]})),
    )
    .await;
    assert_eq!(b["epoch"], epoch, "writes do not change it");
    let (_, same) = call(&h.app, "GET", "/sync", Some(&tok), None).await;
    assert_eq!(same["epoch"], epoch);
    *h.state.generation.write().unwrap() = "rotated-generation".to_owned();
    let (_, changed) = call(&h.app, "GET", "/sync", Some(&tok), None).await;
    assert_ne!(changed["epoch"], epoch);
}

#[tokio::test]
async fn generation_preflight_rejects_dirty_writes_then_returns_full_snapshot() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "restored", 127, None, now)]})),
    )
    .await;
    let old_epoch: String = sqlx::query_scalar("SELECT value FROM settings WHERE key='sync_epoch'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    *h.state.generation.write().unwrap() = "rotated-generation".to_owned();
    let current: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key='seq'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(current >= 1);
    let (status, _) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"since": 999, "epoch": old_epoch, "items": [item(H1, "chore", "dirty", 127, None, now + 1000)]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (_, preflight) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"since": 999, "epoch": old_epoch})),
    )
    .await;
    assert_ne!(preflight["epoch"], old_epoch);
    assert_eq!(preflight["items"].as_array().unwrap().len(), 1);
    let (status, accepted) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"since": preflight["cursor"], "epoch": preflight["epoch"], "items": [item(H1, "chore", "dirty", 127, None, now + 1000)]}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(accepted["items"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn legacy_request_against_generation_server_keeps_push_pull_contract() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let (status, pushed) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"since": 0, "items": [item(C1, "chore", "legacy", 127, None, now)]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(pushed["items"][0]["name"], "legacy");
    assert!(pushed["epoch"].as_str().is_some());
    let legacy_db = db::connect_memory().await.unwrap();
    sqlx::query("INSERT INTO items (id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES ('33333333-3333-4333-8333-333333333333','chore','preexisting',127,10,0,1,1,1)")
        .execute(&legacy_db).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM items")
            .fetch_one(&legacy_db)
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn stale_epoch_rejects_push_before_it_can_mask_restore_sequence_rollback() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    for idx in 0..3 {
        let id = format!("1111111{idx}-1111-4111-8111-111111111111");
        call(
            &h.app,
            "POST",
            "/sync",
            Some(&tok),
            Some(json!({"items": [item(&id, "chore", "backup", 127, None, now + idx)]})),
        )
        .await;
    }
    let old_epoch = h.state.generation.read().unwrap().clone();
    let current: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key='seq'")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert!(current >= 3);
    *h.state.generation.write().unwrap() = "rotated-with-cursor-past-client".to_owned();
    let dirty_id = "22222222-2222-4222-8222-222222222222";
    let (status, _) = call(&h.app, "POST", "/sync", Some(&tok), Some(json!({"since": 3, "epoch": old_epoch, "items": [item(dirty_id, "chore", "offline", 127, None, now + 1000)]}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE id=?")
        .bind(dirty_id)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(
        count, 0,
        "stale generation request must be rejected before dirty rows are inserted"
    );
    let (_, snapshot) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"since": 3, "epoch": old_epoch})),
    )
    .await;
    assert_eq!(snapshot["items"].as_array().unwrap().len(), 3);
    assert_ne!(snapshot["epoch"], old_epoch);
}

#[tokio::test]
async fn populated_old_wire_client_and_missing_optional_fields_remain_compatible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("legacy-wire.db");
    let database_url = format!("sqlite://{}", path.display());
    let legacy_options = sqlx::sqlite::SqliteConnectOptions::from_str(&database_url)
        .unwrap()
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal);
    let legacy = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(legacy_options)
        .await
        .unwrap();
    sqlx::migrate!("../migrations").run(&legacy).await.unwrap();
    sqlx::query("INSERT INTO items (id,kind,name,weekdays,points,sort,active,updated_at,seq) VALUES (?, 'chore','legacy preserved',127,10,0,1,1,1)")
        .bind(C1).execute(&legacy).await.unwrap();
    sqlx::query("UPDATE meta SET val=1 WHERE key='seq'")
        .execute(&legacy)
        .await
        .unwrap();
    sqlx::query("INSERT INTO devices VALUES (?,?,?,?,?,?)")
        .bind("legacy-device")
        .bind("Legacy")
        .bind(bread_tracker_server::auth::hash("legacy-token"))
        .bind(Option::<String>::None)
        .bind(1_i64)
        .bind(1_i64)
        .execute(&legacy)
        .await
        .unwrap();
    legacy.close().await;
    let db = db::connect_existing(&database_url).await.unwrap();
    let generation = db::reconcile_generation_locked(&db, &path).await.unwrap();
    let preserved: String = sqlx::query_scalar("SELECT name FROM items WHERE id=?")
        .bind(C1)
        .fetch_one(&db)
        .await
        .unwrap();
    assert_eq!(
        preserved, "legacy preserved",
        "upgrade path preserves populated row"
    );
    assert!(!generation.is_empty());
    let state = AppState::new(db, generation, Config::from_env(), Arc::new(push::NoPush));
    let app = api::router(state);
    let response = call(
        &app,
        "POST",
        "/sync",
        Some("legacy-token"),
        Some(json!({"since": 0, "items": [], "completions": []})),
    )
    .await;
    assert_eq!(response.0, StatusCode::OK);
    assert_eq!(
        response.1["items"][0]["name"], "legacy preserved",
        "wire response: {:#}",
        response.1
    );
    assert!(response.1["epoch"].as_str().is_some());
}

#[test]
fn pair_cli_completes_while_the_http_server_holds_generation_lock() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("pair-process.db");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let binary = env!("CARGO_BIN_EXE_bread-tracker-server");
    let mut server = Command::new(binary)
        .env("BREADTRACKER_DATABASE_PATH", &database)
        .env("BREADTRACKER_LISTEN", address.to_string())
        .env("BREADTRACKER_FCM_SERVICE_ACCOUNT", "")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    let mut ready = false;
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&address, StdDuration::from_millis(50)).is_ok() {
            ready = true;
            break;
        }
        std::thread::sleep(StdDuration::from_millis(20));
    }
    assert!(ready, "server did not become ready");
    let mut pair = Command::new(binary)
        .arg("pair")
        .env("BREADTRACKER_DATABASE_PATH", &database)
        .env("BREADTRACKER_LISTEN", address.to_string())
        .env("BREADTRACKER_FCM_SERVICE_ACCOUNT", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pair_deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    while std::time::Instant::now() < pair_deadline && pair.try_wait().unwrap().is_none() {
        thread::sleep(StdDuration::from_millis(10));
    }
    if pair.try_wait().unwrap().is_none() {
        let _ = pair.kill();
        let output = pair.wait_with_output().unwrap();
        let _ = server.kill();
        let _ = server.wait();
        panic!(
            "pair CLI hung against running server: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let pair = pair.wait_with_output().unwrap();
    assert!(
        pair.status.success(),
        "pair stderr: {}",
        String::from_utf8_lossy(&pair.stderr)
    );
    assert!(!String::from_utf8_lossy(&pair.stdout).trim().is_empty());
    let marker = Command::new(binary)
        .arg("mark-restored")
        .env("BREADTRACKER_DATABASE_PATH", &database)
        .env("BREADTRACKER_LISTEN", address.to_string())
        .output()
        .unwrap();
    assert!(
        !marker.status.success(),
        "restore marker must refuse an active server"
    );
    assert!(String::from_utf8_lossy(&marker.stderr).contains("server is running"));
    let _ = server.kill();
    let _ = server.wait();
}

#[test]
fn second_server_startup_fails_closed_while_first_owns_generation_lock() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("concurrent-start.db");
    let address1 = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let address2 = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let binary = env!("CARGO_BIN_EXE_bread-tracker-server");
    let spawn = |address: std::net::SocketAddr| {
        Command::new(binary)
            .env("BREADTRACKER_DATABASE_PATH", &database)
            .env("BREADTRACKER_LISTEN", address.to_string())
            .env("BREADTRACKER_FCM_SERVICE_ACCOUNT", "")
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let mut first = spawn(address1);
    let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::net::TcpStream::connect_timeout(&address1, StdDuration::from_millis(50)).is_ok() {
            break;
        }
        if first.try_wait().unwrap().is_some() {
            panic!("first server exited during startup");
        }
        thread::sleep(StdDuration::from_millis(20));
    }
    let mut second = spawn(address2);
    let second_deadline = std::time::Instant::now() + StdDuration::from_secs(5);
    let mut failed = false;
    while std::time::Instant::now() < second_deadline {
        if second.try_wait().unwrap().is_some() {
            failed = true;
            break;
        }
        thread::sleep(StdDuration::from_millis(20));
    }
    if !failed {
        let _ = second.kill();
    }
    let output = second.wait_with_output().unwrap();
    let _ = first.kill();
    let _ = first.wait();
    assert!(
        failed && !output.status.success(),
        "second server must fail closed promptly while first holds generation lock"
    );
}

#[tokio::test]
async fn legacy_client_without_generation_still_syncs() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    let (status, response) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "legacy", 127, None, now)]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["items"][0]["name"], "legacy");
}

#[tokio::test]
async fn a_cursor_ahead_of_the_database_gets_everything() {
    let h = harness().await;
    let tok = paired(&h).await;
    let now = store::now_ms();
    call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"items": [item(C1, "chore", "x", 127, None, now)]})),
    )
    .await;
    let (_, epoch_response) = call(&h.app, "GET", "/sync", Some(&tok), None).await;
    let (status, response) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&tok),
        Some(json!({"since": 999999, "epoch": epoch_response["epoch"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(response["items"].as_array().unwrap().len(), 1);
    assert!(response["cursor"].as_i64().unwrap() < 999999);
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
        Some(json!({"items": [todo, other], "completions": [comp(C1, "2026-10-18", now, None)]})),
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
    assert_eq!(titles, vec!["Book dentist"]);
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
        Some(json!({"items": [todo], "completions": [comp(C1, "2026-10-18", now, Some(now))]})),
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
