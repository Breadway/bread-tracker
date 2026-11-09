//! User names, one-time admin naming, and atomic pairing, against real databases.

use std::str::FromStr;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bread_tracker_server::config::Config;
use bread_tracker_server::push::MockSender;
use bread_tracker_server::{api, auth, db, users, AppState};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;
use tower::ServiceExt;

fn url(dir: &tempfile::TempDir, name: &str) -> String {
    format!("sqlite://{}", dir.path().join(name).display())
}

async fn admin_name(pool: &SqlitePool) -> String {
    sqlx::query_scalar("SELECT name FROM users WHERE id = 1")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

// ---- admin naming: idempotent, durable, preserves established name and data ----

#[tokio::test]
async fn choosing_the_name_admin_in_any_case_is_final() {
    for first in ["Admin", "admin", "ADMIN"] {
        let dir = tempfile::tempdir().unwrap();
        let u = url(&dir, "a.db");
        let pool = db::connect_with_admin(&u, Some(first)).await.unwrap();
        assert_eq!(admin_name(&pool).await, first);
        pool.close().await;
        // Restart with another value: the established name stays, whatever its text.
        let pool = db::connect_with_admin(&u, Some("Changed")).await.unwrap();
        assert_eq!(admin_name(&pool).await, first, "renamed on restart");
        pool.close().await;
        let pool = db::connect_with_admin(&u, Some(first)).await.unwrap();
        assert_eq!(admin_name(&pool).await, first);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 1);
    }
}

#[tokio::test]
async fn api_level_apply_admin_then_changed_keeps_the_first_choice() {
    // The reviewer's regression: apply_admin_name(Admin) then apply_admin_name(Changed).
    let pool = db::connect_memory().await.unwrap();
    assert!(users::apply_admin_name(&pool, "Admin").await.unwrap());
    assert!(!users::apply_admin_name(&pool, "Changed").await.unwrap());
    assert_eq!(admin_name(&pool).await, "Admin");
    assert_eq!(
        count(&pool, "SELECT name_confirmed FROM users WHERE id = 1").await,
        1
    );
}

#[tokio::test]
async fn a_renamed_owner_survives_every_restart_with_data_and_settings_intact() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "owner.db");
    let pool = db::connect_with_admin(&u, Some("Riley")).await.unwrap();
    sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (1, 'i', 'chore', 'Dishes', 10, 1, 1)")
        .execute(&pool).await.unwrap();
    sqlx::query("UPDATE user_settings SET timezone = 'Europe/Berlin' WHERE user_id = 1")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    for restart in [None, Some("Riley"), Some("Someone else"), Some("admin")] {
        let pool = db::connect_with_admin(&u, restart).await.unwrap();
        assert_eq!(admin_name(&pool).await, "Riley", "restart with {restart:?}");
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 1);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 1);
        let tz: String = sqlx::query_scalar("SELECT timezone FROM user_settings WHERE user_id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(tz, "Europe/Berlin");
        pool.close().await;
    }
}

#[tokio::test]
async fn an_unset_name_keeps_the_placeholder_open_for_a_later_choice() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "late.db");
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    assert_eq!(admin_name(&pool).await, "admin");
    assert_eq!(
        count(&pool, "SELECT name_confirmed FROM users WHERE id = 1").await,
        0
    );
    pool.close().await;
    let pool = db::connect_with_admin(&u, Some("Riley")).await.unwrap();
    assert_eq!(admin_name(&pool).await, "Riley");
}

#[tokio::test]
async fn a_name_taken_by_another_user_stops_startup_instead_of_renaming() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "taken.db");
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    users::create(&pool, "Sam", false).await.unwrap();
    pool.close().await;
    let err = db::connect_with_admin(&u, Some("sAm")).await.err().unwrap();
    assert!(err.to_string().contains("already used"), "{err}");
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    assert_eq!(admin_name(&pool).await, "admin");
}

#[tokio::test]
async fn invalid_admin_names_are_refused_and_nothing_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "bad.db");
    for bad in ["bad/name", "  ", &"x".repeat(41)] {
        assert!(
            db::connect_with_admin(&u, Some(bad)).await.is_err(),
            "{bad:?}"
        );
    }
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    assert_eq!(admin_name(&pool).await, "admin");
}

// ---- Unicode case-insensitive uniqueness across creation, lookup, rename and admin paths ----

#[tokio::test]
async fn unicode_case_duplicates_are_rejected_on_every_path() {
    let pool = db::connect_memory().await.unwrap();
    for (a, b) in [
        ("Älex", "älex"),
        ("ÄLINA", "älina"),
        ("Alex", "aLEX"),
        ("Élodie", "éLODIE"),
        ("Σίσυφος", "ΣΊΣΥΦΟΣ"),
        ("Straße", "STRASSE"),
        ("Øystein", "øystein"),
    ] {
        let id = users::create(&pool, a, false).await.unwrap();
        assert!(
            matches!(
                users::create(&pool, b, false).await,
                Err(users::NameError::Taken)
            ),
            "{a} vs {b} accepted by create"
        );
        // Lookup uses the same identity.
        assert_eq!(users::find_by_name(&pool, b).await.unwrap().unwrap().id, id);
        // Rename onto an existing name is refused too.
        let other = users::create(&pool, &format!("other {id}"), false)
            .await
            .unwrap();
        assert!(matches!(
            users::rename(&pool, other, b).await,
            Err(users::NameError::Taken)
        ));
        // Renaming a user to a different case of their own name is allowed.
        assert!(users::rename(&pool, id, b).await.unwrap());
    }
}

#[tokio::test]
async fn the_database_itself_enforces_the_key_for_direct_writes() {
    let pool = db::connect_memory().await.unwrap();
    users::create(&pool, "Älex", false).await.unwrap();
    // A raw insert that supplies the same key is rejected by the unique index.
    let key = users::name_key("älex");
    assert!(sqlx::query(
        "INSERT INTO users (name, name_key, is_admin, created_at) VALUES ('älex', ?, 0, 1)"
    )
    .bind(&key)
    .execute(&pool)
    .await
    .is_err());
    // A raw insert of a non-ASCII name with no key cannot slip past the index.
    assert!(
        sqlx::query("INSERT INTO users (name, is_admin, created_at) VALUES ('Ünal', 0, 1)")
            .execute(&pool)
            .await
            .is_err()
    );
    // ASCII raw inserts still work and get a key, so simple tooling and tests keep working.
    sqlx::query("INSERT INTO users (name, is_admin, created_at) VALUES ('Sam', 0, 1)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        sqlx::query("INSERT INTO users (name, is_admin, created_at) VALUES ('sam', 0, 1)")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO users (name, is_admin, created_at) VALUES ('SAM', 0, 1)")
            .execute(&pool)
            .await
            .is_err()
    );
    // A raw ASCII rename keeps the key in step, so the old name is free again.
    sqlx::query("UPDATE users SET name = 'Sammy' WHERE name = 'Sam'")
        .execute(&pool)
        .await
        .unwrap();
    users::create(&pool, "sam", false).await.unwrap();
}

#[tokio::test]
async fn supported_names_keep_working() {
    // The allowed alphabet is unchanged: letters, digits, space, '.', '_' and '-'.
    let pool = db::connect_memory().await.unwrap();
    for ok in ["Alex", "a.b_c-d e", "李雷", "Zoë", "x".repeat(40).as_str()] {
        assert!(users::valid_name(ok), "{ok}");
        users::create(&pool, ok, false).await.unwrap();
    }
    for bad in [
        "",
        " lead",
        "trail ",
        "bad/char",
        "tab\tname",
        &"x".repeat(41),
    ] {
        assert!(matches!(
            users::create(&pool, bad, false).await,
            Err(users::NameError::Invalid)
        ));
    }
}

#[tokio::test]
async fn existing_unicode_names_are_keyed_at_start_and_collisions_stop_startup() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "existing.db");
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    // Simulate rows written by PR43's original 0002 (no key): the two names only differ by
    // Unicode case, which its ASCII-only NOCASE unique index let through.
    for (id, name) in [(2, "Älex"), (3, "älex"), (4, "Ünal")] {
        sqlx::query(
            "INSERT INTO users (id, name, name_key, is_admin, created_at) VALUES (?, ?, ?, 0, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(format!("pending-{id}"))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("UPDATE users SET name_key = NULL WHERE id > 1")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    // Collision: nothing is changed, startup names both users and what to do.
    let err = db::connect_with_admin(&u, None).await.err().unwrap();
    let msg = err.to_string();
    assert!(msg.contains("Älex") && msg.contains("älex"), "{msg}");
    let raw = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::from_str(&u).unwrap())
        .await
        .unwrap();
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM users WHERE name_key IS NULL").await,
        3
    );

    // The operator renames one while stopped; the next start keys everything.
    sqlx::query("UPDATE users SET name = 'Älex 2', name_key = NULL WHERE id = 3")
        .execute(&raw)
        .await
        .unwrap();
    raw.close().await;
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM users WHERE name_key IS NULL").await,
        0
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 4);
    assert!(matches!(
        users::create(&pool, "ÄLEX", false).await,
        Err(users::NameError::Taken)
    ));
}

// ---- atomic, single-use pairing ----

#[tokio::test]
async fn failed_device_insert_keeps_the_code_and_a_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect_with_admin(&url(&dir, "p.db"), None)
        .await
        .unwrap();
    let code = auth::create_pairing_code(&pool).await.unwrap();
    let devices_before = count(&pool, "SELECT COUNT(*) FROM devices").await;
    let users_before = count(&pool, "SELECT COUNT(*) FROM users").await;

    sqlx::query("CREATE TRIGGER fail_device BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT, 'injected'); END")
        .execute(&pool).await.unwrap();
    assert!(auth::redeem(&pool, &code, "phone").await.is_err());

    // Code, devices and accounts are unchanged by the failed attempt.
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pairing_codes").await, 1);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM devices").await,
        devices_before
    );
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM users").await,
        users_before
    );

    // Retry after the fault clears: the very same code works, once.
    sqlx::query("DROP TRIGGER fail_device")
        .execute(&pool)
        .await
        .unwrap();
    let (id, token) = auth::redeem(&pool, &code, "phone").await.unwrap().unwrap();
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pairing_codes").await, 0);
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM devices").await,
        devices_before + 1
    );
    let owner: i64 = sqlx::query_scalar("SELECT user_id FROM devices WHERE id = ?")
        .bind(&id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(owner, 1);
    assert!(!token.is_empty());
    assert!(auth::redeem(&pool, &code, "again").await.unwrap().is_none());
}

#[tokio::test]
async fn failed_insert_over_http_leaves_the_code_for_the_retry() {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect_with_admin(&url(&dir, "h.db"), None)
        .await
        .unwrap();
    let app = api::router(AppState::new(
        pool.clone(),
        "test-generation".to_owned(),
        Config::from_env(),
        Arc::new(MockSender::default()),
    ));
    let code = auth::create_pairing_code(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER fail_device BEFORE INSERT ON devices BEGIN SELECT RAISE(ABORT, 'injected'); END")
        .execute(&pool).await.unwrap();
    let pair = |code: String| {
        Request::builder()
            .method("POST")
            .uri("/pair")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::json!({"code": code, "device_name": "Pixel"}).to_string(),
            ))
            .unwrap()
    };
    let resp = app.clone().oneshot(pair(code.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM pairing_codes").await, 1);
    sqlx::query("DROP TRIGGER fail_device")
        .execute(&pool)
        .await
        .unwrap();
    let resp = app.oneshot(pair(code)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// SQLite result codes for BUSY and LOCKED, including the extended forms. The server's
/// classifier is crate-private, so this integration test matches the documented codes itself.
const SQLITE_BUSY_CODES: [&str; 8] = ["5", "261", "517", "773", "6", "262", "518", "774"];
const BUSY_RETRY_ATTEMPTS: usize = 3;
const BUSY_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(100);

fn is_busy_database_error(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => db
            .code()
            .as_deref()
            .is_some_and(|code| SQLITE_BUSY_CODES.contains(&code)),
        _ => false,
    }
}

/// Redeem a pairing code, retrying only when SQLite reported BUSY or LOCKED before producing a
/// result. A lost race still returns `None` and is never retried, so a second winner still fails
/// the caller's exactly-one-device assertion.
async fn redeem_with_busy_retry(
    pool: &SqlitePool,
    code: &str,
    device_name: &str,
) -> Result<Option<(String, String)>, sqlx::Error> {
    for attempt in 1..=BUSY_RETRY_ATTEMPTS {
        match auth::redeem(pool, code, device_name).await {
            Err(error) if attempt < BUSY_RETRY_ATTEMPTS && is_busy_database_error(&error) => {
                tokio::time::sleep(BUSY_RETRY_BACKOFF).await;
            }
            result => return result,
        }
    }
    unreachable!("the final attempt returns instead of looping")
}

#[tokio::test]
async fn concurrent_redemptions_of_one_code_create_exactly_one_device() {
    let dir = tempfile::tempdir().unwrap();
    let pool = db::connect_with_admin(&url(&dir, "c.db"), None)
        .await
        .unwrap();
    for round in 0..5 {
        let code = auth::create_pairing_code(&pool).await.unwrap();
        let before = count(&pool, "SELECT COUNT(*) FROM devices").await;
        let mut tasks = Vec::new();
        for n in 0..12 {
            let pool = pool.clone();
            let code = code.clone();
            tasks.push(tokio::spawn(async move {
                redeem_with_busy_retry(&pool, &code, &format!("racer {n}")).await
            }));
        }
        let mut winners = 0;
        for t in tasks {
            if t.await.unwrap().unwrap().is_some() {
                winners += 1;
            }
        }
        assert_eq!(winners, 1, "round {round}");
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM devices").await,
            before + 1
        );
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM pairing_codes").await, 0);
    }
}

#[tokio::test]
async fn expired_and_unknown_codes_change_nothing() {
    let pool = db::connect_memory().await.unwrap();
    sqlx::query("INSERT INTO pairing_codes (code_hash, user_id, expires_at) VALUES (?, 1, 1)")
        .bind(auth::hash("ABCD2345"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(auth::redeem(&pool, "ABCD-2345", "x")
        .await
        .unwrap()
        .is_none());
    assert!(auth::redeem(&pool, "ZZZZ-9999", "x")
        .await
        .unwrap()
        .is_none());
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM devices").await, 0);
}

// ---- per-user scoping replaces the interim single-owner boundary ----

#[tokio::test]
async fn each_users_device_sees_only_its_own_rows() {
    let pool = db::connect_memory().await.unwrap();
    let sam = users::create(&pool, "Sam", false).await.unwrap();
    for (uid, name) in [(1_i64, "ADMIN PRIVATE"), (sam, "SAM PRIVATE")] {
        sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (?, '11111111-1111-4111-8111-111111111111', 'chore', ?, 10, 1, ?)")
            .bind(uid).bind(name).bind(uid).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE meta SET val = 2 WHERE key = 'seq'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO devices (id, user_id, name, token_hash, created_at, last_seen) VALUES ('sam-dev', ?, 'Sam phone', ?, 1, 1)")
        .bind(sam).bind(auth::hash("sam-token")).execute(&pool).await.unwrap();
    let code = auth::create_pairing_code(&pool).await.unwrap();
    let (_, admin_token) = auth::redeem(&pool, &code, "admin phone")
        .await
        .unwrap()
        .unwrap();

    let app = api::router(AppState::new(
        pool,
        "test-generation".to_owned(),
        Config::from_env(),
        Arc::new(MockSender::default()),
    ));
    let pull = |token: String| {
        let app = app.clone();
        async move {
            let req = Request::builder()
                .uri("/sync?since=0")
                .header("authorization", format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let body = http_body_util::BodyExt::collect(resp.into_body())
                .await
                .unwrap()
                .to_bytes();
            String::from_utf8_lossy(&body).to_string()
        }
    };
    let admin = pull(admin_token).await;
    assert!(
        admin.contains("ADMIN PRIVATE") && !admin.contains("SAM PRIVATE"),
        "{admin}"
    );
    let sam_view = pull("sam-token".into()).await;
    assert!(
        sam_view.contains("SAM PRIVATE") && !sam_view.contains("ADMIN PRIVATE"),
        "{sam_view}"
    );
}

mod scheduler_boundary {
    use super::*;
    use bread_tracker_server::push;
    use bread_tracker_server::store::UserDb;
    use chrono::{TimeZone, Utc};

    async fn two_owners_two_devices(pool: &SqlitePool) -> i64 {
        sqlx::query("UPDATE user_settings SET timezone = 'Australia/Perth' WHERE user_id = 1")
            .execute(pool)
            .await
            .unwrap();
        let sam = users::create(pool, "Sam", false).await.unwrap();
        sqlx::query("UPDATE user_settings SET timezone = 'Australia/Perth' WHERE user_id = ?")
            .bind(sam)
            .execute(pool)
            .await
            .unwrap();
        for (uid, item, title) in [
            (1_i64, "admin-item", "ADMIN PRIVATE"),
            (sam, "sam-item", "SAM PRIVATE"),
        ] {
            sqlx::query("INSERT INTO items (user_id, id, kind, name, points, weekdays, remind_time, updated_at, seq) VALUES (?, ?, 'chore', ?, 10, 127, '18:30', 1, 1)")
                .bind(uid).bind(item).bind(title).execute(pool).await.unwrap();
            sqlx::query("INSERT INTO devices (id, user_id, name, token_hash, fcm_token, created_at, last_seen) VALUES (?, ?, 'phone', ?, ?, 1, 1)")
                .bind(format!("dev{uid}")).bind(uid).bind(format!("hash{uid}")).bind(format!("fcm{uid}"))
                .execute(pool).await.unwrap();
        }
        sam
    }

    fn state(pool: &SqlitePool) -> (AppState, Arc<push::MockSender>) {
        let mock = Arc::new(push::MockSender::default());
        let st = AppState::new(
            pool.clone(),
            "test-generation".to_owned(),
            Config::from_env(),
            mock.clone(),
        );
        (st, mock)
    }

    /// 18:30 Perth is 10:30 UTC.
    fn at_1830() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 25, 10, 30, 0).unwrap()
    }

    /// (token, title) of every delivery, sorted.
    fn deliveries(mock: &push::MockSender) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = mock
            .sent
            .lock()
            .unwrap()
            .iter()
            .map(|m| (m.token.clone(), m.title.clone()))
            .collect();
        v.sort();
        v
    }

    #[tokio::test]
    async fn two_owners_two_devices_each_get_only_their_own_private_reminder() {
        let pool = db::connect_memory().await.unwrap();
        let sam = two_owners_two_devices(&pool).await;
        let (st, mock) = state(&pool);
        assert_eq!(push::tick(&st, at_1830()).await.unwrap(), 2);
        assert_eq!(
            deliveries(&mock),
            vec![
                ("fcm1".to_string(), "ADMIN PRIVATE".to_string()),
                (format!("fcm{sam}"), "SAM PRIVATE".to_string()),
            ],
            "exactly one message per owner, to that owner's device only"
        );
        let mut markers: Vec<(i64, String)> =
            sqlx::query_as("SELECT user_id, item_id FROM reminders_sent")
                .fetch_all(&pool)
                .await
                .unwrap();
        markers.sort();
        assert_eq!(
            markers,
            vec![(1, "admin-item".to_string()), (sam, "sam-item".to_string())]
        );
        // A second pass sends nothing: each marker is per owner.
        assert_eq!(push::tick(&st, at_1830()).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn a_user_without_a_device_does_not_consume_or_leak_anothers_reminder() {
        let pool = db::connect_memory().await.unwrap();
        let sam = two_owners_two_devices(&pool).await;
        sqlx::query("DELETE FROM devices WHERE user_id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let (st, mock) = state(&pool);
        assert_eq!(push::tick(&st, at_1830()).await.unwrap(), 1);
        assert_eq!(
            deliveries(&mock),
            vec![(format!("fcm{sam}"), "SAM PRIVATE".to_string())]
        );
        // The admin's reminder is still unsent, so it goes out once a device registers.
        let markers: Vec<i64> = sqlx::query_scalar("SELECT user_id FROM reminders_sent")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(markers, vec![sam]);
    }

    #[tokio::test]
    async fn another_owners_completion_does_not_suppress_the_reminder() {
        let pool = db::connect_memory().await.unwrap();
        sqlx::query("UPDATE user_settings SET timezone = 'Australia/Perth' WHERE user_id = 1")
            .execute(&pool)
            .await
            .unwrap();
        let sam = users::create(&pool, "Sam", false).await.unwrap();
        sqlx::query("INSERT INTO items (user_id, id, kind, name, points, weekdays, remind_time, updated_at, seq) VALUES (1, 'shared', 'chore', 'ADMIN PRIVATE', 10, 127, '18:30', 1, 1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO devices (id, user_id, name, token_hash, fcm_token, created_at, last_seen) VALUES ('d1', 1, 'phone', 'h1', 'fcm1', 1, 1)")
            .execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, seq) VALUES (?, 'shared', '2026-10-25', 1, 1, 2)")
            .bind(sam).execute(&pool).await.unwrap();
        assert!(UserDb::new(&pool, 1)
            .reminder_completions("2026-10-25", "2026-10-24")
            .await
            .unwrap()
            .is_empty());
        let (st, mock) = state(&pool);
        assert_eq!(push::tick(&st, at_1830()).await.unwrap(), 1);
        assert_eq!(
            deliveries(&mock),
            vec![("fcm1".to_string(), "ADMIN PRIVATE".to_string())]
        );
    }

    #[tokio::test]
    async fn another_owners_todo_completion_and_sent_marker_stay_out_of_each_view() {
        let pool = db::connect_memory().await.unwrap();
        let sam = users::create(&pool, "Sam", false).await.unwrap();
        for uid in [1_i64, sam] {
            sqlx::query("INSERT INTO items (user_id, id, kind, name, points, weekdays, remind_time, due_date, updated_at, seq) VALUES (?, 'todo-1', 'todo', 'Pay', 10, 0, '09:00', '2026-10-25', 1, 1)")
                .bind(uid).execute(&pool).await.unwrap();
        }
        sqlx::query("INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, seq) VALUES (?, 'todo-1', '2026-10-01', 1, 1, 2)")
            .bind(sam).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO reminders_sent (user_id, item_id, date, sent_at) VALUES (?, 'todo-1', '2026-10-25', 1)")
            .bind(sam).execute(&pool).await.unwrap();
        let admin = UserDb::new(&pool, 1);
        let other = UserDb::new(&pool, sam);
        assert!(admin
            .reminder_completions("2026-10-25", "2026-10-24")
            .await
            .unwrap()
            .is_empty());
        assert!(admin
            .sent_reminders("2026-10-25", "2026-10-24")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            other
                .reminder_completions("2026-10-25", "2026-10-24")
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            other
                .sent_reminders("2026-10-25", "2026-10-24")
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn scheduler_item_selection_still_uses_the_restored_partial_index() {
        let pool = db::connect_memory().await.unwrap();
        let plan = UserDb::new(&pool, 1)
            .reminder_items_query_plan()
            .await
            .unwrap();
        assert!(
            plan.iter()
                .any(|r| r.3.contains("items_user_reminder_candidates")),
            "{plan:?}"
        );
        assert!(!plan.iter().any(|r| r.3.contains("SCAN items")), "{plan:?}");
    }
}

// ---- TRACK-90: raw SQL cannot leave a stale name key, and startup validates every key ----

async fn key_of(pool: &SqlitePool, id: i64) -> Option<String> {
    sqlx::query_scalar("SELECT name_key FROM users WHERE id = ?")
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn a_raw_rename_to_a_unicode_name_is_refused_instead_of_leaving_a_stale_key() {
    let pool = db::connect_memory().await.unwrap();
    let sam = users::create(&pool, "Sam", false).await.unwrap();
    let r = sqlx::query("UPDATE users SET name = 'Älex' WHERE id = ?")
        .bind(sam)
        .execute(&pool)
        .await;
    assert!(r.is_err(), "silent stale key");
    let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = ?")
        .bind(sam)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        (name.as_str(), key_of(&pool, sam).await.as_deref()),
        ("Sam", Some("sam")),
        "rolled back"
    );
    // Raw rename to ASCII keeps the key in step; users::rename handles Unicode.
    sqlx::query("UPDATE users SET name = 'Sammy' WHERE id = ?")
        .bind(sam)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(key_of(&pool, sam).await.as_deref(), Some("sammy"));
    users::rename(&pool, sam, "Älex").await.unwrap();
    assert_eq!(
        users::find_by_name(&pool, "äLEX")
            .await
            .unwrap()
            .unwrap()
            .id,
        sam
    );
    // A raw rename of a Unicode name to another Unicode name is refused too.
    assert!(sqlx::query("UPDATE users SET name = 'Élan' WHERE id = ?")
        .bind(sam)
        .execute(&pool)
        .await
        .is_err());
    // The supported raw path: name and cleared key together, recomputed at the next start.
    sqlx::query("UPDATE users SET name = 'Élan', name_key = NULL WHERE id = ?")
        .bind(sam)
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn startup_corrects_a_stale_key_and_the_correction_is_findable() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "stale.db");
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    let sam = users::create(&pool, "Sam", false).await.unwrap();
    // Bypass the guard on purpose (drop the trigger), as an old 0005-only database or a manual
    // edit could: new name, old key.
    sqlx::query("DROP TRIGGER users_name_key_rename_unicode")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET name = 'Älex' WHERE id = ?")
        .bind(sam)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        key_of(&pool, sam).await.as_deref(),
        Some("sam"),
        "stale on purpose"
    );
    pool.close().await;

    let pool = db::connect_with_admin(&u, None).await.unwrap();
    assert_eq!(key_of(&pool, sam).await, Some(users::name_key("Älex")));
    assert_eq!(
        users::find_by_name(&pool, "älex")
            .await
            .unwrap()
            .unwrap()
            .id,
        sam
    );
    assert!(matches!(
        users::create(&pool, "ÄLEX", false).await,
        Err(users::NameError::Taken)
    ));
    // The old key is free again.
    users::create(&pool, "sam", false).await.unwrap();
}

#[tokio::test]
async fn startup_with_caseless_duplicates_and_a_stale_key_fails_closed_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let u = url(&dir, "dup.db");
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    let a = users::create(&pool, "Alex", false).await.unwrap();
    let b = users::create(&pool, "Sam", false).await.unwrap();
    sqlx::query("UPDATE users SET name = 'Älex', name_key = ? WHERE id = ?")
        .bind(users::name_key("Älex"))
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    // Sam is renamed to a caseless duplicate of Älex while the key stays 'sam' (guard bypassed).
    sqlx::query("DROP TRIGGER users_name_key_rename_unicode")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET name = 'älex' WHERE id = ?")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    let before: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, name, name_key FROM users ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;

    // The identical failure on every start; the data is never "fixed" by overwriting a user.
    for _ in 0..2 {
        let err = db::connect_with_admin(&u, None)
            .await
            .expect_err("must refuse");
        let msg = err.to_string();
        assert!(
            msg.contains("Älex") && msg.contains("älex") && msg.contains("same name"),
            "{msg}"
        );
    }
    let raw = SqlitePoolOptions::new()
        .connect_with(SqliteConnectOptions::from_str(&u).unwrap())
        .await
        .unwrap();
    let after: Vec<(i64, String, Option<String>)> =
        sqlx::query_as("SELECT id, name, name_key FROM users ORDER BY id")
            .fetch_all(&raw)
            .await
            .unwrap();
    assert_eq!(before, after, "rolled back: no key or name was touched");

    // The documented operator remedy works, and the next start recomputes and succeeds.
    sqlx::query("UPDATE users SET name = 'Sam', name_key = NULL WHERE id = ?")
        .bind(b)
        .execute(&raw)
        .await
        .unwrap();
    raw.close().await;
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    assert_eq!(key_of(&pool, b).await.as_deref(), Some("sam"));
    assert_eq!(key_of(&pool, a).await, Some(users::name_key("Älex")));
}

#[tokio::test]
async fn startup_collision_refuses_before_a_listener_is_ever_bound() {
    // The server binary opens the database before binding or starting the scheduler, so a
    // refusal here is a refusal to serve. Prove it with the real binary on a real file.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("bin.db");
    let u = format!("sqlite://{}", db_path.display());
    let pool = db::connect_with_admin(&u, None).await.unwrap();
    let a = users::create(&pool, "Alex", false).await.unwrap();
    let b = users::create(&pool, "Sam", false).await.unwrap();
    sqlx::query("DROP TRIGGER users_name_key_rename_unicode")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET name = 'Älex' WHERE id = ?")
        .bind(b)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET name = 'älex' WHERE id = ?")
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let out = tokio::process::Command::new(env!("CARGO_BIN_EXE_bread-tracker-server"))
        .env("BREADTRACKER_DATABASE_PATH", &db_path)
        .env("BREADTRACKER_LISTEN", format!("127.0.0.1:{port}"))
        .output()
        .await
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("same name ignoring case"), "{stderr}");
    assert!(
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err(),
        "nothing is listening"
    );
}
