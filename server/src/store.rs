//! Sync storage: last-write-wins upserts with a server-assigned change counter.

use std::collections::{BTreeMap, HashSet};

use chrono::{Datelike, Duration, NaiveDate};
use sqlx::{Connection, SqliteConnection};

use crate::db::Db;
use crate::model::{Completion, Item, SyncResponse};
use crate::xp;

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
    #[error("{0}")]
    LimitReached(String),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

const ITEM_COLS: &str =
    "id, kind, name, weekdays, remind_time, points, due_date, sort, active, updated_at, deleted_at, seq";
const COMPLETION_COLS: &str = "item_id, date, completed_at, updated_at, deleted_at, seq";

fn pull_sql(table: &str, cols: &str) -> String {
    format!("SELECT {cols} FROM {table} WHERE user_id = ? AND seq > ? AND seq <= ? ORDER BY seq")
}

/// The item fields a statistics ledger update can depend on, captured before an upsert.
struct LedgerItem {
    kind: String,
    weekdays: i64,
    points: i64,
    active: bool,
    deleted_at: Option<i64>,
}

impl LedgerItem {
    fn live(&self) -> bool {
        self.deleted_at.is_none()
    }

    fn weekday_bits(&self) -> u8 {
        xp::scheduled_weekday_bits(
            &self.kind,
            self.weekdays,
            self.active,
            self.deleted_at.is_some(),
        )
    }

    fn scheduled_on(&self, day: NaiveDate) -> bool {
        xp::scheduled_on_parts(
            &self.kind,
            self.weekdays,
            self.active,
            self.deleted_at.is_some(),
            day,
        )
    }
}

/// Add `delta` to this user's base XP (points of live completed item-days). Zero is a no-op.
async fn ledger_xp_add(
    conn: &mut SqliteConnection,
    user_id: i64,
    delta: i64,
) -> Result<(), sqlx::Error> {
    if delta == 0 {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO stats_user (user_id, xp_base) VALUES (?, ?)
         ON CONFLICT(user_id) DO UPDATE SET xp_base = xp_base + excluded.xp_base",
    )
    .bind(user_id)
    .bind(delta)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Add `delta` to a date's count of scheduled items that are done. A count that reaches zero
/// removes the row, so `stats_day` holds exactly the dates that can still be perfect.
async fn ledger_day_add(
    conn: &mut SqliteConnection,
    user_id: i64,
    date: &str,
    delta: i64,
) -> Result<(), sqlx::Error> {
    if delta > 0 {
        sqlx::query(
            "INSERT INTO stats_day (user_id, date, done_scheduled) VALUES (?, ?, ?)
             ON CONFLICT(user_id, date) DO UPDATE
               SET done_scheduled = done_scheduled + excluded.done_scheduled",
        )
        .bind(user_id)
        .bind(date)
        .bind(delta)
        .execute(&mut *conn)
        .await?;
    } else if delta < 0 {
        // A row that would reach zero is removed first: `done_scheduled > 0` is also enforced
        // on UPDATE, so decrementing it to zero in place would fail before the cleanup.
        sqlx::query("DELETE FROM stats_day WHERE user_id = ? AND date = ? AND done_scheduled <= ?")
            .bind(user_id)
            .bind(date)
            .bind(-delta)
            .execute(&mut *conn)
            .await?;
        sqlx::query(
            "UPDATE stats_day SET done_scheduled = done_scheduled + ?
             WHERE user_id = ? AND date = ?",
        )
        .bind(delta)
        .bind(user_id)
        .bind(date)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Adjust the per-weekday count of live scheduled items after an item's membership mask changes.
async fn ledger_schedule_delta(
    conn: &mut SqliteConnection,
    user_id: i64,
    old_bits: u8,
    new_bits: u8,
) -> Result<(), sqlx::Error> {
    for weekday in 0..7u8 {
        let was = i64::from(old_bits & (1 << weekday) != 0);
        let now = i64::from(new_bits & (1 << weekday) != 0);
        let delta = now - was;
        if delta > 0 {
            sqlx::query(
                "INSERT INTO stats_schedule (user_id, weekday, scheduled) VALUES (?, ?, ?)
                 ON CONFLICT(user_id, weekday) DO UPDATE
                   SET scheduled = scheduled + excluded.scheduled",
            )
            .bind(user_id)
            .bind(weekday)
            .bind(delta)
            .execute(&mut *conn)
            .await?;
        } else if delta < 0 {
            sqlx::query(
                "UPDATE stats_schedule SET scheduled = scheduled + ?
                 WHERE user_id = ? AND weekday = ?",
            )
            .bind(delta)
            .bind(user_id)
            .bind(weekday)
            .execute(&mut *conn)
            .await?;
            sqlx::query(
                "DELETE FROM stats_schedule WHERE user_id = ? AND weekday = ? AND scheduled <= 0",
            )
            .bind(user_id)
            .bind(weekday)
            .execute(&mut *conn)
            .await?;
        }
    }
    Ok(())
}

/// Base XP, day and schedule queries. Kept as constants so `/stats` and its query-plan test
/// execute exactly the same statements.
const STATS_XP_SQL: &str = "SELECT COALESCE((SELECT xp_base FROM stats_user WHERE user_id = ?), 0)";
const STATS_DAYS_SQL: &str =
    "SELECT date, done_scheduled FROM stats_day WHERE user_id = ? ORDER BY date";
const STATS_SCHEDULE_SQL: &str = "SELECT weekday, scheduled FROM stats_schedule WHERE user_id = ?";
const STATS_HABITS_SQL: &str =
    "SELECT id FROM items WHERE user_id = ? AND deleted_at IS NULL AND kind = 'habit' ORDER BY id";
/// Count consecutive completed days ending at the anchor date. The seed row only exists when
/// the anchor is done, so an empty walk returns zero.
const STATS_STREAK_SQL: &str = "WITH RECURSIVE streak(d, n) AS (
        SELECT ?, 1 WHERE EXISTS (
            SELECT 1 FROM stats_item_date WHERE user_id = ? AND item_id = ? AND date = ?
        )
        UNION ALL
        SELECT date(streak.d, '-1 day'), streak.n + 1 FROM streak
        WHERE EXISTS (
            SELECT 1 FROM stats_item_date
            WHERE user_id = ? AND item_id = ? AND date = date(streak.d, '-1 day')
        )
    ) SELECT COALESCE(MAX(n), 0) FROM streak";

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
        self.apply_push_limited(items, completions, &crate::config::Limits::default())
            .await
    }

    pub async fn apply_push_limited(
        &self,
        items: &[Item],
        completions: &[Completion],
        limits: &crate::config::Limits,
    ) -> Result<(), PushError> {
        // BEGIN IMMEDIATE serializes admission across processes, so normal contention
        // waits instead of upgrading a read snapshot. If SQLite still reports busy after
        // its busy timeout, reuse the shared one-retry policy rather than surfacing an
        // internal error. A retry starts from scratch: neither the counts nor any upsert
        // from the failed attempt can leak into it.
        crate::db::retry_busy(
            |err| matches!(err, PushError::Db(e) if crate::db::is_busy_database_error(e)),
            || self.try_apply_push(items, completions, limits),
        )
        .await
    }

    async fn try_apply_push(
        &self,
        items: &[Item],
        completions: &[Completion],
        limits: &crate::config::Limits,
    ) -> Result<(), PushError> {
        let ceiling = now_ms() + DAY_MS;
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let mut item_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE user_id = ?")
                .bind(self.user_id)
                .fetch_one(&mut *tx)
                .await?;
        let mut completion_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM completions WHERE user_id = ?")
                .bind(self.user_id)
                .fetch_one(&mut *tx)
                .await?;

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
            let existing: Option<(i64, String, i64, i64, bool, Option<i64>)> = sqlx::query_as(
                "SELECT updated_at, kind, weekdays, points, active, deleted_at
                 FROM items WHERE user_id = ? AND id = ?",
            )
            .bind(self.user_id)
            .bind(&it.id)
            .fetch_optional(&mut *tx)
            .await?;
            if existing.as_ref().is_some_and(|e| e.0 >= updated) {
                continue;
            }
            if existing.is_none() {
                if item_count >= limits.max_items {
                    return Err(PushError::LimitReached(format!(
                        "item limit reached ({} per account)",
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
            // TRACK-72: the item's ledger delta uses only fields that can change its
            // contribution. Points and liveness change base XP; kind, active, weekdays and
            // liveness change which days it is scheduled on.
            let previous = existing.map(|e| LedgerItem {
                kind: e.1,
                weekdays: e.2,
                points: e.3,
                active: e.4,
                deleted_at: e.5,
            });
            let current = LedgerItem {
                kind: it.kind.clone(),
                weekdays: it.weekdays,
                points: it.points,
                active: it.active,
                deleted_at: it.deleted_at,
            };
            let done_days: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM stats_item_date WHERE user_id = ? AND item_id = ?",
            )
            .bind(self.user_id)
            .bind(&it.id)
            .fetch_one(&mut *tx)
            .await?;
            let old_xp = previous
                .as_ref()
                .filter(|p| p.live())
                .map(|p| p.points * done_days)
                .unwrap_or(0);
            let new_xp = if current.live() {
                current.points * done_days
            } else {
                0
            };
            ledger_xp_add(&mut tx, self.user_id, new_xp - old_xp).await?;

            let old_bits = previous.as_ref().map(LedgerItem::weekday_bits).unwrap_or(0);
            let new_bits = current.weekday_bits();
            if old_bits != new_bits {
                let dates: Vec<String> = sqlx::query_scalar(
                    "SELECT date FROM stats_item_date WHERE user_id = ? AND item_id = ?",
                )
                .bind(self.user_id)
                .bind(&it.id)
                .fetch_all(&mut *tx)
                .await?;
                for date in dates {
                    let Some(day) = crate::xp::parse(&date) else {
                        continue;
                    };
                    let bit = 1u8 << day.weekday().num_days_from_monday();
                    let was = old_bits & bit != 0;
                    let now = new_bits & bit != 0;
                    if was != now {
                        ledger_day_add(&mut tx, self.user_id, &date, if now { 1 } else { -1 })
                            .await?;
                    }
                }
                ledger_schedule_delta(&mut tx, self.user_id, old_bits, new_bits).await?;
            }
        }

        for c in completions {
            let updated = c.updated_at.min(ceiling);
            let existing: Option<(i64, Option<i64>)> = sqlx::query_as(
                "SELECT updated_at, deleted_at FROM completions
                 WHERE user_id = ? AND item_id = ? AND date = ?",
            )
            .bind(self.user_id)
            .bind(&c.item_id)
            .bind(&c.date)
            .fetch_optional(&mut *tx)
            .await?;
            if existing.as_ref().is_some_and(|e| e.0 >= updated) {
                continue;
            }
            let item: Option<(String, i64, i64, bool, Option<i64>)> = sqlx::query_as(
                "SELECT kind, weekdays, points, active, deleted_at
                 FROM items WHERE user_id = ? AND id = ?",
            )
            .bind(self.user_id)
            .bind(&c.item_id)
            .fetch_optional(&mut *tx)
            .await?;
            let Some((kind, weekdays, points, active, item_deleted)) = item else {
                return Err(PushError::UnknownItem);
            };
            if existing.is_none() {
                if completion_count >= limits.max_completions {
                    return Err(PushError::LimitReached(format!(
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
            // TRACK-72: only a live/tombstone transition changes statistics. A date is counted
            // once however many stored rows encode it, so a second encoding does not add XP.
            let old_live = existing.as_ref().is_some_and(|e| e.1.is_none());
            let new_live = c.deleted_at.is_none();
            if old_live != new_live {
                let Some(day) = crate::xp::parse(&c.date) else {
                    // The shared rules ignore a date they cannot parse; the ledger does too.
                    continue;
                };
                let canonical = day.format("%Y-%m-%d").to_string();
                let item = LedgerItem {
                    kind,
                    weekdays,
                    points,
                    active,
                    deleted_at: item_deleted,
                };
                let live_rows: Option<i64> = sqlx::query_scalar(
                    "SELECT live_rows FROM stats_item_date
                     WHERE user_id = ? AND item_id = ? AND date = ?",
                )
                .bind(self.user_id)
                .bind(&c.item_id)
                .bind(&canonical)
                .fetch_optional(&mut *tx)
                .await?;
                if new_live {
                    if live_rows.is_none() {
                        sqlx::query(
                            "INSERT INTO stats_item_date (user_id, item_id, date, live_rows)
                             VALUES (?, ?, ?, 1)",
                        )
                        .bind(self.user_id)
                        .bind(&c.item_id)
                        .bind(&canonical)
                        .execute(&mut *tx)
                        .await?;
                        if item.scheduled_on(day) {
                            ledger_day_add(&mut tx, self.user_id, &canonical, 1).await?;
                        }
                        if item.live() {
                            ledger_xp_add(&mut tx, self.user_id, item.points).await?;
                        }
                    } else {
                        sqlx::query(
                            "UPDATE stats_item_date SET live_rows = live_rows + 1
                             WHERE user_id = ? AND item_id = ? AND date = ?",
                        )
                        .bind(self.user_id)
                        .bind(&c.item_id)
                        .bind(&canonical)
                        .execute(&mut *tx)
                        .await?;
                    }
                } else if let Some(rows) = live_rows {
                    if rows > 1 {
                        sqlx::query(
                            "UPDATE stats_item_date SET live_rows = live_rows - 1
                             WHERE user_id = ? AND item_id = ? AND date = ?",
                        )
                        .bind(self.user_id)
                        .bind(&c.item_id)
                        .bind(&canonical)
                        .execute(&mut *tx)
                        .await?;
                    } else {
                        sqlx::query(
                            "DELETE FROM stats_item_date
                             WHERE user_id = ? AND item_id = ? AND date = ?",
                        )
                        .bind(self.user_id)
                        .bind(&c.item_id)
                        .bind(&canonical)
                        .execute(&mut *tx)
                        .await?;
                        if item.scheduled_on(day) {
                            ledger_day_add(&mut tx, self.user_id, &canonical, -1).await?;
                        }
                        if item.live() {
                            ledger_xp_add(&mut tx, self.user_id, -item.points).await?;
                        }
                    }
                }
            }
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

    /// Exact statistics from the incremental ledger, without loading retained completions.
    ///
    /// All reads run in one deferred transaction so base XP, perfect days, schedule counts and
    /// streaks come from the same committed snapshot even while a sync writes. Work is
    /// proportional to live perfect-day candidates, live habits and the length of each current
    /// streak, not to the number of retained completions.
    pub async fn stats(&self, today: NaiveDate) -> Result<xp::Stats, sqlx::Error> {
        let mut conn = self.db.acquire().await?;
        self.stats_on(&mut conn, today).await
    }

    /// [`UserDb::stats`] on a caller-held connection, so a measurement test can count the work
    /// of exactly the production statements.
    pub async fn stats_on(
        &self,
        conn: &mut SqliteConnection,
        today: NaiveDate,
    ) -> Result<xp::Stats, sqlx::Error> {
        let mut tx = conn.begin_with("BEGIN DEFERRED").await?;
        let xp_base: i64 = sqlx::query_scalar(STATS_XP_SQL)
            .bind(self.user_id)
            .fetch_one(&mut *tx)
            .await?;
        let days: Vec<(String, i64)> = sqlx::query_as(STATS_DAYS_SQL)
            .bind(self.user_id)
            .fetch_all(&mut *tx)
            .await?;
        let entries: Vec<(i64, i64)> = sqlx::query_as(STATS_SCHEDULE_SQL)
            .bind(self.user_id)
            .fetch_all(&mut *tx)
            .await?;
        let mut scheduled = [0i64; 7];
        for (weekday, count) in entries {
            if let Some(slot) = scheduled.get_mut(weekday as usize) {
                *slot = count;
            }
        }
        let habits: Vec<String> = sqlx::query_scalar(STATS_HABITS_SQL)
            .bind(self.user_id)
            .fetch_all(&mut *tx)
            .await?;

        let mut perfect_days = Vec::new();
        for (date, done) in days {
            let Some(day) = xp::parse(&date) else {
                continue;
            };
            if done > 0 && done == scheduled[day.weekday().num_days_from_monday() as usize] {
                perfect_days.push(date);
            }
        }

        let mut streaks = BTreeMap::new();
        for habit in habits {
            let streak = self.streak_on(&mut tx, &habit, today).await?;
            streaks.insert(habit, streak);
        }
        tx.commit().await?;
        Ok(xp::Stats::from_parts(
            xp_base + xp::PERFECT_DAY_BONUS * perfect_days.len() as i64,
            perfect_days,
            streaks,
        ))
    }

    /// Consecutive completed days for one habit, ending today or yesterday when today is not
    /// done. Walks the ledger's canonical dates, so the cost is the streak length.
    async fn streak_on(
        &self,
        conn: &mut SqliteConnection,
        item_id: &str,
        today: NaiveDate,
    ) -> Result<i64, sqlx::Error> {
        let today_s = today.format("%Y-%m-%d").to_string();
        let done_today: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM stats_item_date
             WHERE user_id = ? AND item_id = ? AND date = ?)",
        )
        .bind(self.user_id)
        .bind(item_id)
        .bind(&today_s)
        .fetch_one(&mut *conn)
        .await?;
        let anchor = if done_today {
            today
        } else {
            today - Duration::days(1)
        };
        let anchor_s = anchor.format("%Y-%m-%d").to_string();
        sqlx::query_scalar(STATS_STREAK_SQL)
            .bind(&anchor_s)
            .bind(self.user_id)
            .bind(item_id)
            .bind(&anchor_s)
            .bind(self.user_id)
            .bind(item_id)
            .fetch_one(conn)
            .await
    }

    /// Plans of every statement `/stats` runs, for the bounded-work regression test.
    pub async fn stats_query_plans(&self) -> Result<Vec<String>, sqlx::Error> {
        let mut plans = Vec::new();
        for sql in [
            STATS_XP_SQL,
            STATS_DAYS_SQL,
            STATS_SCHEDULE_SQL,
            STATS_HABITS_SQL,
        ] {
            let rows: Vec<(i64, i64, i64, String)> =
                sqlx::query_as(&format!("EXPLAIN QUERY PLAN {sql}"))
                    .bind(self.user_id)
                    .fetch_all(&self.db)
                    .await?;
            plans.extend(rows.into_iter().map(|row| row.3));
        }
        let rows: Vec<(i64, i64, i64, String)> =
            sqlx::query_as(&format!("EXPLAIN QUERY PLAN {STATS_STREAK_SQL}"))
                .bind("2000-01-01")
                .bind(self.user_id)
                .bind("habit")
                .bind("2000-01-01")
                .bind(self.user_id)
                .bind("habit")
                .fetch_all(&self.db)
                .await?;
        plans.extend(rows.into_iter().map(|row| row.3));
        Ok(plans)
    }

    /// Rebuild this user's ledger from retained history, replacing it atomically.
    ///
    /// Used once per account by the upgrade backfill and available as the repair path if an
    /// aggregate is ever suspected to be wrong. It reads history and writes only the ledger;
    /// nothing user-visible is changed or deleted. Marks the account built in the same
    /// transaction, so an interruption leaves it pending for the next attempt.
    pub async fn rebuild_stats(&self) -> Result<(), sqlx::Error> {
        let mut tx = self.db.begin_with("BEGIN IMMEDIATE").await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = ?)")
            .bind(self.user_id)
            .fetch_one(&mut *tx)
            .await?;
        if !exists {
            tx.rollback().await?;
            return Ok(());
        }
        let items: Vec<Item> =
            sqlx::query_as::<_, Item>(&format!("SELECT {ITEM_COLS} FROM items WHERE user_id = ?"))
                .bind(self.user_id)
                .fetch_all(&mut *tx)
                .await?;
        let completions: Vec<Completion> = sqlx::query_as::<_, Completion>(&format!(
            "SELECT {COMPLETION_COLS} FROM completions WHERE user_id = ?"
        ))
        .bind(self.user_id)
        .fetch_all(&mut *tx)
        .await?;
        let snapshot = crate::stats::snapshot(&items, &completions);

        for table in [
            "stats_item_date",
            "stats_day",
            "stats_schedule",
            "stats_user",
        ] {
            sqlx::query(&format!("DELETE FROM {table} WHERE user_id = ?"))
                .bind(self.user_id)
                .execute(&mut *tx)
                .await?;
        }
        for ((item_id, date), live_rows) in &snapshot.item_date_live_rows {
            sqlx::query(
                "INSERT INTO stats_item_date (user_id, item_id, date, live_rows)
                 VALUES (?, ?, ?, ?)",
            )
            .bind(self.user_id)
            .bind(item_id)
            .bind(date)
            .bind(live_rows)
            .execute(&mut *tx)
            .await?;
        }
        for (date, done) in &snapshot.day_done_scheduled {
            sqlx::query("INSERT INTO stats_day (user_id, date, done_scheduled) VALUES (?, ?, ?)")
                .bind(self.user_id)
                .bind(date)
                .bind(done)
                .execute(&mut *tx)
                .await?;
        }
        for (weekday, count) in snapshot.schedule_weekdays.iter().enumerate() {
            sqlx::query(
                "INSERT INTO stats_schedule (user_id, weekday, scheduled) VALUES (?, ?, ?)",
            )
            .bind(self.user_id)
            .bind(weekday as i64)
            .bind(count)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("INSERT INTO stats_user (user_id, xp_base) VALUES (?, ?)")
            .bind(self.user_id)
            .bind(snapshot.xp_base)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO stats_state (user_id, built) VALUES (?, 1)
             ON CONFLICT(user_id) DO UPDATE SET built = 1",
        )
        .bind(self.user_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await
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
