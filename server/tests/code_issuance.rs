use std::time::Duration;

use bread_tracker_server::auth::{self, CallerIdentity};
use bread_tracker_server::db;
use sqlx::SqlitePool;

/// SQLite result codes for BUSY and LOCKED, including the extended forms. The server's
/// classifier in `db` is crate-private, so this integration test matches the documented
/// codes itself (the same helper the other file-backed concurrency tests carry).
const SQLITE_BUSY_CODES: [&str; 8] = ["5", "261", "517", "773", "6", "262", "518", "774"];
const BUSY_RETRY_ATTEMPTS: usize = 3;
const BUSY_RETRY_BACKOFF: Duration = Duration::from_millis(100);

fn is_busy_database_error(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Database(db) => db
            .code()
            .as_deref()
            .is_some_and(|code| SQLITE_BUSY_CODES.contains(&code)),
        _ => false,
    }
}

/// Issue one capped pairing code, retrying only a raw SQLite BUSY or LOCKED result. A
/// genuine failure still returns immediately, so the trigger assertion below is unaffected.
async fn issue_with_busy_retry(
    pool: &SqlitePool,
    caller: &CallerIdentity,
) -> Result<Option<String>, sqlx::Error> {
    for attempt in 1..=BUSY_RETRY_ATTEMPTS {
        match auth::create_pairing_code_limited(pool, caller).await {
            Err(error) if attempt < BUSY_RETRY_ATTEMPTS && is_busy_database_error(&error) => {
                tokio::time::sleep(BUSY_RETRY_BACKOFF).await;
            }
            result => return result,
        }
    }
    unreachable!("the final attempt returns instead of looping")
}

/// A real file database (several connections), the admin user and one paired device whose
/// credential belongs to that caller.
async fn caller(pool: &SqlitePool) -> CallerIdentity {
    let mut conn = pool.acquire().await.unwrap();
    let (device_id, token) = auth::insert_device(&mut conn, 1, "phone").await.unwrap();
    CallerIdentity {
        device_id,
        token_hash: auth::hash(&token),
        user_id: 1,
    }
}

async fn codes(pool: &SqlitePool) -> Vec<String> {
    sqlx::query_scalar("SELECT code_hash FROM pairing_codes ORDER BY code_hash")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn file_db() -> (tempfile::TempDir, SqlitePool) {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("codes.db").display());
    let pool = db::connect(&url).await.unwrap();
    (directory, pool)
}

#[tokio::test]
async fn failed_issuance_retains_existing_codes_and_concurrency_respects_cap() {
    let (_directory, pool) = file_db().await;
    let caller = caller(&pool).await;
    for _ in 0..3 {
        auth::create_pairing_code_limited(&pool, &caller)
            .await
            .unwrap()
            .unwrap();
    }
    let before = codes(&pool).await;
    sqlx::query("CREATE TRIGGER fail_code BEFORE INSERT ON pairing_codes BEGIN SELECT RAISE(ABORT, 'injected code insertion failure'); END")
        .execute(&pool).await.unwrap();
    assert!(auth::create_pairing_code_limited(&pool, &caller)
        .await
        .is_err());
    let after = codes(&pool).await;
    assert_eq!(after, before);
    sqlx::query("DROP TRIGGER fail_code")
        .execute(&pool)
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let pool = pool.clone();
        let caller = caller.clone();
        tasks.push(tokio::spawn(async move {
            issue_with_busy_retry(&pool, &caller).await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap().unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pairing_codes WHERE user_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
}

#[tokio::test]
async fn a_device_revoked_while_issuance_waits_is_rejected_without_touching_codes() {
    let (_directory, pool) = file_db().await;
    let caller = caller(&pool).await;
    auth::create_pairing_code_limited(&pool, &caller)
        .await
        .unwrap()
        .unwrap();
    let before = codes(&pool).await;

    // An outside connection takes the writer and revokes the caller without committing yet.
    // Issuance authenticates with the old identity, blocks on BEGIN IMMEDIATE, then runs its
    // recheck after the revocation commits.
    let mut holder = pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let issuer_pool = pool.clone();
    let issuer_caller = caller.clone();
    let issuance = tokio::spawn(async move {
        auth::create_pairing_code_limited(&issuer_pool, &issuer_caller).await
    });
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !issuance.is_finished(),
        "issuance is blocked on the write lock"
    );
    sqlx::query("DELETE FROM devices WHERE id = ?")
        .bind(&caller.device_id)
        .execute(&mut *holder)
        .await
        .unwrap();
    sqlx::query("COMMIT").execute(&mut *holder).await.unwrap();
    drop(holder);

    assert!(
        issuance.await.unwrap().unwrap().is_none(),
        "the revoked caller must not receive a code"
    );
    assert_eq!(codes(&pool).await, before, "no code was pruned or added");
}

#[tokio::test]
async fn a_rotated_credential_is_rejected_without_touching_codes() {
    let (_directory, pool) = file_db().await;
    let caller = caller(&pool).await;
    auth::create_pairing_code_limited(&pool, &caller)
        .await
        .unwrap()
        .unwrap();
    let before = codes(&pool).await;

    // Same device row, different credential: only the hash that authenticated is accepted.
    sqlx::query("UPDATE devices SET token_hash = ? WHERE id = ?")
        .bind(auth::hash("someone-else"))
        .bind(&caller.device_id)
        .execute(&pool)
        .await
        .unwrap();

    assert!(auth::create_pairing_code_limited(&pool, &caller)
        .await
        .unwrap()
        .is_none());
    assert_eq!(codes(&pool).await, before);
}
