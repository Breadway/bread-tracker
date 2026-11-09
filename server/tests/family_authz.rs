//! TRACK-57: authorization for parent writes and for a child's edits of parent-created items.
//!
//! The tests are deliberately written so that removing a `created_by = caller`, `status =
//! 'active'`, or stored-field comparison would make one of them fail. They cover the parent
//! routes, the per-field child matrix, refusal reporting, two-parent isolation, revocation
//! handover and the statistics ledger that parent writes must maintain.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use bread_tracker_server::config::Config;
use bread_tracker_server::push::MockSender;
use bread_tracker_server::{api, auth, db, family, store, users, AppState};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const P1: &str = "11111111-1111-4111-8111-111111111111";
const P2: &str = "22222222-2222-4222-8222-222222222222";
const P3: &str = "33333333-3333-4333-8333-333333333333";
const OWN: &str = "55555555-5555-4555-8555-555555555555";

struct Harness {
    app: Router,
    state: AppState,
}

async fn harness() -> Harness {
    harness_with(Config::from_lookup(|_| None)).await
}

async fn harness_with(config: Config) -> Harness {
    let state = AppState::new(
        db::connect_memory().await.unwrap(),
        uuid::Uuid::new_v4().to_string(),
        config,
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

async fn call_raw(
    app: &Router,
    method: &str,
    uri: &str,
    token: &str,
    body: &str,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

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

async fn offer(h: &Harness, child: &str, parent: &str) -> (i64, i64, i64) {
    let code = issue(h, child).await;
    let (status, body) = call(
        &h.app,
        "POST",
        "/family/links",
        Some(parent),
        Some(json!({"code": code})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let link = body["id"].as_i64().unwrap();
    assert_eq!(body["status"], "pending");
    let parent_id: i64 = sqlx::query_scalar("SELECT parent_id FROM family_links WHERE id = ?")
        .bind(link)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    let child_id: i64 = sqlx::query_scalar("SELECT child_id FROM family_links WHERE id = ?")
        .bind(link)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    (link, child_id, parent_id)
}

async fn accept(h: &Harness, token: &str, link: i64) {
    let (status, body) = call(
        &h.app,
        "POST",
        &format!("/family/links/{link}/accept"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn active_link(h: &Harness, child: &str, parent: &str) -> (i64, i64, i64) {
    let (link, child_id, parent_id) = offer(h, child, parent).await;
    accept(h, child, link).await;
    (link, child_id, parent_id)
}

async fn unlink(h: &Harness, token: &str, link: i64) -> StatusCode {
    call(
        &h.app,
        "DELETE",
        &format!("/family/links/{link}"),
        Some(token),
        None,
    )
    .await
    .0
}

async fn set_default(h: &Harness, token: &str, link: i64, level: &str) -> StatusCode {
    call(
        &h.app,
        "PUT",
        &format!("/family/links/{link}"),
        Some(token),
        Some(json!({"default_permission": level})),
    )
    .await
    .0
}

async fn children(h: &Harness, token: &str) -> (StatusCode, Value) {
    call(&h.app, "GET", "/family/children", Some(token), None).await
}

async fn create_item(h: &Harness, token: &str, child_id: i64, body: Value) -> (StatusCode, Value) {
    call(
        &h.app,
        "POST",
        &format!("/family/children/{child_id}/items"),
        Some(token),
        Some(body),
    )
    .await
}

async fn update_item(
    h: &Harness,
    token: &str,
    child_id: i64,
    item_id: &str,
    body: Value,
) -> (StatusCode, Value) {
    call(
        &h.app,
        "PUT",
        &format!("/family/children/{child_id}/items/{item_id}"),
        Some(token),
        Some(body),
    )
    .await
}

async fn delete_item(
    h: &Harness,
    token: &str,
    child_id: i64,
    item_id: &str,
) -> (StatusCode, Value) {
    call(
        &h.app,
        "DELETE",
        &format!("/family/children/{child_id}/items/{item_id}"),
        Some(token),
        None,
    )
    .await
}

async fn child_progress(h: &Harness, token: &str, child_id: i64) -> (StatusCode, Value) {
    call(
        &h.app,
        "GET",
        &format!("/family/children/{child_id}/items"),
        Some(token),
        None,
    )
    .await
}

async fn child_stats(h: &Harness, token: &str, child_id: i64) -> (StatusCode, Value) {
    call(
        &h.app,
        "GET",
        &format!("/family/children/{child_id}/stats"),
        Some(token),
        None,
    )
    .await
}

fn create_body(id: &str, kind: &str, name: &str, points: i64) -> Value {
    json!({
        "id": id,
        "kind": kind,
        "name": name,
        "weekdays": 127,
        "remind_time": null,
        "points": points,
        "due_date": null,
        "sort": 0,
        "active": true,
        // A client clock must never be used for a parent write; the route ignores this.
        "updated_at": 1
    })
}

async fn sync_batch(h: &Harness, token: &str, body: Value) -> (StatusCode, Value) {
    call(&h.app, "POST", "/sync", Some(token), Some(body)).await
}

fn row(updated_at: i64, name: &str, points: i64) -> Value {
    json!({
        "id": OWN,
        "kind": "chore",
        "name": name,
        "weekdays": 127,
        "remind_time": null,
        "points": points,
        "due_date": null,
        "sort": 0,
        "active": true,
        "updated_at": updated_at,
        "deleted_at": null
    })
}

async fn stored_items(h: &Harness, user_id: i64) -> Vec<bread_tracker_server::model::Item> {
    store::UserDb::new(&h.state.db, user_id)
        .all_items()
        .await
        .unwrap()
}

async fn stored_item(h: &Harness, user_id: i64, id: &str) -> bread_tracker_server::model::Item {
    stored_items(h, user_id)
        .await
        .into_iter()
        .find(|item| item.id == id)
        .unwrap_or_else(|| panic!("item {id} not stored"))
}

/// Turn a stored row into the whole-row payload the app would push, with a newer timestamp and
/// the requested field changes. This is how the tests prove changes are compared by value.
fn edit(stored: &bread_tracker_server::model::Item, changes: Value) -> Value {
    let mut value = serde_json::to_value(stored).unwrap();
    value["updated_at"] = json!(stored.updated_at + 10_000);
    for (key, change) in changes.as_object().unwrap() {
        value[key] = change.clone();
    }
    value
}

fn today() -> String {
    chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string()
}

async fn completion(h: &Harness, token: &str, item_id: &str, date: &str) -> (StatusCode, Value) {
    sync_batch(
        h,
        token,
        json!({"completions": [{
            "item_id": item_id,
            "date": date,
            "completed_at": 1,
            "updated_at": 1,
            "deleted_at": null
        }]}),
    )
    .await
}

#[tokio::test]
async fn no_link_pending_and_revoked_grant_nothing() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;

    // No link at all.
    let (status, body) = children(&h, &parent).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["children"], json!([]));
    assert_eq!(
        child_progress(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        child_stats(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // Pending is not active: every route that would grant access is a 404, not a 403.
    let (link, _, _) = offer(&h, &child, &parent).await;
    assert_eq!(children(&h, &parent).await.1["children"], json!([]));
    assert_eq!(
        child_progress(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        child_stats(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        update_item(&h, &parent, child_id, P1, json!({"name": "x"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_item(&h, &parent, child_id, P1).await.0,
        StatusCode::NOT_FOUND
    );

    accept(&h, &child, link).await;
    let (status, body) = children(&h, &parent).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["children"].as_array().unwrap().len(), 1);
    assert_eq!(body["children"][0]["user_id"], child_id);
    assert_eq!(body["children"][0]["permission"], "complete_only");

    let (status, body) =
        create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // Revoked, even though the item row still exists for the child.
    assert_eq!(unlink(&h, &child, link).await, StatusCode::OK);
    assert_eq!(children(&h, &parent).await.1["children"], json!([]));
    assert_eq!(
        child_progress(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        child_stats(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P2, "chore", "Bins", 10))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        update_item(&h, &parent, child_id, P1, json!({"name": "x"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_item(&h, &parent, child_id, P1).await.0,
        StatusCode::NOT_FOUND
    );

    // DESIGN.md Revocation: the item stays with the child as their own, and a pending offline
    // edit is applied because no active link restricts it any more.
    let revoked = stored_item(&h, child_id, P1).await;
    assert_eq!(revoked.created_by, Some(child_id));
    assert_eq!(revoked.permission_override, None);
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&revoked, json!({"name": "Kid's bins"}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_item(&h, child_id, P1).await.name, "Kid's bins");
}

#[tokio::test]
async fn parent_writes_only_their_own_items_and_rejects_an_id_collision() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    active_link(&h, &child, &parent).await;

    let before = store::now_ms();
    let (status, body) = create_item(
        &h,
        &parent,
        child_id,
        json!({
            "id": P1, "kind": "chore", "name": "  Bins  ", "weekdays": 127,
            "points": 10, "sort": 0, "active": true, "updated_at": 1
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = store::now_ms();
    assert_eq!(body["name"], "Bins", "the server stores the trimmed name");
    assert_eq!(body["created_by"], parent_id);
    let stored = stored_item(&h, child_id, P1).await;
    assert_eq!(stored.created_by, Some(parent_id));
    assert!(
        stored.updated_at >= before && stored.updated_at <= after,
        "parent writes use server time, not the client's 1"
    );

    // Editing their own item is allowed, and the server timestamp moves.
    let (status, body) = update_item(&h, &parent, child_id, P1, json!({"name": "Recycling"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let edited = stored_item(&h, child_id, P1).await;
    assert_eq!(edited.name, "Recycling");
    assert!(edited.updated_at >= stored.updated_at);

    // The child creates their own item; the parent must never touch it.
    let (status, body) =
        sync_batch(&h, &child, json!({"items": [row(1_000, "Own chore", 10)]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let own_before = stored_item(&h, child_id, OWN).await;
    assert_eq!(
        update_item(&h, &parent, child_id, OWN, json!({"name": "Hijacked"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_item(&h, &parent, child_id, OWN).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(stored_item(&h, child_id, OWN).await, own_before);

    // An id that already exists is refused, not overwritten.
    let (status, _) = create_item(
        &h,
        &parent,
        child_id,
        create_body(OWN, "chore", "Hijacked", 10),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(stored_item(&h, child_id, OWN).await, own_before);

    // Delete is a tombstone and is only for the parent's own item.
    assert_eq!(
        delete_item(&h, &parent, child_id, P1).await.0,
        StatusCode::OK
    );
    assert!(stored_item(&h, child_id, P1).await.deleted_at.is_some());
    assert_eq!(
        update_item(&h, &parent, child_id, P1, json!({"name": "back"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn parent_reads_only_assigned_items_completions_and_assigned_xp() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    active_link(&h, &child, &parent).await;

    for (id, name, points) in [(P1, "Assigned one", 10), (P2, "Assigned two", 20)] {
        let (status, body) = create_item(
            &h,
            &parent,
            child_id,
            create_body(id, "chore", name, points),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    // The child has a private item with a large completion and a completion on each assigned
    // item. The parent must see only the latter.
    let (status, body) =
        sync_batch(&h, &child, json!({"items": [row(1_000, "Private", 100)]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let date = today();
    completion(&h, &child, OWN, &date).await;
    completion(&h, &child, P1, &date).await;
    completion(&h, &child, P2, &date).await;

    let (status, body) = child_progress(&h, &parent, child_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let ids: Vec<&str> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![P1, P2], "no private item may appear");
    let completion_items: Vec<&str> = body["completions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|completion| completion["item_id"].as_str().unwrap())
        .collect();
    assert_eq!(completion_items, vec![P1, P2]);
    assert_eq!(body["stats"]["xp"], 30, "10 + 20, never the private 100");
    assert_eq!(body["stats"]["perfect_days"], json!([]));
    assert_eq!(body["stats"]["streaks"], json!({}));
    assert!(body.get("settings").is_none());
    assert!(body.get("devices").is_none());
    assert!(body.get("tokens").is_none());

    // The dedicated stats route agrees and leaks no full statistics.
    let (status, body) = child_stats(&h, &parent, child_id).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["xp"], 30);
    assert_eq!(body["perfect_days"], json!([]));
    assert_eq!(body["streaks"], json!({}));

    // The parent's own settings route still returns the parent's account, never the child's.
    let (status, _) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&child),
        Some(json!({"timezone": "Europe/Berlin"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, body) = call(&h.app, "GET", "/settings", Some(&parent), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["timezone"], "UTC");
}

#[tokio::test]
async fn complete_only_refuses_every_item_field_but_accepts_completions() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    active_link(&h, &child, &parent).await;
    let (status, body) =
        create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored = stored_item(&h, child_id, P1).await;
    for (change, field) in [
        (json!({"name": "Hijacked"}), "name"),
        (json!({"due_date": "2026-12-01"}), "due_date"),
        (json!({"weekdays": 0}), "weekdays"),
        (json!({"remind_time": "09:00"}), "remind_time"),
        (json!({"active": false}), "active"),
        (json!({"points": 999}), "points"),
        (json!({"deleted_at": 12345}), "deleted_at"),
    ] {
        let (status, body) =
            sync_batch(&h, &child, json!({"items": [edit(&stored, change)]})).await;
        assert_eq!(status, StatusCode::OK, "{field}: {body}");
        assert_eq!(
            body["refused"].as_array().unwrap().len(),
            1,
            "{field}: {body}"
        );
        assert_eq!(body["refused"][0]["id"], P1);
        assert_eq!(body["refused"][0]["fields"], json!([field]));
        assert_eq!(body["refused"][0]["item"]["name"], "Bins");
        assert_eq!(stored_item(&h, child_id, P1).await, stored, "{field}");
    }

    // A whole row with no changed field is not an edit, even with a newer timestamp.
    let (status, body) = sync_batch(&h, &child, json!({"items": [edit(&stored, json!({}))]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
    assert_eq!(stored_item(&h, child_id, P1).await.name, "Bins");

    // Completions are always allowed, including the tombstone (un-tick).
    let date = today();
    let (status, body) = completion(&h, &child, P1, &date).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
    let completions = store::UserDb::new(&h.state.db, child_id)
        .all_completions()
        .await
        .unwrap();
    assert_eq!(completions.len(), 1);
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"completions": [{
            "item_id": P1, "date": date, "completed_at": 1,
            "updated_at": 10_000, "deleted_at": 10_000
        }]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
}

#[tokio::test]
async fn complete_and_reschedule_allows_only_the_reschedule_fields() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = active_link(&h, &child, &parent).await;
    assert_eq!(
        set_default(&h, &parent, link, "complete_and_reschedule").await,
        StatusCode::OK
    );
    let (status, body) =
        create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let stored = stored_item(&h, child_id, P1).await;
    for (change, field) in [
        (json!({"name": "Hijacked"}), "name"),
        (json!({"points": 999}), "points"),
        (json!({"kind": "habit"}), "kind"),
        (json!({"sort": 9}), "sort"),
        (json!({"deleted_at": 12345}), "deleted_at"),
    ] {
        let (status, body) =
            sync_batch(&h, &child, json!({"items": [edit(&stored, change)]})).await;
        assert_eq!(status, StatusCode::OK, "{field}: {body}");
        assert_eq!(body["refused"][0]["fields"], json!([field]), "{body}");
        assert_eq!(stored_item(&h, child_id, P1).await, stored, "{field}");
    }

    // Allowed reschedule fields update, one at a time.
    for change in [
        json!({"due_date": "2026-12-01"}),
        json!({"weekdays": 3}),
        json!({"remind_time": "07:30"}),
        json!({"active": false}),
    ] {
        let before = stored_item(&h, child_id, P1).await;
        let (status, body) = sync_batch(
            &h,
            &child,
            json!({"items": [edit(&before, change.clone())]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("refused").is_none(), "{change}: {body}");
    }

    // A row mixing an allowed and a disallowed change is refused whole: the due date must not
    // slip through while the name is refused.
    let before = stored_item(&h, child_id, P1).await;
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&before, json!({"due_date": "2027-01-01", "name": "Mixed"}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["refused"][0]["fields"], json!(["name"]));
    assert_eq!(stored_item(&h, child_id, P1).await, before);
}

#[tokio::test]
async fn full_and_item_override_control_the_matrix_and_permission_changes_take_effect() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = active_link(&h, &child, &parent).await;
    assert_eq!(set_default(&h, &parent, link, "full").await, StatusCode::OK);

    let (status, body) =
        create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for change in [
        json!({"name": "Renamed"}),
        json!({"points": 25}),
        json!({"kind": "habit"}),
        json!({"sort": 4}),
    ] {
        let before = stored_item(&h, child_id, P1).await;
        let (status, body) = sync_batch(
            &h,
            &child,
            json!({"items": [edit(&before, change.clone())]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.get("refused").is_none(), "{change}: {body}");
    }
    let before = stored_item(&h, child_id, P1).await;
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&before, json!({"deleted_at": 12345}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
    assert!(stored_item(&h, child_id, P1).await.deleted_at.is_some());

    // An item override beats a permissive default.
    let (status, body) = create_item(
        &h,
        &parent,
        child_id,
        create_body(P2, "chore", "Override", 10),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    match family::set_item_override(&h.state.db, parent_id, child_id, P2, Some("complete_only"))
        .await
        .unwrap()
    {
        family::OverrideOutcome::Updated { .. } => {}
        other => panic!("unexpected override outcome: {other:?}"),
    }
    let stored = stored_item(&h, child_id, P2).await;
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&stored, json!({"name": "No"}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["refused"][0]["fields"], json!(["name"]));

    // A permission change between two syncs applies to the next write.
    assert_eq!(
        set_default(&h, &parent, link, "complete_only").await,
        StatusCode::OK
    );
    match family::set_item_override(&h.state.db, parent_id, child_id, P2, None)
        .await
        .unwrap()
    {
        family::OverrideOutcome::Updated { .. } => {}
        other => panic!("unexpected override outcome: {other:?}"),
    }
    let stored = stored_item(&h, child_id, P2).await;
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&stored, json!({"points": 50}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["refused"][0]["fields"], json!(["points"]));

    assert_eq!(set_default(&h, &parent, link, "full").await, StatusCode::OK);
    let stored = stored_item(&h, child_id, P2).await;
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&stored, json!({"points": 50}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
    assert_eq!(stored_item(&h, child_id, P2).await.points, 50);

    // Rows the child owns are never restricted.
    let (status, body) = sync_batch(&h, &child, json!({"items": [row(1_000, "Mine", 10)]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let own = stored_item(&h, child_id, OWN).await;
    let (status, body) = sync_batch(
        &h,
        &child,
        json!({"items": [edit(&own, json!({"name": "Mine again"}))]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
}

#[tokio::test]
async fn two_parents_on_one_child_stay_isolated() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_p1_id, p1) = user_with_token(&h, "Parent one").await;
    let (_p2_id, p2) = user_with_token(&h, "Parent two").await;
    active_link(&h, &child, &p1).await;
    active_link(&h, &child, &p2).await;

    assert_eq!(
        create_item(&h, &p1, child_id, create_body(P1, "chore", "From one", 10))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        create_item(&h, &p2, child_id, create_body(P2, "chore", "From two", 20))
            .await
            .0,
        StatusCode::OK
    );
    let (status, _) = sync_batch(&h, &child, json!({"items": [row(1_000, "Own", 100)]})).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = child_progress(&h, &p1, child_id).await;
    assert_eq!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![P1],
        "a parent sees only items it created, never the other parent's or the child's"
    );
    let (_, body) = child_progress(&h, &p2, child_id).await;
    assert_eq!(
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec![P2]
    );

    assert_eq!(
        update_item(&h, &p1, child_id, P2, json!({"name": "stolen"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        delete_item(&h, &p1, child_id, P2).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        update_item(&h, &p2, child_id, P1, json!({"name": "stolen"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );

    // XP is also per parent.
    let date = today();
    completion(&h, &child, P1, &date).await;
    assert_eq!(child_stats(&h, &p1, child_id).await.1["xp"], 10);
    assert_eq!(child_stats(&h, &p2, child_id).await.1["xp"], 0);
}

#[tokio::test]
async fn assigned_created_by_and_active_filters_are_load_bearing() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_p1_id, p1) = user_with_token(&h, "Parent one").await;
    let (_p2_id, p2) = user_with_token(&h, "Parent two").await;
    active_link(&h, &child, &p1).await;
    // p2 is only pending: if the `status = 'active'` filter were dropped from the read paths,
    // p2 would be able to read or write.
    offer(&h, &child, &p2).await;
    create_item(&h, &p1, child_id, create_body(P1, "chore", "One", 10)).await;

    assert_eq!(
        child_progress(&h, &p2, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        child_stats(&h, &p2, child_id).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        update_item(&h, &p2, child_id, P1, json!({"name": "stolen"}))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        create_item(&h, &p2, child_id, create_body(P2, "chore", "Two", 10))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(children(&h, &p2).await.1["children"], json!([]));

    // If the `created_by = caller` filter were dropped, the child's own item would appear in
    // p1's listing and stats.
    sync_batch(&h, &child, json!({"items": [row(1_000, "Private", 100)]})).await;
    let (_, body) = child_progress(&h, &p1, child_id).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], P1);
    assert_eq!(body["stats"]["xp"], 0);
}

#[tokio::test]
async fn unlink_handover_accepts_a_pending_offline_child_edit() {
    // Parent revokes while the child has an offline edit.
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = active_link(&h, &child, &parent).await;
    create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    let stored = stored_item(&h, child_id, P1).await;
    let pending = edit(&stored, json!({"name": "Offline rename"}));
    assert_eq!(unlink(&h, &parent, link).await, StatusCode::OK);
    let (status, body) = sync_batch(&h, &child, json!({"items": [pending]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
    let handed = stored_item(&h, child_id, P1).await;
    assert_eq!(handed.name, "Offline rename");
    assert_eq!(handed.created_by, Some(child_id));
    assert_eq!(
        child_progress(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );

    // Child revokes while they have an offline edit.
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = active_link(&h, &child, &parent).await;
    create_item(&h, &parent, child_id, create_body(P2, "habit", "Read", 15)).await;
    let stored = stored_item(&h, child_id, P2).await;
    let pending = edit(&stored, json!({"name": "Read more"}));
    assert_eq!(unlink(&h, &child, link).await, StatusCode::OK);
    let (status, body) = sync_batch(&h, &child, json!({"items": [pending]})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.get("refused").is_none(), "{body}");
    assert_eq!(stored_item(&h, child_id, P2).await.name, "Read more");
    assert_eq!(
        child_stats(&h, &parent, child_id).await.0,
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn a_refused_row_does_not_fail_the_batch_and_completions_still_apply() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    active_link(&h, &child, &parent).await;
    create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    let stored = stored_item(&h, child_id, P1).await;
    let date = today();

    let (status, body) = sync_batch(
        &h,
        &child,
        json!({
            "items": [
                edit(&stored, json!({"name": "Hijacked"})),
                row(1_000, "Still accepted", 10)
            ],
            "completions": [{
                "item_id": P1, "date": date, "completed_at": 1,
                "updated_at": 1, "deleted_at": null
            }]
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["refused"].as_array().unwrap().len(), 1);
    assert_eq!(body["refused"][0]["id"], P1);
    assert_eq!(body["refused"][0]["fields"], json!(["name"]));
    assert_eq!(body["refused"][0]["item"]["name"], "Bins");

    // The refused row was not written, the unrelated row was, and the completion was accepted.
    assert_eq!(stored_item(&h, child_id, P1).await, stored);
    assert!(stored_items(&h, child_id)
        .await
        .iter()
        .any(|item| item.id == OWN && item.name == "Still accepted"));
    let completions = store::UserDb::new(&h.state.db, child_id)
        .all_completions()
        .await
        .unwrap();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].item_id, P1);

    // Malformed JSON is still a 400; refusals are not the whole-batch failure path.
    let (status, _) = call_raw(&h.app, "POST", "/sync", &child, "{\"items\": [").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn parent_creation_counts_against_the_child_item_quota() {
    let config = Config::from_lookup(|key| {
        (key == "BREADTRACKER_MAX_ITEMS_PER_USER").then(|| "2".to_string())
    });
    let h = harness_with(config).await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    active_link(&h, &child, &parent).await;

    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P1, "chore", "One", 10))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P2, "chore", "Two", 10))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P3, "chore", "Three", 10))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    // Tombstones keep counting until a safe retention protocol exists.
    assert_eq!(
        delete_item(&h, &parent, child_id, P1).await.0,
        StatusCode::OK
    );
    assert_eq!(
        create_item(&h, &parent, child_id, create_body(P3, "chore", "Three", 10))
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn parent_writes_maintain_the_child_statistics_ledger() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    active_link(&h, &child, &parent).await;
    create_item(&h, &parent, child_id, create_body(P1, "chore", "Bins", 10)).await;
    let date = today();
    completion(&h, &child, P1, &date).await;

    let (status, stats) = call(
        &h.app,
        "GET",
        &format!("/stats?date={date}"),
        Some(&child),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{stats}");
    assert_eq!(stats["xp"], 10);
    assert_eq!(child_stats(&h, &parent, child_id).await.1["xp"], 10);

    // A points edit must rescale the XP already earned by the stored completion.
    assert_eq!(
        update_item(&h, &parent, child_id, P1, json!({"points": 25}))
            .await
            .0,
        StatusCode::OK
    );
    let (_, stats) = call(
        &h.app,
        "GET",
        &format!("/stats?date={date}"),
        Some(&child),
        None,
    )
    .await;
    assert_eq!(stats["xp"], 25);

    // Deleting the parent item drops the XP its completions earned.
    assert_eq!(
        delete_item(&h, &parent, child_id, P1).await.0,
        StatusCode::OK
    );
    let (_, stats) = call(
        &h.app,
        "GET",
        &format!("/stats?date={date}"),
        Some(&child),
        None,
    )
    .await;
    assert_eq!(stats["xp"], 0);
    assert_eq!(child_stats(&h, &parent, child_id).await.1["xp"], 0);
}
