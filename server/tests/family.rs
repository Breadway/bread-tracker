//! TRACK-56: family codes, the child-approved link flow, unlink from either side, and the
//! durable per-code attempt budget.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use bread_tracker_server::config::Config;
use bread_tracker_server::push::MockSender;
use bread_tracker_server::{api, auth, db, family, store, users, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::SqlitePool;
use tower::ServiceExt;

struct Harness {
    app: Router,
    state: AppState,
}

async fn harness() -> Harness {
    harness_at(None).await
}

async fn harness_at(path: Option<&std::path::Path>) -> Harness {
    let db = match path {
        Some(path) => db::connect(&format!("sqlite://{}", path.display()))
            .await
            .unwrap(),
        None => db::connect_memory().await.unwrap(),
    };
    let state = AppState::new(
        db,
        uuid::Uuid::new_v4().to_string(),
        Config::from_lookup(|_| None),
        Arc::new(MockSender::default()),
    );
    Harness {
        app: api::router(state.clone()),
        state,
    }
}

async fn call(
    app: &Router,
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
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// Create a user and pair a device for them, returning `(user_id, token)`.
async fn user_with_token(h: &Harness, name: &str) -> (i64, String) {
    let id = users::create(&h.state.db, name, false).await.unwrap();
    let code = auth::create_pairing_code_for(&h.state.db, id)
        .await
        .unwrap();
    let (status, body) = call(
        &h.app,
        "POST",
        "/pair",
        None,
        Some(json!({"code": code, "device_name": name})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (id, body["token"].as_str().unwrap().to_string())
}

async fn issue(h: &Harness, token: &str) -> String {
    let (status, body) = call(&h.app, "POST", "/family/code", Some(token), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["code"].as_str().unwrap().to_string()
}

async fn redeem(h: &Harness, token: &str, code: &str) -> (StatusCode, Value) {
    call(
        &h.app,
        "POST",
        "/family/links",
        Some(token),
        Some(json!({"code": code})),
    )
    .await
}

async fn accept(h: &Harness, token: &str, link: i64) -> (StatusCode, Value) {
    call(
        &h.app,
        "POST",
        &format!("/family/links/{link}/accept"),
        Some(token),
        None,
    )
    .await
}

async fn unlink(h: &Harness, token: &str, link: i64) -> (StatusCode, Value) {
    call(
        &h.app,
        "DELETE",
        &format!("/family/links/{link}"),
        Some(token),
        None,
    )
    .await
}

async fn scalar(pool: &SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

async fn pending_link(h: &Harness, child: &str, parent: &str) -> i64 {
    let code = issue(h, child).await;
    let (status, body) = redeem(h, parent, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pending");
    body["id"].as_i64().unwrap()
}

#[tokio::test]
async fn single_use_pending_accept_and_either_side_unlink() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;

    let code = issue(&h, &child).await;
    assert_eq!(code.replace('-', "").len(), 12, "{code}");
    assert_eq!(code.len(), 14, "{code}");

    let (status, body) = redeem(&h, &parent, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pending");
    assert_eq!(body["direction"], "parent");
    assert_eq!(body["other_name"], "Kid");
    assert_eq!(body["permission"], "complete_only");
    assert_eq!(body["pending_expired"], false);
    let link = body["id"].as_i64().unwrap();

    // The code is consumed and bound to the redeeming parent.
    assert_eq!(
        scalar(
            &h.state.db,
            &format!(
                "SELECT used_by FROM family_codes WHERE code_hash = '{}'",
                auth::hash(&auth::normalize_code(&code))
            )
        )
        .await,
        parent_id
    );

    // A pending link grants nothing: no permission and no active party on either side.
    assert_eq!(
        family::permission_for(&h.state.db, parent_id, child_id)
            .await
            .unwrap(),
        None
    );
    assert!(family::active_children(&h.state.db, parent_id)
        .await
        .unwrap()
        .is_empty());
    assert!(family::active_parents(&h.state.db, child_id)
        .await
        .unwrap()
        .is_empty());

    // Both sides see the request, from their own direction, with the other display name.
    let (status, body) = call(&h.app, "GET", "/family/links", Some(&child), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["links"].as_array().unwrap().len(), 1);
    assert_eq!(body["links"][0]["direction"], "child");
    assert_eq!(body["links"][0]["status"], "pending");
    assert_eq!(body["links"][0]["other_name"], "Parent");

    // The parent can never accept: only the child is allowed, and an unowned id is a 404.
    let (status, _) = accept(&h, &parent, link).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = accept(&h, &child, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        family::permission_for(&h.state.db, parent_id, child_id)
            .await
            .unwrap()
            .as_deref(),
        Some("complete_only")
    );
    let children = family::active_children(&h.state.db, parent_id)
        .await
        .unwrap();
    assert_eq!(children.len(), 1);
    assert_eq!(children[0].user_id, child_id);
    assert_eq!(children[0].name, "Kid");
    let parents = family::active_parents(&h.state.db, child_id).await.unwrap();
    assert_eq!(parents.len(), 1);
    assert_eq!(parents[0].user_id, parent_id);

    // A second redemption of the same code is refused, and the code was not re-charged.
    let (status, _) = redeem(&h, &parent, &code).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Either side can unlink; the parent's access disappears at once.
    let (status, body) = unlink(&h, &parent, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        family::permission_for(&h.state.db, parent_id, child_id)
            .await
            .unwrap(),
        None
    );
    assert!(family::active_children(&h.state.db, parent_id)
        .await
        .unwrap()
        .is_empty());
    assert!(family::active_parents(&h.state.db, child_id)
        .await
        .unwrap()
        .is_empty());
    let (status, body) = call(&h.app, "GET", "/family/links", Some(&child), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["links"][0]["status"], "revoked");
    assert!(body["links"][0]["revoked_at"].as_i64().is_some());
}

#[tokio::test]
async fn expired_codes_and_expired_pending_links_are_refused() {
    let h = harness().await;
    let (_child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;

    let code = issue(&h, &child).await;
    sqlx::query(
        "UPDATE family_codes SET created_at = ?, expires_at = ?
         WHERE child_id = (SELECT id FROM users WHERE name = 'Kid')",
    )
    .bind(store::now_ms() - 2)
    .bind(store::now_ms() - 1)
    .execute(&h.state.db)
    .await
    .unwrap();
    let (status, _) = redeem(&h, &parent, &code).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_links").await,
        0
    );
    assert_eq!(
        scalar(
            &h.state.db,
            "SELECT COUNT(*) FROM family_codes WHERE used_at IS NOT NULL"
        )
        .await,
        0,
        "an expired code is not consumed"
    );

    let link = pending_link(&h, &child, &parent).await;
    sqlx::query("UPDATE family_links SET pending_expires_at = ? WHERE id = ?")
        .bind(store::now_ms() - 1)
        .bind(link)
        .execute(&h.state.db)
        .await
        .unwrap();
    let (status, body) = accept(&h, &child, link).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        scalar(
            &h.state.db,
            &format!("SELECT COUNT(*) FROM family_links WHERE id = {link} AND status = 'pending'")
        )
        .await,
        1,
        "an expired request stays pending until accepted or revoked"
    );
    let (status, body) = call(&h.app, "GET", "/family/links", Some(&child), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["links"][0]["pending_expired"], true);
}

#[tokio::test]
async fn self_link_is_refused_without_consuming_the_code() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;

    let code = issue(&h, &child).await;
    let (status, body) = redeem(&h, &child, &code).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_links").await,
        0
    );
    assert_eq!(
        scalar(
            &h.state.db,
            &format!(
                "SELECT COUNT(*) FROM family_codes WHERE code_hash = '{}' AND used_at IS NULL",
                auth::hash(&auth::normalize_code(&code))
            )
        )
        .await,
        1,
        "the refused self-link must not consume the child's code"
    );

    // The same code still works for a real parent.
    let (status, body) = redeem(&h, &parent, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "pending");
    assert_eq!(
        family::permission_for(&h.state.db, _parent_id, child_id)
            .await
            .unwrap(),
        None,
        "still pending until the child accepts"
    );
}

#[tokio::test]
async fn either_side_unlinks_and_a_revoke_can_be_followed_by_a_new_request() {
    let h = harness().await;
    let (_child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;

    let link = pending_link(&h, &child, &parent).await;
    let (status, body) = accept(&h, &child, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = unlink(&h, &child, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Re-link after revoke: the unique pair row is reused, so it starts pending again and the
    // child must accept it again.
    let code = issue(&h, &child).await;
    let (status, body) = redeem(&h, &parent, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], link);
    assert_eq!(body["status"], "pending");
    let (status, body) = accept(&h, &child, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The parent can end the same link too.
    let (status, body) = unlink(&h, &parent, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_links").await,
        1,
        "re-linking must reuse the pair rather than insert a second row"
    );
    let (status, _) = redeem(&h, &parent, &code).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_link_id_the_caller_does_not_own_is_a_404() {
    let h = harness().await;
    let (_child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    let (_intruder_id, intruder) = user_with_token(&h, "Intruder").await;

    let link = pending_link(&h, &child, &parent).await;
    assert_eq!(accept(&h, &intruder, link).await.0, StatusCode::NOT_FOUND);
    assert_eq!(unlink(&h, &intruder, link).await.0, StatusCode::NOT_FOUND);
    assert_eq!(accept(&h, &parent, link).await.0, StatusCode::NOT_FOUND);

    let (status, body) = call(&h.app, "GET", "/family/links", Some(&intruder), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["links"].as_array().unwrap().is_empty());

    // The child still owns the request and can accept it after the refused attempts.
    let (status, body) = accept(&h, &child, link).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(unlink(&h, &intruder, link).await.0, StatusCode::NOT_FOUND);
    assert_eq!(
        scalar(
            &h.state.db,
            &format!("SELECT COUNT(*) FROM family_links WHERE id = {link} AND status = 'active'")
        )
        .await,
        1,
        "a refused unlink changes nothing"
    );
}

#[tokio::test]
async fn the_per_code_attempt_budget_blocks_guessing_and_success_is_not_charged() {
    let h = harness().await;
    let (_child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;

    let bad = "AAAA-BBBB-CCCC";
    for attempt in 1..=5 {
        // Isolate the per-code limiter from the shared per-source admission.
        sqlx::query("DELETE FROM public_auth_limits")
            .execute(&h.state.db)
            .await
            .unwrap();
        let (status, _) = redeem(&h, &parent, bad).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
    }
    assert_eq!(
        scalar(
            &h.state.db,
            &format!(
                "SELECT failures FROM family_code_attempts WHERE code_hash = '{}'",
                auth::hash(&auth::normalize_code(bad))
            )
        )
        .await,
        5
    );
    sqlx::query("DELETE FROM public_auth_limits")
        .execute(&h.state.db)
        .await
        .unwrap();
    let (status, _) = redeem(&h, &parent, bad).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

    // A different code has its own budget, and a successful redemption clears it rather than
    // spending a failure.
    sqlx::query("DELETE FROM public_auth_limits")
        .execute(&h.state.db)
        .await
        .unwrap();
    let good = issue(&h, &child).await;
    let (status, body) = redeem(&h, &parent, &good).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        scalar(
            &h.state.db,
            &format!(
                "SELECT COUNT(*) FROM family_code_attempts WHERE code_hash = '{}'",
                auth::hash(&auth::normalize_code(&good))
            )
        )
        .await,
        0,
        "a success must not leave or spend a per-code failure"
    );
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM public_auth_limits").await,
        0,
        "a success must not spend the shared per-source budget"
    );
}

#[tokio::test]
async fn two_parents_cannot_redeem_one_code_concurrently() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("family.db");
    let h = harness_at(Some(&path)).await;
    let (_child_id, child) = user_with_token(&h, "Kid").await;
    let (_p1, parent_one) = user_with_token(&h, "Parent One").await;
    let (_p2, parent_two) = user_with_token(&h, "Parent Two").await;

    let code = issue(&h, &child).await;
    let (first, second) = tokio::join!(
        redeem(&h, &parent_one, &code),
        redeem(&h, &parent_two, &code)
    );
    let statuses = [first.0, second.0];
    assert_eq!(
        statuses.iter().filter(|s| **s == StatusCode::OK).count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == StatusCode::UNAUTHORIZED)
            .count(),
        1,
        "{statuses:?}"
    );
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_links").await,
        1
    );
    assert_eq!(
        scalar(
            &h.state.db,
            "SELECT COUNT(*) FROM family_codes WHERE used_at IS NOT NULL"
        )
        .await,
        1
    );
}

#[tokio::test]
async fn deleting_an_account_cascades_its_links_and_codes() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;

    let link = pending_link(&h, &child, &parent).await;
    let _ = accept(&h, &child, link).await;
    let _unused = issue(&h, &child).await;
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_codes").await,
        2,
        "a used code stays as history while the child's open codes are capped"
    );

    let removed = users::remove(&h.state.db, child_id).await.unwrap();
    assert!(removed.is_ok());
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_links").await,
        0
    );
    assert_eq!(
        scalar(&h.state.db, "SELECT COUNT(*) FROM family_codes").await,
        0,
        "the child's codes belong to the child"
    );

    // Deleting a parent removes the link but keeps the child's code row as history, with the
    // redeeming parent cleared instead of the whole row cascading.
    let (_child2, child_two) = user_with_token(&h, "Kid Two").await;
    let (parent2, parent_two) = user_with_token(&h, "Parent Two").await;
    let code = issue(&h, &child_two).await;
    let (status, body) = redeem(&h, &parent_two, &code).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let link2 = body["id"].as_i64().unwrap();
    let _ = accept(&h, &child_two, link2).await;
    let removed = users::remove(&h.state.db, parent2).await.unwrap();
    assert!(removed.is_ok());
    assert_eq!(
        scalar(
            &h.state.db,
            &format!("SELECT COUNT(*) FROM family_links WHERE id = {link2}")
        )
        .await,
        0
    );
    assert_eq!(
        scalar(
            &h.state.db,
            &format!(
                "SELECT COUNT(*) FROM family_codes WHERE code_hash = '{}' AND used_by IS NULL",
                auth::hash(&auth::normalize_code(&code))
            )
        )
        .await,
        1
    );
}
