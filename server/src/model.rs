//! Wire and storage types, with validation for client-supplied rows.

use chrono::{NaiveDate, NaiveTime};
use serde::{Deserialize, Serialize};

pub const KINDS: [&str; 3] = ["chore", "habit", "todo"];

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
    pub fn validate(&self) -> Result<(), String> {
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
