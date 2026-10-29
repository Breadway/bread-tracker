use bread_tracker_server::{auth, db};

#[tokio::test]
async fn failed_issuance_retains_existing_codes_and_concurrency_respects_cap() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("codes.db").display());
    let pool = db::connect(&url).await.unwrap();
    for _ in 0..3 {
        auth::create_pairing_code_limited(&pool, 1).await.unwrap();
    }
    let before: Vec<String> =
        sqlx::query_scalar("SELECT code_hash FROM pairing_codes ORDER BY code_hash")
            .fetch_all(&pool)
            .await
            .unwrap();
    sqlx::query("CREATE TRIGGER fail_code BEFORE INSERT ON pairing_codes BEGIN SELECT RAISE(ABORT, 'injected code insertion failure'); END")
        .execute(&pool).await.unwrap();
    assert!(auth::create_pairing_code_limited(&pool, 1).await.is_err());
    let after: Vec<String> =
        sqlx::query_scalar("SELECT code_hash FROM pairing_codes ORDER BY code_hash")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(after, before);
    sqlx::query("DROP TRIGGER fail_code")
        .execute(&pool)
        .await
        .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let pool = pool.clone();
        tasks.push(tokio::spawn(async move {
            auth::create_pairing_code_limited(&pool, 1).await
        }));
    }
    for task in tasks {
        task.await.unwrap().unwrap();
    }
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pairing_codes WHERE user_id = 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
}
