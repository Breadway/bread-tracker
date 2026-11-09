//! Incremental statistics derived from retained history.
//!
//! The product rules in [`crate::xp`] are defined over all retained completions. `/stats` used
//! to load that whole history on every request, so this module keeps the aggregates the rules
//! need, without changing what they are:
//!
//! - [`Snapshot`] is computed once from history (the one-time upgrade rebuild and the explicit
//!   repair path). It is the only place that walks completions to build statistics.
//! - Sync maintains the same aggregates row by row in the same transaction as the accepted
//!   write, so a reader never sees a half-applied change.
//! - `/stats` reads only `stats_user`, `stats_day`, `stats_schedule`,
//!   `stats_schedule_created`, `stats_item_date` (for streak walks) and the live habit list.
//!   It never scans `completions`, and its schedule work is proportional to distinct live
//!   creation dates rather than retained history.
//!
//! `stats_schedule_created` records, per weekday and creation date, how many live scheduled
//! items carry that bound. An item counts toward a date only from the day after it was created
//! (TRACK-65), so the read subtracts the rows whose creation date is on or after the date from
//! the per-weekday total. Legacy NULL rows never appear there and count on every date.
//!
//! Dates are canonicalised through [`crate::xp::parse`], the same parser the shared rules use,
//! so two stored encodings of one calendar date count once and an unparseable legacy date counts
//! never, exactly as the reference `xp::compute` treats them. An item's weekday membership comes
//! from [`crate::xp::scheduled_weekday_bits`], so the ledger and the reference cannot diverge.

use std::collections::{BTreeMap, HashMap};

use crate::db::Db;
use crate::model::{Completion, Item};
use crate::store::UserDb;
use crate::xp;

/// Aggregate state derived from one user's retained items and completions.
///
/// `item_date_live_rows` maps `(item_id, canonical date)` to the number of live stored rows for
/// that date. A positive count is one completed day for that item.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub item_date_live_rows: BTreeMap<(String, String), i64>,
    pub xp_base: i64,
    pub day_done_scheduled: BTreeMap<String, i64>,
    pub schedule_weekdays: [i64; 7],
    /// Live scheduled items with a creation date, counted per `(weekday, created_on)`. The
    /// read subtracts these from the per-weekday totals for dates on or before the creation
    /// date, which is how an item counts only from the day after it was created.
    pub schedule_created: BTreeMap<(i64, String), i64>,
}

/// Build the ledgers from history. Cost is one pass over the user's items and completions; the
/// result is written by [`UserDb::rebuild_stats`] or the upgrade backfill, never per request.
pub fn snapshot(items: &[Item], completions: &[Completion]) -> Snapshot {
    let live: HashMap<&str, &Item> = items
        .iter()
        .filter(|i| i.deleted_at.is_none())
        .map(|i| (i.id.as_str(), i))
        .collect();

    let mut item_date_live_rows: BTreeMap<(String, String), i64> = BTreeMap::new();
    for c in completions.iter().filter(|c| c.deleted_at.is_none()) {
        let Some(date) = xp::parse(&c.date) else {
            // The rules skip a date they cannot parse; so does the ledger.
            continue;
        };
        let canonical = date.format("%Y-%m-%d").to_string();
        *item_date_live_rows
            .entry((c.item_id.clone(), canonical))
            .or_insert(0) += 1;
    }

    let mut item_days: HashMap<&str, i64> = HashMap::new();
    for (item_id, _) in item_date_live_rows.keys() {
        *item_days.entry(item_id.as_str()).or_insert(0) += 1;
    }

    let mut xp_base = 0i64;
    let mut schedule_weekdays = [0i64; 7];
    let mut schedule_created: BTreeMap<(i64, String), i64> = BTreeMap::new();
    for item in live.values() {
        xp_base += item.points * item_days.get(item.id.as_str()).copied().unwrap_or(0);
        let bits = xp::scheduled_weekday_bits(
            &item.kind,
            item.weekdays,
            item.active,
            item.deleted_at.is_some(),
        );
        let created = item
            .created_on
            .as_deref()
            .and_then(xp::parse)
            .map(|day| day.format("%Y-%m-%d").to_string());
        for (weekday, count) in schedule_weekdays.iter_mut().enumerate() {
            if bits & (1 << weekday) != 0 {
                *count += 1;
                if let Some(created) = &created {
                    *schedule_created
                        .entry((weekday as i64, created.clone()))
                        .or_insert(0) += 1;
                }
            }
        }
    }

    let mut day_done_scheduled: BTreeMap<String, i64> = BTreeMap::new();
    for (item_id, date) in item_date_live_rows.keys() {
        let Some(item) = live.get(item_id.as_str()) else {
            continue;
        };
        let Some(day) = xp::parse(date) else {
            continue;
        };
        if xp::scheduled_on_parts(
            &item.kind,
            item.weekdays,
            item.active,
            item.deleted_at.is_some(),
            item.created_on.as_deref(),
            day,
        ) {
            *day_done_scheduled.entry(date.clone()).or_insert(0) += 1;
        }
    }

    Snapshot {
        item_date_live_rows,
        xp_base,
        day_done_scheduled,
        schedule_weekdays,
        schedule_created,
    }
}

/// Build the ledger for every account whose marker says it has not been built yet.
///
/// Runs once after the migration set on an upgraded database, before the server starts serving,
/// and is a no-op on every later start. Each account is rebuilt in its own transaction and only
/// then marked built, so an interrupted upgrade leaves the rest for the next attempt. Accounts
/// created later are seeded as built by migration 0014's trigger and maintained by sync.
pub async fn backfill_pending(pool: &Db) -> Result<usize, sqlx::Error> {
    let present: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'stats_state')",
    )
    .fetch_one(pool)
    .await?;
    if !present {
        // A test-built migrator without the ledger version: nothing to do.
        return Ok(0);
    }
    let pending: Vec<i64> =
        sqlx::query_scalar("SELECT user_id FROM stats_state WHERE built = 0 ORDER BY user_id")
            .fetch_all(pool)
            .await?;
    let mut built = 0;
    for user_id in pending {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id = ?)")
            .bind(user_id)
            .fetch_one(pool)
            .await?;
        if !exists {
            // Removed between the scan and now; its rows cascaded with it.
            continue;
        }
        UserDb::new(pool, user_id).rebuild_stats().await?;
        built += 1;
    }
    if built > 0 {
        // Debug, not info: the CLI shares the server binary and writes its answer to stdout.
        tracing::debug!(
            users = built,
            "built the statistics ledger from retained history"
        );
    }
    Ok(built)
}
