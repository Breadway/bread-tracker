//! TRACK-55: the family schema and item provenance must be additive.
//!
//! No family route exists yet. These tests pin the storage contract the later routes depend
//! on: old clients keep syncing unchanged, the server owns `created_by`, an item override is
//! optional and validated, deleting a creator never deletes the child's item, and the parent
//! listing index exists and serves the intended query.

use bread_tracker_server::model::Item;
use bread_tracker_server::{db, store::UserDb, users};

const I1: &str = "11111111-1111-4111-8111-111111111111";
const I2: &str = "22222222-2222-4222-8222-222222222222";

fn item(id: &str, name: &str, updated_at: i64) -> Item {
    Item {
        id: id.to_string(),
        kind: "chore".to_string(),
        name: name.to_string(),
        weekdays: 127,
        remind_time: None,
        points: 10,
        due_date: None,
        sort: 0,
        active: true,
        updated_at,
        deleted_at: None,
        created_on: None,
        created_by: None,
        permission_override: None,
        effective_permission: None,
        seq: 0,
    }
}

async fn scalar(pool: &sqlx::SqlitePool, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn old_client_sync_still_works_and_gets_server_provenance() {
    let pool = db::connect_memory().await.unwrap();
    let user = UserDb::new(&pool, 1);

    // An old client sends neither field. The push is accepted and the server attributes the
    // item to its owner, which keeps existing behavior unchanged.
    user.apply_push(&[item(I1, "Dishes", 1000)], &[])
        .await
        .unwrap();
    let stored = user.pull(0).await.unwrap().items;
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].created_by, Some(1));
    assert_eq!(stored[0].permission_override, None);

    // A second old-client push still wins on the fields it owns and cannot move provenance.
    let mut edit = item(I1, "Dishes and bins", 2000);
    edit.created_by = Some(99);
    edit.permission_override = Some("full".to_string());
    user.apply_push(&[edit], &[]).await.unwrap();
    let stored = user.pull(0).await.unwrap().items;
    assert_eq!(stored[0].name, "Dishes and bins");
    assert_eq!(
        stored[0].created_by,
        Some(1),
        "client-supplied provenance must be ignored"
    );
    assert_eq!(
        stored[0].permission_override, None,
        "a client cannot set the override through /sync"
    );

    // A later family route may set an override; a normal sync write must not clear it.
    sqlx::query(
        "UPDATE items SET permission_override = 'complete_and_reschedule'
         WHERE user_id = 1 AND id = ?",
    )
    .bind(I1)
    .execute(&pool)
    .await
    .unwrap();
    user.apply_push(&[item(I1, "Dishes", 3000)], &[])
        .await
        .unwrap();
    let stored = user.pull(0).await.unwrap().items;
    assert_eq!(
        stored[0].permission_override.as_deref(),
        Some("complete_and_reschedule"),
        "the sync upsert must not write the override column"
    );
}

#[tokio::test]
async fn wire_format_stays_compatible_in_both_directions() {
    let old_json = serde_json::json!({
        "id": I1, "kind": "chore", "name": "Dishes", "weekdays": 127,
        "remind_time": null, "points": 10, "due_date": null, "sort": 0,
        "active": true, "updated_at": 1, "deleted_at": null
    });
    let parsed: Item = serde_json::from_value(old_json).unwrap();
    assert_eq!(parsed.created_by, None);
    assert_eq!(parsed.permission_override, None);

    // A server that has not been upgraded ignores the new fields rather than rejecting them.
    let new_json = serde_json::json!({
        "id": I2, "kind": "habit", "name": "Read", "weekdays": 127,
        "remind_time": null, "points": 15, "due_date": null, "sort": 0,
        "active": true, "updated_at": 2, "deleted_at": null,
        "created_by": 1, "permission_override": "complete_only", "future_field": true
    });
    let parsed: Item = serde_json::from_value(new_json).unwrap();
    assert_eq!(parsed.created_by, Some(1));
    assert_eq!(parsed.permission_override.as_deref(), Some("complete_only"));

    let json = serde_json::to_value(&parsed).unwrap();
    assert_eq!(json["created_by"], 1);
    assert_eq!(json["permission_override"], "complete_only");

    // An item without provenance still serializes without the fields, exactly like an old
    // server's response, so an old app is unaffected.
    let old = item(I1, "Dishes", 1);
    let json = serde_json::to_value(&old).unwrap();
    assert!(json.get("created_by").is_none());
    assert!(json.get("permission_override").is_none());
}

#[tokio::test]
async fn links_and_permissions_are_validated_and_deleting_a_parent_keeps_the_item() {
    let pool = db::connect_memory().await.unwrap();
    let child = users::create(&pool, "Child", false).await.unwrap();
    assert_ne!(child, 1);

    sqlx::query(
        "INSERT INTO family_links (parent_id, child_id, status, created_at)
         VALUES (1, ?, 'pending', 1000)",
    )
    .bind(child)
    .execute(&pool)
    .await
    .unwrap();

    // A user cannot link to themselves, the pair is unique, and the enums are enforced.
    for sql in [
        "INSERT INTO family_links (parent_id, child_id, status, created_at) VALUES (1, 1, 'pending', 1)".to_string(),
        format!(
            "INSERT INTO family_links (parent_id, child_id, status, created_at) VALUES (1, {child}, 'pending', 1)"
        ),
        format!(
            "INSERT INTO family_links (parent_id, child_id, status, created_at) VALUES (1, {child}, 'later', 1)"
        ),
        format!(
            "INSERT INTO family_links (parent_id, child_id, status, default_permission, created_at) VALUES (1, {child}, 'pending', 'everything', 1)"
        ),
        format!(
            "INSERT INTO family_links (parent_id, child_id, status, notify_on_complete, created_at) VALUES (1, {child}, 'pending', 2, 1)"
        ),
    ] {
        assert!(
            sqlx::query(&sql).execute(&pool).await.is_err(),
            "must reject: {sql}"
        );
    }

    // A parent-added item is stored under the child with the parent as creator. Deleting the
    // parent clears provenance and leaves the item on the child's list.
    sqlx::query(
        "INSERT INTO items (user_id, id, kind, name, points, updated_at, seq, created_by)
         VALUES (?, ?, 'chore', 'Bins', 10, 1, 2, 1)",
    )
    .bind(child)
    .bind(I1)
    .execute(&pool)
    .await
    .unwrap();
    assert!(
        sqlx::query("UPDATE items SET permission_override = 'everything' WHERE id = ?")
            .bind(I1)
            .execute(&pool)
            .await
            .is_err()
    );
    sqlx::query("UPDATE items SET permission_override = 'complete_only' WHERE id = ?")
        .bind(I1)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("DELETE FROM users WHERE id = 1")
        .execute(&pool)
        .await
        .unwrap();
    let kept: (i64, Option<i64>, Option<String>) = sqlx::query_as(
        "SELECT user_id, created_by, permission_override FROM items WHERE user_id = ? AND id = ?",
    )
    .bind(child)
    .bind(I1)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        kept,
        (child, None, Some("complete_only".to_string())),
        "the child keeps the item and only its provenance is cleared"
    );
    assert_eq!(
        scalar(&pool, "SELECT COUNT(*) FROM family_links").await,
        0,
        "deleting the parent removes the link"
    );
    let violations: Vec<(String, i64, String, i64)> = sqlx::query_as("PRAGMA foreign_key_check")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert!(violations.is_empty(), "{violations:?}");
}

#[tokio::test]
async fn parent_listing_index_exists_and_is_used() {
    let pool = db::connect_memory().await.unwrap();
    assert_eq!(
        scalar(
            &pool,
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'items_user_created_by_seq'"
        )
        .await,
        1
    );
    let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(
        "EXPLAIN QUERY PLAN SELECT id FROM items
         WHERE user_id = ? AND created_by = ? AND seq > ? ORDER BY seq",
    )
    .bind(1_i64)
    .bind(1_i64)
    .bind(0_i64)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert!(
        plan.iter()
            .any(|r| r.3.contains("items_user_created_by_seq")),
        "{plan:?}"
    );
}
