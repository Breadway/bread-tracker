//! TRACK-65: an item's creation date is recorded once and never rewritten.
//!
//! The rule that uses it lives in `xp.rs` and the shared vectors. These tests cover the storage
//! contract the app and the server depend on: old clients omit the field and the server fills it
//! on first sight, a later push cannot move it, a pre-migration row keeps no lower bound, and the
//! field stays additive on the wire.

use bread_tracker_server::{db, model::Item, store::UserDb};
use chrono::{NaiveDate, Utc};

const I1: &str = "11111111-1111-4111-8111-111111111111";
const I2: &str = "22222222-2222-4222-8222-222222222222";

fn item(id: &str, name: &str, created_on: Option<&str>, updated_at: i64) -> Item {
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
        created_on: created_on.map(str::to_string),
        created_by: None,
        permission_override: None,
        seq: 0,
    }
}

fn date(s: &str) -> NaiveDate {
    NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap()
}

#[tokio::test]
async fn first_sight_fills_the_date_and_later_pushes_never_move_it() {
    let pool = db::connect_memory().await.unwrap();
    let user = UserDb::new(&pool, 1);
    let tz = user.settings().await.unwrap().tz();
    let today = Utc::now().with_timezone(&tz).date_naive();

    // An old client does not send the field. The row is new, so the server records today.
    user.apply_push(&[item(I1, "Dishes", None, 1000)], &[])
        .await
        .unwrap();
    let stored = user.pull(0).await.unwrap().items;
    assert_eq!(stored.len(), 1);
    let today = today.format("%Y-%m-%d").to_string();
    assert_eq!(
        stored[0].created_on.as_deref(),
        Some(today.as_str()),
        "first sight records the user's local date"
    );

    // A later push claiming an earlier creation date is ignored: the stored value is immutable.
    user.apply_push(&[item(I1, "Dishes", Some("2026-01-01"), 2000)], &[])
        .await
        .unwrap();
    let stored = user.pull(0).await.unwrap().items;
    assert_eq!(
        stored[0].created_on,
        Some(today),
        "a later push must not overwrite the recorded creation date"
    );
    assert_eq!(stored[0].updated_at, 2000, "the rest of the row still wins");

    // A new app sends its own date on the first push, and that is what is kept.
    user.apply_push(&[item(I2, "Recycling", Some("2026-10-01"), 1000)], &[])
        .await
        .unwrap();
    let stored = user.pull(0).await.unwrap().items;
    let recycling = stored.iter().find(|i| i.id == I2).unwrap();
    assert_eq!(
        recycling.created_on.as_deref(),
        Some("2026-10-01"),
        "the first writer supplies the date"
    );
}

#[tokio::test]
async fn a_pre_migration_row_keeps_no_lower_bound_when_it_is_pushed_again() {
    let pool = db::connect_memory().await.unwrap();
    let user = UserDb::new(&pool, 1);
    user.apply_push(&[item(I1, "Dishes", None, 1000)], &[])
        .await
        .unwrap();
    // This is exactly how 0013 leaves every existing row.
    sqlx::query("UPDATE items SET created_on = NULL WHERE user_id = 1 AND id = ?")
        .bind(I1)
        .execute(&pool)
        .await
        .unwrap();

    // An upgraded app may now send a date for an old item; the stored NULL must win.
    user.apply_push(&[item(I1, "Dishes", Some("2026-10-01"), 2000)], &[])
        .await
        .unwrap();
    let stored = user.pull(0).await.unwrap().items;
    assert_eq!(
        stored[0].created_on, None,
        "a legacy item keeps no lower bound so past stats do not change"
    );
}

#[tokio::test]
async fn created_on_is_additive_on_the_wire() {
    // Missing on input is accepted and stays absent when serialized, so an old app and an old
    // server both keep working (no deny_unknown_fields, no required field).
    let parsed: Item = serde_json::from_value(serde_json::json!({
        "id": I1, "kind": "chore", "name": "Dishes", "weekdays": 127,
        "remind_time": null, "points": 10, "due_date": null, "sort": 0,
        "active": true, "updated_at": 1, "deleted_at": null
    }))
    .unwrap();
    assert_eq!(parsed.created_on, None);
    let json = serde_json::to_value(&parsed).unwrap();
    assert!(
        json.get("created_on").is_none(),
        "an old-format item must not grow the field in responses"
    );

    // An unknown extra field is still ignored rather than rejected.
    let parsed: Item = serde_json::from_value(serde_json::json!({
        "id": I1, "kind": "chore", "name": "Dishes", "weekdays": 127,
        "remind_time": null, "points": 10, "due_date": null, "sort": 0,
        "active": true, "updated_at": 1, "deleted_at": null, "future_field": true
    }))
    .unwrap();
    assert!(parsed.created_on.is_none());
}

#[test]
fn created_on_validation_rejects_garbage_and_far_future_dates() {
    let today = date("2026-10-19");
    let mut it = item(I1, "Dishes", None, 1);
    it.validate(today).unwrap();
    it.created_on = Some("2026-10-01".into());
    it.validate(today).unwrap();
    // A device can be a calendar day ahead of the server's UTC date.
    it.created_on = Some("2026-10-20".into());
    it.validate(today).unwrap();
    for bad in ["not-a-date", "2026-10-21", "2019-12-31"] {
        it.created_on = Some(bad.into());
        assert!(
            it.validate(today).is_err(),
            "{bad} should be rejected as a creation date"
        );
    }
}
