//! TRACK-82: install provenance decides the neutral timezone default.
//!
//! A database is "fresh" only when it was an empty file before the migration set began. The
//! current device count is not provenance: revoking or losing every device must not turn an
//! adopted household into a new installation. These tests build real 0001-only file databases,
//! upgrade them through `db::connect`, and check both the recorded marker and the timezone.
//!
//! The independent reviewer case that failed at the rejected head is reproduced in
//! `independent_reviewer_defaults_scenario`: a legacy 0001 database with a Perth owner and a
//! chore, zero current devices, upgraded to the current set.

use std::borrow::Cow;
use std::path::Path;
use std::str::FromStr;

use bread_tracker_server::{db, users};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

const CHORE: &str = "11111111-1111-4111-8111-111111111111";

fn url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

async fn open(path: &Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&url(path))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap()
}

/// Apply only migration 0001, as the last single-user release would have.
async fn only_0001(pool: &SqlitePool) {
    let mut m = sqlx::migrate!("../migrations");
    m.migrations = Cow::Owned(
        m.migrations
            .iter()
            .filter(|x| x.version == 1)
            .cloned()
            .collect(),
    );
    assert_eq!(m.migrations.len(), 1);
    m.run(pool).await.unwrap();
}

/// A legacy single-user database with real data, Perth and one paired device. `keep_device`
/// models an ordinary upgrade; false models the owner who revoked or lost every device.
async fn legacy_database(path: &Path, keep_device: bool) {
    let pool = open(path).await;
    only_0001(&pool).await;
    sqlx::query(
        "INSERT INTO items (id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq)
         VALUES (?, 'chore', 'Dishes', 127, '18:30', 10, NULL, 0, 1, 1000, NULL, 1)",
    )
    .bind(CHORE)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO completions (item_id, date, completed_at, updated_at, deleted_at, seq)
         VALUES (?, '2026-10-19', 2000, 2000, NULL, 2)",
    )
    .bind(CHORE)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE settings SET value = 'Australia/Perth' WHERE key = 'timezone'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE settings SET value = '22:00' WHERE key = 'quiet_start'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE settings SET value = '07:00' WHERE key = 'quiet_end'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO devices (id, name, token_hash, created_at, last_seen)
         VALUES ('dev-1', 'Pixel', 'hash-1', 777, 888)",
    )
    .execute(&pool)
    .await
    .unwrap();
    if !keep_device {
        sqlx::query("DELETE FROM devices")
            .execute(&pool)
            .await
            .unwrap();
    }
    pool.close().await;
}

async fn scalar_text(pool: &SqlitePool, sql: &str) -> String {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn count(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn admin_timezone(pool: &SqlitePool) -> String {
    scalar_text(pool, "SELECT timezone FROM user_settings WHERE user_id = 1").await
}

async fn provenance(pool: &SqlitePool) -> String {
    scalar_text(
        pool,
        "SELECT value FROM install_provenance WHERE key = 'install_kind'",
    )
    .await
}

#[tokio::test]
async fn revoked_all_devices_keep_the_household_timezone() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("revoked.db");
    legacy_database(&path, false).await;

    let pool = db::connect(&url(&path)).await.unwrap();
    assert_eq!(
        admin_timezone(&pool).await,
        "Australia/Perth",
        "zero current devices does not make an adopted household new"
    );
    assert_eq!(
        scalar_text(
            &pool,
            "SELECT quiet_start FROM user_settings WHERE user_id = 1"
        )
        .await,
        "22:00"
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 1);
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM completions").await, 1);
    assert_eq!(provenance(&pool).await, "populated");
    pool.close().await;

    // Repeated startup must not reclassify or re-run the repair.
    let pool = db::connect(&url(&path)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "Australia/Perth");
    assert_eq!(provenance(&pool).await, "populated");
    assert_eq!(
        count(&pool, "SELECT COUNT(*) FROM install_provenance").await,
        1
    );
}

#[tokio::test]
async fn offline_device_keeps_the_household_timezone() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("offline.db");
    legacy_database(&path, true).await;
    {
        // A device that has not been seen for a long time is offline, not absent.
        let pool = open(&path).await;
        sqlx::query("UPDATE devices SET last_seen = 1")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    }

    let pool = db::connect(&url(&path)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "Australia/Perth");
    assert_eq!(provenance(&pool).await, "populated");
}

#[tokio::test]
async fn fresh_file_gets_utc_and_records_fresh_provenance() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fresh.db");

    let pool = db::connect(&url(&path)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "UTC");
    assert_eq!(provenance(&pool).await, "fresh");
    pool.close().await;

    let pool = db::connect(&url(&path)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "UTC");
    assert_eq!(provenance(&pool).await, "fresh");
}

#[tokio::test]
async fn repeated_startup_keeps_an_operator_choice_on_a_fresh_install() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("fresh-choice.db");
    let first = db::connect(&url(&path)).await.unwrap();
    assert_eq!(admin_timezone(&first).await, "UTC");
    sqlx::query("UPDATE user_settings SET timezone = 'Australia/Perth' WHERE user_id = 1")
        .execute(&first)
        .await
        .unwrap();
    first.close().await;

    for round in 0..3 {
        let pool = db::connect(&url(&path)).await.unwrap();
        assert_eq!(
            admin_timezone(&pool).await,
            "Australia/Perth",
            "round {round}: the repair is a one-time migration, not a per-start reset"
        );
        assert_eq!(provenance(&pool).await, "fresh");
        pool.close().await;
    }
}

#[tokio::test]
async fn new_user_in_a_populated_database_gets_utc_for_that_user() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("populated-new-user.db");
    legacy_database(&path, false).await;

    let pool = db::connect(&url(&path)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "Australia/Perth");
    let sam = users::create(&pool, "Sam", false).await.unwrap();
    let settings: (String, String, String, bool) = sqlx::query_as(
        "SELECT timezone, quiet_start, quiet_end, reminders_enabled FROM user_settings WHERE user_id = ?",
    )
    .bind(sam)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        settings,
        ("UTC".to_string(), String::new(), String::new(), true)
    );
    assert_eq!(
        admin_timezone(&pool).await,
        "Australia/Perth",
        "creating a user must not touch the adopted owner"
    );
}

#[tokio::test]
async fn direct_migration_run_without_the_recording_helper_preserves_settings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("direct.db");
    legacy_database(&path, false).await;

    // A ledger built without db::prepare (a direct sqlx::migrate!, an older binary) has no
    // recorded provenance. It must be treated as populated rather than reset to UTC.
    let pool = open(&path).await;
    sqlx::migrate!("../migrations").run(&pool).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "Australia/Perth");
    assert_eq!(provenance(&pool).await, "populated");
}

/// The independent reviewer case from the bug report: a populated legacy owner in
/// Australia/Perth with zero current devices was upgraded to UTC, shifting reminder and day
/// semantics. Fresh installs, reopened databases and new users must still get the neutral UTC
/// default, so this keeps both halves in one scenario.
#[tokio::test]
async fn independent_reviewer_defaults_scenario() {
    let directory = tempfile::tempdir().unwrap();

    // Populated Perth owner, all devices revoked or removed.
    let populated = directory.path().join("reviewer-populated.db");
    legacy_database(&populated, false).await;
    let pool = db::connect(&url(&populated)).await.unwrap();
    assert_eq!(
        admin_timezone(&pool).await,
        "Australia/Perth",
        "UTC vs Perth regression"
    );
    assert_eq!(count(&pool, "SELECT COUNT(*) FROM items").await, 1);
    pool.close().await;

    // Reopening keeps it.
    let pool = db::connect(&url(&populated)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "Australia/Perth");
    let new_user = users::create(&pool, "Robin", false).await.unwrap();
    let tz: String = sqlx::query_scalar("SELECT timezone FROM user_settings WHERE user_id = ?")
        .bind(new_user)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tz, "UTC", "a new user always gets the neutral default");
    pool.close().await;

    // A brand new file still gets UTC for its owner.
    let blank = directory.path().join("reviewer-fresh.db");
    let pool = db::connect(&url(&blank)).await.unwrap();
    assert_eq!(admin_timezone(&pool).await, "UTC");
    assert_eq!(provenance(&pool).await, "fresh");
}
