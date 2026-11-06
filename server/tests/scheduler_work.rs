//! Execute the actual scheduler projections on populated history, counting SQLite VM steps.
use bread_tracker_server::{db, store};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

#[tokio::test]
async fn zero_result_scheduler_work_does_not_walk_retained_history() {
    let pool = db::connect_memory().await.unwrap();
    sqlx::raw_sql("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO items(user_id,id,kind,name,points,updated_at,seq) SELECT 1,printf('i%d',x),'chore','old',1,1,x FROM n;
    WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<120000) INSERT INTO completions(user_id,item_id,date,completed_at,updated_at,seq) SELECT 1,printf('i%d',x%10000),printf('2020-%06d',x),1,1,x FROM n;
    WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<10000) INSERT INTO reminders_sent(user_id,item_id,date,sent_at) SELECT 1,printf('i%d',x),'2020-01-01',1 FROM n; ANALYZE;")
        .execute(&pool).await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let steps = Arc::new(AtomicUsize::new(0));
    let counter = steps.clone();
    conn.lock_handle()
        .await
        .unwrap()
        .set_progress_handler(1, move || {
            counter.fetch_add(1, Ordering::Relaxed);
            true
        });
    let cases = [
        ("items", store::scheduler_items_sql(), vec!["1"]),
        (
            "completions",
            store::scheduler_completions_sql(),
            vec!["1", "2026-10-25", "2026-10-24", "1", "1"],
        ),
        (
            "sent",
            store::SCHEDULER_SENT_SQL.into(),
            vec!["1", "2026-10-25", "2026-10-24"],
        ),
    ];
    for (name, sql, binds) in cases {
        let explain = format!("EXPLAIN QUERY PLAN {sql}");
        let mut plan = sqlx::query_as::<_, (i64, i64, i64, String)>(&explain);
        for b in &binds {
            plan = plan.bind(*b);
        }
        let p = plan.fetch_all(&mut *conn).await.unwrap();
        steps.store(0, Ordering::Relaxed);
        let mut q = sqlx::query(&sql);
        for b in binds {
            q = q.bind(b);
        }
        assert!(q.fetch_all(&mut *conn).await.unwrap().is_empty());
        let n = steps.load(Ordering::Relaxed);
        println!("{name}: {n} VM steps; {p:?}");
        assert!(
            p.iter().all(|r| !r.3.contains("SCAN items")
                && !r.3.contains("SCAN completions")
                && !r.3.contains("SCAN reminders_sent")),
            "{p:?}"
        );
        assert!(
            n < 1000,
            "{name}: {n} steps for zero candidates must not scale with retained history"
        );
    }
}
