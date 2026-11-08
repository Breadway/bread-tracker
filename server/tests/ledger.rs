//! Real SQLx ledger behavior for the multi-user schema, on populated file databases.
//!
//! The default branch has applied 0001 (init) and 0003 (hygiene indexes). The durable pairing
//! limiter (PR31) reserves 0004. This branch adds 0002 (users, which rebuilds five tables) and
//! the additive forward repair 0005. SQLx applies any version it has not recorded, so the
//! ledgers a deployment can be in are all upgraded by the same migration set; these tests
//! build each ledger for real, fill it with data, upgrade it, and check what survived.

use std::borrow::Cow;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use bread_tracker_server::db;
use sha2::{Digest, Sha384};
use sqlx::migrate::{Migration, MigrationType, Migrator};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::SqlitePool;

const I1: &str = "11111111-1111-4111-8111-111111111111";
const I2: &str = "22222222-2222-4222-8222-222222222222";
const I3: &str = "33333333-3333-4333-8333-333333333333";

/// Published checksums (SHA-384 of the file). These migrations are applied on real databases
/// and must never change.
const SHA_0001: &str = "43b80a728988ff17392c658746718dfd9cb3fe020834569cd8488bb59db8bc64358b893275b6dac6d392dc85f24c176c";
const SHA_0003: &str = "30ac24599e921d7a167a83c6df30d88b388258eb10bf3ac4625d50920d42c8873d75a661b6bb93bafd262a0581f048e0";
/// 0002 and 0005 are published (PR43 e4195dd and 6f9986e) and may be recorded on real databases.
const SHA_0002: &str = "f74d93be96b8fda33d2425e9baa9155aa485e71ccb6b556ba28dbeb0a8140e1409fbffa18b14d02ea00b1af8df73ff94";
const SHA_0005: &str = "a54fae5719b8b335d52b072f5a82fe3eeed050e18e715e5f8143fd82b9b02c0359c31ae5555567a9a4aaff3cd1c008d3";
const SHA_0004: &str = "229011bcf178a2afa3eb4277c56a4866e4cc51a292418b12ea336d0b1ae0975b17ae0a53abf71a4511abcb67ed7aa60e";

/// The 0004 file as PR31 publishes it, copied byte for byte so the combined tree can be tested
/// before PR31 is integrated.
const SQL_0004: &str = include_str!("fixtures/0004_pair_rate_limits.sql");

/// The four indexes migration 0003 creates.
const HYGIENE_INDEXES: [&str; 4] = [
    "items_kind_id",
    "items_reminder_candidates",
    "completions_date_item",
    "completions_live_date",
];

/// Added by 0005: keeps the scheduler's lookup by item id indexed under the per-user key.
const REPAIR_INDEX: &str = "completions_item_date";

fn embedded() -> Migrator {
    sqlx::migrate!("../migrations")
}

/// The embedded set restricted to `versions`, plus PR31's 0004 when asked for.
fn migrator(versions: &[i64], with_0004: bool) -> Migrator {
    let mut m = embedded();
    let mut list: Vec<Migration> = m
        .migrations
        .iter()
        .filter(|x| versions.contains(&x.version))
        .cloned()
        .collect();
    if with_0004 {
        list.push(Migration::new(
            4,
            Cow::Borrowed("pair rate limits"),
            MigrationType::Simple,
            Cow::Borrowed(SQL_0004),
            false,
        ));
    }
    list.sort_by_key(|x| x.version);
    m.migrations = Cow::Owned(list);
    m
}

/// Every migration this branch ships, plus PR31's 0004: the tree after both are integrated.
fn combined() -> Migrator {
    let all: Vec<i64> = embedded().migrations.iter().map(|m| m.version).collect();
    migrator(&all, true)
}

async fn open(path: &Path) -> SqlitePool {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true)
        .foreign_keys(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap()
}

fn url(path: &Path) -> String {
    format!("sqlite://{}", path.display())
}

/// Close a pool and make sure no connection it owned is left for the next open to race.
///
/// `SqlitePool::close()` in sqlx 0.8.6 can return while a connection is still closing, and can
/// leave an idle connection untouched until the pool itself is dropped (launchbadge/sqlx#3217,
/// fixed only in sqlx 0.9). Dropping the pool here forces those connections through their WAL
/// checkpoint instead of letting the next open race them. A reopening that still meets a busy
/// database is absorbed by the bounded retry in `db::connect_with_migrator`.
async fn close_pool(pool: SqlitePool) {
    pool.close().await;
    drop(pool);
}

/// Fill a 0001 database the way a live single-user server would.
async fn populate_0001(pool: &SqlitePool) {
    let q = |sql: &'static str| sqlx::query(sql);
    q("INSERT INTO items (id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq)
       VALUES ('11111111-1111-4111-8111-111111111111', 'chore', 'Dishes', 127, '18:30', 10, NULL, 0, 1, 1000, NULL, 1),
              ('22222222-2222-4222-8222-222222222222', 'habit', 'Read', 127, NULL, 15, NULL, 1, 1, 1000, 5000, 3),
              ('33333333-3333-4333-8333-333333333333', 'todo', 'Rent', 0, '09:00', 20, '2026-10-30', 2, 1, 1000, NULL, 6)")
        .execute(pool).await.unwrap();
    q(
        "INSERT INTO completions (item_id, date, completed_at, updated_at, deleted_at, seq)
       VALUES ('11111111-1111-4111-8111-111111111111', '2026-10-19', 2000, 2000, NULL, 2),
              ('11111111-1111-4111-8111-111111111111', '2026-10-20', 3000, 3000, 4000, 4),
              ('22222222-2222-4222-8222-222222222222', '2026-10-19', 2000, 2000, NULL, 5)",
    )
    .execute(pool)
    .await
    .unwrap();
    q("UPDATE meta SET val = 6 WHERE key = 'seq'")
        .execute(pool)
        .await
        .unwrap();
    q("INSERT INTO devices (id, name, token_hash, fcm_token, created_at, last_seen) VALUES ('dev-1', 'Pixel', 'tok-1', 'fcm-1', 777, 888), ('dev-2', 'Tablet', 'tok-2', NULL, 900, 900)")
        .execute(pool).await.unwrap();
    q("INSERT INTO pairing_codes (code_hash, expires_at) VALUES ('code-hash', 99999999999999)")
        .execute(pool)
        .await
        .unwrap();
    q("INSERT INTO reminders_sent (item_id, date, sent_at) VALUES ('11111111-1111-4111-8111-111111111111', '2026-10-20', 5)")
        .execute(pool).await.unwrap();
    q("UPDATE settings SET value = 'Europe/Berlin' WHERE key = 'timezone'")
        .execute(pool)
        .await
        .unwrap();
    q("UPDATE settings SET value = '22:00' WHERE key = 'quiet_start'")
        .execute(pool)
        .await
        .unwrap();
}

/// Build a populated ledger: 0001 with data, then the later versions in `then` (already
/// deployed by an earlier release), exactly as SQLx would have recorded them.
async fn deployed(path: &Path, then: &[i64], with_0004: bool) {
    let pool = open(path).await;
    migrator(&[1], false).run(&pool).await.unwrap();
    populate_0001(&pool).await;
    if !then.is_empty() || with_0004 {
        let mut versions = vec![1];
        versions.extend_from_slice(then);
        migrator(&versions, with_0004).run(&pool).await.unwrap();
    }
    close_pool(pool).await;
}

async fn scalar(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn index_names(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'index' ORDER BY name")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn ledger(pool: &SqlitePool) -> Vec<(i64, String, bool)> {
    sqlx::query_as::<_, (i64, Vec<u8>, bool)>(
        "SELECT version, checksum, success FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|(v, c, s)| (v, hex::encode(c), s))
    .collect()
}

fn checksum_of(sql: &str) -> String {
    hex::encode(Sha384::digest(sql.as_bytes()))
}

/// Everything the upgrade must preserve, whichever ledger it started from.
async fn assert_upgraded_and_intact(pool: &SqlitePool) {
    // Row counts unchanged and every row owned by the admin.
    assert_eq!(scalar(pool, "SELECT COUNT(*) FROM items").await, 3);
    assert_eq!(scalar(pool, "SELECT COUNT(*) FROM completions").await, 3);
    assert_eq!(scalar(pool, "SELECT COUNT(*) FROM devices").await, 2);
    assert_eq!(scalar(pool, "SELECT COUNT(*) FROM pairing_codes").await, 1);
    assert_eq!(scalar(pool, "SELECT COUNT(*) FROM reminders_sent").await, 1);
    assert_eq!(scalar(pool, "SELECT COUNT(*) FROM users").await, 1);
    for table in [
        "items",
        "completions",
        "devices",
        "pairing_codes",
        "reminders_sent",
    ] {
        assert_eq!(
            scalar(
                pool,
                &format!("SELECT COUNT(*) FROM {table} WHERE user_id = 1")
            )
            .await,
            scalar(pool, &format!("SELECT COUNT(*) FROM {table}")).await,
            "{table}"
        );
    }
    // Values, tombstones, sequence numbers, tokens, global settings.
    let item: (String, i64, Option<i64>, i64) =
        sqlx::query_as("SELECT name, points, deleted_at, seq FROM items WHERE id = ?")
            .bind(I2)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(item, ("Read".to_string(), 15, Some(5000), 3));
    // TRACK-65: the column is additive, and every item that predates it has no lower bound.
    assert_eq!(
        scalar(
            pool,
            "SELECT COUNT(*) FROM pragma_table_info('items') WHERE name = 'created_on'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(pool, "SELECT COUNT(*) FROM items WHERE created_on IS NULL").await,
        3,
        "pre-existing items must keep no lower bound"
    );
    let dev: (String, Option<String>, i64, i64) = sqlx::query_as(
        "SELECT name, fcm_token, created_at, last_seen FROM devices WHERE token_hash = 'tok-1'",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(dev, ("Pixel".into(), Some("fcm-1".into()), 777, 888));
    assert_eq!(
        scalar(pool, "SELECT val FROM meta WHERE key = 'seq'").await,
        6
    );
    let tz: String = sqlx::query_scalar("SELECT timezone FROM user_settings WHERE user_id = 1")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(tz, "Europe/Berlin");
    // TRACK-82: a database that had a schema before this run is recorded as populated, so its
    // adopted settings survive even when every device is later revoked.
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT value FROM install_provenance WHERE key = 'install_kind'"
        )
        .fetch_one(pool)
        .await
        .unwrap(),
        "populated"
    );
    let quiet: String =
        sqlx::query_scalar("SELECT quiet_start FROM user_settings WHERE user_id = 1")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(quiet, "22:00");

    // The hygiene indexes from 0003 exist, whatever order 0002 and 0003 ran in.
    let names = index_names(pool).await;
    for idx in HYGIENE_INDEXES {
        assert!(names.contains(&idx.to_string()), "{idx} missing: {names:?}");
    }
    for idx in [
        "items_user_seq",
        "completions_user_seq",
        "users_name_key",
        REPAIR_INDEX,
    ] {
        assert!(names.contains(&idx.to_string()), "{idx} missing: {names:?}");
    }
    // The old global sync indexes went with the old tables and are not resurrected.
    assert!(!names.contains(&"items_seq".to_string()));

    // The per-user schema is really there.
    assert_eq!(
        scalar(
            pool,
            "SELECT COUNT(*) FROM pragma_table_info('items') WHERE name = 'user_id'"
        )
        .await,
        1
    );
    assert_eq!(
        scalar(
            pool,
            "SELECT COUNT(*) FROM pragma_foreign_key_list('items') WHERE \"table\" = 'users' AND on_delete = 'CASCADE'"
        )
        .await,
        1
    );

    let violations: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(pool)
        .await
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(integrity, "ok");

    // The scheduler's selection still uses the 0003 partial index after the rebuild.
    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "EXPLAIN QUERY PLAN SELECT id FROM items WHERE active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    assert!(
        plan.iter()
            .any(|r| r.3.contains("items_reminder_candidates")),
        "{plan:?}"
    );
}

/// Applied migrations must keep their published checksums.
fn assert_ledger(rows: &[(i64, String, bool)], expected: &[i64], with_0004: bool) {
    let versions: Vec<i64> = rows.iter().map(|r| r.0).collect();
    assert_eq!(versions, expected, "{rows:?}");
    assert!(
        rows.iter().all(|r| r.2),
        "a recorded migration failed: {rows:?}"
    );
    let sum = |v: i64| rows.iter().find(|r| r.0 == v).map(|r| r.1.as_str());
    assert_eq!(sum(1), Some(SHA_0001), "0001 must be byte-identical");
    assert_eq!(sum(2), Some(SHA_0002), "0002 must be byte-identical");
    assert_eq!(sum(3), Some(SHA_0003), "0003 must be byte-identical");
    assert_eq!(sum(5), Some(SHA_0005), "0005 must be byte-identical");
    if with_0004 {
        assert_eq!(sum(4), Some(SHA_0004), "0004 must be byte-identical");
    }
}

#[test]
fn published_migration_files_keep_their_checksums() {
    let m = combined();
    let sum = |v: i64| {
        hex::encode(
            &m.migrations
                .iter()
                .find(|x| x.version == v)
                .unwrap()
                .checksum[..],
        )
    };
    assert_eq!(sum(1), SHA_0001);
    assert_eq!(sum(2), SHA_0002);
    assert_eq!(sum(3), SHA_0003);
    assert_eq!(sum(4), SHA_0004);
    assert_eq!(sum(5), SHA_0005);
    assert_eq!(checksum_of(SQL_0004), SHA_0004);
    // Versions are unique and strictly increasing; nothing collides with 0004.
    let versions: Vec<i64> = m.migrations.iter().map(|x| x.version).collect();
    assert_eq!(versions, vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]);
}

#[tokio::test]
async fn upgrade_from_0001_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.db");
    deployed(&path, &[], false).await;

    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        false,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn upgrade_from_0001_and_0003_restores_the_hygiene_indexes_0002_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("b.db");
    deployed(&path, &[3], false).await;

    // Precondition, on the real file: 0003 is applied and its indexes exist.
    let pool = open(&path).await;
    let before = index_names(&pool).await;
    for idx in HYGIENE_INDEXES {
        assert!(before.contains(&idx.to_string()), "{idx}");
    }
    close_pool(pool).await;

    // The unrepaired late 0002 (this branch without 0005) drops them, and SQLx will never
    // rerun the recorded 0003. This documents the defect the forward repair exists for.
    let broken = dir.path().join("b-unrepaired.db");
    std::fs::copy(&path, &broken).unwrap();
    let pool = open(&broken).await;
    migrator(&[1, 2, 3], false).run(&pool).await.unwrap();
    let lost = index_names(&pool).await;
    for idx in ["items_kind_id", "completions_date_item"] {
        assert!(!lost.contains(&idx.to_string()), "{idx} should be gone");
    }
    // ...and running the repair afterwards restores them (what an operator's upgrade does).
    migrator(&[1, 2, 3, 5, 6, 7], false)
        .run(&pool)
        .await
        .unwrap();
    assert!(index_names(&pool)
        .await
        .contains(&"items_kind_id".to_string()));
    close_pool(pool).await;

    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        false,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn upgrade_from_0001_0003_and_0004() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.db");
    deployed(&path, &[3], true).await;

    let pool = db::connect_with_migrator(&url(&path), None, &combined())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        true,
    );
    assert_upgraded_and_intact(&pool).await;
    // 0004's table was not touched by the rebuild.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM pair_rate_limits").await,
        0
    );
}

#[tokio::test]
async fn upgrade_from_0001_and_0004_without_0003() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c2.db");
    deployed(&path, &[], true).await;
    let pool = db::connect_with_migrator(&url(&path), None, &combined())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        true,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn upgrade_from_the_published_pr43_head_0001_and_0002() {
    // A deployment that ran PR43 at e4195dd (0002 only) gets 0003 and 0005 on the next start.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("d.db");
    deployed(&path, &[2], false).await;
    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        false,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn upgrade_from_0001_0002_0003_in_that_order() {
    // 0002 then 0003 is the order a fresh install takes. No index was ever lost.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("e.db");
    deployed(&path, &[2, 3], false).await;
    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        false,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn fresh_database_gets_the_whole_set_in_version_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fresh.db");
    let pool = db::connect_with_migrator(&url(&path), None, &combined())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        true,
    );
    let names = index_names(&pool).await;
    for idx in HYGIENE_INDEXES {
        assert!(names.contains(&idx.to_string()), "{idx}");
    }
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM users").await, 1);
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT value FROM install_provenance WHERE key = 'install_kind'"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        "fresh"
    );
    let tz: String = sqlx::query_scalar("SELECT timezone FROM user_settings WHERE user_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(tz, "UTC", "a new installation gets the neutral default");
}

#[tokio::test]
async fn a_second_start_is_a_no_op_on_every_ledger() {
    for (name, then, with_0004) in [
        ("a", vec![], false),
        ("b", vec![3], false),
        ("c", vec![3], true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.db"));
        deployed(&path, &then, with_0004).await;
        let m = if with_0004 { combined() } else { embedded() };
        let pool = db::connect_with_migrator(&url(&path), Some("Riley"), &m)
            .await
            .unwrap();
        let first = ledger(&pool).await;
        let users: (i64, String) = sqlx::query_as("SELECT id, name FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        close_pool(pool).await;
        let pool = db::connect_with_migrator(&url(&path), Some("Riley"), &m)
            .await
            .unwrap();
        assert_eq!(ledger(&pool).await, first, "{name}");
        let again: (i64, String) = sqlx::query_as("SELECT id, name FROM users")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(users, again, "{name}");
        assert_upgraded_and_intact(&pool).await;
    }
}

#[tokio::test]
async fn reopening_after_close_retries_while_a_writer_holds_the_lock() {
    // TRACK-92: the second start reopened the file immediately after `pool.close().await` and,
    // on a stalled CI disk, hit `(code: 5) database is locked` because the previous connection
    // was still closing and checkpointing the WAL. `close_pool` now drops the pool so no
    // connection is left behind, and the open path retries BUSY with bounded backoff. Force
    // both halves here: a writer holds the write lock past the 5s connection busy timeout
    // while the reopen runs, so an unretried open fails, then a loop exercises the ordinary
    // close/reopen path.
    use sqlx::Connection;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("locked.db");
    deployed(&path, &[3], false).await;

    let mut writer = sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::from_str(&url(&path))
            .unwrap()
            .busy_timeout(Duration::from_secs(5)),
    )
    .await
    .unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut writer)
        .await
        .unwrap();
    sqlx::query("UPDATE meta SET val = val WHERE key = 'seq'")
        .execute(&mut writer)
        .await
        .unwrap();

    let release = tokio::spawn(async move {
        // Longer than the 5s busy_timeout, so the first open attempt cannot wait it out.
        tokio::time::sleep(Duration::from_millis(5_600)).await;
        sqlx::query("COMMIT").execute(&mut writer).await.unwrap();
    });

    let reopened = db::connect_with_migrator(&url(&path), None, &embedded()).await;
    release.await.unwrap();
    let pool = reopened.expect(
        "a reopen racing a held write lock must retry SQLITE_BUSY, not fail the second start",
    );
    let expected = ledger(&pool).await;
    assert_eq!(expected.len(), 12);
    close_pool(pool).await;

    for round in 0..24 {
        let pool = db::connect_with_migrator(&url(&path), None, &embedded())
            .await
            .unwrap_or_else(|err| panic!("reopen {round} failed: {err:#}"));
        assert_eq!(ledger(&pool).await, expected, "round {round}");
        close_pool(pool).await;
    }
}

#[tokio::test]
async fn this_branch_alone_refuses_a_database_that_already_ran_0004() {
    // PR43 without PR31's migration cannot adopt a database that recorded 0004: SQLx reports
    // the missing version and the server refuses to start instead of guessing. This is why
    // the stack must integrate the limiter's 0004 before (or with) this branch.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("needs0004.db");
    deployed(&path, &[3], true).await;
    let err = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .expect_err("must refuse")
        .to_string();
    assert!(err.contains("4"), "{err}");
    // Nothing was changed by the refusal: still the 0001+0003+0004 ledger with legacy tables.
    let pool = open(&path).await;
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM pragma_table_info('items') WHERE name = 'user_id'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM _sqlx_migrations").await,
        3
    );
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
}

#[tokio::test]
async fn an_edited_published_migration_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("edited.db");
    deployed(&path, &[3], false).await;
    let mut m = embedded();
    let mut list = m.migrations.to_vec();
    for x in &mut list {
        if x.version == 3 {
            *x = Migration::new(
                3,
                x.description.clone(),
                MigrationType::Simple,
                Cow::Owned(format!("{}\n-- edited", x.sql)),
                false,
            );
        }
    }
    m.migrations = Cow::Owned(list);
    let err = db::connect_with_migrator(&url(&path), None, &m)
        .await
        .expect_err("must refuse")
        .to_string();
    assert!(err.contains("3"), "{err}");
}

/// A migrator whose version `v` carries an extra failing statement, to prove rollback.
fn failing_at(mut m: Migrator, v: i64) -> Migrator {
    let list: Vec<Migration> = m
        .migrations
        .iter()
        .map(|x| {
            if x.version == v {
                Migration::new(
                    x.version,
                    x.description.clone(),
                    MigrationType::Simple,
                    Cow::Owned(format!(
                        "{}\nSELECT * FROM table_that_does_not_exist;",
                        x.sql
                    )),
                    false,
                )
            } else {
                x.clone()
            }
        })
        .collect();
    m.migrations = Cow::Owned(list);
    m
}

#[tokio::test]
async fn a_failed_0002_rolls_back_completely_and_a_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fail2.db");
    deployed(&path, &[3], true).await;

    let err = db::connect_with_migrator(&url(&path), None, &failing_at(combined(), 2)).await;
    assert!(err.is_err());

    // The file is exactly the 0001+0003+0004 database it was: legacy tables, all data, indexes.
    let pool = open(&path).await;
    assert_eq!(
        ledger(&pool).await.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1, 3, 4]
    );
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM completions").await, 3);
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM devices").await, 2);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN ('users', 'items_new', 'completions_new', 'devices_new')"
        )
        .await,
        0
    );
    let names = index_names(&pool).await;
    for idx in HYGIENE_INDEXES {
        assert!(names.contains(&idx.to_string()), "{idx}");
    }
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM pragma_table_info('devices') WHERE name = 'user_id'"
        )
        .await,
        0
    );
    close_pool(pool).await;

    let pool = db::connect_with_migrator(&url(&path), None, &combined())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        true,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn a_failed_0005_keeps_the_0002_result_and_a_retry_completes_the_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fail5.db");
    deployed(&path, &[3], false).await;

    let err = db::connect_with_migrator(&url(&path), None, &failing_at(embedded(), 5)).await;
    assert!(err.is_err());

    // 0002 committed in its own transaction. 0005 left nothing behind: no name_key column,
    // no partial index, nothing recorded.
    let pool = open(&path).await;
    assert_eq!(
        ledger(&pool).await.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM pragma_table_info('users') WHERE name IN ('name_key', 'name_confirmed')"
        )
        .await,
        0
    );
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM items WHERE user_id = 1").await,
        3
    );
    close_pool(pool).await;

    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        false,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn sync_pull_searches_the_per_user_sequence_indexes() {
    let pool = db::connect_memory().await.unwrap();
    let plans = bread_tracker_server::store::UserDb::new(&pool, 1)
        .pull_query_plans()
        .await
        .unwrap();
    let joined = plans.join(" | ");
    assert!(joined.contains("items_user_seq"), "{joined}");
    assert!(joined.contains("completions_user_seq"), "{joined}");
    assert!(!joined.contains("SCAN"), "{joined}");
    assert!(!joined.contains("TEMP B-TREE"), "{joined}");
}

#[tokio::test]
async fn items_belong_to_their_user_and_removing_one_user_keeps_the_rest() {
    // The sibling of the migration: after upgrading, a second user holds the same ids.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("two.db");
    deployed(&path, &[3], false).await;
    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    let sam = bread_tracker_server::users::create(&pool, "Sam", false)
        .await
        .unwrap();
    sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (?, ?, 'chore', 'Sam', 1, 1, 7)")
        .bind(sam)
        .bind(I1)
        .execute(&pool)
        .await
        .unwrap();
    assert!(sqlx::query("INSERT INTO items (user_id, id, kind, name, points, updated_at, seq) VALUES (?, ?, 'chore', 'dup', 1, 1, 8)")
        .bind(sam)
        .bind(I1)
        .execute(&pool)
        .await
        .is_err());
    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(sam)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
    let _ = I3;
}

/// Run exactly what PR43 head 6f9986e shipped (0001, 0002, 0003, 0005) on `path`, so the
/// database is in the state of a deployment that already upgraded with that release.
async fn upgrade_like_6f9986e(path: &Path, with_0004: bool) {
    let pool = open(path).await;
    migrator(&[1, 2, 3, 5], with_0004).run(&pool).await.unwrap();
    close_pool(pool).await;
}

#[tokio::test]
async fn a_database_already_upgraded_by_6f9986e_receives_only_the_new_0006() {
    for (name, then, with_0004) in [
        ("a", vec![], false),
        ("b", vec![3], false),
        ("c", vec![3], true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{name}.db"));
        deployed(&path, &then, with_0004).await;
        upgrade_like_6f9986e(&path, with_0004).await;
        let pool = open(&path).await;
        let before = ledger(&pool).await;
        assert_eq!(
            before.iter().map(|r| r.0).collect::<Vec<_>>(),
            vec![1, 2, 3, 5]
                .into_iter()
                .chain(if with_0004 { Some(4) } else { None })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        );
        close_pool(pool).await;

        let m = if with_0004 { combined() } else { embedded() };
        let pool = db::connect_with_migrator(&url(&path), None, &m)
            .await
            .unwrap();
        let after = ledger(&pool).await;
        // Every existing migration is untouched; guard/settings/invites/admission are additive.
        assert_eq!(after.len(), before.len() + 8, "{name}");
        for row in &before {
            assert!(after.contains(row), "{name}: {row:?} changed");
        }
        assert_eq!(after.last().unwrap().0, 13);
        assert_upgraded_and_intact(&pool).await;
    }
}

#[tokio::test]
async fn a_failed_0006_keeps_everything_before_it_and_a_retry_completes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fail6.db");
    deployed(&path, &[3], false).await;
    upgrade_like_6f9986e(&path, false).await;

    let err = db::connect_with_migrator(&url(&path), None, &failing_at(embedded(), 6)).await;
    assert!(err.is_err());
    let pool = open(&path).await;
    assert_eq!(
        ledger(&pool).await.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1, 2, 3, 5]
    );
    // 0006 changed nothing: the 0005 rename trigger is still the one in place.
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM sqlite_master WHERE type = 'trigger' AND name = 'users_name_key_rename'").await,
        1
    );
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
    close_pool(pool).await;

    let pool = db::connect_with_migrator(&url(&path), None, &embedded())
        .await
        .unwrap();
    assert_ledger(
        &ledger(&pool).await,
        &[1, 2, 3, 5, 6, 7, 8, 9, 10, 11, 12, 13],
        false,
    );
    assert_upgraded_and_intact(&pool).await;
}

#[tokio::test]
async fn experimental_invites004_is_rejected_without_rewriting_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("old-invites.db");
    deployed(&path, &[2], false).await;
    let pool = open(&path).await;
    let checksum = hex::decode("ea989dda2ed972c914b78d423bc7dff9f81f5043252d7b616c287c8cbd82c1645ff96462022c625bd3e286c110bb1ed6").unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations(version, description, success, checksum, execution_time) VALUES (4, 'invites', 1, ?, 0)")
        .bind(checksum).execute(&pool).await.unwrap();
    let before = ledger(&pool).await;
    close_pool(pool).await;
    let error = db::connect(&url(&path)).await.unwrap_err();
    assert!(error.to_string().contains("experimental invites004"));
    let pool = open(&path).await;
    assert_eq!(ledger(&pool).await, before);
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
}

#[tokio::test]
async fn experimental_settings003_is_rejected_without_rewriting_any_ledger_or_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("experiment.db");
    deployed(&path, &[2], false).await;
    let pool = open(&path).await;
    let mut old = migrator(&[1, 2], false);
    let mut list = old.migrations.to_vec();
    list.push(Migration::new(
        3,
        Cow::Borrowed("user settings"),
        MigrationType::Simple,
        Cow::Borrowed(include_str!("fixtures/experimental_0003_user_settings.sql")),
        false,
    ));
    old.migrations = Cow::Owned(list);
    old.run(&pool).await.unwrap();
    let before = ledger(&pool).await;
    let settings: (String, String) =
        sqlx::query_as("SELECT timezone,quiet_start FROM user_settings WHERE user_id=1")
            .fetch_one(&pool)
            .await
            .unwrap();
    close_pool(pool).await;
    let err = db::connect(&url(&path)).await.unwrap_err().to_string();
    assert!(
        err.contains("unsupported experimental settings003 ledger")
            && err.contains("recreate ONLY"),
        "{err}"
    );
    let pool = open(&path).await;
    assert_eq!(ledger(&pool).await, before);
    assert_eq!(
        sqlx::query_as::<_, (String, String)>(
            "SELECT timezone,quiet_start FROM user_settings WHERE user_id=1"
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        settings
    );
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
}

#[tokio::test]
async fn experimental_neutral_timezone_0005_is_rejected_without_rewriting_data() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("neutral-0005.db");
    deployed(&path, &[2], false).await;
    let pool = open(&path).await;
    let checksum = hex::decode("401b8424ab749e4d1644fee4ea86f401143869ed8e2f227f46475ae519bf92b5fd59c3106f4be298bc57f92f58d7ae11").unwrap();
    sqlx::query("INSERT INTO _sqlx_migrations(version, description, success, checksum, execution_time) VALUES (5, 'neutral default timezone', 1, ?, 0)")
        .bind(checksum).execute(&pool).await.unwrap();
    let before = ledger(&pool).await;
    close_pool(pool).await;
    let err = db::connect(&url(&path)).await.unwrap_err().to_string();
    assert!(
        err.contains("neutral-timezone 0005 ledger") && err.contains("cannot be reconstructed"),
        "{err}"
    );
    let pool = open(&path).await;
    assert_eq!(ledger(&pool).await, before);
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM items").await, 3);
    // The legacy timezone is untouched: a rejection never rewrites data.
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = 'timezone'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "Europe/Berlin"
    );
}

#[tokio::test]
async fn already_published_upstream_0006_gets_only_settings0007_with_values_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("upstream.db");
    deployed(&path, &[2, 3, 5, 6], false).await;
    let pool = open(&path).await;
    let before = ledger(&pool).await;
    close_pool(pool).await;
    let pool = db::connect(&url(&path)).await.unwrap();
    let after = ledger(&pool).await;
    assert_eq!(after.len(), before.len() + 7);
    for row in before {
        assert!(after.contains(&row));
    }
    assert_eq!(after.last().unwrap().0, 13);
    assert_upgraded_and_intact(&pool).await;
    let second = bread_tracker_server::users::create(&pool, "New user", false)
        .await
        .unwrap();
    let settings = bread_tracker_server::store::UserDb::new(&pool, second)
        .settings()
        .await
        .unwrap();
    assert_eq!(
        (
            settings.timezone.as_str(),
            settings.quiet_start.as_str(),
            settings.reminders_enabled
        ),
        ("UTC", "", true)
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='settings'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn failed_settings0007_rolls_back_the_drop_copy_and_indexes_and_retry_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fail7.db");
    deployed(&path, &[2, 3, 5, 6], false).await;
    assert!(
        db::connect_with_migrator(&url(&path), None, &failing_at(embedded(), 7))
            .await
            .is_err()
    );
    let pool = open(&path).await;
    assert_eq!(scalar(&pool, "SELECT COUNT(*) FROM settings").await, 4);
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE name='user_settings'"
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM _sqlx_migrations WHERE version=7"
        )
        .await,
        0
    );
    close_pool(pool).await;
    let pool = db::connect(&url(&path)).await.unwrap();
    assert_upgraded_and_intact(&pool).await;
}
