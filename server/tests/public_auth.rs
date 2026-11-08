use std::net::IpAddr;

use bread_tracker_server::{db, error::ApiError, public_auth};

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
            match public_auth::admit(&pool, source).await {
                Ok(tx) => {
                    public_auth::fail(tx, source).await.unwrap();
                    true
                }
                Err(ApiError::TooManyRequests) => false,
                Err(error) => panic!("unexpected admission error: {error}"),
            }
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
