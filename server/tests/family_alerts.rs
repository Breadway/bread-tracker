//! TRACK-61: optional completion alerts to the parent.
//!
//! These tests drive the real sync and family routes against a `MockSender`, so they exercise
//! the enqueue inside the child's write transaction and the send-time checks in the scheduler
//! tick. Every negative case asserts both that no message was sent and that the pending row was
//! resolved, because suppression is final rather than deferred.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use bread_tracker_server::config::Config;
use bread_tracker_server::push::{self, MockSender};
use bread_tracker_server::{api, auth, db, users, AppState};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower::ServiceExt;

const A1: &str = "11111111-1111-4111-8111-111111111111";
const A2: &str = "22222222-2222-4222-8222-222222222222";
const A3: &str = "33333333-3333-4333-8333-333333333333";
const A4: &str = "44444444-4444-4444-8444-444444444444";
const A5: &str = "55555555-5555-4555-8555-555555555555";
const PRIVATE: &str = "99999999-9999-4999-8999-999999999999";

struct Harness {
    app: Router,
    state: AppState,
    mock: Arc<MockSender>,
}

async fn harness() -> Harness {
    let mock = Arc::new(MockSender::default());
    let state = AppState::new(
        db::connect_memory().await.unwrap(),
        uuid::Uuid::new_v4().to_string(),
        Config::from_lookup(|_| None),
        mock.clone(),
    );
    Harness {
        app: api::router(state.clone()),
        state,
        mock,
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

async fn issue(h: &Harness, child: &str) -> String {
    let (status, body) = call(&h.app, "POST", "/family/code", Some(child), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body["code"].as_str().unwrap().to_string()
}

async fn active_link(h: &Harness, child: &str, parent: &str) -> (i64, i64) {
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
    let child_id: i64 = sqlx::query_scalar("SELECT child_id FROM family_links WHERE id = ?")
        .bind(link)
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    let (status, body) = call(
        &h.app,
        "POST",
        &format!("/family/links/{link}/accept"),
        Some(child),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    (link, child_id)
}

async fn set_notify(h: &Harness, parent: &str, link: i64, on: bool) {
    let (status, body) = call(
        &h.app,
        "PUT",
        &format!("/family/links/{link}"),
        Some(parent),
        Some(json!({"notify_on_complete": on})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["notify_on_complete"], on);
}

async fn create_item(h: &Harness, parent: &str, child_id: i64, id: &str, name: &str) {
    let (status, body) = call(
        &h.app,
        "POST",
        &format!("/family/children/{child_id}/items"),
        Some(parent),
        Some(json!({
            "id": id,
            "kind": "chore",
            "name": name,
            "weekdays": 127,
            "remind_time": null,
            "points": 10,
            "due_date": null,
            "sort": 0,
            "active": true,
            "updated_at": 1
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn register_parent_device(h: &Harness, parent: &str) {
    let (status, body) = call(
        &h.app,
        "PUT",
        "/device",
        Some(parent),
        Some(json!({"fcm_token": "parent-fcm-token"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

fn completion_row(item_id: &str) -> Value {
    json!({
        "item_id": item_id,
        "date": chrono::Utc::now().date_naive().format("%Y-%m-%d").to_string(),
        "completed_at": 1,
        "updated_at": 1,
        "deleted_at": null
    })
}

async fn complete(h: &Harness, child: &str, item_ids: &[&str]) {
    let completions: Vec<Value> = item_ids.iter().map(|id| completion_row(id)).collect();
    let (status, body) = call(
        &h.app,
        "POST",
        "/sync",
        Some(child),
        Some(json!({"completions": completions})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

async fn pending_alerts(h: &Harness) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM family_alerts WHERE sent_at IS NULL")
        .fetch_one(&h.state.db)
        .await
        .unwrap()
}

/// A tick far enough past the batch window that the enqueued rows are eligible. It is also the
/// instant quiet-hours tests build their window around.
fn after_window() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::seconds(120)
}

async fn messages(h: &Harness) -> Vec<bread_tracker_server::push::Message> {
    h.mock.sent.lock().unwrap().clone()
}

/// Parent and child with an active link, alerts on, a device registered on the parent, and the
/// given assigned items created under the child.
async fn family_world(h: &Harness, items: &[(&str, &str)]) -> (String, i64, String) {
    let (_parent_id, parent) = user_with_token(h, "Parent").await;
    let (_child_id, child) = user_with_token(h, "Kid").await;
    let (link, child_id) = active_link(h, &child, &parent).await;
    set_notify(h, &parent, link, true).await;
    for (id, name) in items {
        create_item(h, &parent, child_id, id, name).await;
    }
    register_parent_device(h, &parent).await;
    (parent, child_id, child)
}

#[tokio::test]
async fn a_burst_across_two_syncs_sends_one_message_with_assigned_names_only() {
    let h = harness().await;
    let (_parent, _child_id, child) = family_world(
        &h,
        &[
            (A1, "Bins"),
            (A2, "Dishes"),
            (A3, "Laundry"),
            (A4, "Plants"),
            (A5, "Rubbish"),
        ],
    )
    .await;
    // The child also owns a private item and completes it in the same window.
    let (status, body) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&child),
        Some(json!({"items": [{
            "id": PRIVATE, "kind": "habit", "name": "Private diary", "weekdays": 127,
            "remind_time": null, "points": 5, "due_date": null, "sort": 0, "active": true,
            "updated_at": 1, "deleted_at": null
        }]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    complete(&h, &child, &[A1, A2, A3, PRIVATE]).await;
    complete(&h, &child, &[A4, A5]).await;
    assert_eq!(pending_alerts(&h).await, 5, "only the assigned items queue");

    // Before the batch window closes nothing is sent, even though every row is enqueued.
    assert_eq!(push::tick(&h.state, Utc::now()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());

    // After it, one message covers the burst.
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 1);
    let sent = messages(&h).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].token, "parent-fcm-token");
    assert_eq!(sent[0].title, "5 items completed");
    for name in ["Bins", "Dishes", "Laundry", "Plants", "Rubbish"] {
        assert!(sent[0].body.contains(name), "{}", sent[0].body);
    }
    assert!(!sent[0].body.contains("Private diary"));
    assert!(!sent[0].body.contains(A1));
    assert!(!sent[0].body.contains("parent-fcm-token"));
    assert!(sent[0].data.is_empty(), "no ids or tokens in the payload");
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM family_alerts")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(retained, 5, "rows are resolved, not deleted");
    assert_eq!(pending_alerts(&h).await, 0);

    // A second tick, or a second device pass, never repeats it.
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert_eq!(messages(&h).await.len(), 1);
}

#[tokio::test]
async fn one_completion_sends_exactly_one_message() {
    let h = harness().await;
    let (_parent, _child_id, child) = family_world(&h, &[(A1, "Bins")]).await;

    complete(&h, &child, &[A1]).await;
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 1);

    let sent = messages(&h).await;
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert_eq!(sent[0].title, "Bins");
    assert_eq!(sent[0].body, "Assigned item completed");
    assert!(sent[0].data.is_empty());
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
}

#[tokio::test]
async fn a_crash_between_enqueue_and_send_retries_once_without_duplicating() {
    let h = harness().await;
    let (_parent, _child_id, child) = family_world(&h, &[(A1, "Bins"), (A2, "Dishes")]).await;

    complete(&h, &child, &[A1, A2]).await;
    assert_eq!(pending_alerts(&h).await, 2);

    // The process dies here: nothing was sent. A fresh state over the same database is the
    // restart, and its tick delivers the pending burst once.
    let restarted = AppState::new(
        h.state.db.clone(),
        uuid::Uuid::new_v4().to_string(),
        Config::from_lookup(|_| None),
        h.mock.clone(),
    );
    assert_eq!(push::tick(&restarted, after_window()).await.unwrap(), 1);
    assert_eq!(messages(&h).await.len(), 1);

    // The child's client re-sends the same completions after reconnecting: the unique key and
    // the unchanged live state mean no second alert, and the tick stays quiet.
    complete(&h, &child, &[A1, A2]).await;
    assert_eq!(pending_alerts(&h).await, 0);
    assert_eq!(push::tick(&restarted, after_window()).await.unwrap(), 0);
    assert_eq!(messages(&h).await.len(), 1);
}

#[tokio::test]
async fn a_revoked_link_sends_nothing_and_is_not_replayed() {
    let h = harness().await;
    let (parent, _child_id, child) = family_world(&h, &[(A1, "Bins")]).await;
    let link: i64 = sqlx::query_scalar("SELECT id FROM family_links")
        .fetch_one(&h.state.db)
        .await
        .unwrap();

    complete(&h, &child, &[A1]).await;
    assert_eq!(pending_alerts(&h).await, 1);

    // Revoke after the completion was accepted but before the tick.
    let (status, _) = call(
        &h.app,
        "DELETE",
        &format!("/family/links/{link}"),
        Some(&parent),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
    assert_eq!(pending_alerts(&h).await, 0, "resolved without sending");
}

#[tokio::test]
async fn a_child_private_item_sends_nothing() {
    let h = harness().await;
    let (_parent, _child_id, child) = family_world(&h, &[(A1, "Bins")]).await;

    let (status, body) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&child),
        Some(json!({"items": [{
            "id": PRIVATE, "kind": "chore", "name": "Private diary", "weekdays": 127,
            "remind_time": null, "points": 5, "due_date": null, "sort": 0, "active": true,
            "updated_at": 1, "deleted_at": null
        }]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    complete(&h, &child, &[PRIVATE]).await;

    assert_eq!(pending_alerts(&h).await, 0);
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
}

#[tokio::test]
async fn the_parent_reminders_switch_off_suppresses_and_does_not_defer() {
    let h = harness().await;
    let (parent, _child_id, child) = family_world(&h, &[(A1, "Bins")]).await;
    complete(&h, &child, &[A1]).await;

    let (status, body) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&parent),
        Some(json!({"reminders_enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
    assert_eq!(pending_alerts(&h).await, 0, "suppressed, not queued");

    // Turning reminders back on later does not replay the old alert.
    let (status, body) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&parent),
        Some(json!({"reminders_enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
}

#[tokio::test]
async fn the_parent_quiet_hours_suppress_and_do_not_defer() {
    let h = harness().await;
    let (parent, _child_id, child) = family_world(&h, &[(A1, "Bins")]).await;
    complete(&h, &child, &[A1]).await;

    // A two hour window around the tick instant, in the parent's default UTC.
    let at = after_window();
    let start = (at - Duration::hours(1)).format("%H:%M").to_string();
    let end = (at + Duration::hours(1)).format("%H:%M").to_string();
    let (status, body) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&parent),
        Some(json!({"timezone": "UTC", "quiet_start": start, "quiet_end": end})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert_eq!(push::tick(&h.state, at).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
    assert_eq!(pending_alerts(&h).await, 0, "resolved during quiet hours");

    // Once quiet hours end the alert is gone, not released late.
    let (status, body) = call(
        &h.app,
        "PUT",
        "/settings",
        Some(&parent),
        Some(json!({"quiet_start": "", "quiet_end": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
}

#[tokio::test]
async fn alerts_are_off_by_default_and_enqueue_nothing() {
    let h = harness().await;
    let (_parent_id, parent) = user_with_token(&h, "Parent").await;
    let (_child_id, child) = user_with_token(&h, "Kid").await;
    let (link, child_id) = active_link(&h, &child, &parent).await;
    // The default is off; only an explicit parent toggle enables alerts.
    let (status, body) = call(&h.app, "GET", "/family/links", Some(&parent), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        !body["links"][0]["notify_on_complete"].as_bool().unwrap(),
        "{body}"
    );
    assert_eq!(body["links"][0]["id"], link);
    create_item(&h, &parent, child_id, A1, "Bins").await;
    register_parent_device(&h, &parent).await;

    complete(&h, &child, &[A1]).await;
    assert_eq!(pending_alerts(&h).await, 0);
    assert_eq!(push::tick(&h.state, after_window()).await.unwrap(), 0);
    assert!(messages(&h).await.is_empty());
}

#[tokio::test]
async fn the_childs_own_reminders_are_unchanged() {
    let h = harness().await;
    let (_parent, child_id, child) = family_world(&h, &[(A1, "Bins")]).await;

    // A reminder on a private child item still fires to the child's device only.
    let (status, body) = call(
        &h.app,
        "POST",
        "/sync",
        Some(&child),
        Some(json!({"items": [{
            "id": PRIVATE, "kind": "chore", "name": "Private diary", "weekdays": 127,
            "remind_time": "12:00", "points": 5, "due_date": null, "sort": 0, "active": true,
            "updated_at": 1, "deleted_at": null
        }]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(
        &h.app,
        "PUT",
        "/device",
        Some(&child),
        Some(json!({"fcm_token": "child-fcm-token"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stored: Option<i64> =
        sqlx::query_scalar("SELECT user_id FROM devices WHERE fcm_token = 'child-fcm-token'")
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(stored, Some(child_id));

    // Completing an assigned item must not touch the child's reminder bookkeeping.
    complete(&h, &child, &[A1]).await;
    assert_eq!(pending_alerts(&h).await, 1);
    let reminders: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM reminders_sent")
        .fetch_one(&h.state.db)
        .await
        .unwrap();
    assert_eq!(reminders, 0);
    // The child item is still scheduled and the child's own settings are untouched.
    let remind: Option<String> =
        sqlx::query_scalar("SELECT remind_time FROM items WHERE user_id = ? AND id = ?")
            .bind(child_id)
            .bind(PRIVATE)
            .fetch_one(&h.state.db)
            .await
            .unwrap();
    assert_eq!(remind.as_deref(), Some("12:00"));
}
