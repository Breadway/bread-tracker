use bread_tracker_server::{auth, db};

#[tokio::test]
async fn independent_pools_share_a_device_budget_and_expiry_reopens_it() {
    let directory = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", directory.path().join("rate.db").display());
    let first = db::connect(&url).await.unwrap();
    let code = auth::create_pairing_code(&first).await.unwrap();
    let (device, _) = auth::redeem(&first, &code, "phone").await.unwrap().unwrap();
    let second = db::connect(&url).await.unwrap();
    let mut tasks = Vec::new();
    for index in 0..20 {
        let pool = if index % 2 == 0 {
            first.clone()
        } else {
            second.clone()
        };
        let device = device.clone();
        tasks.push(tokio::spawn(async move {
            auth::allow_request(&pool, &device, 5).await.unwrap()
        }));
    }
    let mut count = 0;
    for task in tasks {
        count += usize::from(task.await.unwrap());
    }
    assert_eq!(count, 5);

    // Denied requests must not keep incrementing the stored counter.
    let stored: i64 =
        sqlx::query_scalar("SELECT requests FROM authenticated_rate_limits WHERE device_id = ?")
            .bind(&device)
            .fetch_one(&first)
            .await
            .unwrap();
    assert_eq!(stored, 5);

    // A second device token has its own budget even while the first is exhausted.
    let other_code = auth::create_pairing_code(&first).await.unwrap();
    let (other_device, _) = auth::redeem(&first, &other_code, "tablet")
        .await
        .unwrap()
        .unwrap();
    for _ in 0..5 {
        assert!(auth::allow_request(&second, &other_device, 5)
            .await
            .unwrap());
    }
    assert!(!auth::allow_request(&first, &device, 5).await.unwrap());

    sqlx::query("UPDATE authenticated_rate_limits SET window_started_at = 0")
        .execute(&first)
        .await
        .unwrap();
    assert!(auth::allow_request(&second, &device, 5).await.unwrap());
    let stored: i64 =
        sqlx::query_scalar("SELECT requests FROM authenticated_rate_limits WHERE device_id = ?")
            .bind(&device)
            .fetch_one(&first)
            .await
            .unwrap();
    assert_eq!(stored, 1, "an expired window restarts at one");
    sqlx::query("DELETE FROM devices WHERE id = ?")
        .bind(&device)
        .execute(&first)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authenticated_rate_limits")
        .fetch_one(&first)
        .await
        .unwrap();
    assert_eq!(rows, 1, "the deleted device's row cascades away");
}
