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

/// Result of [`UserDb::apply_push`].
#[derive(Debug, PartialEq, Eq)]
pub enum PushOutcome {
    Applied { skipped_completions: usize },
    /// A per-user row ceiling was reached; nothing from the batch was stored.
    LimitReached(String),
}

/// Storage for one user. Every query on a user-owned table (items, completions,
/// reminders_sent) lives here and binds `user_id`, and the id is private, so a handler cannot
/// run such a query without naming whose data it is. Build one per request from the
/// authenticated caller, never from request input.
#[derive(Clone)]
pub struct UserDb {
    db: Db,
    user_id: i64,
}

impl UserDb {
    pub fn new(db: &Db, user_id: i64) -> Self {
        Self { db: db.clone(), user_id }
    }

    pub fn user_id(&self) -> i64 {
        self.user_id
    }

    /// Apply client rows. A row only wins when its `updated_at` is newer than the stored one.
    /// Future timestamps are clamped to a day ahead so a bad phone clock cannot freeze a row.
    /// An id that another user also holds is a different row: keys are (user_id, id).
    ///
    /// `limits` caps how many rows (tombstones included) the user may hold. The counts are read
    /// and enforced inside the write transaction, so concurrent requests cannot both slip under
    /// the ceiling. Hitting a ceiling rolls the whole batch back and reports which one.
    ///
    /// A completion whose item the user does not have (neither stored nor in this batch) is not
    /// stored. It is skipped rather than rejected so a phone with such a row cannot be stuck
    /// retrying a batch that never succeeds; the number skipped is returned.
    pub async fn apply_push(
        &self,
        items: &[Item],
        completions: &[Completion],
        limits: &crate::config::Limits,
    ) -> Result<PushOutcome, sqlx::Error> {
        let ceiling = now_ms() + DAY_MS;
        let mut tx = self.db.begin().await?;
        let mut item_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE user_id = ?")
            .bind(self.user_id)
            .fetch_one(&mut *tx)
            .await?;
        let mut completion_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM completions WHERE user_id = ?")
            .bind(self.user_id)
            .fetch_one(&mut *tx)
            .await?;
        let mut orphans = 0;

        for it in items {
            let updated = it.updated_at.min(ceiling);
            let existing: Option<i64> = sqlx::query_scalar("SELECT updated_at FROM items WHERE user_id = ? AND id = ?")
                .bind(self.user_id)
                .bind(&it.id)
                .fetch_optional(&mut *tx)
                .await?;
            if existing.is_some_and(|e| e >= updated) {
                continue;
            }
            if existing.is_none() {
                if item_count >= limits.max_items {
                    return Ok(PushOutcome::LimitReached(format!(
                        "item limit reached ({} per account); delete items you no longer need",
                        limits.max_items
                    )));
                }
                item_count += 1;
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
            let existing: Option<i64> =
                sqlx::query_scalar("SELECT updated_at FROM completions WHERE user_id = ? AND item_id = ? AND date = ?")
                    .bind(self.user_id)
                    .bind(&c.item_id)
                    .bind(&c.date)
                    .fetch_optional(&mut *tx)
                    .await?;
            if existing.is_some_and(|e| e >= updated) {
                continue;
            }
            let owned: Option<i64> = sqlx::query_scalar("SELECT 1 FROM items WHERE user_id = ? AND id = ?")
                .bind(self.user_id)
                .bind(&c.item_id)
                .fetch_optional(&mut *tx)
                .await?;
            if owned.is_none() {
                orphans += 1;
                continue;
            }
            if existing.is_none() {
                if completion_count >= limits.max_completions {
                    return Ok(PushOutcome::LimitReached(format!(
                        "history limit reached ({} entries per account)",
                        limits.max_completions
                    )));
                }
                completion_count += 1;
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
        Ok(PushOutcome::Applied { skipped_completions: orphans })
    }

    /// This user's rows changed after `since`, read from one snapshot together with the new
    /// cursor. The cursor is the global change counter, so it also advances when other users
    /// write; their rows are never returned, the gaps are harmless.
    pub async fn pull(&self, since: i64) -> Result<SyncResponse, sqlx::Error> {
        let mut tx = self.db.begin().await?;
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
            "SELECT {ITEM_COLS} FROM items WHERE user_id = ? AND seq > ? AND seq <= ? ORDER BY seq"
        ))
        .bind(self.user_id)
        .bind(since)
        .bind(cursor)
        .fetch_all(&mut *tx)
        .await?;
        let completions = sqlx::query_as::<_, Completion>(&format!(
            "SELECT {COMPLETION_COLS} FROM completions WHERE user_id = ? AND seq > ? AND seq <= ? ORDER BY seq"
        ))
        .bind(self.user_id)
        .bind(since)
        .bind(cursor)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(SyncResponse { epoch: epoch.to_string(), cursor, items, completions, server_time: now_ms() })
    }

    pub async fn all_items(&self) -> Result<Vec<Item>, sqlx::Error> {
        sqlx::query_as::<_, Item>(&format!("SELECT {ITEM_COLS} FROM items WHERE user_id = ?"))
            .bind(self.user_id)
            .fetch_all(&self.db)
            .await
    }

    pub async fn all_completions(&self) -> Result<Vec<Completion>, sqlx::Error> {
        sqlx::query_as::<_, Completion>(&format!("SELECT {COMPLETION_COLS} FROM completions WHERE user_id = ?"))
            .bind(self.user_id)
            .fetch_all(&self.db)
            .await
    }

    /// Completions the scheduler needs: everything dated `date`, plus every completion of a
    /// to-do (a to-do ticked on an earlier day is still done on its due date).
    pub async fn reminder_completions(&self, date: &str) -> Result<Vec<Completion>, sqlx::Error> {
        sqlx::query_as::<_, Completion>(&format!(
            "SELECT {COMPLETION_COLS} FROM completions
             WHERE user_id = ? AND (date = ? OR item_id IN (SELECT id FROM items WHERE user_id = ? AND kind = 'todo'))"
        ))
        .bind(self.user_id)
        .bind(date)
        .bind(self.user_id)
        .fetch_all(&self.db)
        .await
    }

    /// Items the scheduler can remind about, instead of loading every row each tick.
    pub async fn reminder_items(&self) -> Result<Vec<Item>, sqlx::Error> {
        sqlx::query_as::<_, Item>(&format!(
            "SELECT {ITEM_COLS} FROM items
             WHERE user_id = ? AND active = 1 AND deleted_at IS NULL AND remind_time IS NOT NULL"
        ))
        .bind(self.user_id)
        .fetch_all(&self.db)
        .await
    }

    /// (item_id, date) of reminders already sent on either of two dates.
    pub async fn sent_reminders(&self, d1: &str, d2: &str) -> Result<std::collections::HashSet<(String, String)>, sqlx::Error> {
        Ok(sqlx::query_as::<_, (String, String)>(
            "SELECT item_id, date FROM reminders_sent WHERE user_id = ? AND date IN (?, ?)",
        )
        .bind(self.user_id)
        .bind(d1)
        .bind(d2)
        .fetch_all(&self.db)
        .await?
        .into_iter()
        .collect())
    }

    /// This user's reminder settings. Every user has a row (created with the user).
    pub async fn settings(&self) -> Result<UserSettings, sqlx::Error> {
        sqlx::query_as::<_, UserSettings>(
            "SELECT timezone, quiet_start, quiet_end, reminders_enabled FROM user_settings WHERE user_id = ?",
        )
        .bind(self.user_id)
        .fetch_one(&self.db)
        .await
    }

    /// Replace this user's settings in one statement.
    pub async fn save_settings(&self, s: &UserSettings) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE user_settings SET timezone = ?, quiet_start = ?, quiet_end = ?, reminders_enabled = ? WHERE user_id = ?",
        )
        .bind(&s.timezone)
        .bind(&s.quiet_start)
        .bind(&s.quiet_end)
        .bind(s.reminders_enabled)
        .bind(self.user_id)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn mark_reminder_sent(&self, item_id: &str, date: &str) -> Result<(), sqlx::Error> {
        sqlx::query("INSERT OR IGNORE INTO reminders_sent (user_id, item_id, date, sent_at) VALUES (?, ?, ?, ?)")
            .bind(self.user_id)
            .bind(item_id)
            .bind(date)
            .bind(now_ms())
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

/// One user's reminder settings. A new user starts with UTC, no quiet hours and reminders on.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct UserSettings {
    pub timezone: String,
    pub quiet_start: String,
    pub quiet_end: String,
    pub reminders_enabled: bool,
}

impl UserSettings {
    /// The user's timezone; an unknown name (it is validated on write) falls back to UTC.
    pub fn tz(&self) -> chrono_tz::Tz {
        std::str::FromStr::from_str(&self.timezone).unwrap_or(chrono_tz::UTC)
    }
}

const ITEM_COLS: &str =
    "id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq";
const COMPLETION_COLS: &str = "item_id, date, completed_at, updated_at, deleted_at, seq";

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
#[allow(clippy::field_reassign_with_default)]
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
