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
    sqlx::query("UPDATE settings SET value = 'Europe/Berlin' WHERE key = 'timezone'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;

    for restart in [None, Some("Riley"), Some("Someone else"), Some("admin")] {
        let pool = db::connect_with_admin(&u, restart).await.unwrap();
        assert_eq!(admin_name(&pool).await, "Riley", "restart with {restart:?}");
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 1);
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 1);
        let tz: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = 'timezone'")
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
                auth::redeem(&pool, &code, &format!("racer {n}")).await
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

// ---- no partial multi-user exposure ----

#[tokio::test]
async fn a_second_users_device_and_data_are_not_served_before_scoping_exists() {
    let pool = db::connect_memory().await.unwrap();
    let sam = users::create(&pool, "Sam", false).await.unwrap();
    sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (?, '11111111-1111-4111-8111-111111111111', 'chore', 'SAM PRIVATE', 10, 1, 1)")
        .bind(sam).execute(&pool).await.unwrap();
    sqlx::query("UPDATE meta SET val = 1 WHERE key = 'seq'")
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
        Config::from_env(),
        Arc::new(MockSender::default()),
    ));
    let get = |token: &str| {
        Request::builder()
            .uri("/sync?since=0")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    // The admin never sees Sam's row (pull is bound to the first user)...
    let resp = app.clone().oneshot(get(&admin_token)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["items"].as_array().unwrap().len(), 0, "{v}");
    assert!(!String::from_utf8_lossy(&body).contains("SAM PRIVATE"));
    // ...and Sam's own device is refused outright until the scoping change gives Sam a view.
    let resp = app.oneshot(get("sam-token")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}
