use std::net::IpAddr;

use bread_tracker_server::{db, error::ApiError, public_auth};

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

fn is_busy_api_error(error: &ApiError) -> bool {
    match error {
        ApiError::Internal(inner) => inner.chain().any(|cause| {
            cause
                .downcast_ref::<sqlx::Error>()
                .is_some_and(is_busy_database_error)
        }),
        _ => false,
    }
}

/// Admit and then record a failure, retrying the whole attempt only when SQLite reported BUSY or
/// LOCKED before the transaction committed. A busy rollback writes nothing, so a retry cannot
/// double-count an admission; a budget refusal still returns false exactly as before.
async fn admit_and_fail_with_busy_retry(pool: &sqlx::SqlitePool, source: IpAddr) -> bool {
    for attempt in 1..=BUSY_RETRY_ATTEMPTS {
        match public_auth::admit(pool, source).await {
            Ok(tx) => match public_auth::fail(tx, source).await {
                Ok(()) => return true,
                Err(error) if attempt < BUSY_RETRY_ATTEMPTS && is_busy_api_error(&error) => {
                    tokio::time::sleep(BUSY_RETRY_BACKOFF).await;
                }
                Err(error) => panic!("unexpected failure recording admission: {error}"),
            },
            Err(ApiError::TooManyRequests) => return false,
            Err(error) if attempt < BUSY_RETRY_ATTEMPTS && is_busy_api_error(&error) => {
                tokio::time::sleep(BUSY_RETRY_BACKOFF).await;
            }
            Err(error) => panic!("unexpected admission error: {error}"),
        }
    }
    unreachable!("the final attempt returns instead of looping")
}

#[tokio::test]
async fn concurrent_connections_share_one_failure_budget_and_restart_preserves_it() {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("auth.db").display());
    let first = db::connect(&url).await.unwrap();
    let second = db::connect(&url).await.unwrap();
    let source: IpAddr = "203.0.113.1".parse().unwrap();
    let mut tasks = Vec::new();
    for index in 0..20 {
        let pool = if index % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        tasks.push(tokio::spawn(async move {
            admit_and_fail_with_busy_retry(&pool, source).await
        }));
    }
    let mut admitted = 0;
    for task in tasks {
        admitted += usize::from(task.await.unwrap());
    }
    assert_eq!(admitted, 5);
    first.close().await;
    second.close().await;
    let reopened = db::connect(&url).await.unwrap();
    assert!(matches!(
        public_auth::admit(&reopened, source).await,
        Err(ApiError::TooManyRequests)
    ));
    let failures: i64 = sqlx::query_scalar("SELECT failures FROM public_auth_limits")
        .fetch_one(&reopened)
        .await
        .unwrap();
    assert_eq!(failures, 5);
}
