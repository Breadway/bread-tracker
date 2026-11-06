//! TRACK-75 on a real file database with several connections (the in-memory test pool has a
//! single connection and cannot interleave anything): fault injection that must roll back both
//! users, a caller removed mid-flight, and concurrent takeovers of one token.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use bread_tracker_server::config::Config;
use bread_tracker_server::push::MockSender;
use bread_tracker_server::{api, auth, db, users, AppState};
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt;

struct World {
    app: axum::Router,
    pool: SqlitePool,
    _dir: tempfile::TempDir,
}

/// Users 1 (Alice), plus `extra` more, each with one paired device and its bearer token.
async fn world(extra: &[&str]) -> (World, Vec<(i64, String)>) {
    let dir = tempfile::tempdir().unwrap();
    let url = format!("sqlite://{}", dir.path().join("t.db").display());
    let pool = db::connect_with_admin(&url, Some("Alice")).await.unwrap();
    let app = api::router(AppState::new(
        pool.clone(),
        "test-generation".to_owned(),
        Config::from_env(),
        Arc::new(MockSender::default()),
    ));
    let mut people = vec![(1_i64, pair(&app, &pool, 1).await)];
    for name in extra {
        let uid = users::create(&pool, name, false).await.unwrap();
        people.push((uid, pair(&app, &pool, uid).await));
    }
    (
        World {
            app,
            pool,
            _dir: dir,
        },
        people,
    )
}

async fn pair(app: &axum::Router, pool: &SqlitePool, uid: i64) -> String {
    let code = auth::create_pairing_code_for(pool, uid).await.unwrap();
    let (s, v) = call(
        app,
        "POST",
        "/pair",
        None,
        Some(json!({"code": code, "device_name": "phone"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    v["token"].as_str().unwrap().to_string()
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header("content-type", "application/json")
            .body(Body::from(b.to_string()))
            .unwrap(),
        None => req.body(Body::empty()).unwrap(),
    };
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .unwrap()
        .to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn tokens(pool: &SqlitePool) -> Vec<(i64, Option<String>)> {
    sqlx::query_as("SELECT user_id, fcm_token FROM devices ORDER BY user_id")
        .fetch_all(pool)
        .await
        .unwrap()
}

async fn set(w: &World, token: &str, fcm: &str) -> StatusCode {
    call(
        &w.app,
        "PUT",
        "/device",
        Some(token),
        Some(json!({"fcm_token": fcm})),
    )
    .await
    .0
}

#[tokio::test]
async fn caller_assignment_failing_after_the_clear_rolls_back_the_previous_holder() {
    let (w, p) = world(&["Bob"]).await;
    let (bob, bob_tok) = (p[1].0, p[1].1.clone());
    assert_eq!(set(&w, &p[0].1, "alice-fcm").await, StatusCode::OK);
    assert_eq!(set(&w, &bob_tok, "bob-fcm").await, StatusCode::OK);
    let before = tokens(&w.pool).await;

    sqlx::query(&format!(
        "CREATE TRIGGER fail_bob BEFORE UPDATE OF fcm_token ON devices \
         WHEN NEW.user_id = {bob} AND NEW.fcm_token = 'alice-fcm' BEGIN SELECT RAISE(ABORT, 'injected'); END"
    ))
    .execute(&w.pool)
    .await
    .unwrap();
    assert_eq!(
        set(&w, &bob_tok, "alice-fcm").await,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        tokens(&w.pool).await,
        before,
        "both users exactly as before"
    );

    sqlx::query("DROP TRIGGER fail_bob")
        .execute(&w.pool)
        .await
        .unwrap();
    assert_eq!(
        set(&w, &bob_tok, "alice-fcm").await,
        StatusCode::OK,
        "retry succeeds"
    );
    assert_eq!(
        tokens(&w.pool).await,
        vec![(1, None), (bob, Some("alice-fcm".into()))]
    );
}

#[tokio::test]
async fn a_caller_removed_after_authentication_cannot_take_a_token_and_clears_nobody() {
    // Deterministic race. The request authenticates, then its handler blocks on the write lock
    // (an outside connection holds BEGIN IMMEDIATE). While it waits, that connection deletes
    // the caller's device and commits. The handler's transaction then runs and must find the
    // caller gone: 401, and Alice's token untouched.
    let (w, p) = world(&["Bob"]).await;
    let (bob_tok, alice_tok) = (p[1].1.clone(), p[0].1.clone());
    assert_eq!(set(&w, &alice_tok, "alice-fcm").await, StatusCode::OK);
    // Authenticate once so last_seen is fresh and the request does not write before the handler.
    assert_eq!(
        call(&w.app, "GET", "/stats", Some(&bob_tok), None).await.0,
        StatusCode::OK
    );

    let mut holder = w.pool.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *holder)
        .await
        .unwrap();
    let app = w.app.clone();
    let req = tokio::spawn(async move {
        call(
            &app,
            "PUT",
            "/device",
            Some(&bob_tok),
            Some(json!({"fcm_token": "alice-fcm"})),
        )
        .await
        .0
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !req.is_finished(),
        "the handler is blocked on the write lock"
    );
    sqlx::query("DELETE FROM devices WHERE user_id != 1")
        .execute(&mut *holder)
        .await
        .unwrap();
    sqlx::query("COMMIT").execute(&mut *holder).await.unwrap();
    drop(holder);

    assert_eq!(req.await.unwrap(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        tokens(&w.pool).await,
        vec![(1, Some("alice-fcm".into()))],
        "Alice kept her token"
    );
}

#[tokio::test]
async fn concurrent_takeovers_of_one_token_always_leave_one_holder_and_every_device() {
    let (w, p) = world(&["Bob", "Cy", "Di", "Ed"]).await;
    for round in 0..15 {
        let mut tasks = Vec::new();
        for (_, t) in &p {
            let (app, t) = (w.app.clone(), t.clone());
            tasks.push(tokio::spawn(async move {
                call(
                    &app,
                    "PUT",
                    "/device",
                    Some(&t),
                    Some(json!({"fcm_token": "shared-install"})),
                )
                .await
                .0
            }));
        }
        for t in tasks {
            assert_eq!(t.await.unwrap(), StatusCode::OK, "round {round}");
        }
        let holders: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM devices WHERE fcm_token = 'shared-install'")
                .fetch_one(&w.pool)
                .await
                .unwrap();
        let devices: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM devices")
            .fetch_one(&w.pool)
            .await
            .unwrap();
        assert_eq!((holders, devices), (1, 5), "round {round}");
    }
}

#[tokio::test]
async fn every_user_ends_with_their_own_distinct_token_after_mixed_concurrent_moves() {
    // Each user also registers a private token while others fight over the shared one: nobody's
    // private token may be cleared by somebody else's takeover.
    let (w, p) = world(&["Bob", "Cy"]).await;
    for (i, (_, t)) in p.iter().enumerate() {
        assert_eq!(set(&w, t, &format!("private-{i}")).await, StatusCode::OK);
    }
    let mut tasks = Vec::new();
    for (_, t) in &p {
        let (app, t) = (w.app.clone(), t.clone());
        tasks.push(tokio::spawn(async move {
            call(
                &app,
                "PUT",
                "/device",
                Some(&t),
                Some(json!({"fcm_token": "contested"})),
            )
            .await
            .0
        }));
    }
    for t in tasks {
        assert_eq!(t.await.unwrap(), StatusCode::OK);
    }
    let held: Vec<Option<String>> =
        sqlx::query_scalar("SELECT fcm_token FROM devices ORDER BY user_id")
            .fetch_all(&w.pool)
            .await
            .unwrap();
    assert_eq!(
        held.iter()
            .filter(|t| t.as_deref() == Some("contested"))
            .count(),
        1
    );
    assert_eq!(
        held.iter().filter(|t| t.is_none()).count(),
        2,
        "the two losers hold nothing, and nothing else moved"
    );
}

#[tokio::test]
async fn concurrent_disjoint_settings_patches_merge_after_external_writer_releases() {
    let (w, p) = world(&["Bob"]).await;
    let bob = p[1].1.clone();
    for _ in 0..8 {
        assert_eq!(
            call(
                &w.app,
                "PUT",
                "/settings",
                Some(&bob),
                Some(json!({"timezone":"UTC","reminders_enabled":true}))
            )
            .await
            .0,
            StatusCode::OK
        );
        let mut writer = w.pool.begin_with("BEGIN IMMEDIATE").await.unwrap();
        let a = w.app.clone();
        let t = bob.clone();
        let first = tokio::spawn(async move {
            call(
                &a,
                "PUT",
                "/settings",
                Some(&t),
                Some(json!({"timezone":"Asia/Tokyo"})),
            )
            .await
        });
        let a = w.app.clone();
        let t = bob.clone();
        let second = tokio::spawn(async move {
            call(
                &a,
                "PUT",
                "/settings",
                Some(&t),
                Some(json!({"reminders_enabled":false})),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(!first.is_finished() && !second.is_finished());
        sqlx::query("SELECT 1").execute(&mut *writer).await.unwrap();
        writer.commit().await.unwrap();
        assert_eq!(first.await.unwrap().0, StatusCode::OK);
        assert_eq!(second.await.unwrap().0, StatusCode::OK);
        let (_, v) = call(&w.app, "GET", "/settings", Some(&bob), None).await;
        assert_eq!(v["timezone"], "Asia/Tokyo");
        assert_eq!(v["reminders_enabled"], false);
        let (_, admin) = call(&w.app, "GET", "/settings", Some(&p[0].1), None).await;
        assert_eq!(admin["timezone"], "Australia/Perth");
    }
}

#[tokio::test]
async fn competing_quiet_pair_patches_validate_serialized_result_and_reject_atomically() {
    let (w, p) = world(&["Bob"]).await;
    let bob = &p[1].1;
    call(
        &w.app,
        "PUT",
        "/settings",
        Some(bob),
        Some(json!({"quiet_start":"22:00","quiet_end":"07:00"})),
    )
    .await;
    let a = w.app.clone();
    let t = bob.clone();
    let clear = tokio::spawn(async move {
        call(
            &a,
            "PUT",
            "/settings",
            Some(&t),
            Some(json!({"quiet_start":"","quiet_end":""})),
        )
        .await
    });
    let a = w.app.clone();
    let t = bob.clone();
    let partial = tokio::spawn(async move {
        call(
            &a,
            "PUT",
            "/settings",
            Some(&t),
            Some(json!({"quiet_start":"23:00","timezone":"Asia/Tokyo"})),
        )
        .await
    });
    assert_eq!(clear.await.unwrap().0, StatusCode::OK);
    let result = partial.await.unwrap();
    assert!(result.0 == StatusCode::OK || result.0 == StatusCode::BAD_REQUEST);
    let (_, v) = call(&w.app, "GET", "/settings", Some(bob), None).await;
    assert_eq!(v["quiet_start"], "");
    assert_eq!(v["quiet_end"], "");
    if result.0 == StatusCode::BAD_REQUEST {
        assert_eq!(v["timezone"], "UTC");
    }
    let before = v;
    let (status, _) = call(
        &w.app,
        "PUT",
        "/settings",
        Some(bob),
        Some(json!({"timezone":"Europe/Berlin","quiet_start":"bad"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        call(&w.app, "GET", "/settings", Some(bob), None).await.1,
        before
    );
}
