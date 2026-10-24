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
pub async fn apply_push(
    db: &Db,
    items: &[Item],
    completions: &[Completion],
) -> Result<(), sqlx::Error> {
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
    Ok(SyncResponse {
        cursor,
        items,
        completions,
        server_time: now_ms(),
    })
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

/// Items the scheduler can remind about, instead of loading every row each tick.
pub async fn reminder_items(db: &Db) -> Result<Vec<Item>, sqlx::Error> {
    sqlx::query_as::<_, Item>(&format!(
        "SELECT {ITEM_COLS} FROM items WHERE active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL"
    ))
    .fetch_all(db)
    .await
}

/// Representative query plan for scheduler item selection. Kept as a testable statement
/// because the partial index must match the scheduler predicate exactly.
pub async fn reminder_items_query_plan(
    db: &Db,
) -> Result<Vec<(i64, i64, i64, String)>, sqlx::Error> {
    sqlx::query_as(
        "EXPLAIN QUERY PLAN SELECT id FROM items WHERE active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL",
    )
    .fetch_all(db)
    .await
}

pub async fn reminder_completions(
    db: &Db,
    date: &str,
    yesterday: &str,
) -> Result<Vec<Completion>, sqlx::Error> {
    sqlx::query_as::<_, Completion>(&format!(
        "SELECT {COMPLETION_COLS} FROM completions WHERE date IN (?, ?)
         UNION
         SELECT {COMPLETION_COLS} FROM completions
         WHERE item_id IN (SELECT id FROM items WHERE kind = 'todo')"
    ))
    .bind(date)
    .bind(yesterday)
    .fetch_all(db)
    .await
}

pub async fn item_ids(
    db: &Db,
    ids: &[String],
) -> Result<std::collections::HashSet<String>, sqlx::Error> {
    if ids.is_empty() {
        return Ok(Default::default());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!("SELECT id FROM items WHERE id IN ({placeholders})");
    let mut q = sqlx::query_scalar::<_, String>(&sql);
    for id in ids {
        q = q.bind(id);
    }
    Ok(q.fetch_all(db).await?.into_iter().collect())
}

/// How many rows a cleanup pass removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Pruned {
    pub reminders: u64,
    pub items: u64,
    pub completions: u64,
    pub pairing_codes: u64,
}

/// Delete expired pairing and reminder bookkeeping. Tombstones stay until the protocol offers
/// device cursor acknowledgements or a forced-full-resync contract.
pub async fn prune(db: &Db, now: i64) -> Result<Pruned, sqlx::Error> {
    let day_before_yesterday = (chrono::DateTime::from_timestamp_millis(now)
        .unwrap_or_default()
        .date_naive()
        - chrono::Duration::days(2))
    .format("%Y-%m-%d")
    .to_string();
    // reminders_sent.date is a local date in the user's zone, so keep a margin of days.
    let reminders = sqlx::query("DELETE FROM reminders_sent WHERE date < ?")
        .bind(day_before_yesterday)
        .execute(db)
        .await?
        .rows_affected();
    let pairing_codes = sqlx::query("DELETE FROM pairing_codes WHERE expires_at < ?")
        .bind(now)
        .execute(db)
        .await?
        .rows_affected();
    Ok(Pruned {
        reminders,
        pairing_codes,
        ..Pruned::default()
    })
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
