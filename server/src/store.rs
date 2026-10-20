//! Sync storage: last-write-wins upserts with a server-assigned change counter.

use sqlx::SqliteConnection;

use crate::db::Db;
use crate::model::{Completion, Item, SyncResponse};

const DAY_MS: i64 = 86_400_000;

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

async fn next_seq(conn: &mut SqliteConnection) -> Result<i64, sqlx::Error> {
    sqlx::query_scalar("UPDATE meta SET val = val + 1 WHERE key = 'seq' RETURNING val")
        .fetch_one(conn)
        .await
}

/// Apply client rows. A row only wins when its `updated_at` is newer than the stored one.
/// Future timestamps are clamped to a day ahead so a bad phone clock cannot freeze a row.
pub async fn apply_push(db: &Db, items: &[Item], completions: &[Completion]) -> Result<(), sqlx::Error> {
    let ceiling = now_ms() + DAY_MS;
    let mut tx = db.begin().await?;

    for it in items {
        let updated = it.updated_at.min(ceiling);
        let existing: Option<i64> = sqlx::query_scalar("SELECT updated_at FROM items WHERE id = ?")
            .bind(&it.id)
            .fetch_optional(&mut *tx)
            .await?;
        if existing.is_some_and(|e| e >= updated) {
            continue;
        }
        let seq = next_seq(&mut tx).await?;
        sqlx::query(
            "INSERT INTO items (id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
               kind = excluded.kind, name = excluded.name, weekdays = excluded.weekdays,
               remind_time = excluded.remind_time, points = excluded.points, due_date = excluded.due_date,
               sort = excluded.sort, active = excluded.active, updated_at = excluded.updated_at,
               deleted_at = excluded.deleted_at, seq = excluded.seq",
        )
        .bind(&it.id)
        .bind(&it.kind)
        .bind(it.name.trim())
        .bind(it.weekdays)
        .bind(&it.remind_time)
        .bind(it.points)
        .bind(&it.due_date)
        .bind(it.sort)
        .bind(it.active)
        .bind(updated)
        .bind(it.deleted_at)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    }

    for c in completions {
        let updated = c.updated_at.min(ceiling);
        let existing: Option<i64> =
            sqlx::query_scalar("SELECT updated_at FROM completions WHERE item_id = ? AND date = ?")
                .bind(&c.item_id)
                .bind(&c.date)
                .fetch_optional(&mut *tx)
                .await?;
        if existing.is_some_and(|e| e >= updated) {
            continue;
        }
        let seq = next_seq(&mut tx).await?;
        sqlx::query(
            "INSERT INTO completions (item_id, date, completed_at, updated_at, deleted_at, seq)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT(item_id, date) DO UPDATE SET
               completed_at = excluded.completed_at, updated_at = excluded.updated_at,
               deleted_at = excluded.deleted_at, seq = excluded.seq",
        )
        .bind(&c.item_id)
        .bind(&c.date)
        .bind(c.completed_at.min(ceiling))
        .bind(updated)
        .bind(c.deleted_at)
        .bind(seq)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await
}

const ITEM_COLS: &str =
    "id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq";
const COMPLETION_COLS: &str = "item_id, date, completed_at, updated_at, deleted_at, seq";

/// Rows changed after `since`, read from one snapshot together with the new cursor.
pub async fn pull(db: &Db, since: i64) -> Result<SyncResponse, sqlx::Error> {
    let mut tx = db.begin().await?;
    let cursor: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key = 'seq'")
        .fetch_one(&mut *tx)
        .await?;
    // A cursor ahead of the database means it was restored from an older copy. Serve
    // everything; the client also notices cursor < its own and starts over.
    let since = if since > cursor { 0 } else { since };
    let epoch: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key = 'epoch'")
        .fetch_optional(&mut *tx)
        .await?
        .unwrap_or(0);
    let items = sqlx::query_as::<_, Item>(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE seq > ? AND seq <= ? ORDER BY seq"
    ))
    .bind(since)
    .bind(cursor)
    .fetch_all(&mut *tx)
    .await?;
    let completions = sqlx::query_as::<_, Completion>(&format!(
        "SELECT {COMPLETION_COLS} FROM completions WHERE seq > ? AND seq <= ? ORDER BY seq"
    ))
    .bind(since)
    .bind(cursor)
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(SyncResponse { epoch: epoch.to_string(), cursor, items, completions, server_time: now_ms() })
}

pub async fn all_items(db: &Db) -> Result<Vec<Item>, sqlx::Error> {
    sqlx::query_as::<_, Item>(&format!("SELECT {ITEM_COLS} FROM items"))
        .fetch_all(db)
        .await
}

pub async fn all_completions(db: &Db) -> Result<Vec<Completion>, sqlx::Error> {
    sqlx::query_as::<_, Completion>(&format!("SELECT {COMPLETION_COLS} FROM completions"))
        .fetch_all(db)
        .await
}

/// Completions the scheduler needs: everything dated `date`, plus every completion of a
/// to-do (a to-do ticked on an earlier day is still done on its due date).
pub async fn reminder_completions(db: &Db, date: &str) -> Result<Vec<Completion>, sqlx::Error> {
    sqlx::query_as::<_, Completion>(&format!(
        "SELECT {COMPLETION_COLS} FROM completions
         WHERE date = ? OR item_id IN (SELECT id FROM items WHERE kind = 'todo')"
    ))
    .bind(date)
    .fetch_all(db)
    .await
}

/// Items the scheduler can remind about, instead of loading every row each tick.
pub async fn reminder_items(db: &Db) -> Result<Vec<Item>, sqlx::Error> {
    sqlx::query_as::<_, Item>(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL"
    ))
    .fetch_all(db)
    .await
}

/// How many rows a cleanup pass removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub reminders: u64,
    pub items: u64,
    pub completions: u64,
    pub pairing_codes: u64,
}

/// Delete data that can never matter again: reminder bookkeeping older than two days,
/// expired pairing codes, and tombstones older than `tombstone_days`. A tombstone only has
/// to survive until every device has synced once after the delete, so the retention is long;
/// a device that stays offline longer than that would keep a row that was deleted elsewhere.
pub async fn prune(db: &Db, now: i64, tombstone_days: i64) -> Result<Pruned, sqlx::Error> {
    let cutoff = now - tombstone_days * DAY_MS;
    let day_before_yesterday = (chrono::DateTime::from_timestamp_millis(now).unwrap_or_default().date_naive()
        - chrono::Duration::days(2))
    .format("%Y-%m-%d")
    .to_string();
    let mut p = Pruned::default();
    // reminders_sent.date is a local date in the user's zone, so keep a margin of days.
    p.reminders = sqlx::query("DELETE FROM reminders_sent WHERE date < ?")
        .bind(day_before_yesterday)
        .execute(db)
        .await?
        .rows_affected();
    p.pairing_codes = sqlx::query("DELETE FROM pairing_codes WHERE expires_at < ?")
        .bind(now)
        .execute(db)
        .await?
        .rows_affected();
    p.completions = sqlx::query("DELETE FROM completions WHERE deleted_at IS NOT NULL AND deleted_at < ?")
        .bind(cutoff)
        .execute(db)
        .await?
        .rows_affected();
    p.items = sqlx::query("DELETE FROM items WHERE deleted_at IS NOT NULL AND deleted_at < ?")
        .bind(cutoff)
        .execute(db)
        .await?
        .rows_affected();
    Ok(p)
}

pub async fn get_setting(db: &Db, key: &str) -> Result<String, sqlx::Error> {
    sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?")
        .bind(key)
        .fetch_optional(db)
        .await
        .map(|v| v.unwrap_or_default())
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(key)
        .bind(value)
        .execute(db)
        .await?;
    Ok(())
}
