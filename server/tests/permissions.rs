//! TRACK-58: effective permissions, per-link configuration and the sequence bump that makes a
//! permission change reach the child without pretending an item was edited.

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

const P1: &str = "11111111-1111-4111-8111-111111111111";
const P2: &str = "22222222-2222-4222-8222-222222222222";
const OWN: &str = "33333333-3333-4333-8333-333333333333";

struct Harness {
    app: Router,
    state: AppState,
}

async fn harness() -> Harness {
    let state = AppState::new(
        db::connect_memory().await.unwrap(),
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

async fn round_trip(h: &Harness, child: &str, parent: &str, accept: bool) -> (i64, i64, i64) {
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
    let child_id: i64 = sqlx::query_scalar("SELECT child_id FROM family_links WHERE id = ?")
        .bind(link)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    let parent_id: i64 = sqlx::query_scalar("SELECT parent_id FROM family_links WHERE id = ?")
        .bind(link)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    if accept {
        let (status, body) = call(
            &h.app,
            "POST",
            &format!("/family/links/{link}/accept"),
            Some(child),
            None,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    }
    (link, child_id, parent_id)
}

async fn insert_item(
    pool: &SqlitePool,
    user_id: i64,
    id: &str,
    created_by: Option<i64>,
    permission_override: Option<&str>,
    updated_at: i64,
    due_date: Option<&str>,
) -> i64 {
    let seq: i64 =
        sqlx::query_scalar("UPDATE meta SET val = val + 1 WHERE key = 'seq' RETURNING val")
            .fetch_one(pool)
            .await
            .unwrap();
    sqlx::query(
        "INSERT INTO items (user_id, id, kind, name, weekdays, points, due_date, active,
                            updated_at, created_by, permission_override, seq)
         VALUES (?, ?, 'chore', 'Bins', 127, 10, ?, 1, ?, ?, ?, ?)",
    )
    .bind(user_id)
    .bind(id)
    .bind(due_date)
    .bind(updated_at)
    .bind(created_by)
    .bind(permission_override)
    .bind(seq)
    .execute(pool)
    .await
    .unwrap();
    seq
}

async fn item_row(
    pool: &SqlitePool,
    user_id: i64,
    id: &str,
) -> (i64, i64, Option<i64>, Option<String>) {
    sqlx::query_as(
        "SELECT seq, updated_at, created_by, permission_override FROM items
         WHERE user_id = ? AND id = ?",
    )
    .bind(user_id)
    .bind(id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn cursor(h: &Harness, token: &str, since: i64) -> (i64, Value) {
    let (status, body) = call(
        &h.app,
        "GET",
        &format!("/sync?since={since}"),
        Some(token),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (body["cursor"].as_i64().unwrap(), body)
}

async fn update_link(h: &Harness, token: &str, link: i64, body: Value) -> (StatusCode, Value) {
    call(
        &h.app,
        "PUT",
        &format!("/family/links/{link}"),
        Some(token),
        Some(body),
    )
    .await
}

#[tokio::test]
async fn override_beats_default_in_full_and_incremental_pulls() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (_link, _, _) = round_trip(&h, &child, &parent, true).await;

    insert_item(&h.state.db, child_id, P1, Some(parent_id), None, 500, None).await;
    insert_item(&h.state.db, child_id, OWN, Some(child_id), None, 500, None).await;

    // Default complete_only, and the child's own item has no effective family permission.
    let (first_cursor, body) = cursor(&h, &child, 0).await;
    let items = body["items"].as_array().unwrap();
    let parent_item = items.iter().find(|i| i["id"] == P1).unwrap();
    assert_eq!(parent_item["created_by"], parent_id);
    assert_eq!(parent_item["effective_permission"], "complete_only");
    let own_item = items.iter().find(|i| i["id"] == OWN).unwrap();
    assert_eq!(own_item["created_by"], child_id);
    assert!(own_item.get("effective_permission").is_none());

    // A per-item override wins in the next incremental pull, without an item edit.
    match family::set_item_override(&h.state.db, parent_id, child_id, P1, Some("full"))
        .await
        .unwrap()
    {
        family::OverrideOutcome::Updated {
            effective_permission,
        } => assert_eq!(effective_permission, "full"),
        other => panic!("unexpected outcome: {other:?}"),
    }
    let (_, body) = cursor(&h, &child, first_cursor).await;
    let items = body["items"].as_array().unwrap();
    let parent_item = items.iter().find(|i| i["id"] == P1).unwrap();
    assert_eq!(parent_item["effective_permission"], "full");
    assert_eq!(parent_item["permission_override"], "full");
    assert_eq!(parent_item["updated_at"], 500);
    assert!(
        items.iter().all(|i| i["id"] != OWN),
        "only the changed item is pulled"
    );

    // The full snapshot agrees.
    let (_, body) = cursor(&h, &child, 0).await;
    let parent_item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == P1)
        .unwrap();
    assert_eq!(parent_item["effective_permission"], "full");
}

#[tokio::test]
async fn default_change_bumps_seq_without_touching_updated_at() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = round_trip(&h, &child, &parent, true).await;

    insert_item(&h.state.db, child_id, P1, Some(parent_id), None, 500, None).await;
    insert_item(&h.state.db, child_id, P2, Some(parent_id), None, 600, None).await;
    insert_item(&h.state.db, child_id, OWN, Some(child_id), None, 700, None).await;
    let own_before = item_row(&h.state.db, child_id, OWN).await;
    let (pull_cursor, _) = cursor(&h, &child, 0).await;

    let (status, body) = update_link(
        &h,
        &parent,
        link,
        json!({"default_permission": "full", "notify_on_complete": true}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["permission"], "full");
    assert_eq!(body["notify_on_complete"], true);

    // Both of this parent's items moved past the cursor; their updated_at did not move, and
    // the child's own item was not touched at all.
    for (id, updated_at) in [(P1, 500), (P2, 600)] {
        let (seq, stored_updated_at, created_by, _) = item_row(&h.state.db, child_id, id).await;
        assert!(seq > pull_cursor, "{id} seq {seq} must pass {pull_cursor}");
        assert_eq!(stored_updated_at, updated_at);
        assert_eq!(created_by, Some(parent_id));
    }
    assert_eq!(item_row(&h.state.db, child_id, OWN).await, own_before);

    let (_, body) = cursor(&h, &child, pull_cursor).await;
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{body}");
    for item in items {
        assert_eq!(item["effective_permission"], "full");
    }
    let p1 = items.iter().find(|i| i["id"] == P1).unwrap();
    assert_eq!(p1["updated_at"], 500);
}

#[tokio::test]
async fn invalid_enum_is_rejected_and_changes_nothing() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = round_trip(&h, &child, &parent, true).await;
    insert_item(&h.state.db, child_id, P1, Some(parent_id), None, 500, None).await;
    let before = item_row(&h.state.db, child_id, P1).await;

    let (status, body) = update_link(
        &h,
        &parent,
        link,
        json!({"default_permission": "everything"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (permission, notify): (String, i64) = sqlx::query_as(
        "SELECT default_permission, notify_on_complete FROM family_links WHERE id = ?",
    )
    .bind(link)
    .fetch_one(&h.state.db)
    .await
    .unwrap();
    assert_eq!(permission, "complete_only");
    assert_eq!(notify, 0);
    assert_eq!(item_row(&h.state.db, child_id, P1).await, before);

    assert_eq!(
        family::set_item_override(&h.state.db, parent_id, child_id, P1, Some("everything"))
            .await
            .unwrap(),
        family::OverrideOutcome::InvalidPermission
    );
    assert_eq!(item_row(&h.state.db, child_id, P1).await, before);

    // An empty update is a client bug, not a silent success.
    let (status, _) = update_link(&h, &parent, link, json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn only_the_parent_changes_an_active_link_or_an_override() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (intruder_id, intruder) = user_with_token(&h, "Intruder").await;

    // Pending links cannot be configured.
    let (link, _, _) = round_trip(&h, &child, &parent, false).await;
    let (status, body) =
        update_link(&h, &parent, link, json!({"default_permission": "full"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");

    let (status, body) = call(
        &h.app,
        "POST",
        &format!("/family/links/{link}/accept"),
        Some(&child),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The child and an unrelated parent cannot change it, and an unowned id is a 404.
    let (status, _) = update_link(&h, &child, link, json!({"default_permission": "full"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = update_link(&h, &intruder, link, json!({"default_permission": "full"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = update_link(
        &h,
        &intruder,
        999_999,
        json!({"default_permission": "full"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    insert_item(&h.state.db, child_id, P1, Some(parent_id), None, 500, None).await;
    insert_item(&h.state.db, child_id, OWN, Some(child_id), None, 500, None).await;

    // The child cannot set an override through the parent helper, and the parent cannot
    // override the child's own item.
    assert_eq!(
        family::set_item_override(&h.state.db, child_id, child_id, P1, Some("full"))
            .await
            .unwrap(),
        family::OverrideOutcome::NotFound
    );
    assert_eq!(
        family::set_item_override(&h.state.db, parent_id, child_id, OWN, Some("full"))
            .await
            .unwrap(),
        family::OverrideOutcome::NotCreator
    );
    assert_eq!(
        family::set_item_override(&h.state.db, intruder_id, child_id, P1, Some("full"))
            .await
            .unwrap(),
        family::OverrideOutcome::NotFound
    );

    // After a revoke the link cannot be configured again.
    let (status, body) = call(
        &h.app,
        "DELETE",
        &format!("/family/links/{link}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = update_link(&h, &parent, link, json!({"notify_on_complete": true})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_child_sync_never_sets_provenance_or_an_override() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (_link, _, _) = round_trip(&h, &child, &parent, true).await;
    insert_item(
        &h.state.db,
        child_id,
        P1,
        Some(parent_id),
        Some("complete_only"),
        500,
        None,
    )
    .await;
    let (first_cursor, _) = cursor(&h, &child, 0).await;

    let (status, body) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&child),
        Some(json!({"items": [{
            "id": P1, "kind": "chore", "name": "Bins", "weekdays": 127,
            "remind_time": null, "points": 10, "due_date": null, "sort": 0,
            "active": true, "updated_at": 900, "deleted_at": null,
            "created_by": child_id, "permission_override": "full"
        }]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (_, body) = cursor(&h, &child, first_cursor).await;
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["created_by"], parent_id, "provenance is immutable");
    assert_eq!(
        item["permission_override"], "complete_only",
        "the sync upsert must not write the override column"
    );
    assert_eq!(item["effective_permission"], "complete_only");
    assert_eq!(item["updated_at"], 900, "the child's real edit still wins");
}

#[tokio::test]
async fn tightening_the_default_keeps_the_childs_existing_edits() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = round_trip(&h, &child, &parent, true).await;

    insert_item(
        &h.state.db,
        child_id,
        P1,
        Some(parent_id),
        None,
        500,
        Some("2026-01-01"),
    )
    .await;
    let (status, body) =
        update_link(&h, &parent, link, json!({"default_permission": "full"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The child edits a reschedule field while the link is full.
    let (status, body) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&child),
        Some(json!({"items": [{
            "id": P1, "kind": "chore", "name": "Bins", "weekdays": 127,
            "remind_time": null, "points": 10, "due_date": "2026-02-02", "sort": 0,
            "active": true, "updated_at": 900, "deleted_at": null
        }]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (pull_cursor, _) = cursor(&h, &child, 0).await;

    // Tightening changes future edits only: the stored child edit remains, and the item is
    // delivered again with the lower effective level because its seq moved.
    let (status, body) = update_link(
        &h,
        &parent,
        link,
        json!({"default_permission": "complete_only"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, body) = cursor(&h, &child, pull_cursor).await;
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["effective_permission"], "complete_only");
    assert_eq!(item["due_date"], "2026-02-02");
    assert_eq!(item["updated_at"], 900);
}

#[tokio::test]
async fn parent_listing_resolves_the_same_effective_permission() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (link, _, _) = round_trip(&h, &child, &parent, false).await;

    // A pending link has no listing at all.
    assert!(family::assigned_items(&h.state.db, parent_id, child_id)
        .await
        .unwrap()
        .is_none());

    let (status, body) = call(
        &h.app,
        "POST",
        &format!("/family/links/{link}/accept"),
        Some(&child),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    insert_item(&h.state.db, child_id, P1, Some(parent_id), None, 500, None).await;
    insert_item(
        &h.state.db,
        child_id,
        P2,
        Some(parent_id),
        Some("full"),
        600,
        None,
    )
    .await;
    insert_item(&h.state.db, child_id, OWN, Some(child_id), None, 700, None).await;

    let listed = family::assigned_items(&h.state.db, parent_id, child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(listed.len(), 2, "only items this parent created");
    assert_eq!(listed[0].id, P1);
    assert_eq!(
        listed[0].effective_permission.as_deref(),
        Some("complete_only")
    );
    assert_eq!(listed[1].id, P2);
    assert_eq!(listed[1].effective_permission.as_deref(), Some("full"));
    assert!(listed.iter().all(|i| i.created_by == Some(parent_id)));

    // Tightening the link default is reflected in the parent's listing too.
    let (status, body) = update_link(
        &h,
        &parent,
        link,
        json!({"default_permission": "complete_and_reschedule"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listed = family::assigned_items(&h.state.db, parent_id, child_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        listed[0].effective_permission.as_deref(),
        Some("complete_and_reschedule")
    );
    assert_eq!(
        listed[1].effective_permission.as_deref(),
        Some("full"),
        "an item override still wins"
    );
}

#[tokio::test]
async fn clearing_an_override_returns_to_the_link_default() {
    let h = harness().await;
    let (child_id, child) = user_with_token(&h, "Kid").await;
    let (parent_id, parent) = user_with_token(&h, "Parent").await;
    let (_link, _, _) = round_trip(&h, &child, &parent, true).await;
    insert_item(
        &h.state.db,
        child_id,
        P1,
        Some(parent_id),
        Some("full"),
        500,
        None,
    )
    .await;
    let (first_cursor, _) = cursor(&h, &child, 0).await;

    match family::set_item_override(&h.state.db, parent_id, child_id, P1, None)
        .await
        .unwrap()
    {
        family::OverrideOutcome::Updated {
            effective_permission,
        } => assert_eq!(effective_permission, "complete_only"),
        other => panic!("unexpected outcome: {other:?}"),
    }
    let (_, body) = cursor(&h, &child, first_cursor).await;
    let item = &body["items"].as_array().unwrap()[0];
    assert_eq!(item["effective_permission"], "complete_only");
    assert!(item.get("permission_override").is_none());
    assert_eq!(item["updated_at"], 500);
}

#[tokio::test]
async fn sync_pull_still_uses_the_per_user_sequence_indexes() {
    let pool = db::connect_memory().await.unwrap();
    let plans = store::UserDb::new(&pool, 1)
        .pull_query_plans()
        .await
        .unwrap();
    let joined = plans.join(" | ");
    assert!(joined.contains("items_user_seq"), "{joined}");
    assert!(joined.contains("completions_user_seq"), "{joined}");
    assert!(!joined.contains("SCAN"), "{joined}");
    assert!(!joined.contains("TEMP B-TREE"), "{joined}");
}
