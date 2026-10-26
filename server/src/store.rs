//! Sync storage: last-write-wins upserts with a server-assigned change counter.

use std::collections::HashSet;

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

/// Why a push was refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum PushError {
    /// A completion names an item the caller neither has (live or tombstoned) nor sends in the
    /// same batch. Nothing was written.
    #[error("completion references an unknown item")]
    UnknownItem,
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

const ITEM_COLS: &str =
    "id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq";
const COMPLETION_COLS: &str = "item_id, date, completed_at, updated_at, deleted_at, seq";

fn pull_sql(table: &str, cols: &str) -> String {
    format!("SELECT {cols} FROM {table} WHERE user_id = ? AND seq > ? AND seq <= ? ORDER BY seq")
}

/// Storage for one user. Every query on a user-owned table (items, completions,
/// reminders_sent) lives here and binds `user_id`, and the id is private, so a handler cannot
/// run such a query without naming whose data it is. Build one per request from the
/// authenticated caller (`auth::UserId`), never from request input.
#[derive(Clone)]
pub struct UserDb {
    db: Db,
    user_id: i64,
}

impl UserDb {
    pub fn new(db: &Db, user_id: i64) -> Self {
        Self {
            db: db.clone(),
            user_id,
        }
    }

    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// Apply client rows. A row only wins when its `updated_at` is newer than the stored one.
    /// Future timestamps are clamped to a day ahead so a bad phone clock cannot freeze a row.
    /// An id that another user also holds is a different row: keys are (user_id, id).
    ///
    /// Completion references are checked against this user's items inside the same write
    /// transaction, before any row is written: a completion may name an item the user has
    /// (live or tombstoned, so an old offline client whose item was deleted on another device
    /// still syncs) or one sent in the same batch. Anything else, including an item that only
    /// another user holds, is refused with [`PushError::UnknownItem`] and nothing is written.
    pub async fn apply_push(
        &self,
        items: &[Item],
        completions: &[Completion],
    ) -> Result<(), PushError> {
        let ceiling = now_ms() + DAY_MS;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;

        let incoming: HashSet<&str> = items.iter().map(|i| i.id.as_str()).collect();
        let mut unknown: Vec<&str> = completions
            .iter()
            .map(|c| c.item_id.as_str())
            .filter(|id| !incoming.contains(id))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if !unknown.is_empty() {
            unknown.sort_unstable();
            let placeholders = vec!["?"; unknown.len()].join(",");
            let sql = format!("SELECT id FROM items WHERE user_id = ? AND id IN ({placeholders})");
            let mut q = sqlx::query_scalar::<_, String>(&sql).bind(self.user_id);
            for id in &unknown {
                q = q.bind(*id);
            }
            let known: HashSet<String> = q.fetch_all(&mut *tx).await?.into_iter().collect();
            if unknown.iter().any(|id| !known.contains(*id)) {
                return Err(PushError::UnknownItem);
            }
        }

        for it in items {
            let updated = it.updated_at.min(ceiling);
            let existing: Option<i64> =
                sqlx::query_scalar("SELECT updated_at FROM items WHERE user_id = ? AND id = ?")
                    .bind(self.user_id)
                    .bind(&it.id)
                    .fetch_optional(&mut *tx)
                    .await?;
            if existing.is_some_and(|e| e >= updated) {
                continue;
            }
            let seq = next_seq(&mut tx).await?;
            sqlx::query(
                "INSERT INTO items (user_id, id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(user_id, id) DO UPDATE SET
                   kind = excluded.kind, name = excluded.name, weekdays = excluded.weekdays,
                   remind_time = excluded.remind_time, points = excluded.points, due_date = excluded.due_date,
                   sort = excluded.sort, active = excluded.active, updated_at = excluded.updated_at,
                   deleted_at = excluded.deleted_at, seq = excluded.seq",
            )
            .bind(self.user_id)
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
            let existing: Option<i64> = sqlx::query_scalar(
                "SELECT updated_at FROM completions WHERE user_id = ? AND item_id = ? AND date = ?",
            )
            .bind(self.user_id)
            .bind(&c.item_id)
            .bind(&c.date)
            .fetch_optional(&mut *tx)
            .await?;
            if existing.is_some_and(|e| e >= updated) {
                continue;
            }
            let seq = next_seq(&mut tx).await?;
            sqlx::query(
                "INSERT INTO completions (user_id, item_id, date, completed_at, updated_at, deleted_at, seq)
                 VALUES (?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(user_id, item_id, date) DO UPDATE SET
                   completed_at = excluded.completed_at, updated_at = excluded.updated_at,
                   deleted_at = excluded.deleted_at, seq = excluded.seq",
            )
            .bind(self.user_id)
            .bind(&c.item_id)
            .bind(&c.date)
            .bind(c.completed_at.min(ceiling))
            .bind(updated)
            .bind(c.deleted_at)
            .bind(seq)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// This user's rows changed after `since`, read from one snapshot together with the new
    /// cursor. The cursor is the global change counter, so it also advances when other users
    /// write; their rows are never returned and the gaps are harmless. Both reads search the
    /// `(user_id, seq)` index; see [`UserDb::pull_query_plans`].
    pub async fn pull(&self, since: i64) -> Result<SyncResponse, sqlx::Error> {
        let mut tx = self.db.begin().await?;
        let cursor: i64 = sqlx::query_scalar("SELECT val FROM meta WHERE key = 'seq'")
            .fetch_one(&mut *tx)
            .await?;
        let items = sqlx::query_as::<_, Item>(&pull_sql("items", ITEM_COLS))
            .bind(self.user_id)
            .bind(since)
            .bind(cursor)
            .fetch_all(&mut *tx)
            .await?;
        let completions =
            sqlx::query_as::<_, Completion>(&pull_sql("completions", COMPLETION_COLS))
                .bind(self.user_id)
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

    /// Query plans of the two statements `pull` runs, so a test can prove they use the per-user
    /// sequence indexes instead of scanning and sorting every user's rows.
    pub async fn pull_query_plans(&self) -> Result<Vec<String>, sqlx::Error> {
        let mut plans = Vec::new();
        for (table, cols) in [("items", ITEM_COLS), ("completions", COMPLETION_COLS)] {
            let rows: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(&format!("EXPLAIN QUERY PLAN {}", pull_sql(table, cols)))
                    .bind(self.user_id)
                    .bind(0_i64)
                    .bind(i64::MAX)
                    .fetch_all(&self.db)
                    .await?;
            plans.extend(rows.into_iter().map(|r| r.3));
        }
        Ok(plans)
    }

    pub async fn all_items(&self) -> Result<Vec<Item>, sqlx::Error> {
        sqlx::query_as::<_, Item>(&format!("SELECT {ITEM_COLS} FROM items WHERE user_id = ?"))
            .bind(self.user_id)
            .fetch_all(&self.db)
            .await
    }

    pub async fn all_completions(&self) -> Result<Vec<Completion>, sqlx::Error> {
        sqlx::query_as::<_, Completion>(&format!(
            "SELECT {COMPLETION_COLS} FROM completions WHERE user_id = ?"
        ))
        .bind(self.user_id)
        .fetch_all(&self.db)
        .await
    }

    /// Items the scheduler can remind about, instead of loading every row each tick.
    ///
    /// `INDEXED BY` keeps the partial index from migration 0003 (restored by 0005 after the
    /// users rebuild) in use. Without it the extra `user_id` filter makes SQLite prefer
    /// `items_user_seq` and walk every row of the user on each tick. The index holds only
    /// reminder candidates, and `user_id` is applied to those rows.
    pub async fn reminder_items(&self) -> Result<Vec<Item>, sqlx::Error> {
        sqlx::query_as::<_, Item>(&scheduler_items_sql())
            .bind(self.user_id)
            .fetch_all(&self.db)
            .await
    }

    /// Representative query plan for the scheduler's item selection. Kept as a testable
    /// statement because the partial index must keep serving the scheduler predicate.
    pub async fn reminder_items_query_plan(
        &self,
    ) -> Result<Vec<(i64, i64, i64, String)>, sqlx::Error> {
        sqlx::query_as(
            "EXPLAIN QUERY PLAN SELECT id FROM items INDEXED BY items_user_reminder_candidates
             WHERE user_id = ? AND active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL",
        )
        .bind(self.user_id)
        .fetch_all(&self.db)
        .await
    }

    /// Completions the scheduler needs: everything dated `date` or `yesterday`, plus every
    /// completion of a to-do (a to-do ticked on an earlier day is still done on its due date).
    pub async fn reminder_completions(
        &self,
        date: &str,
        yesterday: &str,
    ) -> Result<Vec<Completion>, sqlx::Error> {
        sqlx::query_as::<_, Completion>(&scheduler_completions_sql())
            .bind(self.user_id)
            .bind(date)
            .bind(yesterday)
            .bind(self.user_id)
            .bind(self.user_id)
            .fetch_all(&self.db)
            .await
    }

    /// (item_id, date) of reminders already sent on either of two dates.
    pub async fn sent_reminders(
        &self,
        d1: &str,
        d2: &str,
    ) -> Result<HashSet<(String, String)>, sqlx::Error> {
        Ok(sqlx::query_as::<_, (String, String)>(SCHEDULER_SENT_SQL)
            .bind(self.user_id)
            .bind(d1)
            .bind(d2)
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .collect())
    }

    pub async fn settings(&self) -> Result<UserSettings, sqlx::Error> {
        sqlx::query_as("SELECT timezone, quiet_start, quiet_end, reminders_enabled FROM user_settings WHERE user_id = ?")
            .bind(self.user_id).fetch_one(&self.db).await
    }

    pub async fn mark_reminder_sent(&self, item_id: &str, date: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO reminders_sent (user_id, item_id, date, sent_at) VALUES (?, ?, ?, ?)",
        )
        .bind(self.user_id)
        .bind(item_id)
        .bind(date)
        .bind(now_ms())
        .execute(&self.db)
        .await?;
        Ok(())
    }
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

/// Per-user reminder preferences. Unknown stored timezone falls back to neutral UTC.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UserSettings {
    pub timezone: String,
    pub quiet_start: String,
    pub quiet_end: String,
    pub reminders_enabled: bool,
}
impl UserSettings {
    pub fn tz(&self) -> chrono_tz::Tz {
        std::str::FromStr::from_str(&self.timezone).unwrap_or(chrono_tz::UTC)
    }
}

/// Actual scheduler SQL, exposed for populated EXPLAIN/VM-step regression tests.
pub fn scheduler_items_sql() -> String {
    format!("SELECT {ITEM_COLS} FROM items INDEXED BY items_user_reminder_candidates WHERE user_id = ? AND active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL")
}
pub fn scheduler_completions_sql() -> String {
    format!("SELECT {COMPLETION_COLS} FROM completions WHERE user_id = ? AND date IN (?, ?) UNION SELECT {COMPLETION_COLS} FROM completions WHERE user_id = ? AND item_id IN (SELECT id FROM items WHERE user_id = ? AND kind = 'todo')")
}
pub const SCHEDULER_SENT_SQL: &str =
    "SELECT item_id, date FROM reminders_sent WHERE user_id = ? AND date IN (?, ?)";
