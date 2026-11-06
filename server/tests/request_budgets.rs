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
    sqlx::query("UPDATE authenticated_rate_limits SET window_started_at = 0")
        .execute(&first)
        .await
        .unwrap();
    assert!(auth::allow_request(&second, &device, 5).await.unwrap());
    sqlx::query("DELETE FROM devices WHERE id = ?")
        .bind(&device)
        .execute(&first)
        .await
        .unwrap();
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM authenticated_rate_limits")
        .fetch_one(&first)
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
