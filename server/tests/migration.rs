//! Migration 0002 (users) against a database built from migration 0001 with data in it.

use std::borrow::Cow;
use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use bread_tracker_server::config::Config;
use bread_tracker_server::push::MockSender;
use bread_tracker_server::{api, auth, db, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tower::ServiceExt;

const I1: &str = "11111111-1111-4111-8111-111111111111";
const I2: &str = "22222222-2222-4222-8222-222222222222";
const TOKEN: &str = "old-phone-token";

/// A file database at schema 0001 only, holding what a live single-user server would hold.
async fn old_database(path: &std::path::Path) {
    let url = format!("sqlite://{}", path.display());
    let opts = SqliteConnectOptions::from_str(&url).unwrap().create_if_missing(true).foreign_keys(true);
    let pool = SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.unwrap();

    let mut only_first = sqlx::migrate!("../migrations");
    only_first.migrations = Cow::Owned(only_first.migrations.iter().filter(|m| m.version == 1).cloned().collect());
    assert_eq!(only_first.migrations.len(), 1);
    only_first.run(&pool).await.unwrap();

    let q = |sql: &'static str| sqlx::query(sql);
    q("INSERT INTO items (id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq)
       VALUES ('11111111-1111-4111-8111-111111111111', 'chore', 'Dishes', 127, '18:30', 10, NULL, 0, 1, 1000, NULL, 1),
              ('22222222-2222-4222-8222-222222222222', 'habit', 'Read', 127, NULL, 15, NULL, 1, 1, 1000, 5000, 3)")
        .execute(&pool).await.unwrap();
    q("INSERT INTO completions (item_id, date, completed_at, updated_at, deleted_at, seq)
       VALUES ('11111111-1111-4111-8111-111111111111', '2026-10-19', 2000, 2000, NULL, 2),
              ('11111111-1111-4111-8111-111111111111', '2026-10-20', 3000, 3000, 4000, 4),
              ('22222222-2222-4222-8222-222222222222', '2026-10-19', 2000, 2000, NULL, 5)")
        .execute(&pool).await.unwrap();
    q("UPDATE meta SET val = 5 WHERE key = 'seq'").execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO devices (id, name, token_hash, fcm_token, created_at, last_seen) VALUES ('dev-1', 'Pixel', ?, 'fcm-1', 777, 888)")
        .bind(auth::hash(TOKEN))
        .execute(&pool).await.unwrap();
    q("INSERT INTO devices (id, name, token_hash, created_at, last_seen) VALUES ('dev-2', 'Tablet', 'hash-2', 900, 900)")
        .execute(&pool).await.unwrap();
    q("INSERT INTO pairing_codes (code_hash, expires_at) VALUES ('code-hash', 99999999999999)").execute(&pool).await.unwrap();
    q("INSERT INTO reminders_sent (item_id, date, sent_at) VALUES ('11111111-1111-4111-8111-111111111111', '2026-10-20', 5)")
        .execute(&pool).await.unwrap();
    q("UPDATE settings SET value = 'Europe/Berlin' WHERE key = 'timezone'").execute(&pool).await.unwrap();
    q("UPDATE settings SET value = '22:00' WHERE key = 'quiet_start'").execute(&pool).await.unwrap();
    pool.close().await;
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn call(app: &axum::Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let req = Request::builder().method(method).uri(uri).header("authorization", format!("Bearer {TOKEN}"));
    let req = match body {
        Some(b) => req.header("content-type", "application/json").body(Body::from(b.to_string())).unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(resp.status().is_success(), "{method} {uri} -> {}", resp.status());
    serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn existing_database_migrates_and_the_admin_owns_everything() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    old_database(&path).await;

    let pool = db::connect_with_admin(&format!("sqlite://{}", path.display()), Some("Riley")).await.unwrap();

    // Row counts are unchanged.
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 2);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM completions").await, 3);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM devices").await, 2);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pairing_codes").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM reminders_sent").await, 1);

    // One user, the admin, named from the configuration, owning every row.
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 1);
    let (id, name, admin, created): (i64, String, bool, i64) =
        sqlx::query_as("SELECT id, name, is_admin, created_at FROM users").fetch_one(&pool).await.unwrap();
    assert_eq!((id, name.as_str(), admin), (1, "Riley", true));
    assert_eq!(created, 777, "created when the first device was paired");
    for table in ["items", "completions", "devices", "pairing_codes", "reminders_sent"] {
        assert_eq!(count(&pool, &format!("SELECT COUNT(*) FROM {table} WHERE user_id = 1")).await, count(&pool, &format!("SELECT COUNT(*) FROM {table}")).await, "{table}");
    }

    // Values survived the rebuild, including tombstones and sequence numbers.
    let item: (String, i64, Option<i64>, i64) =
        sqlx::query_as("SELECT name, points, deleted_at, seq FROM items WHERE id = ?").bind(I2).fetch_one(&pool).await.unwrap();
    assert_eq!(item, ("Read".to_string(), 15, Some(5000), 3));
    let comp: (i64, Option<i64>, i64) = sqlx::query_as("SELECT completed_at, deleted_at, seq FROM completions WHERE item_id = ? AND date = '2026-10-20'")
        .bind(I1).fetch_one(&pool).await.unwrap();
    assert_eq!(comp, (3000, Some(4000), 4));
    let dev: (String, Option<String>, i64, i64) =
        sqlx::query_as("SELECT name, fcm_token, created_at, last_seen FROM devices WHERE id = 'dev-1'").fetch_one(&pool).await.unwrap();
    assert_eq!(dev, ("Pixel".into(), Some("fcm-1".into()), 777, 888));
    assert_eq!(count(&pool, "SELECT val FROM meta WHERE key = 'seq'").await, 5);
    // The global settings moved to the admin's own row, and the old table is gone.
    let st: (String, String, String, bool) =
        sqlx::query_as("SELECT timezone, quiet_start, quiet_end, reminders_enabled FROM user_settings WHERE user_id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(st, ("Europe/Berlin".into(), "22:00".into(), "".into(), true));
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM user_settings").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM sqlite_master WHERE name = 'settings'").await, 0);

    // No dangling references anywhere.
    let violations: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check").fetch_all(&pool).await.unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check").fetch_one(&pool).await.unwrap();
    assert_eq!(integrity, "ok");

    // Composite keys: a second user can hold the same item id and completion without touching user 1's rows.
    sqlx::query("INSERT INTO users (id, name, is_admin, created_at) VALUES (2, 'Sam', 0, 1)").execute(&pool).await.unwrap();
    let sam: (String, bool) = sqlx::query_as("SELECT timezone, reminders_enabled FROM user_settings WHERE user_id = 2").fetch_one(&pool).await.unwrap();
    assert_eq!(sam, ("UTC".into(), true), "a new user gets defaults, not the admin's settings");
    sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (2, ?, 'chore', 'Sam dishes', 10, 1, 6)")
        .bind(I1).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, seq) VALUES (2, ?, '2026-10-19', 1, 1, 7)")
        .bind(I1).execute(&pool).await.unwrap();
    let name_of_1: String = sqlx::query_scalar("SELECT name FROM items WHERE user_id = 1 AND id = ?").bind(I1).fetch_one(&pool).await.unwrap();
    assert_eq!(name_of_1, "Dishes");
    // ...but a duplicate within one user is still rejected.
    assert!(sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (2, ?, 'chore', 'dup', 10, 1, 8)")
        .bind(I1).execute(&pool).await.is_err());
    // User names are unique regardless of case.
    assert!(sqlx::query("INSERT INTO users (name, is_admin, created_at) VALUES ('sam', 0, 1)").execute(&pool).await.is_err());
    // Removing a user removes everything they own, and nothing of anyone else's.
    sqlx::query("DELETE FROM users WHERE id = 2").execute(&pool).await.unwrap();
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM user_settings").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 2);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM completions").await, 3);

    // The old app still syncs with its old token and the unchanged wire format.
    let state = AppState::new(pool.clone(), Config::from_env(), Arc::new(MockSender::default()));
    let app = api::router(state);
    let pulled = call(&app, "GET", "/sync?since=0", None).await;
    assert_eq!(pulled["items"].as_array().unwrap().len(), 2);
    assert_eq!(pulled["completions"].as_array().unwrap().len(), 3);
    let pushed = call(&app, "POST", "/sync", Some(json!({
        "since": pulled["cursor"],
        "items": [{"id": "33333333-3333-4333-8333-333333333333", "kind": "todo", "name": "Pay rent", "weekdays": 127, "remind_time": null,
                   "points": 20, "due_date": "2026-10-25", "sort": 0, "active": true, "updated_at": 9000, "deleted_at": null}],
        "completions": []
    }))).await;
    assert_eq!(pushed["items"].as_array().unwrap().len(), 1, "only the new row came back");
    assert!(pushed["cursor"].as_i64().unwrap() > 5);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM items WHERE user_id = 1").await, 3);
    let stats = call(&app, "GET", "/stats?date=2026-10-20", None).await;
    assert!(stats["xp"].as_i64().unwrap() > 0);
    // A pairing code created before the migration still works and lands on the admin.
    let code = auth::create_pairing_code(&pool).await.unwrap();
    let (device_id, _) = auth::redeem(&pool, &code, "New phone").await.unwrap().unwrap();
    let owner: i64 = sqlx::query_scalar("SELECT user_id FROM devices WHERE id = ?").bind(device_id).fetch_one(&pool).await.unwrap();
    assert_eq!(owner, 1);
}

#[tokio::test]
async fn admin_name_is_applied_once_and_never_overrides_an_established_name() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("fresh.db").display());

    // A brand new database has the admin too, with the placeholder name.
    let pool = db::connect_with_admin(&url, None).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(name, "admin");
    pool.close().await;

    let pool = db::connect_with_admin(&url, Some("Riley")).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(name, "Riley");
    pool.close().await;

    // A later start with another value leaves the established name alone.
    let pool = db::connect_with_admin(&url, Some("Someone else")).await.unwrap();
    let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = 1").fetch_one(&pool).await.unwrap();
    assert_eq!(name, "Riley");
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM users").fetch_one(&pool).await.unwrap(), 1);
    pool.close().await;

    // An invalid name is refused at startup rather than stored.
    assert!(db::connect_with_admin(&url, Some("bad/name")).await.is_err());
    assert!(db::connect_with_admin(&url, Some("  ")).await.is_err());
}

#[tokio::test]
async fn migrating_twice_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old.db");
    old_database(&path).await;
    let url = format!("sqlite://{}", path.display());
    db::connect(&url).await.unwrap().close().await;
    let pool = db::connect(&url).await.unwrap();
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 2);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 1);
}
