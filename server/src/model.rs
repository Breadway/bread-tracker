//! Wire and storage types, with validation for client-supplied rows.

use chrono::{NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};

pub const KINDS: [&str; 3] = ["chore", "habit", "todo"];

/// Permission levels a family link or item override can hold. The database enforces the same
/// list; no client-write path accepts either field, so these are server-set in a later change.
pub const PERMISSIONS: [&str; 3] = ["complete_only", "complete_and_reschedule", "full"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub weekdays: i64,
    pub remind_time: Option<String>,
    pub points: i64,
    pub due_date: Option<String>,
    pub sort: i64,
    pub active: bool,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    /// The day the item was created, `YYYY-MM-DD`, in the creator's local date.
    ///
    /// Additive on the wire. Missing means "no lower bound", which is how every item that
    /// predates TRACK-65 is stored, so upgrading changes no existing stats. The server records
    /// the first value it sees and never overwrites it (see `store::UserDb::apply_push`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_on: Option<String>,
    /// The user who created this item. Server-set provenance: a client's value is ignored,
    /// and an item created through the normal sync path is attributed to its owner.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<i64>,
    /// An item-level permission override. NULL means the family link's default applies.
    /// Server-set for now; clients cannot write it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_override: Option<String>,
    #[serde(default)]
    pub seq: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Completion {
    pub item_id: String,
    pub date: String,
    pub completed_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    #[serde(default)]
    pub seq: i64,
}

fn bad(msg: &str) -> String {
    msg.to_string()
}

impl Item {
    /// `today` is the server's current UTC date; a client's local creation date can be at most
    /// one day ahead of it.
    pub fn validate(&self, today: NaiveDate) -> Result<(), String> {
        uuid::Uuid::parse_str(&self.id).map_err(|_| bad("item id must be a uuid"))?;
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(bad("item kind must be chore, habit or todo"));
        }
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > 200 {
            return Err(bad("item name must be 1 to 200 characters"));
        }
        if !(0..=127).contains(&self.weekdays) {
            return Err(bad("weekdays must be a 7-bit mask"));
        }
        if let Some(t) = &self.remind_time {
            NaiveTime::parse_from_str(t, "%H:%M").map_err(|_| bad("remind_time must be HH:MM"))?;
        }
        if let Some(d) = &self.due_date {
            NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map_err(|_| bad("due_date must be YYYY-MM-DD"))?;
        }
        if let Some(c) = &self.created_on {
            let date = NaiveDate::parse_from_str(c, "%Y-%m-%d")
                .map_err(|_| bad("created_on must be YYYY-MM-DD"))?;
            let (y, m, d) = EARLIEST_COMPLETION_DATE;
            if date < NaiveDate::from_ymd_opt(y, m, d).unwrap()
                || date > today + chrono::Duration::days(MAX_CREATED_ON_DAYS_AHEAD)
            {
                return Err(bad("created_on is out of range"));
            }
        }
        if !(0..=1000).contains(&self.points) {
            return Err(bad("points must be 0 to 1000"));
        }
        Ok(())
    }
}

/// Completions dated further ahead of the server's UTC date than this are refused. Real
/// phones are at most about a day ahead (time zones); the rest is margin for clock error.
pub const MAX_COMPLETION_DAYS_AHEAD: i64 = 7;
/// Nothing in this app predates this, so an earlier date is a bug or garbage.
pub const EARLIEST_COMPLETION_DATE: (i32, u32, u32) = (2020, 1, 1);
/// A client's local date can be ahead of the server's UTC date by its offset, but no further.
pub const MAX_CREATED_ON_DAYS_AHEAD: i64 = 1;

impl Completion {
    /// `today` is the server's current UTC date.
    pub fn validate(&self, today: NaiveDate) -> Result<(), String> {
        uuid::Uuid::parse_str(&self.item_id)
            .map_err(|_| bad("completion item_id must be a uuid"))?;
        let date = NaiveDate::parse_from_str(&self.date, "%Y-%m-%d")
            .map_err(|_| bad("date must be YYYY-MM-DD"))?;
        let (y, m, d) = EARLIEST_COMPLETION_DATE;
        if date > today + chrono::Duration::days(MAX_COMPLETION_DAYS_AHEAD)
            || date < NaiveDate::from_ymd_opt(y, m, d).unwrap()
        {
            return Err(bad("completion date is out of range"));
        }
        Ok(())
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct SyncRequest {
    #[serde(default)]
    pub epoch: Option<String>,
    #[serde(default)]
    pub since: i64,
    #[serde(default)]
    pub items: Vec<Item>,
    #[serde(default)]
    pub completions: Vec<Completion>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncResponse {
    /// Database generation. Missing remains compatible with older servers and clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub epoch: Option<String>,
    pub cursor: i64,
    pub items: Vec<Item>,
    pub completions: Vec<Completion>,
    pub server_time: i64,
}
